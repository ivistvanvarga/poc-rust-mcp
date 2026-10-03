// SPDX-License-Identifier: BSD-3-Clause
//! The `Calculator`: its tools, its `ServerHandler` hooks, and its page size.
//!
//! Relocated here from `src/server.rs`, with one deliberate rewrite: the tool functions are
//! **private**, because rmcp's `#[tool]` macro makes them so, and `server::testing` deliberately does
//! not widen that. So `arithmetic_tools_report_results` and friends call the tools the way a client
//! does — through `ServerHandler::call_tool` — which is also a better test, since it exercises the
//! dispatch rather than the function.
//!
//! Protocol-level behaviour lives in `tests/mcp_features.rs` and `tests/stdio_protocol.rs`; this file
//! covers handler logic directly, where a failure points at one function.

mod support;

use poc_rust_mcp::{
    db::Store,
    prompts, resources,
    server::{Calculator, testing},
};
use rmcp::{
    ServerHandler,
    model::{
        ArgumentInfo, CallToolRequestParam, CompleteRequestParam, ListToolsResult, LoggingLevel,
        PaginatedRequestParam, PaginatedRequestParamInner, PromptReference, Reference,
        ResourceReference, SubscribeRequestParam,
    },
};
use serde_json::{Value, json};
use std::borrow::Cow;
use support::request_context;

fn calculator() -> Calculator {
    Calculator::new(Store::disabled())
}

fn with_page_size(page_size: usize) -> Calculator {
    Calculator::with_list_page_size(Store::disabled(), page_size)
}

/// Call a tool the way a client does and return its text, asserting it did not fail.
async fn call(calculator: &Calculator, name: &str, arguments: Value) -> String {
    let result = call_result(calculator, name, arguments).await;
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

/// A `tools/call` request in the shape rmcp expects.
fn call_param(name: &str, arguments: Value) -> CallToolRequestParam {
    CallToolRequestParam {
        name: Cow::Owned(name.to_owned()),
        arguments: Some(
            arguments
                .as_object()
                .expect("tool arguments must be a JSON object")
                .clone(),
        ),
    }
}

/// Call a tool and return the whole `CallToolResult`, error content included.
async fn call_result(calculator: &Calculator, name: &str, arguments: Value) -> Value {
    let result = calculator
        .call_tool(call_param(name, arguments), request_context())
        .await
        .expect("a known tool should dispatch");
    serde_json::to_value(result).expect("a tool result must serialise")
}

fn cursor(cursor: &str) -> PaginatedRequestParam {
    Some(PaginatedRequestParamInner {
        cursor: Some(cursor.to_owned()),
    })
}

fn prompt_ref(name: &str, argument: &str, value: &str) -> CompleteRequestParam {
    CompleteRequestParam {
        r#ref: Reference::Prompt(PromptReference {
            name: name.to_owned(),
        }),
        argument: ArgumentInfo {
            name: argument.to_owned(),
            value: value.to_owned(),
        },
    }
}

fn resource_ref(uri: &str, argument: &str, value: &str) -> CompleteRequestParam {
    CompleteRequestParam {
        r#ref: Reference::Resource(ResourceReference {
            uri: uri.to_owned(),
        }),
        argument: ArgumentInfo {
            name: argument.to_owned(),
            value: value.to_owned(),
        },
    }
}

fn tool_names(result: &ListToolsResult) -> Vec<&str> {
    result.tools.iter().map(|tool| tool.name.as_ref()).collect()
}

#[tokio::test]
async fn arithmetic_tools_report_results() {
    let calculator = calculator();
    assert_eq!(
        call(&calculator, "add", json!({ "a": 2, "b": 3 })).await,
        "2 + 3 = 5"
    );
    assert_eq!(
        call(&calculator, "sub", json!({ "a": 7, "b": 2 })).await,
        "7 - 2 = 5"
    );
    assert_eq!(
        call(&calculator, "mul", json!({ "a": 7, "b": 2 })).await,
        "7 * 2 = 14"
    );
}

