// SPDX-License-Identifier: BSD-3-Clause
//! End-to-end tests that drive the compiled binary over the real stdio JSON-RPC transport.
//!
//! These cover the *protocol mechanics* — the handshake, the advertised capabilities, the JSON-RPC
//! error paths and the stdout discipline. The features built on top of them (prompts, resources,
//! logging, completion, pagination) are in `tests/mcp_features.rs`; tools themselves are the
//! original subject of this file.
//!
//! Both share the harness in `tests/support/mod.rs`.

mod support;

use std::time::Duration;

use serde_json::json;
use support::{DEAD_DATABASE_URL, PROTOCOL_VERSION, Server};

/// The MCP revision implemented by rmcp 0.1.5.
/// Capability keys the server advertises at the top level of `initialize`. Nested flags —
/// `resources.subscribe`, and the `listChanged` flags — are asserted separately below.
const SERVER_CAPABILITIES: [&str; 4] = ["tools", "prompts", "resources", "logging"];

#[tokio::test]
async fn initialize_advertises_tools_capability_and_real_server_identity() {
    let server = Server::start(None).await;
    let result = server.initialize_result();

    assert_eq!(result["protocolVersion"], json!(PROTOCOL_VERSION));
    // `ServerInfo::default()` from rmcp would report name "rmcp" and `"capabilities": {}`, which
    // makes clients see zero tools.
    assert_eq!(result["serverInfo"]["name"], env!("CARGO_PKG_NAME"));
    assert_eq!(result["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(
        result["instructions"]
            .as_str()
            .is_some_and(|instructions| instructions.contains("DATABASE_URL")),
        "instructions should mention how storage is configured: {result}"
    );
}

#[tokio::test]
async fn initialize_advertises_exactly_the_capabilities_the_server_implements() {
    let server = Server::start(None).await;
    let capabilities = &server.initialize_result()["capabilities"];

    for capability in SERVER_CAPABILITIES {
        assert!(
            !capabilities[capability].is_null(),
            "{capability} must be advertised: {capabilities}"
        );
    }

    // The catalogues are all static, so a client must not be told to watch for changes: claiming
    // `listChanged` would make clients poll for a change that cannot happen.
    assert!(
        capabilities["tools"].get("listChanged").is_none(),
        "tools.listChanged must not be advertised: {capabilities}"
    );
    assert!(
        capabilities["resources"]["subscribe"] == json!(true),
        "resources/subscribe is implemented, so it must be advertised: {capabilities}"
    );
    // Sampling, elicitation and roots are *client* capabilities; a server must not claim them.
    for client_side in ["sampling", "elicitation", "roots"] {
        assert!(
            capabilities[client_side].is_null(),
            "{client_side} is a client capability, not a server one: {capabilities}"
        );
    }
}

#[tokio::test]
async fn ping_is_answered() {
    let mut server = Server::start(None).await;
    let response = server.request(2, "ping", json!({})).await;

    assert_eq!(response["result"], json!({}), "{response}");
    assert!(response.get("error").is_none(), "{response}");
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
    // The tool box is HashMap-backed; the server sorts so clients see a stable order.
    let served: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    let mut sorted = served.clone();
    sorted.sort_unstable();
    assert_eq!(served, sorted, "tools/list must arrive sorted");

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
async fn progress_and_cancellation_notifications_do_not_disturb_the_server() {
    // Both are client-initiated notifications with no response. rmcp cancels the request token
    // itself before the handler hook runs, so all that is observable — and all that is tested — is
    // that the server neither replies to them nor stops working.
    let mut server = Server::start(None).await;

    server
        .notify(
            "notifications/progress",
            json!({ "progressToken": "t1", "progress": 50, "total": 100 }),
        )
        .await;
    server
        .notify(
            "notifications/cancelled",
            json!({ "requestId": 999, "reason": "client changed its mind" }),
        )
        .await;

    // A request after them must still be answered; a notification that broke dispatch would show up
    // as a timeout here.
    assert_eq!(
        server
            .tool_text(90, "add", json!({ "a": 20, "b": 22 }))
            .await,
        "20 + 22 = 42"
    );
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
