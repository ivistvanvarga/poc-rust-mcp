// SPDX-License-Identifier: BSD-3-Clause
//! The MCP server: tools, prompts, resources, logging and completion, all over one `Calculator`.
//!
//! The [`impl ServerHandler`] block is hand-written. `#[tool_handler]` only fills in the methods the
//! block does not define (here, `get_tool`), so the sorted, paginated `list_tools` and the explicit
//! `call_tool` stay. See [`ServerHandler::list_tools`].

// Logging (SEP-2577) and resource subscribe/unsubscribe (SEP-2567) are deprecated in rmcp 3.10, but
// they are still the live mechanisms for the 2025-11-25 protocol this server advertises. Keep them
// with their deprecation warnings silenced here rather than dropping working behaviour.
#![allow(deprecated)]

use std::{
    borrow::Cow,
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::{
        CallToolRequestParams, CallToolResponse, CancelledNotificationParam, CompleteRequestParams,
        CompleteResult, CompletionInfo, GetPromptRequestParams, GetPromptResponse, Implementation,
        JsonObject, ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult,
        ListToolsResult, LoggingLevel, LoggingMessageNotification, LoggingMessageNotificationParam,
        PaginatedRequestParams, ProgressNotificationParam, ProtocolVersion,
        ReadResourceRequestParams, ReadResourceResponse, Reference, ResourceUpdatedNotification,
        ResourceUpdatedNotificationParam, ServerCapabilities, ServerConfig, ServerNotification,
        SetLevelRequestParams, SubscribeRequestParams, UnsubscribeRequestParams,
    },
    schemars,
    service::{NotificationContext, Peer, RequestContext},
    tool, tool_handler, tool_router,
};

use crate::{db::Store, prompts, resources};

/// The operations a tool can record, which is therefore a closed vocabulary.
///
/// [`prompts`] and [`resources`] both validate against it, so it lives next to the tools that
/// define it rather than being duplicated per surface.
pub const OPERATIONS: [&str; 4] = ["add", "div", "mul", "sub"];

/// Logger name attached to every `notifications/message` this server sends.
pub const LOGGER: &str = "poc-rust-mcp";

/// Rows per page when a client does not ask otherwise.
pub const DEFAULT_LIST_PAGE_SIZE: usize = 20;

/// Upper bound on concurrent resource subscriptions from one client.
///
/// The set is per-`Calculator` and grows for the life of the session, so without a bound a client
/// could make the server hold an arbitrary amount of memory one `resources/subscribe` at a time.
pub const MAX_SUBSCRIPTIONS: usize = 256;

/// Clamp a client-supplied history limit into `1..=max`.
///
/// `max` comes from configuration (`storage.max_history_rows`) so a client can never ask for an
/// unbounded result set.
#[must_use]
pub fn clamp_history_limit(limit: i64, max: i64) -> i64 {
    limit.clamp(1, max.max(1))
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
pub struct DivisionRequest {
    #[schemars(description = "The dividend")]
    pub dividend: f64,
    #[schemars(description = "The divisor; dividing by zero is an error")]
    pub divisor: f64,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
pub struct HistoryRequest {
    #[schemars(
        description = "How many of the most recent calculations to return (clamped to 1..max_history_rows)"
    )]
    pub limit: i64,
}

