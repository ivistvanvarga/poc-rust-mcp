//! The MCP features beyond tools, driven through the real stdio transport.
//!
//! `tests/stdio_protocol.rs` covers protocol mechanics; this file proves each server-side feature
//! rmcp 0.1.5 exposes is actually **reachable through the protocol** — which a unit test cannot
//! show, because a handler can be perfectly correct and never routed:
//!
//! | Feature | Method(s) | What is proved here |
//! | --- | --- | --- |
//! | prompts | `prompts/list`, `prompts/get` | catalogue, rendering, argument validation |
//! | resources | `resources/list`, `resources/templates/list`, `resources/read` | catalogue, routing, degradation |
//! | subscription | `resources/subscribe`, `unsubscribe`, `notifications/resources/updated` | notifications arrive, and only for what changed |
//! | logging | `logging/setLevel`, `notifications/message` | the level filter actually suppresses |
//! | completion | `completion/complete` | prompt arguments and template placeholders |
//! | pagination | cursor on all four `*/list` | a full walk with no gaps, duplicates or infinite loop |
//!
//! Storage-dependent assertions run with `DATABASE_URL` unset, so the suite stays hermetic: the
//! point of most of them is the *degradation* path, which needs no database. The one place a live
//! database would change the answer is noted where it occurs.

mod support;

use serde_json::{Value, json};
use support::{Server, cursor};

/// The history resource, and the operation-scoped ones, spelled out rather than imported so the
/// wire format itself is asserted.
const HISTORY_URI: &str = "calc://history";
const MUL_OPERATION_URI: &str = "calc://history/operation/mul";
const ADD_OPERATION_URI: &str = "calc://history/operation/add";

/// Page size small enough that the seven tools do not fit on one page, so a cursor must be walked.
const SMALL_PAGE_SIZE: &str = "3";

/// Every buffered resource-updated URI, sorted, so two sets can be compared without depending on the
/// order detached tasks happened to run in.
fn announced(server: &Server) -> Vec<String> {
    let mut uris: Vec<String> = server
        .buffered("notifications/resources/updated")
        .iter()
        .map(|update| update["params"]["uri"].as_str().expect("a uri").to_owned())
        .collect();
    uris.sort_unstable();
    uris
}

fn messages(values: &Value) -> Vec<&str> {
    values
        .as_array()
        .expect("completion values must be an array")
        .iter()
        .map(|value| value.as_str().expect("completion values must be strings"))
        .collect()
}

