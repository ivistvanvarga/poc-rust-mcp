//! The prompt catalogue.
//!
//! A prompt is not a tool and runs nothing: `prompts/get` returns messages that the client hands to
//! a model, which then decides which tools to call. So every prompt here is written to name the
//! tool or resource it wants the model to reach for, rather than to encode the answer itself — the
//! model's context and the stored history are the inputs, not this file.
//!
//! Prompts are validated on the way out ([`get`]), not just described: an unknown prompt, an
//! unknown argument or an unknown operation is a client error, so a typo fails loudly instead of
//! producing a subtly wrong message.

use rmcp::{
    Error as McpError,
    model::{
        GetPromptResult, JsonObject, Prompt, PromptArgument, PromptMessage, PromptMessageContent,
        PromptMessageRole,
    },
};

use crate::{
    resources::{HISTORY_URI, OPERATION_URI_PREFIX},
    server::OPERATIONS,
};

/// Summarise the recorded calculation history, optionally narrowed to one operation.
pub const REVIEW_HISTORY: &str = "review_calculation_history";

/// Report whether history storage is configured and reachable, and explain any degradation.
pub const CHECK_STORAGE: &str = "check_storage_health";

/// The argument both this catalogue and the resource template use to narrow by operation, so one
/// completion list serves `prompts/get` and `resources/read` alike.
pub const OPERATION_ARGUMENT: &str = "operation";

/// Every prompt this server offers, in a stable order.
///
/// Built per call rather than held in a `static`: `Prompt` owns `String`s, so there is no const
/// form, and `prompts/list` is not hot.
#[must_use]
pub fn list() -> Vec<Prompt> {
    vec![
        Prompt::new(
            REVIEW_HISTORY,
            Some(
                "Review the recorded calculation history and summarise what has been calculated. \
                 Optionally narrowed to a single operation."
                    .to_owned(),
            ),
            Some(vec![PromptArgument {
                name: OPERATION_ARGUMENT.to_owned(),
                description: Some(
                    "Restrict the review to one operation: add, sub, mul or div. Omit for all of \
                     them."
                        .to_owned(),
                ),
                required: Some(false),
            }]),
        ),
        Prompt::new(
            CHECK_STORAGE,
            Some(
                "Report whether calculation history storage is configured and reachable, and \
                 explain what that means for recorded calculations."
                    .to_owned(),
            ),
            None,
        ),
    ]
}

/// Render a prompt into the messages a client should hand to a model.
///
/// # Errors
///
/// Returns `invalid_params` for an unknown prompt name, an unknown or non-string argument, or an
/// operation outside [`OPERATIONS`] — each naming what was accepted.
pub fn get(name: &str, arguments: Option<&JsonObject>) -> Result<GetPromptResult, McpError> {
    match name {
        REVIEW_HISTORY => {
            reject_unknown_arguments(arguments, &[OPERATION_ARGUMENT])?;
            let operation = optional_operation(arguments, OPERATION_ARGUMENT)?;
            let uri = match &operation {
                Some(operation) => format!("{OPERATION_URI_PREFIX}{operation}"),
                None => HISTORY_URI.to_owned(),
            };
            let scope = operation.as_ref().map_or_else(
                || "every operation".to_owned(),
                |operation| format!("only the `{operation}` calculations"),
            );
            Ok(user_prompt(format!(
                "Read the MCP resource {uri} and summarise {scope} in the calculation history. \
                 Call `calc_history` first if the resource read reports that storage is \
                 unavailable. Highlight anything that looks like a mistake, such as repeated \
                 operands or an unexpected number of divisions."
            )))
        }
        CHECK_STORAGE => {
            reject_unknown_arguments(arguments, &[])?;
            Ok(user_prompt(
                "Call the `db_status` tool and report whether calculation history storage is \
                 configured and reachable. If it reports that storage is unavailable, explain that \
                 the calculator still works and that no history is being recorded. Do not call \
                 `clear_calc_history`."
                    .to_owned(),
            ))
        }
        unknown => Err(McpError::invalid_params(
            format!(
                "unknown prompt {unknown:?}; expected one of {}",
                names().join(", ")
            ),
            None,
        )),
    }
}