#[tokio::test]
async fn div_guards_against_zero_divisor() {
    let calculator = calculator();
    assert_eq!(
        call(
            &calculator,
            "div",
            json!({ "dividend": 9.0, "divisor": 2.0 })
        )
        .await,
        "9 / 2 = 4.5"
    );

    // A failed tool is a successful JSON-RPC result carrying the message, so the model can react to it.
    let by_zero = call_result(
        &calculator,
        "div",
        json!({ "dividend": 9.0, "divisor": 0.0 }),
    )
    .await;
    assert_eq!(by_zero["isError"], json!(true));
    assert_eq!(by_zero["content"][0]["text"], json!("division by zero"));
}

#[tokio::test]
async fn an_unknown_tool_is_an_invalid_params_error() {
    // `tests/stdio_protocol.rs` covers that *some* error reaches the client; this pins which one, so a
    // regression that turns a bad tool name into a tool-level `isError` result (which a model would
    // try to interpret as a calculation failure) is caught here.
    let error = calculator()
        .call_tool(call_param("nope", json!({})), request_context())
        .await
        .expect_err("an unknown tool must not succeed");
    assert_eq!(error.code.0, -32602, "{error}");
    assert_eq!(error.message, "tool not found", "{error}");
}

#[test]
fn tool_names_are_sorted_and_complete() {
    assert_eq!(
        calculator().tool_names(),
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
}

#[test]
fn server_info_advertises_only_capabilities_that_are_implemented() {
    let info = calculator().get_info();
    assert!(info.capabilities.tools.is_some());
    assert!(info.capabilities.prompts.is_some());
    assert!(info.capabilities.resources.is_some());
    assert!(
        info.capabilities
            .resources
            .is_some_and(|resources| resources.subscribe == Some(true)),
        "resources/subscribe is implemented, so it must be advertised"
    );
    assert!(info.capabilities.logging.is_some());
    assert_eq!(info.server_info.name, "poc-rust-mcp");
    assert!(info.instructions.is_some());

    // The catalogues never change, so a client must not be told to watch for changes.
    assert!(
        info.capabilities
            .tools
            .is_some_and(|tools| tools.list_changed.is_none()),
        "tool_list_changed must not be advertised for a static tool box"
    );
}

#[tokio::test]
async fn the_tool_list_is_sorted_and_paginated() {
    let calculator = with_page_size(3);
    let context = request_context();

    let first = calculator
        .list_tools(None, context.clone())
        .await
        .expect("listing should succeed");
    assert_eq!(
        tool_names(&first),
        ["add", "calc_history", "clear_calc_history"]
    );
    let second_cursor = first.next_cursor.expect("a second page must be offered");

    let second = calculator
        .list_tools(cursor(&second_cursor), context.clone())
        .await
        .expect("listing should succeed");
    assert_eq!(tool_names(&second), ["db_status", "div", "mul"]);
    let third_cursor = second.next_cursor.expect("a third page must be offered");

    let third = calculator
        .list_tools(cursor(&third_cursor), context)
        .await
        .expect("listing should succeed");
    assert_eq!(tool_names(&third), ["sub"]);
    assert_eq!(third.next_cursor, None, "the last page ends the walk");
}

#[tokio::test]
async fn every_catalogue_fits_on_one_page_by_default() {
    // The default page size is larger than any catalogue here, so a client never has to walk a cursor
    // for the small lists — but `nextCursor` must still be absent, not merely unused.
    let calculator = calculator();
    let context = request_context();

    let tools = calculator
        .list_tools(None, context.clone())
        .await
        .expect("tools");
    assert_eq!(tools.tools.len(), 7);
    assert_eq!(tools.next_cursor, None);

    let listed = calculator
        .list_prompts(None, context.clone())
        .await
        .expect("prompts");
    assert_eq!(listed.prompts.len(), prompts::names().len());
    assert_eq!(listed.next_cursor, None);

    let listed = calculator
        .list_resources(None, context.clone())
        .await
        .expect("resources");
    assert_eq!(listed.resources.len(), resources::list().len());
    assert_eq!(listed.next_cursor, None);

    let listed = calculator
        .list_resource_templates(None, context)
        .await
        .expect("templates");
    assert_eq!(
        listed.resource_templates.len(),
        resources::templates().len()
    );
    assert_eq!(listed.next_cursor, None);
}

#[tokio::test]
async fn a_cursor_that_is_not_a_page_index_is_a_client_error() {
    // Silently answering with the whole list would hide the client's bug.
    let error = calculator()
        .list_tools(cursor("banana"), request_context())
        .await
        .expect_err("must be rejected");
    assert!(error.message.contains("banana"), "{error}");

    // A cursor past the end is a client that walked too far, not a protocol error: it is answered with
    // an empty final page rather than an error, so a simple loop terminates.
    let past_the_end = calculator()
        .list_tools(cursor("9999"), request_context())
        .await
        .expect("an over-run cursor is not an error");
    assert!(past_the_end.tools.is_empty());
    assert_eq!(past_the_end.next_cursor, None);
}

#[tokio::test]
async fn a_page_size_of_zero_would_never_terminate_and_is_clamped() {
    let calculator = with_page_size(0);
    let first = calculator
        .list_tools(None, request_context())
        .await
        .expect("listing should succeed");
    assert_eq!(first.tools.len(), 1, "a zero page size must become one");
    assert!(first.next_cursor.is_some());
}

#[tokio::test]
async fn subscribing_is_refused_for_a_resource_this_server_does_not_serve() {
    // Accepting a subscription that could never fire is a silent no-op the client cannot detect.
    let error = calculator()
        .subscribe(
            SubscribeRequestParam {
                uri: "file:///etc/passwd".to_owned(),
            },
            request_context(),
        )
        .await
        .expect_err("must be refused");
    assert!(error.message.contains("file:///etc/passwd"), "{error}");
}

#[tokio::test]
async fn unsubscribing_something_never_subscribed_is_not_an_error() {
    calculator()
        .unsubscribe(
            rmcp::model::UnsubscribeRequestParam {
                uri: resources::HISTORY_URI.to_owned(),
            },
            request_context(),
        )
        .await
        .expect("the client's intent is satisfied either way");
}

#[tokio::test]
async fn completion_answers_for_prompts_and_resource_templates() {
    let calculator = calculator();
    let context = request_context();

    let prompt = calculator
        .complete(
            prompt_ref(prompts::REVIEW_HISTORY, prompts::OPERATION_ARGUMENT, "s"),
            context.clone(),
        )
        .await
        .expect("completing a known prompt argument should succeed");
    assert_eq!(prompt.completion.values, ["sub"]);
    assert_eq!(prompt.completion.total, Some(1));
    assert_eq!(prompt.completion.has_more, Some(false));

    let template = calculator
        .complete(
            resource_ref(
                &format!("{}{{operation}}", resources::OPERATION_URI_PREFIX),
                "operation",
                "m",
            ),
            context,
        )
        .await
        .expect("completing a template placeholder should succeed");
    assert_eq!(template.completion.values, ["mul"]);
}

#[tokio::test]
async fn completion_rejects_something_that_does_not_exist() {
    // An empty list is a valid protocol answer, so it would hide the client's mistake.
    let error = calculator()
        .complete(
            prompt_ref("review_everything", prompts::OPERATION_ARGUMENT, ""),
            request_context(),
        )
        .await
        .expect_err("must be rejected");
    assert!(error.message.contains("review_everything"), "{error}");
}

#[test]
fn log_levels_rank_in_the_order_syslog_defines() {
    let ordered = [
        LoggingLevel::Debug,
        LoggingLevel::Info,
        LoggingLevel::Notice,
        LoggingLevel::Warning,
        LoggingLevel::Error,
        LoggingLevel::Critical,
        LoggingLevel::Alert,
        LoggingLevel::Emergency,
    ];
    for pair in ordered.windows(2) {
        assert!(
            testing::rank(pair[0].clone()) < testing::rank(pair[1].clone()),
            "{:?} should rank below {:?}",
            pair[0],
            pair[1]
        );
    }
}

#[tokio::test]
async fn set_level_drops_notifications_below_it() {
    let calculator = calculator();
    assert!(
        testing::level_enabled(&calculator, LoggingLevel::Info),
        "Info is the default floor"
    );
    assert!(testing::level_enabled(&calculator, LoggingLevel::Emergency));

    calculator
        .set_level(
            rmcp::model::SetLevelRequestParam {
                level: LoggingLevel::Error,
            },
            request_context(),
        )
        .await
        .expect("setting the level should succeed");

    assert!(!testing::level_enabled(&calculator, LoggingLevel::Info));
    assert!(testing::level_enabled(&calculator, LoggingLevel::Error));
    assert!(testing::level_enabled(&calculator, LoggingLevel::Critical));
}
