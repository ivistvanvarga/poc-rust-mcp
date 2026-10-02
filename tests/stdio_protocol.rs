//! End-to-end tests that drive the compiled binary over the real stdio JSON-RPC transport.
//!
//! These are the only tests that exercise `initialize`, `tools/list`, `tools/call`, the JSON-RPC
//! error paths and the stdout discipline together. Everything else is unit-tested closer to the
//! code.
//!
//! Two transport quirks this file exists to protect against:
//!   * rmcp 0.1.5 answers the request, so the tests must read each response *before* closing stdin;
//!     dropping stdin makes the server shut down and silently drop in-flight responses.
//!   * Any non-JSON byte on the server's stdout (or on this side's stdin) corrupts the stream, so
//!     stdout is asserted to be pure JSON.

use std::{process::Stdio, time::Duration};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

/// Generous ceiling for a single response; the server is local and answers in milliseconds.
/// Anything slower means something is blocking, and the test should fail rather than hang.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// `DATABASE_URL` pointing at a port nothing listens on.
const DEAD_DATABASE_URL: &str = "postgres://mcp:mcp@127.0.0.1:1/mcp";

/// The MCP revision implemented by rmcp 0.1.5.
const PROTOCOL_VERSION: &str = "2024-11-05";

struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    initialize_result: Value,
}

impl Server {
    /// Start the server with `DATABASE_URL` unset, run the MCP handshake, and return on success.
    async fn start(database_url: Option<&str>) -> Self {
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

        let mut child = command.spawn().expect("failed to spawn MCP server binary");
        let stdin = child.stdin.take().expect("server stdin was not piped");
        let stdout = BufReader::new(child.stdout.take().expect("server stdout was not piped"));
        let mut server = Self {
            child,
            stdin,
            stdout,
            initialize_result: Value::Null,
        };

        server.initialize_result =
            server.request(1, "initialize", initialize_params()).await["result"].clone();
        server.notify("notifications/initialized", json!({})).await;
        server
    }

    /// The `initialize` result captured during startup.
    const fn initialize_result(&self) -> &Value {
        &self.initialize_result
    }

    /// Stop the server and reap it, so no stray process is left behind.
    async fn shutdown(mut self) {
        self.child.kill().await.expect("failed to stop MCP server");
    }

    async fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await;
    }

    async fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
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

    /// Read lines until the response with `id` shows up, skipping anything else the server emits.
    async fn response(&mut self, id: i64) -> Value {
        loop {
            let mut line = String::new();
            let read = tokio::time::timeout(RESPONSE_TIMEOUT, self.stdout.read_line(&mut line))
                .await
                .unwrap_or_else(|_| {
                    panic!("timed out after {RESPONSE_TIMEOUT:?} waiting for response id={id}")
                })
                .expect("failed to read from server stdout");

            assert_ne!(read, 0, "server closed stdout before responding to id={id}");
            let line = line.trim();
            assert!(!line.is_empty(), "server wrote a blank line to stdout");

            // Anything that is not JSON means the server wrote a diagnostic to stdout and
            // corrupted the protocol stream.
            let message: Value = serde_json::from_str(line).unwrap_or_else(|error| {
                panic!("server wrote non-JSON to stdout: {line:?} ({error})")
            });

            if message.get("id").and_then(Value::as_i64) == Some(id) {
                return message;
            }
        }
    }

    /// Call a tool and return the `result` object.
    async fn call_tool(&mut self, id: i64, name: &str, arguments: Value) -> Value {
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
    async fn tool_text(&mut self, id: i64, name: &str, arguments: Value) -> String {
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
}

fn initialize_params() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": { "name": "integration-test", "version": "0.0.1" },
    })
}

#[tokio::test]
async fn initialize_advertises_tools_capability_and_real_server_identity() {
    let server = Server::start(None).await;
    let result = server.initialize_result();

    assert_eq!(result["protocolVersion"], json!(PROTOCOL_VERSION));
    // `ServerInfo::default()` from rmcp would report name "rmcp" and `"capabilities": {}`, which
    // makes clients see zero tools.
    assert_eq!(result["serverInfo"]["name"], env!("CARGO_PKG_NAME"));
    assert_eq!(result["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(result["capabilities"]["tools"], json!({}));
    assert!(
        result["instructions"]
            .as_str()
            .is_some_and(|instructions| instructions.contains("DATABASE_URL")),
        "instructions should mention how storage is configured: {result}"
    );
}

#[tokio::test]
async fn tools_list_exposes_every_tool_with_an_object_schema() {
    let mut server = Server::start(None).await;

    let response = server.request(2, "tools/list", json!({})).await;
    let tools = response["result"]["tools"]
        .as_array()
        .expect("tools/list returned no tools array");

    let mut names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "add",
            "calc_history",
            "clear_calc_history",
            "db_status",
            "div",
            "mul",
            "sub",
        ]
    );

    for tool in tools {
        assert!(
            tool["description"].as_str().is_some_and(|d| !d.is_empty()),
            "tool {tool} has no description"
        );
        assert_eq!(
            tool["inputSchema"]["type"],
            json!("object"),
            "tool {tool} has a non-object input schema"
        );
    }
}

