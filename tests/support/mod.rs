// SPDX-License-Identifier: BSD-3-Clause
//! A minimal JSON-RPC client for the compiled binary, over the real stdio transport.
//!
//! Both `tests/stdio_protocol.rs` and `tests/mcp_features.rs` drive the server this way, because
//! nothing else proves that a handler is actually reachable through the protocol — a handler can
//! be perfectly correct and still never be routed. Sharing one harness keeps the awkward parts in
//! one place; each test binary compiles its own copy, which is how Cargo integration tests work.
//!
//! Two transport quirks this file exists to protect against:
//!   * rmcp 0.1.5 answers the request, so a test must read each response *before* closing stdin;
//!     dropping stdin makes the server shut down and silently drop in-flight responses.
//!   * Any non-JSON byte on the server's stdout (or on this side's stdin) corrupts the stream, so
//!     stdout is asserted to be pure JSON.

// Cargo compiles this module into each test binary that uses it, so an item only one of them needs
// is dead code in the other. Warning about that would be noise about the harness, not about a test.
#![allow(dead_code)]

use std::{process::Stdio, time::Duration};

use rmcp::{
    model::{ClientInfo, Implementation, InitializeRequestParam},
    service::{AtomicU32RequestIdProvider, Peer, RequestContext, RoleServer},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

/// Generous ceiling for a single response; the server is local and answers in milliseconds.
/// Anything slower means something is blocking, and the test should fail rather than hang.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// `DATABASE_URL` pointing at a port nothing listens on.
pub const DEAD_DATABASE_URL: &str = "postgres://mcp:mcp@127.0.0.1:1/mcp";

/// The MCP revision implemented by rmcp 0.1.5.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// How long to wait after a round-trip for detached tasks to enqueue. Comfortably longer than a
/// scheduling turn and far shorter than [`RESPONSE_TIMEOUT`], so a *present* notification is never
/// missed for being early.
pub const SETTLE: Duration = Duration::from_millis(20);

pub struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    initialize_result: Value,
    /// Server-initiated notifications seen while waiting for a response.
    ///
    /// This buffering is not optional. The server pushes `notifications/message` and
    /// `notifications/resources/updated` from detached tasks, so either can land *before* the
    /// response that caused it — and a reader that skipped it would lose it, leaving
    /// [`Self::notification`] waiting for a message that already arrived. Buffering also makes
    /// "nothing was pushed" directly observable via [`Self::buffered_notifications`], which is how
    /// a notification *filter* is tested without relying on a timeout.
    pending: Vec<Value>,
}

impl Server {
    /// Start the server with `DATABASE_URL` unset, run the MCP handshake, and return on success.
    pub async fn start(database_url: Option<&str>) -> Self {
        Self::start_with(database_url, &[]).await
    }

    /// As [`Self::start`], with extra environment for the child.
    ///
    /// Used to drive settings that only exist in configuration, such as a page size small enough
    /// to page through.
    pub async fn start_with(database_url: Option<&str>, env: &[(&str, &str)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_poc-rust-mcp"));
        command
            // Never inherit the developer's environment: an ambient DATABASE_URL would silently
            // change what these tests exercise.
            .env_remove("DATABASE_URL")
            .env("RUST_LOG", "warn")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(url) = database_url {
            command.env("DATABASE_URL", url);
        }
        for (key, value) in env {
            command.env(key, value);
        }

        let mut child = command.spawn().expect("failed to spawn MCP server binary");
        let stdin = child.stdin.take().expect("server stdin was not piped");
        let stdout = BufReader::new(child.stdout.take().expect("server stdout was not piped"));
        let mut server = Self {
            child,
            stdin,
            stdout,
            initialize_result: Value::Null,
            pending: Vec::new(),
        };

        server.initialize_result =
            server.request(1, "initialize", initialize_params()).await["result"].clone();
        server.notify("notifications/initialized", json!({})).await;
        server
    }

    /// The `initialize` result captured during startup.
    pub const fn initialize_result(&self) -> &Value {
        &self.initialize_result
    }

    /// Stop the server and reap it, so no stray process is left behind.
    pub async fn shutdown(mut self) {
        self.child.kill().await.expect("failed to stop MCP server");
    }

    pub async fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await;
    }