/// Candidate values for a prompt argument, filtered by what the client has typed so far.
///
/// # Errors
///
/// Returns `invalid_params` for an unknown prompt or an argument that prompt does not have, so a
/// client asking about the wrong thing finds out rather than silently receiving nothing.
pub fn completions(name: &str, argument: &str, value: &str) -> Result<Vec<String>, McpError> {
    match (name, argument) {
        (REVIEW_HISTORY, OPERATION_ARGUMENT) => Ok(prefixed(OPERATIONS, value)),
        (CHECK_STORAGE, _) | (REVIEW_HISTORY, _) => Err(McpError::invalid_params(
            format!("prompt {name:?} has no argument {argument:?} to complete"),
            None,
        )),
        unknown => Err(McpError::invalid_params(
            format!(
                "unknown prompt {unknown:?}; expected one of {}",
                names().join(", ")
            ),
            None,
        )),
    }
}

/// The prompt names, in catalogue order.
///
/// Kept as its own list rather than derived from [`list`] so the common case — naming the
/// alternatives in an error message — does not build a `Vec<Prompt>`; the unit test below is what
/// stops the two from drifting apart.
#[must_use]
pub fn names() -> Vec<&'static str> {
    vec![REVIEW_HISTORY, CHECK_STORAGE]
}

/// One user message, which is what every prompt here produces: the model is the one doing the work.
fn user_prompt(text: String) -> GetPromptResult {
    GetPromptResult {
        description: None,
        messages: vec![PromptMessage {
            role: PromptMessageRole::User,
            content: PromptMessageContent::text(text),
        }],
    }
}

/// Reject any argument this prompt does not declare, which is how a misspelled argument is caught.
fn reject_unknown_arguments(
    arguments: Option<&JsonObject>,
    accepted: &[&str],
) -> Result<(), McpError> {
    let Some(arguments) = arguments else {
        return Ok(());
    };
    let unknown: Vec<&str> = arguments
        .keys()
        .map(String::as_str)
        .filter(|key| !accepted.contains(key))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    Err(McpError::invalid_params(
        format!(
            "unknown argument{} {}; this prompt accepts {}",
            if unknown.len() == 1 { "" } else { "s" },
            unknown
                .iter()
                .map(|name| format!("{name:?}"))
                .collect::<Vec<_>>()
                .join(", "),
            accepted.join(", "),
        ),
        None,
    ))
}

/// Read an optional operation argument, rejecting anything outside [`OPERATIONS`].
///
/// The set is closed because it is the set of tools that record history: an operation no tool can
/// produce would make the narrowed resource permanently empty.
fn optional_operation(
    arguments: Option<&JsonObject>,
    key: &str,
) -> Result<Option<String>, McpError> {
    let Some(value) = arguments.and_then(|arguments| arguments.get(key)) else {
        return Ok(None);
    };
    let Some(operation) = value.as_str() else {
        return Err(McpError::invalid_params(
            format!("argument {key:?} must be a string"),
            None,
        ));
    };
    if !OPERATIONS.contains(&operation) {
        return Err(McpError::invalid_params(
            format!(
                "unknown operation {operation:?}; expected one of {}",
                OPERATIONS.join(", ")
            ),
            None,
        ));
    }
    Ok(Some(operation.to_owned()))
}

/// Candidates starting with `value`, which is the whole of a `completion/complete` contract.
fn prefixed<'a, I>(candidates: I, value: &str) -> Vec<String>
where
    I: IntoIterator<Item = &'a str>,
{
    candidates
        .into_iter()
        .filter(|candidate| candidate.starts_with(value))
        .map(str::to_owned)
        .collect()
}