#[tokio::test]
async fn prompts_list_every_prompt_with_its_arguments() {
    let mut server = Server::start(None).await;
    let response = server.request(10, "prompts/list", json!({})).await;
    let prompts = response["result"]["prompts"]
        .as_array()
        .expect("prompts/list returned no prompts array");

    assert_eq!(prompts.len(), 2, "{prompts:?}");
    for prompt in prompts {
        assert!(
            prompt["description"]
                .as_str()
                .is_some_and(|text| !text.is_empty()),
            "prompt {prompt} has no description"
        );
        // Every argument must say whether it is required, or a client cannot decide what to send.
        for argument in prompt["arguments"].as_array().into_iter().flatten() {
            assert!(
                argument["required"].is_boolean(),
                "argument {argument} does not declare `required`"
            );
        }
    }

    let names: Vec<&str> = prompts
        .iter()
        .map(|prompt| prompt["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["review_calculation_history", "check_storage_health"]
    );
}

#[tokio::test]
async fn prompts_get_renders_messages_that_name_a_resource_the_server_serves() {
    let mut server = Server::start(None).await;

    let response = server
        .request(
            11,
            "prompts/get",
            json!({ "name": "review_calculation_history" }),
        )
        .await;
    let text = response["result"]["messages"][0]["content"]["text"]
        .as_str()
        .expect("a prompt must render text content")
        .to_owned();
    assert!(text.contains(HISTORY_URI), "{text}");

    // The narrowed form must point at the operation-scoped resource, not the whole log.
    let narrowed = server
        .request(
            12,
            "prompts/get",
            json!({ "name": "review_calculation_history", "arguments": { "operation": "div" } }),
        )
        .await;
    let text = narrowed["result"]["messages"][0]["content"]["text"]
        .as_str()
        .expect("a prompt must render text content");
    assert!(text.contains("calc://history/operation/div"), "{text}");
}

#[tokio::test]
async fn prompts_get_rejects_an_unknown_prompt_or_argument() {
    let mut server = Server::start(None).await;

    let unknown = server
        .request(13, "prompts/get", json!({ "name": "review_everything" }))
        .await;
    assert_eq!(unknown["error"]["code"], json!(-32602), "{unknown}");
    assert!(
        unknown["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("review_calculation_history")),
        "the error should list the real prompts: {unknown}"
    );

    // A misspelled argument must not be ignored: silently dropping it would widen the prompt's
    // meaning without telling anyone.
    let misspelled = server
        .request(
            14,
            "prompts/get",
            json!({
                "name": "review_calculation_history",
                "arguments": { "operationn": "add" },
            }),
        )
        .await;
    assert_eq!(misspelled["error"]["code"], json!(-32602), "{misspelled}");

    let unknown_operation = server
        .request(
            15,
            "prompts/get",
            json!({
                "name": "review_calculation_history",
                "arguments": { "operation": "banana" },
            }),
        )
        .await;
    assert_eq!(
        unknown_operation["error"]["code"],
        json!(-32602),
        "{unknown_operation}"
    );
}

#[tokio::test]
async fn resources_list_one_resource_and_two_templates() {
    let mut server = Server::start(None).await;

    let listed = server.request(20, "resources/list", json!({})).await;
    let resources = listed["result"]["resources"]
        .as_array()
        .expect("resources/list returned no resources array");
    assert_eq!(resources.len(), 1, "{resources:?}");
    assert_eq!(resources[0]["uri"], json!(HISTORY_URI));
    assert_eq!(resources[0]["mimeType"], json!("text/plain"));

    let templates = server
        .request(21, "resources/templates/list", json!({}))
        .await;
    let templates = templates["result"]["resourceTemplates"]
        .as_array()
        .expect("resources/templates/list returned no templates array");
    assert_eq!(templates.len(), 2, "{templates:?}");

    // The per-id and per-operation URIs are templates, not enumerated resources: the id set is
    // unbounded and changes with every call, so listing it would be a lie a client could cache.
    let uris: Vec<&str> = templates
        .iter()
        .map(|template| template["uriTemplate"].as_str().unwrap())
        .collect();
    assert!(uris.contains(&"calc://history/{id}"), "{uris:?}");
    assert!(
        uris.contains(&"calc://history/operation/{operation}"),
        "{uris:?}"
    );
}

#[tokio::test]
async fn reading_the_history_resource_degrades_when_storage_is_off() {
    // Storage degrading rather than failing is the whole contract, and it must hold for resources
    // too — a client reading a resource gets an explanation, not a dead end.
    let mut server = Server::start(None).await;
    let response = server
        .request(30, "resources/read", json!({ "uri": HISTORY_URI }))
        .await;

    assert!(response.get("error").is_none(), "{response}");
    let content = &response["result"]["contents"][0];
    assert_eq!(content["uri"], json!(HISTORY_URI));
    assert!(
        content["text"]
            .as_str()
            .is_some_and(|text| text.contains("DATABASE_URL")),
        "the content should explain what to set, got {content}"
    );

    // rmcp 0.1.5 serialises `ResourceContents` with an enum-level `rename_all`, which renames the
    // variants but not the fields inside them, so a `mimeType` set here would reach the wire as
    // `mime_type` — not a key in the MCP schema. The server therefore omits the optional field
    // rather than emit it wrongly; this pins that, and would flag the day rmcp is upgraded.
    assert!(
        content.get("mimeType").is_none() && content.get("mime_type").is_none(),
        "resources/read must omit the mis-serialised media type, got {content}"
    );
}

#[tokio::test]
async fn a_uri_this_server_does_not_serve_is_resource_not_found() {
    let mut server = Server::start(None).await;

    for (id, uri) in [
        (31, "file:///etc/passwd"),
        (32, "calc://history/latest"),
        (33, "calc://history/operation/banana"),
    ] {
        let response = server
            .request(id, "resources/read", json!({ "uri": uri }))
            .await;
        assert_eq!(
            response["error"]["code"],
            json!(-32002),
            "{uri} should be RESOURCE_NOT_FOUND: {response}"
        );
    }
}

#[tokio::test]
async fn subscribing_to_a_resource_this_server_does_not_serve_is_refused() {
    // Accepting a subscription that could never fire is worse than an error the client can act on.
    let mut server = Server::start(None).await;
    let response = server
        .request(
            40,
            "resources/subscribe",
            json!({ "uri": "file:///etc/passwd" }),
        )
        .await;

    assert_eq!(response["error"]["code"], json!(-32002), "{response}");
}

#[tokio::test]
async fn a_write_notifies_the_client_of_the_resources_it_changed() {
    // Every notification claim here is made against a *quiesced* stream (`quiesce`), never against
    // whatever happens to have arrived by the time a response was read: notifications come from
    // detached tasks, so "nothing was pushed" cannot be concluded from an unread message.
    let mut server = Server::start(None).await;
    let mut id = 50;
    server
        .request(id, "resources/subscribe", json!({ "uri": HISTORY_URI }))
        .await;

    server
        .tool_text(51, "add", json!({ "a": 2, "b": 40 }))
        .await;
    id = 52;
    server.quiesce(&mut id, "tools/list", 25, 3).await;

    assert_eq!(
        announced(&server),
        [HISTORY_URI.to_owned()],
        "a subscribed write must announce the whole log, and nothing else"
    );

    server
        .request(52, "resources/unsubscribe", json!({ "uri": HISTORY_URI }))
        .await;
    server.tool_text(53, "add", json!({ "a": 1, "b": 1 })).await;
    id = 53;
    server.quiesce(&mut id, "tools/list", 25, 3).await;

    assert_eq!(
        announced(&server),
        [HISTORY_URI.to_owned()],
        "nothing may be announced after unsubscribing; the earlier write's announcement still stands"
    );
}

#[tokio::test]
async fn a_write_only_notifies_the_resources_it_actually_changes() {
    // All three resources a history write could plausibly touch are subscribed, which is the point:
    // with only the affected ones subscribed, an implementation that invalidated *more* would be
    // invisible, because the subscription filter would swallow the extra announcement.
    let mut server = Server::start(None).await;
    let mut id = 60;
    for uri in [HISTORY_URI, MUL_OPERATION_URI, ADD_OPERATION_URI] {
        server
            .request(id, "resources/subscribe", json!({ "uri": uri }))
            .await;
        id += 1;
    }

    server.tool_text(63, "mul", json!({ "a": 6, "b": 7 })).await;
    server.tool_text(64, "add", json!({ "a": 1, "b": 2 })).await;
    server.quiesce(&mut id, "tools/list", 30, 4).await;

    // Each write invalidates exactly two resources: the whole log, and its own operation slice.
    // Compared as a sorted multiset, because the writes announce from separate detached tasks and
    // may interleave freely; only the order *within* one write is fixed.
    assert_eq!(
        announced(&server),
        [
            HISTORY_URI.to_owned(),
            HISTORY_URI.to_owned(),
            ADD_OPERATION_URI.to_owned(),
            MUL_OPERATION_URI.to_owned(),
        ],
        "each write must announce exactly the two resources it invalidates"
    );
}

#[tokio::test]
async fn logging_set_level_controls_which_notifications_arrive() {
    let mut server = Server::start(None).await;
    let mut id = 71;

    // Info is the default floor, so a write is announced without any setup.
    server.tool_text(70, "add", json!({ "a": 1, "b": 2 })).await;
    server.quiesce(&mut id, "tools/list", 25, 3).await;

    let logged = server.notification("notifications/message").await;
    assert_eq!(logged["params"]["level"], json!("info"));
    assert_eq!(logged["params"]["logger"], json!("poc-rust-mcp"));
    assert!(
        logged["params"]["data"]
            .as_str()
            .is_some_and(|data| data.contains("recorded add")),
        "the log line should name the calculation: {logged}"
    );

    // Raising the floor must silence subsequent writes. Distinct operations keep the suppressed ones
    // identifiable in the settled set below.
    let raised = server
        .request(72, "logging/setLevel", json!({ "level": "error" }))
        .await;
    assert_eq!(raised["result"], json!({}), "{raised}");

    server.tool_text(73, "sub", json!({ "a": 9, "b": 4 })).await;
    server.tool_text(74, "mul", json!({ "a": 3, "b": 4 })).await;
    id = 75;
    server.quiesce(&mut id, "tools/list", 25, 3).await;

    assert!(
        server.buffered("notifications/message").is_empty(),
        "no write below the configured level may be announced, got {:?}",
        server.buffered("notifications/message")
    );

    // Lowering it again must bring them back, proving setLevel filters rather than breaks.
    server
        .request(76, "logging/setLevel", json!({ "level": "debug" }))
        .await;
    server
        .tool_text(77, "div", json!({ "dividend": 9.0, "divisor": 3.0 }))
        .await;
    id = 78;
    server.quiesce(&mut id, "tools/list", 25, 3).await;

    let logged = server.buffered_text("notifications/message");
    assert!(
        logged.contains("recorded div -> 3"),
        "lowering the level must resume announcements, got {logged}"
    );
    assert!(
        !logged.contains("recorded sub") && !logged.contains("recorded mul"),
        "the writes made while the floor was raised must stay suppressed, got {logged}"
    );
}

#[tokio::test]
async fn completion_answers_for_prompt_arguments_and_resource_placeholders() {
    let mut server = Server::start(None).await;

    let prompt = server
        .request(
            80,
            "completion/complete",
            json!({
                "ref": { "type": "ref/prompt", "name": "review_calculation_history" },
                "argument": { "name": "operation", "value": "" },
            }),
        )
        .await;
    assert_eq!(
        messages(&prompt["result"]["completion"]["values"]),
        ["add", "div", "mul", "sub"]
    );
    assert_eq!(prompt["result"]["completion"]["hasMore"], json!(false));

    // Filtered by what has been typed so far.
    let filtered = server
        .request(
            81,
            "completion/complete",
            json!({
                "ref": { "type": "ref/prompt", "name": "review_calculation_history" },
                "argument": { "name": "operation", "value": "s" },
            }),
        )
        .await;
    assert_eq!(
        messages(&filtered["result"]["completion"]["values"]),
        ["sub"]
    );

    let template = server
        .request(
            82,
            "completion/complete",
            json!({
                "ref": { "type": "ref/resource", "uri": "calc://history/operation/{operation}" },
                "argument": { "name": "operation", "value": "m" },
            }),
        )
        .await;
    assert_eq!(
        messages(&template["result"]["completion"]["values"]),
        ["mul"]
    );
}

#[tokio::test]
async fn completing_something_that_does_not_exist_is_an_error_not_an_empty_list() {
    // An empty list is a valid protocol answer, so it would hide the client's own mistake.
    let mut server = Server::start(None).await;

    let no_such_prompt = server
        .request(
            83,
            "completion/complete",
            json!({
                "ref": { "type": "ref/prompt", "name": "review_everything" },
                "argument": { "name": "operation", "value": "" },
            }),
        )
        .await;
    assert_eq!(
        no_such_prompt["error"]["code"],
        json!(-32602),
        "{no_such_prompt}"
    );

    let no_such_placeholder = server
        .request(
            84,
            "completion/complete",
            json!({
                "ref": { "type": "ref/resource", "uri": "calc://history" },
                "argument": { "name": "operation", "value": "" },
            }),
        )
        .await;
    assert_eq!(
        no_such_placeholder["error"]["code"],
        json!(-32602),
        "{no_such_placeholder}"
    );
}

#[tokio::test]
async fn every_list_method_pages_without_gaps_duplicates_or_an_infinite_loop() {
    // One walk over all four paginated methods with a deliberately tiny page size. This is the only
    // test that can prove pagination at all: every catalogue fits on a default page, so a cursor
    // bug would otherwise be unreachable.
    let mut server = Server::start_with(None, &[("MCP_LIST_PAGE_SIZE", SMALL_PAGE_SIZE)]).await;
    let mut id = 100;

    // "Covered exactly once" is the contract a cursor has to keep. Order is deliberately not
    // asserted here: `tools/list` is sorted by the server, but the prompt and resource catalogues
    // keep their own declaration order, and both are stable.
    let walked = |collected: Vec<String>, total: usize| {
        assert_eq!(
            collected.len(),
            total,
            "the walk must cover every entry exactly once"
        );
        let mut deduplicated = collected.clone();
        deduplicated.sort_unstable();
        deduplicated.dedup();
        assert_eq!(
            deduplicated.len(),
            collected.len(),
            "the walk must not repeat an entry"
        );
    };

    // tools/list: 7 tools over pages of 3.
    let mut names: Vec<String> = Vec::new();
    let mut next: Option<String> = None;
    for _ in 0..5 {
        let response = server
            .request(id, "tools/list", cursor(next.as_deref()))
            .await;
        id += 1;
        names.extend(
            response["result"]["tools"]
                .as_array()
                .expect("tools array")
                .iter()
                .map(|tool| tool["name"].as_str().unwrap().to_owned()),
        );
        next = response["result"]["nextCursor"].as_str().map(str::to_owned);
        if next.is_none() {
            break;
        }
    }
    assert_eq!(next, None, "the walk must terminate");
    walked(names, 7);

    // prompts/list: 2 prompts, so the second page is the (empty) tail.
    let mut names: Vec<String> = Vec::new();
    let mut pages = 0;
    let mut next: Option<String> = None;
    while pages < 5 {
        let response = server
            .request(id, "prompts/list", cursor(next.as_deref()))
            .await;
        id += 1;
        pages += 1;
        names.extend(
            response["result"]["prompts"]
                .as_array()
                .expect("prompts array")
                .iter()
                .map(|prompt| prompt["name"].as_str().unwrap().to_owned()),
        );
        next = response["result"]["nextCursor"].as_str().map(str::to_owned);
        if next.is_none() {
            break;
        }
    }
    assert_eq!(next, None);
    assert_eq!(
        pages, 1,
        "2 prompts at 3 per page fits on one page, so no cursor at all"
    );
    walked(names, 2);

    for (method, key) in [
        ("resources/list", "resources"),
        ("resources/templates/list", "resourceTemplates"),
    ] {
        let response = server.request(id, method, json!({})).await;
        id += 1;
        let entries = response["result"][key].as_array().expect("entries array");
        assert!(
            entries.len() <= 3,
            "{method} should have been paged: {entries:?}"
        );
        assert!(
            response["result"]["nextCursor"].is_null(),
            "{method} has {} entries, so one page of 3 must be the whole list",
            entries.len()
        );
    }
}

#[tokio::test]
async fn a_cursor_that_is_not_a_page_index_is_rejected() {
    let mut server = Server::start_with(None, &[("MCP_LIST_PAGE_SIZE", SMALL_PAGE_SIZE)]).await;
    let response = server
        .request(110, "tools/list", json!({ "cursor": "banana" }))
        .await;

    assert_eq!(response["error"]["code"], json!(-32602), "{response}");
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("banana")),
        "the error should quote the bad cursor: {response}"
    );
}

#[tokio::test]
async fn a_page_size_of_zero_is_rejected_at_startup() {
    // Pagination that can never advance would hang a well-behaved client, so it is a configuration
    // error rather than something to discover while paging.
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_poc-rust-mcp"));
    let output = command
        .env_remove("DATABASE_URL")
        .env("MCP_LIST_PAGE_SIZE", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .expect("failed to run the server");

    assert!(
        !output.status.success(),
        "a zero page size must be rejected"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("server.list_page_size"),
        "the error should name the offending key: {stderr}"
    );
}
