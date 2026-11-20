// SPDX-License-Identifier: BSD-3-Clause
//! The resource surface: calculation history, exposed as readable MCP resources.
//!
//! A resource is a read-only document a client can pull in wholesale, complementing the tools: a
//! model that wants to *see* the history reads a resource, whereas a model that wants to *run*
//! arithmetic calls a tool.
//!
//! ```text
//! calc://history                       every recorded calculation, newest first   (listed)
//! calc://history/{id}                  one recorded calculation                    (template)
//! calc://history/operation/{operation} every recorded calculation of one operation (template)
//! ```
//!
//! The two per-row and per-operation URIs are **templates**, not enumerated resources: the set of
//! ids is unbounded and changes with every call, so listing them would be a lie that a client could
//! cache. [`parse`] is the single place that decides which URI means what.
//!
//! Every read goes through [`Store`], so it degrades with everything else: if storage is disabled
//! the resource still resolves and its *content* explains why it is empty, because a disabled
//! database is not a missing resource. Only a URI this server does not serve, or an id with no row
//! behind it, is a `resource_not_found`.

use rmcp::{
    ErrorData as McpError,
    model::{ReadResourceResult, Resource, ResourceContents, ResourceTemplate},
};

use crate::{
    db::{HistoryEntry, Store},
    server::OPERATIONS,
};

/// Every recorded calculation, most recent first.
pub const HISTORY_URI: &str = "calc://history";

/// Prefix of the per-operation URI; also what [`crate::prompts::OPERATION_ARGUMENT`] completes.
pub const OPERATION_URI_PREFIX: &str = "calc://history/operation/";

/// Media type of every resource here: a human-readable log, not JSON, and a client should not have
/// to guess how to parse it.
pub const MIME_TYPE: &str = "text/plain";

/// What a resource URI points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// [`HISTORY_URI`]: everything recorded.
    History,
    /// One row, by id.
    Entry(i64),
    /// Every row of one operation.
    Operation(String),
}

/// Content returned for a resource that resolved but has nothing to show.
///
/// Shared with the `calc_history` tool so a client gets the same words whichever way it asks.
pub const NO_HISTORY: &str = "no calculations recorded yet";

/// The resource list, in a stable order.
#[must_use]
pub fn list() -> Vec<Resource> {
    vec![
        Resource::new(HISTORY_URI, "calculation-history")
            .with_description(
                "Every recorded calculator call, most recent first, one per line. Empty when \
             storage is unavailable — the content says so.",
            )
            .with_mime_type(MIME_TYPE),
    ]
}

/// The resource templates, in a stable order.
#[must_use]
pub fn templates() -> Vec<ResourceTemplate> {
    vec![
        ResourceTemplate::new(
            format!("{OPERATION_URI_PREFIX}{{operation}}"),
            "calculation-history-by-operation",
        )
        .with_description(format!(
            "Recorded calculator calls narrowed to one operation ({}) and identified by \
             name, e.g. {OPERATION_URI_PREFIX}add.",
            OPERATIONS.join(", ")
        ))
        .with_mime_type(MIME_TYPE),
        ResourceTemplate::new("calc://history/{id}", "calculation-history-by-id")
            .with_description(
                "One recorded calculator call, identified by id, e.g. calc://history/7. Only \
                 ids within the recent-history window are addressable.",
            )
            .with_mime_type(MIME_TYPE),
    ]
}

/// Decide what a URI points at.
///
/// # Errors
///
/// Returns `resource_not_found` for anything this server does not serve, including a near-miss
/// scheme — a client asking for `file:///etc/passwd` must be told no, not be silently redirected.
pub fn parse(uri: &str) -> Result<Target, McpError> {
    if uri == HISTORY_URI {
        return Ok(Target::History);
    }
    let Some(rest) = uri
        .strip_prefix(HISTORY_URI)
        .and_then(|r| r.strip_prefix('/'))
    else {
        return Err(not_served(uri));
    };
    if let Some(operation) = rest.strip_prefix("operation/") {
        if !OPERATIONS.contains(&operation) {
            return Err(McpError::resource_not_found(
                format!(
                    "unknown operation {operation:?} in {uri}; expected one of {}",
                    OPERATIONS.join(", ")
                ),
                None,
            ));
        }
        return Ok(Target::Operation(operation.to_owned()));
    }
    // Anything left must be a bare id. Rejecting non-numeric input here is what keeps a URI like
    // `calc://history/latest` from being a permanent 404 that looks like an outage.
    let id = rest.parse::<i64>().ok().filter(|id| *id > 0).ok_or_else(|| {
        McpError::resource_not_found(
            format!("{uri:?} does not identify a calculation; expected calc://history, calc://history/<id> or calc://history/operation/<operation>"),
            None,
        )
    })?;
    Ok(Target::Entry(id))
}