#[tokio::test]
async fn arithmetic_tools_return_results_as_text_content() {
    let mut server = Server::start(None).await;

    assert_eq!(
        server
            .tool_text(20, "add", json!({ "a": 2, "b": 40 }))
            .await,
        "2 + 40 = 42"
    );
    assert_eq!(
        server.tool_text(21, "sub", json!({ "a": 7, "b": 2 })).await,
        "7 - 2 = 5"
    );
    assert_eq!(
        server.tool_text(22, "mul", json!({ "a": 7, "b": 6 })).await,
        "7 * 6 = 42"
    );
    assert_eq!(
        server
            .tool_text(23, "div", json!({ "dividend": 9.0, "divisor": 2.0 }))
            .await,
        "9 / 2 = 4.5"
    );
}

#[tokio::test]
async fn division_by_zero_is_a_tool_error_not_a_protocol_error() {
    let mut server = Server::start(None).await;

    let result = server
        .call_tool(30, "div", json!({ "dividend": 9.0, "divisor": 0.0 }))
        .await;

    // A failed tool must be reported as a successful JSON-RPC response with isError set, so the
    // model can see and react to the message.
    assert_eq!(result["isError"], json!(true));
    assert_eq!(result["content"][0]["text"], json!("division by zero"));
}

#[tokio::test]
async fn malformed_arguments_produce_a_json_rpc_error() {
    let mut server = Server::start(None).await;

    let response = server
        .request(
            40,
            "tools/call",
            json!({ "name": "add", "arguments": { "a": "not-a-number", "b": 1 } }),
        )
        .await;

    assert_eq!(response["error"]["code"], json!(-32602), "{response}");
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("i32")),
        "error should name the expected type: {response}"
    );
}

#[tokio::test]
async fn unknown_tool_is_rejected_rather_than_silently_ignored() {
    let mut server = Server::start(None).await;

    let response = server
        .request(50, "tools/call", json!({ "name": "nope", "arguments": {} }))
        .await;

    assert!(response.get("error").is_some(), "{response}");
}

#[tokio::test]
async fn storage_tools_report_disabled_storage_and_calculators_keep_working() {
    // No DATABASE_URL at all: the server must still be fully usable as a calculator.
    let mut server = Server::start(None).await;

    for (id, name, arguments) in [
        (60, "db_status", json!({})),
        (61, "calc_history", json!({ "limit": 10 })),
        (62, "clear_calc_history", json!({})),
    ] {
        let result = server.call_tool(id, name, arguments).await;
        assert_eq!(
            result["isError"],
            json!(true),
            "{name} should report no storage"
        );
        assert!(
            result["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("DATABASE_URL")),
            "{name} should explain that storage is unconfigured, got {result}"
        );
    }

    // The important guarantee: arithmetic is unaffected by the missing database.
    assert_eq!(
        server.tool_text(63, "mul", json!({ "a": 6, "b": 7 })).await,
        "6 * 7 = 42"
    );
}

#[tokio::test]
async fn arithmetic_does_not_block_while_the_database_is_unreachable() {
    let mut server = Server::start(Some(DEAD_DATABASE_URL)).await;

    // Regression guard: history writes once waited for sqlx's connect timeout, so every calculator
    // call stalled for as long as the database took to give up. The budget must therefore be well
    // under the 3s acquire timeout, otherwise a reintroduced blocking write still passes.
    let started = std::time::Instant::now();
    assert_eq!(
        server.tool_text(70, "add", json!({ "a": 1, "b": 2 })).await,
        "1 + 2 = 3"
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "add blocked for {elapsed:?} with an unreachable database"
    );

    // The storage probe may take a moment (one bounded connect attempt), but it must fail rather
    // than hang forever.
    let result = server.call_tool(71, "db_status", json!({})).await;
    assert_eq!(result["isError"], json!(true));
}

#[tokio::test]
async fn stdout_carries_only_json_rpc_messages() {
    let mut server = Server::start(None).await;

    // Exercise the paths that log the most: storage degradation, tool errors and bad arguments.
    let _ = server.tool_text(80, "add", json!({ "a": 1, "b": 1 })).await;
    let _ = server.call_tool(81, "db_status", json!({})).await;
    let _ = server
        .request(
            82,
            "tools/call",
            json!({ "name": "add", "arguments": { "a": {}, "b": 1 } }),
        )
        .await;

    // `response` already asserts JSON on every line it reads; reaching here means the whole
    // exchange was pure JSON-RPC. Shutting down must not hang or panic.
    server.shutdown().await;
}