/// Two integer operands. `add`, `sub` and `mul` each use one of these; a named struct keeps the
/// generated input schema identical to the old `#[tool(param)]` pair.
#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct AddRequest {
    a: i32,
    b: i32,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct SubRequest {
    a: i32,
    b: i32,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct MulRequest {
    a: i32,
    b: i32,
}

#[derive(Debug, Clone)]
pub struct Calculator {
    store: Store,
    /// The tool router built once per instance by the constructors. Regenerating it per call would
    /// rebuild every tool's schema on every `tools/list` and `tools/call`.
    tool_router: ToolRouter<Self>,
    /// The connected client, installed from `RequestContext::peer` in `call_tool`.
    ///
    /// Shared with every clone because rmcp may serve concurrent requests off one instance; the
    /// detached notification task needs a peer after the request has returned.
    peer: Arc<Mutex<Option<Peer<RoleServer>>>>,
    /// Resource URIs this client asked to be told about, from `resources/subscribe`.
    subscriptions: Arc<Mutex<BTreeSet<String>>>,
    /// The level from `logging/setLevel`; notifications below it are dropped.
    log_level: Arc<Mutex<LoggingLevel>>,
    list_page_size: usize,
    subscription_limit: usize,
}

#[tool_router]
impl Calculator {
    #[must_use]
    pub fn new(store: Store) -> Self {
        Self::with_list_page_size(store, DEFAULT_LIST_PAGE_SIZE)
    }

    /// As [`Self::new`], with a page size for the `*/list` methods. Zero is clamped to one, because
    /// a page size of zero would never terminate.
    #[must_use]
    pub fn with_list_page_size(store: Store, list_page_size: usize) -> Self {
        Self {
            store,
            tool_router: Self::tool_router(),
            peer: Arc::default(),
            subscriptions: Arc::default(),
            log_level: Arc::new(Mutex::new(LoggingLevel::Info)),
            list_page_size: list_page_size.max(1),
            subscription_limit: MAX_SUBSCRIPTIONS,
        }
    }

    /// As [`Self::new`], with an upper bound on concurrent resource subscriptions. Zero is clamped
    /// to one for the same reason as the page size: a limit of zero would reject every subscription,
    /// duplicate or not.
    #[must_use]
    pub fn with_subscription_limit(mut self, limit: usize) -> Self {
        self.subscription_limit = limit.max(1);
        self
    }

    /// Names of every tool this server exposes, sorted.
    ///
    /// `ToolRouter::list_all()` sorts, so a `HashMap`-backed router cannot leak its arbitrary order.
    #[must_use]
    pub fn tool_names(&self) -> Vec<String> {
        self.tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect()
    }

    #[tool(description = "Add two integers")]
    async fn add(&self, Parameters(AddRequest { a, b }): Parameters<AddRequest>) -> String {
        let sum = a.wrapping_add(b);
        self.recorded_success(
            "add",
            serde_json::json!({ "a": a, "b": b }),
            &sum.to_string(),
        );
        format!("{a} + {b} = {sum}")
    }

    #[tool(description = "Subtract the second integer from the first")]
    async fn sub(&self, Parameters(SubRequest { a, b }): Parameters<SubRequest>) -> String {
        let difference = a.wrapping_sub(b);
        self.recorded_success(
            "sub",
            serde_json::json!({ "a": a, "b": b }),
            &difference.to_string(),
        );
        format!("{a} - {b} = {difference}")
    }

    #[tool(description = "Multiply two integers")]
    async fn mul(&self, Parameters(MulRequest { a, b }): Parameters<MulRequest>) -> String {
        let product = a.wrapping_mul(b);
        self.recorded_success(
            "mul",
            serde_json::json!({ "a": a, "b": b }),
            &product.to_string(),
        );
        format!("{a} * {b} = {product}")
    }

    #[tool(description = "Divide two floats")]
    async fn div(
        &self,
        Parameters(request): Parameters<DivisionRequest>,
    ) -> Result<String, String> {
        let inputs = serde_json::json!({
            "dividend": request.dividend,
            "divisor": request.divisor,
        });
        if request.divisor == 0.0 {
            self.recorded_failure("div", inputs.clone(), "division by zero");
            return Err("division by zero".to_owned());
        }
        let quotient = request.dividend / request.divisor;
        self.recorded_success("div", inputs, &quotient.to_string());
        Ok(format!(
            "{} / {} = {}",
            request.dividend, request.divisor, quotient
        ))
    }

    #[tool(description = "Report whether database storage is configured and reachable")]
    async fn db_status(&self) -> Result<String, String> {
        if !self.store.is_configured() {
            return Err(Store::DISABLED.to_owned());
        }
        self.store.status().await
    }

    #[tool(description = "List previously recorded calculations, most recent first")]
    async fn calc_history(
        &self,
        Parameters(request): Parameters<HistoryRequest>,
    ) -> Result<String, String> {
        let limit = clamp_history_limit(request.limit, self.store.max_history_rows());
        let entries = self.store.list(limit).await?;
        if entries.is_empty() {
            return Ok(resources::NO_HISTORY.to_owned());
        }
        Ok(entries
            .iter()
            .map(crate::db::HistoryEntry::to_line)
            .collect::<Vec<_>>()
            .join("\n"))
    }

    #[tool(description = "Delete all recorded calculations from the database")]
    async fn clear_calc_history(&self) -> Result<String, String> {
        let removed = self.store.clear().await?;
        // The history the client can see is now empty, so subscribed resources changed even though
        // no *calculation* was written.
        self.announce_write("clear", "cleared calculation history".to_owned());
        Ok(format!("deleted {removed} recorded calculations"))
    }

    /// Record a successful call and tell the client about it.
    fn recorded_success(&self, operation: &str, inputs: serde_json::Value, result: &str) {
        self.store.record_success(operation, inputs, result);
        self.announce_write(operation, format!("recorded {operation} -> {result}"));
    }

    /// Record a failed call and tell the client about it.
    fn recorded_failure(&self, operation: &str, inputs: serde_json::Value, error: &str) {
        self.store.record_failure(operation, inputs, error);
        self.announce_write(operation, format!("recorded failed {operation}: {error}"));
    }

    /// Tell the client that history changed: an info log line, plus a resource-updated notification
    /// for every subscribed resource this write affects.
    ///
    /// Sent from a detached task, because a tool must not wait on the client, and announced on the
    /// *attempt* to record rather than on its success: the write is fire-and-forget by design, so
    /// its outcome is not available here. `notifications/resources/updated` is advisory — "this may
    /// have changed, re-read it" — so over-announcing is harmless, whereas a client left waiting
    /// after its resource did change is not.
    ///
    /// The log line is always sent **before** the resource updates, in one task, and the peer's sink
    /// is FIFO. Together those give the test suite a sentinel: having received a *later* write's log
    /// line, any notification an *earlier* write produced must already have been delivered.
    fn announce_write(&self, operation: &str, message: String) {
        let Some(peer) = self.peer() else { return };
        let send_log = self.level_enabled(LoggingLevel::Info);
        let invalidated: Vec<String> = {
            let subscriptions = self
                .subscriptions
                .lock()
                .expect("subscriptions mutex poisoned");
            resources::invalidated_by_write(operation)
                .into_iter()
                .filter(|uri| subscriptions.contains(uri))
                .collect()
        };
        if !send_log && invalidated.is_empty() {
            return;
        }

        tokio::spawn(async move {
            if send_log {
                let log = ServerNotification::LoggingMessageNotification(
                    LoggingMessageNotification::new(
                        LoggingMessageNotificationParam::new(
                            LoggingLevel::Info,
                            serde_json::json!(message),
                        )
                        .with_logger(LOGGER),
                    ),
                );
                if let Err(error) = peer.send_notification(log).await {
                    tracing::debug!(%error, "could not send log notification");
                }
            }
            for uri in invalidated {
                let updated = ServerNotification::ResourceUpdatedNotification(
                    ResourceUpdatedNotification::new(ResourceUpdatedNotificationParam::new(uri)),
                );
                if let Err(error) = peer.send_notification(updated).await {
                    tracing::debug!(%error, "could not send resource-updated notification");
                }
            }
        });
    }

    /// The connected client, if the session is up.
    fn peer(&self) -> Option<Peer<RoleServer>> {
        self.peer
            .lock()
            .expect("peer mutex poisoned")
            .as_ref()
            .cloned()
    }

    /// Whether a notification at `level` passes the level from `logging/setLevel`.
    fn level_enabled(&self, level: LoggingLevel) -> bool {
        let configured = *self.log_level.lock().expect("log level mutex poisoned");
        rank(level) >= rank(configured)
    }
}

/// Rank a `LoggingLevel` so it can be compared; rmcp's enum derives `PartialEq` but not `Ord`.
const fn rank(level: LoggingLevel) -> u8 {
    match level {
        LoggingLevel::Debug => 0,
        LoggingLevel::Info => 1,
        LoggingLevel::Notice => 2,
        LoggingLevel::Warning => 3,
        LoggingLevel::Error => 4,
        LoggingLevel::Critical => 5,
        LoggingLevel::Alert => 6,
        LoggingLevel::Emergency => 7,
    }
}

/// The page of `items` a cursor points at, plus the cursor for the next one.
///
/// The cursor is the index of the page's first item, decimal-encoded: opaque to the client by spec
/// convention, and rejected rather than guessed when it is not a number, so a client bug surfaces
/// instead of being silently answered with the whole list.
///
/// # Errors
///
/// Returns `invalid_params` for a cursor that is not a plain index.
fn page<T: Clone>(
    items: &[T],
    params: Option<&PaginatedRequestParams>,
    page_size: usize,
) -> Result<(Vec<T>, Option<String>), McpError> {
    let start = match params.and_then(|params| params.cursor.as_deref()) {
        None => 0,
        Some(cursor) => cursor.parse::<usize>().map_err(|_| {
            McpError::invalid_params(format!("cursor {cursor:?} is not a valid cursor"), None)
        })?,
    };
    let remaining = items.get(start..).unwrap_or_default();
    let (page, rest) = remaining.split_at(page_size.min(remaining.len()));
    let next_cursor = (!rest.is_empty()).then(|| (start + page.len()).to_string());
    Ok((page.to_vec(), next_cursor))
}

/// Deserialise a tool's arguments into its declared request type before the router sees them.
///
/// rmcp 3.x's router deliberately rewrites argument-deserialisation failures into tool-level errors
/// (`isError: true`). This server's contract, unchanged from 0.1, is that a client sending the wrong
/// JSON *type* is a protocol error (`invalid_params`), while a well-formed call whose *operation*
/// fails (division by zero) is a tool error. Validating here keeps that split: only the first case
/// is intercepted, and the router still handles unknown names and no-argument tools.
fn validate_tool_arguments(name: &str, arguments: Option<&JsonObject>) -> Result<(), McpError> {
    let Some(arguments) = arguments else {
        return Ok(());
    };
    let value = serde_json::Value::Object(arguments.clone());
    let parsed = match name {
        "add" => serde_json::from_value::<AddRequest>(value).map(|_| ()),
        "sub" => serde_json::from_value::<SubRequest>(value).map(|_| ()),
        "mul" => serde_json::from_value::<MulRequest>(value).map(|_| ()),
        "div" => serde_json::from_value::<DivisionRequest>(value).map(|_| ()),
        "calc_history" => serde_json::from_value::<HistoryRequest>(value).map(|_| ()),
        // No-argument tools and unknown names are left to the router (which reports the latter).
        _ => Ok(()),
    };
    parsed.map_err(|error| {
        McpError::invalid_params(format!("failed to deserialize parameters: {error}"), None)
    })
}

// `#[tool_handler]` only generates methods this block does not define, so `get_tool` comes from the
// router while `list_tools`, `call_tool` and `get_info` stay hand-written. `list_tools` sorts and
// paginates, which the generated version cannot; `call_tool` installs the peer before dispatch.
#[tool_handler]
impl ServerHandler for Calculator {
    async fn list_tools(
        &self,
        params: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let mut tools = self.tool_router.list_all();
        tools.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        let (tools, next_cursor) = page(&tools, params.as_ref(), self.list_page_size)?;
        let mut result = ListToolsResult::with_all_items(tools);
        result.next_cursor = next_cursor;
        Ok(result)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        // The peer lives on the context in 3.x. Keep a clone so detached notification tasks can
        // still reach it after this request returns.
        *self.peer.lock().expect("peer mutex poisoned") = Some(context.peer.clone());
        // 3.x's router turns argument-deserialisation failures into tool errors (`isError: true`).
        // Pre-validate here so a client that sends the wrong JSON type still gets the JSON-RPC
        // `invalid_params` it got before the upgrade; a valid request whose operation fails (e.g.
        // division by zero) still reaches the tool and stays a tool error.
        validate_tool_arguments(&request.name, request.arguments.as_ref())?;
        let context = ToolCallContext::new(self, request, context);
        self.tool_router.call(context).await
    }

    async fn list_prompts(
        &self,
        params: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        let catalogue = prompts::list();
        let (prompts, next_cursor) = page(&catalogue, params.as_ref(), self.list_page_size)?;
        let mut result = ListPromptsResult::with_all_items(prompts);
        result.next_cursor = next_cursor;
        Ok(result)
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, McpError> {
        Ok(prompts::get(&request.name, request.arguments.as_ref())?.into())
    }

    async fn list_resources(
        &self,
        params: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let catalogue = resources::list();
        let (resources, next_cursor) = page(&catalogue, params.as_ref(), self.list_page_size)?;
        let mut result = ListResourcesResult::with_all_items(resources);
        result.next_cursor = next_cursor;
        Ok(result)
    }

    async fn list_resource_templates(
        &self,
        params: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        let catalogue = resources::templates();
        let (resource_templates, next_cursor) =
            page(&catalogue, params.as_ref(), self.list_page_size)?;
        let mut result = ListResourceTemplatesResult::with_all_items(resource_templates);
        result.next_cursor = next_cursor;
        Ok(result)
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        Ok(resources::read(&self.store, &request.uri).await?.into())
    }

    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        // Refuse a URI this server does not serve rather than accepting a subscription that could
        // never fire: a silent no-op subscription is worse than an error the client can act on.
        resources::parse(&request.uri)?;
        {
            let mut subscriptions = self
                .subscriptions
                .lock()
                .expect("subscriptions mutex poisoned");
            if !subscriptions.contains(&request.uri)
                && subscriptions.len() >= self.subscription_limit
            {
                return Err(McpError::invalid_params(
                    "too many resource subscriptions",
                    None,
                ));
            }
            subscriptions.insert(request.uri.clone());
        }
        tracing::debug!(uri = ?request.uri, "client subscribed to a resource");
        Ok(())
    }

    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        // Unsubscribing something never subscribed is a no-op, not an error: the client's intent —
        // "stop telling me" — is satisfied either way.
        self.subscriptions
            .lock()
            .expect("subscriptions mutex poisoned")
            .remove(&request.uri);
        tracing::debug!(uri = ?request.uri, "client unsubscribed from a resource");
        Ok(())
    }

    async fn set_level(
        &self,
        request: SetLevelRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        *self.log_level.lock().expect("log level mutex poisoned") = request.level;
        Ok(())
    }

    async fn complete(
        &self,
        request: CompleteRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, McpError> {
        let reference = &request.r#ref;
        let values = match reference {
            Reference::Prompt(reference) => prompts::completions(
                &reference.name,
                &request.argument.name,
                &request.argument.value,
            )?,
            Reference::Resource(reference) => {
                resources::complete(
                    &self.store,
                    &reference.uri,
                    &request.argument.name,
                    &request.argument.value,
                )
                .await?
            }
            // `Reference` is non-exhaustive in 3.x; a future variant must fail loudly rather than
            // complete against the wrong catalogue.
            _ => {
                return Err(McpError::invalid_params(
                    "unsupported completion reference",
                    None,
                ));
            }
        };
        // `hasMore: false` because every list here is filtered in full; `total` reports the size of
        // the filtered set, not of the whole catalogue.
        let mut completion = CompletionInfo::default();
        completion.total = Some(values.len() as u32);
        completion.values = values;
        completion.has_more = Some(false);
        Ok(CompleteResult::new(completion))
    }

    /// Progress and cancellation are accepted and deliberately do nothing here.
    ///
    /// rmcp already cancels a request's `CancellationToken` when `notifications/cancelled` arrives,
    /// before this hook runs, so the transport-level half of the feature is automatic. The other
    /// half has nothing to act on: every tool is a single atomic call that returns its whole result
    /// at once, so there is no partial work to abandon and nothing to report progress *for*. These
    /// hooks exist so that is a decision on record rather than an omission; the tests assert the
    /// server neither replies to nor breaks on such notifications.
    async fn on_cancelled(
        &self,
        notification: CancelledNotificationParam,
        _context: NotificationContext<RoleServer>,
    ) {
        tracing::debug!(
            id = ?notification.request_id,
            reason = ?notification.reason,
            "cancellation received; no in-flight work to abandon"
        );
    }

    async fn on_progress(
        &self,
        _notification: ProgressNotificationParam,
        _context: NotificationContext<RoleServer>,
    ) {
    }

    /// Only advertise the revisions whose session lifecycle this handler implements. 2026-07-28
    /// removes `initialize` and `ping` and replaces subscriptions with `subscriptions/listen`, none
    /// of which exist here.
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(ProtocolVersion::known_up_to(&ProtocolVersion::V_2025_11_25))
    }

    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            // Every capability advertised here is implemented above. Notably absent:
            // `*ListChanged`, because the tool, prompt and resource catalogues are all static —
            // claiming otherwise would make clients poll for a change that cannot happen. And
            // completions, which the spec defines no capability flag for. Logging is deprecated by
            // SEP-2577 but remains the mechanism for `logging/setLevel` at this protocol ceiling.
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_resources()
                .enable_resources_subscribe()
                .enable_logging()
                .build(),
        )
        .with_protocol_version(ProtocolVersion::V_2025_11_25)
        .with_server_info(Implementation::new(
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
        ))
        .with_instructions(
            "A calculator server. Every arithmetic call is recorded to the configured \
             database (PostgreSQL, MySQL/MariaDB or SQLite) when DATABASE_URL is set; use \
             db_status, calc_history and clear_calc_history to inspect storage, or read the \
             calc://history resource to pull the whole log into context. Subscribe to \
             calc://history to be told when it changes.",
        )
    }
}

/// Private access for the relocated unit tests in `tests/server.rs`.
///
/// As in [`crate::db::testing`], a child module can read its parent's private items, so these are
/// shims rather than visibility changes. The tool functions themselves are **not** reachable this
/// way: rmcp's `#[tool]` macro keeps them private and this module deliberately does not widen that,
/// so the tests reach them the way a client does, through `ServerHandler::call_tool`.
#[doc(hidden)]
pub mod testing {
    use super::{Calculator, LoggingLevel, rank as level_rank};

    /// Whether a notification at `level` would pass the configured floor.
    pub fn level_enabled(calculator: &Calculator, level: LoggingLevel) -> bool {
        calculator.level_enabled(level)
    }

    /// The numeric rank a `LoggingLevel` is ordered by.
    pub fn rank(level: LoggingLevel) -> u8 {
        level_rank(level)
    }
}