    pub async fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await;
        self.response(id).await
    }

    async fn send(&mut self, message: &Value) {
        let mut line = serde_json::to_string(message).expect("failed to serialise request");
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .expect("failed to write to server stdin");
        self.stdin
            .flush()
            .await
            .expect("failed to flush server stdin");
    }

    /// Read lines until the response with `id` shows up, buffering any notification on the way.
    pub async fn response(&mut self, id: i64) -> Value {
        loop {
            let message = self.read_message(id).await;
            if message.get("id").and_then(Value::as_i64) == Some(id) {
                return message;
            }
            if message.get("method").is_some() {
                self.pending.push(message);
            }
        }
    }

    /// One line off the server's stdout, asserted to be a JSON-RPC message.
    async fn read_message(&mut self, waiting_for: impl std::fmt::Display) -> Value {
        let mut line = String::new();
        let read = tokio::time::timeout(RESPONSE_TIMEOUT, self.stdout.read_line(&mut line))
            .await
            .unwrap_or_else(|_| {
                panic!("timed out after {RESPONSE_TIMEOUT:?} waiting for {waiting_for}")
            })
            .expect("failed to read from server stdout");

        assert_ne!(read, 0, "server closed stdout before {waiting_for}");

        let line = line.trim();
        assert!(!line.is_empty(), "server wrote a blank line to stdout");

        // Anything that is not JSON means the server wrote a diagnostic to stdout and corrupted the
        // protocol stream.
        serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("server wrote non-JSON to stdout: {line:?} ({error})"))
    }

    /// Take the next buffered or incoming server-initiated **notification** with `method`.
    ///
    /// The mirror of [`Self::response`], for the features where the server talks first. Anything
    /// else read on the way is buffered rather than dropped, so no test can lose a notification to
    /// a race with the response that caused it.
    pub async fn notification(&mut self, method: &str) -> Value {
        if let Some(index) = self
            .pending
            .iter()
            .position(|message| message["method"] == json!(method))
        {
            return self.pending.remove(index);
        }
        loop {
            let message = self.read_message(method).await;
            if message["method"] == json!(method) {
                return message;
            }
            self.pending.push(message);
        }
    }

    /// Notifications observed so far and not yet consumed, without waiting for more.
    ///
    /// Lets a test assert that something was *not* pushed, which a timeout cannot prove: absence
    /// is only meaningful against a known point in the stream.
    pub fn buffered_notifications(&self) -> &[Value] {
        &self.pending
    }

    /// As [`Self::notification`], but skips notifications of the same method until one whose JSON
    /// contains `needle`.
    ///
    /// Needed because every write pushes a log line, so "the next `notifications/message`" is
    /// whichever write happened first — not necessarily the one under test. Naming a sentinel
    /// (`"recorded add -> 7"`) makes the wait unambiguous, which is what lets a test assert that an
    /// *earlier* write pushed nothing: the sink is FIFO, so once a later write's notification has
    /// arrived, anything an earlier one sent must already have been delivered.
    pub async fn notification_containing(&mut self, method: &str, needle: &str) -> Value {
        loop {
            let found = self
                .pending
                .iter()
                .position(|message| {
                    message["method"] == json!(method) && message.to_string().contains(needle)
                })
                .map(|index| self.pending.remove(index));
            if let Some(message) = found {
                return message;
            }
            let message = self
                .read_message(format!("{method} containing {needle:?}"))
                .await;
            // Responses are not ours to keep; only notifications are held for later assertions.
            if message.get("method").is_some() {
                self.pending.push(message);
            }
        }
    }

    /// Drive harmless round-trips until the server has stopped pushing notifications.
    ///
    /// Push notifications originate in detached tasks and several writes may be in flight at once, so
    /// **no** particular notification's arrival proves the stream has drained — a later write's
    /// notification can precede an earlier one's, and a whole batch can still be unscheduled. Polling to
    /// quiescence is therefore the only way to assert on *everything* that was sent, which is what the
    /// subscription and log-level tests need: they must claim a set, not just find a member.
    ///
    /// Bounded twice over — at most `rounds` requests, and at least `quiet` consecutive idle ones — so a
    /// server that never stops pushing fails the test instead of hanging it.
    pub async fn quiesce(&mut self, next_id: &mut i64, method: &str, rounds: usize, quiet: usize) {
        let mut idle = 0;
        for _ in 0..rounds {
            let before = self.pending.len();
            let id = *next_id;
            *next_id += 1;
            self.request(id, method, json!({})).await;
            // A detached task may enqueue just after the response it did not block.
            tokio::time::sleep(SETTLE).await;
            if self.pending.len() == before {
                idle += 1;
            } else {
                idle = 0;
            }
            if idle >= quiet {
                return;
            }
        }
    }

    /// Notifications of `method` observed so far and not yet consumed, without waiting for more.
    ///
    /// Lets a test assert that something was *not* pushed, which a timeout cannot prove: absence is
    /// only meaningful against a known point in the stream. Scoping to one method matters, because a
    /// single tool call can push a log line *and* a resource update, so "the buffer is empty" is the
    /// wrong question when only one of the two is under test.
    pub fn buffered(&self, method: &str) -> Vec<&Value> {
        self.pending
            .iter()
            .filter(|message| message["method"] == json!(method))
            .collect()
    }

    /// The JSON of every buffered notification with `method`, flattened for substring checks.
    pub fn buffered_text(&self, method: &str) -> String {
        self.buffered(method)
            .into_iter()
            .map(|message| message.to_string())
            .collect()
    }

    /// Call a tool and return the `result` object.
    pub async fn call_tool(&mut self, id: i64, name: &str, arguments: Value) -> Value {
        let response = self
            .request(
                id,
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await;
        response
            .get("result")
            .unwrap_or_else(|| panic!("tools/call {name} returned no result: {response}"))
            .clone()
    }

    /// Call a tool that is expected to succeed and return its text content.
    pub async fn tool_text(&mut self, id: i64, name: &str, arguments: Value) -> String {
        let result = self.call_tool(id, name, arguments).await;
        assert_eq!(
            result["isError"],
            json!(false),
            "tool {name} unexpectedly failed: {result}"
        );
        result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("tool {name} returned no text content: {result}"))
            .to_owned()
    }

    /// Call a tool that is expected to fail and return its text content.
    pub async fn tool_error(&mut self, id: i64, name: &str, arguments: Value) -> String {
        let result = self.call_tool(id, name, arguments).await;
        assert_eq!(
            result["isError"],
            json!(true),
            "tool {name} unexpectedly succeeded: {result}"
        );
        result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("tool {name} returned no text content: {result}"))
            .to_owned()
    }
}