/// Read a resource.
///
/// # Errors
///
/// Returns `resource_not_found` only for a URI this server does not serve, or an id with no row
/// behind it. A database that is down is *not* an error: the resource resolves and its content
/// carries the reason, which is the same degradation contract the tools follow.
pub async fn read(store: &Store, uri: &str) -> Result<ReadResourceResult, McpError> {
    let target = parse(uri)?;
    let entries = match store.list(store.max_history_rows()).await {
        Ok(entries) => entries,
        Err(error) => return Ok(text(uri, &error)),
    };

    match target {
        Target::History => Ok(text(uri, &render(entries.iter()))),
        Target::Entry(id) => entries
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| text(uri, &entry.to_line()))
            .ok_or_else(|| {
                McpError::resource_not_found(
                    format!(
                        "no calculation with id {id} among the most recent {}",
                        entries.len()
                    ),
                    None,
                )
            }),
        Target::Operation(operation) => {
            let matching = entries.iter().filter(|entry| entry.operation == operation);
            let rendered: Vec<&HistoryEntry> = matching.collect();
            if rendered.is_empty() {
                return Ok(text(
                    uri,
                    &format!("no `{operation}` calculations recorded"),
                ));
            }
            Ok(text(uri, &render(rendered)))
        }
    }
}

/// Candidate values for a template's `{placeholder}`, filtered by what the client has typed.
///
/// # Errors
///
/// Returns `invalid_params` when `argument` is not a placeholder of `uri`, so completing the wrong
/// thing is loud rather than an empty list.
pub async fn complete(
    store: &Store,
    uri: &str,
    argument: &str,
    value: &str,
) -> Result<Vec<String>, McpError> {
    let placeholder = format!("{{{argument}}}");
    if !uri.contains(&placeholder) {
        return Err(McpError::invalid_params(
            format!("{uri:?} has no placeholder {argument:?} to complete"),
            None,
        ));
    }

    let candidates: Vec<String> = if uri.starts_with(OPERATION_URI_PREFIX) {
        OPERATIONS.iter().map(|name| (*name).to_owned()).collect()
    } else {
        // Ids come from the store, so completion reflects what is actually addressable. An
        // unreachable database yields no suggestions rather than a failure: completion is advisory.
        match store.list(store.max_history_rows()).await {
            Ok(entries) => entries.iter().map(|entry| entry.id.to_string()).collect(),
            Err(_) => Vec::new(),
        }
    };

    Ok(candidates
        .into_iter()
        .filter(|candidate| candidate.starts_with(value))
        .collect())
}

/// Whether a write to `operation` changes what this resource shows.
///
/// A row's own `calc://history/{id}` resource is deliberately excluded: once written it never
/// changes, so notifying about it would make clients re-read identical bytes forever.
#[must_use]
pub fn invalidated_by_write(operation: &str) -> [String; 2] {
    [
        HISTORY_URI.to_owned(),
        format!("{OPERATION_URI_PREFIX}{operation}"),
    ]
}

/// Render rows exactly as the `calc_history` tool does, so both surfaces read the same.
fn render<'a>(entries: impl IntoIterator<Item = &'a HistoryEntry>) -> String {
    let lines: Vec<String> = entries.into_iter().map(HistoryEntry::to_line).collect();
    if lines.is_empty() {
        return NO_HISTORY.to_owned();
    }
    lines.join("\n")
}

/// A single text body for `uri`.
///
/// rmcp 3.x serialises [`ResourceContents`] with `mimeType`, so the media type advertised in the
/// catalogue can now be emitted on reads too. `tests/mcp_features.rs` pins the wire key.
fn text(uri: &str, body: &str) -> ReadResourceResult {
    ReadResourceResult::new(vec![
        ResourceContents::text(body, uri).with_mime_type(MIME_TYPE),
    ])
}

fn not_served(uri: &str) -> McpError {
    McpError::resource_not_found(
        format!(
            "{uri:?} is not a resource of this server; try {HISTORY_URI}, \
             {OPERATION_URI_PREFIX}<operation> or calc://history/<id>"
        ),
        None,
    )
}
