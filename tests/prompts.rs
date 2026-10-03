//! The prompt catalogue, tested directly against its public functions.
//!
//! Relocated here from `src/prompts.rs`: the catalogue is a pure function of its inputs, so the only
//! thing private about these tests was their address. The protocol-level reachability lives in
//! `tests/mcp_features.rs`, which drives `prompts/list` and `prompts/get` over real JSON-RPC — do not
//! duplicate an assertion between the two.

use poc_rust_mcp::{
    prompts::{self, CHECK_STORAGE, OPERATION_ARGUMENT, REVIEW_HISTORY},
    resources::{HISTORY_URI, OPERATION_URI_PREFIX},
    server::OPERATIONS,
};
use rmcp::model::{GetPromptResult, JsonObject, PromptMessageContent};

fn arguments(pairs: &[(&str, &str)]) -> JsonObject {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), serde_json::json!(value)))
        .collect()
}

/// The single user message a prompt renders to, which is the only shape this catalogue emits.
fn text_of(result: &GetPromptResult) -> &str {
    let message = result
        .messages
        .first()
        .expect("a prompt must produce a message");
    match &message.content {
        PromptMessageContent::Text { text } => text,
        other => panic!("expected text content, got {other:?}"),
    }
}

#[test]
fn every_catalogue_entry_has_a_description() {
    let prompts = prompts::list();
    assert_eq!(prompts.len(), 2);
    for prompt in prompts {
        assert!(
            prompt.description.is_some_and(|text| !text.is_empty()),
            "{} has no description",
            prompt.name
        );
    }
}

#[test]
fn the_name_list_matches_the_catalogue_in_order() {
    // `prompts::names()` is a hand-written list so error messages stay cheap; this is what stops it
    // from going stale when a prompt is added.
    assert_eq!(
        prompts::names(),
        prompts::list()
            .iter()
            .map(|prompt| &*prompt.name)
            .collect::<Vec<_>>()
    );
}

#[test]
fn the_review_prompt_points_at_a_resource_the_server_actually_serves() {
    let all = prompts::get(REVIEW_HISTORY, None).expect("no arguments is valid");
    assert!(
        text_of(&all).contains(HISTORY_URI),
        "the prompt should name the unfiltered resource: {}",
        text_of(&all)
    );

    let narrowed = prompts::get(
        REVIEW_HISTORY,
        Some(&arguments(&[(OPERATION_ARGUMENT, "div")])),
    )
    .expect("a known operation is valid");
    let text = text_of(&narrowed);
    assert!(
        text.contains(&format!("{OPERATION_URI_PREFIX}div")),
        "{text}"
    );
    assert!(text.contains("only the `div`"), "{text}");
}

#[test]
fn the_storage_prompt_names_the_tool_and_forbids_destructive_tools() {
    let result = prompts::get(CHECK_STORAGE, None).expect("no arguments is valid");
    let text = text_of(&result);
    assert!(text.contains("db_status"), "{text}");
    assert!(
        text.contains("Do not call `clear_calc_history`"),
        "a prompt that clears history on a read-only question is a trap: {text}"
    );
}

#[test]
fn unknown_prompts_and_arguments_are_client_errors() {
    let unknown_prompt = prompts::get("review_everything", None).expect_err("must be rejected");
    assert!(
        unknown_prompt.message.contains("review_everything"),
        "{unknown_prompt}"
    );
    assert!(
        unknown_prompt.message.contains(REVIEW_HISTORY),
        "the error should list what is available: {unknown_prompt}"
    );

    let misspelled = prompts::get(REVIEW_HISTORY, Some(&arguments(&[("operationn", "add")])))
        .expect_err("a misspelled argument must be rejected");
    assert!(misspelled.message.contains("operationn"), "{misspelled}");

    let no_arguments = prompts::get(CHECK_STORAGE, Some(&arguments(&[("operation", "add")])))
        .expect_err("must be");
    assert!(no_arguments.message.contains("operation"), "{no_arguments}");
}

#[test]
fn an_operation_no_tool_produces_is_rejected() {
    let error = prompts::get(
        REVIEW_HISTORY,
        Some(&arguments(&[(OPERATION_ARGUMENT, "banana")])),
    )
    .expect_err("must be rejected");
    assert!(error.message.contains("banana"), "{error}");
    for operation in OPERATIONS {
        assert!(
            error.message.contains(operation),
            "{error} should list {operation}"
        );
    }
    // Every operation the catalogue accepts is one a tool can actually record.
    assert!(
        prompts::get(
            REVIEW_HISTORY,
            Some(&arguments(&[(OPERATION_ARGUMENT, "mul")]))
        )
        .is_ok()
    );
}

#[test]
fn a_non_string_argument_is_rejected() {
    let arguments: JsonObject = [("operation".to_owned(), serde_json::json!(7))]
        .into_iter()
        .collect();
    let error = prompts::get(REVIEW_HISTORY, Some(&arguments)).expect_err("must be rejected");
    assert!(error.message.contains("must be a string"), "{error}");
}

#[test]
fn completions_filter_by_what_has_been_typed() {
    assert_eq!(
        prompts::completions(REVIEW_HISTORY, OPERATION_ARGUMENT, "").expect("valid"),
        ["add", "div", "mul", "sub"]
    );
    assert_eq!(
        prompts::completions(REVIEW_HISTORY, OPERATION_ARGUMENT, "d").expect("valid"),
        ["div"]
    );
    assert!(
        prompts::completions(REVIEW_HISTORY, OPERATION_ARGUMENT, "zzz")
            .expect("valid, just no matches")
            .is_empty()
    );
}

#[test]
fn completing_something_that_does_not_exist_is_an_error_not_an_empty_list() {
    // An empty list would be a valid protocol answer and would hide the client's mistake.
    let no_such_argument =
        prompts::completions(REVIEW_HISTORY, "operationn", "").expect_err("must fail");
    assert!(
        no_such_argument.message.contains("operationn"),
        "{no_such_argument}"
    );

    let no_such_prompt =
        prompts::completions("nope", OPERATION_ARGUMENT, "").expect_err("must fail");
    assert!(no_such_prompt.message.contains("nope"), "{no_such_prompt}");

    let no_arguments =
        prompts::completions(CHECK_STORAGE, OPERATION_ARGUMENT, "").expect_err("must fail");
    assert!(no_arguments.message.contains("operation"), "{no_arguments}");
}