/// `params.cursor`, absent on a first page.
pub fn cursor(cursor: Option<&str>) -> Value {
    match cursor {
        None => json!({}),
        Some(cursor) => json!({ "cursor": cursor }),
    }
}

/// A `RequestContext` for calling a handler directly, without a transport.
///
/// Every rmcp handler takes one, and constructing it needs a `Peer`. The peer built here is
/// *disconnected* — the outbound half of `Peer::new` is dropped — so any attempt to send through it
/// fails immediately. That is exactly the "no client attached" case, and it is the right one for
/// these tests: they exercise handler logic, not delivery.
pub fn request_context() -> RequestContext<RoleServer> {
    RequestContext {
        ct: tokio_util::sync::CancellationToken::new(),
        id: rmcp::model::RequestId::Number(0),
        peer: Peer::new(
            std::sync::Arc::new(AtomicU32RequestIdProvider::default()),
            client_info(),
        )
        .0,
    }
}

fn client_info() -> ClientInfo {
    InitializeRequestParam {
        protocol_version: Default::default(),
        capabilities: Default::default(),
        client_info: Implementation {
            name: "integration-test".to_owned(),
            version: "0.0.1".to_owned(),
        },
    }
}

pub fn initialize_params() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": { "name": "integration-test", "version": "0.0.1" },
    })
}
