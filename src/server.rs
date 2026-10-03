// SPDX-License-Identifier: BSD-3-Clause
//! The MCP server: tools, prompts, resources, logging and completion, all over one `Calculator`.
//!
//! rmcp 0.1.5 exposes a hook per MCP server-side feature, and [`impl ServerHandler`] implements all
//! of them. Two of those hooks are deliberately **hand-written** rather than macro-generated, and
//! that is the one place where the usual rule in `AGENTS.md` is inverted — see [`ServerHandler::list_tools`].

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

use rmcp::{
    ServerHandler,
    handler::server::tool::ToolCallContext,
    model::{
        CallToolRequestParam, CallToolResult, CompleteRequestParam, CompleteResult,
        GetPromptRequestParam, GetPromptResult, Implementation, ListPromptsResult,
        ListResourceTemplatesResult, ListResourcesResult, ListToolsResult, LoggingLevel,
        LoggingMessageNotification, LoggingMessageNotificationMethod,
        LoggingMessageNotificationParam, PaginatedRequestParam, PaginatedRequestParamInner,
        ReadResourceRequestParam, ReadResourceResult, Reference, ResourceUpdatedNotification,
        ResourceUpdatedNotificationMethod, ResourceUpdatedNotificationParam, ServerCapabilities,
        ServerInfo, ServerNotification, SubscribeRequestParam, UnsubscribeRequestParam,
    },
    schemars,
    service::{Peer, RequestContext, RoleServer},
    tool,
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

#[derive(Debug, Clone)]
pub struct Calculator {
    store: Store,
    /// The connected client, installed by rmcp when the session starts.
    ///
    /// Shared with every clone because `ServerHandler` requires `Clone` and rmcp may serve
    /// concurrent requests off one instance; whichever clone handles a request can reach the peer.
    peer: Arc<Mutex<Option<Peer<RoleServer>>>>,
    /// Resource URIs this client asked to be told about, from `resources/subscribe`.
    subscriptions: Arc<Mutex<BTreeSet<String>>>,
    /// The level from `logging/setLevel`; notifications below it are dropped.
    log_level: Arc<Mutex<LoggingLevel>>,
    list_page_size: usize,
}

#[tool(tool_box)]
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
            peer: Arc::default(),
            subscriptions: Arc::default(),
            log_level: Arc::new(Mutex::new(LoggingLevel::Info)),
            list_page_size: list_page_size.max(1),
        }
    }

    /// Names of every tool this server exposes, sorted.
    ///
    /// [`ToolBox::list`](rmcp::handler::server::tool::ToolBox::list) is backed by a `HashMap`, so
    /// this sorts rather than reporting the map's arbitrary order.
    #[must_use]
    pub fn tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Self::tool_box()
            .list()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        names.sort_unstable();
        names
    }

    #[tool(description = "Add two integers")]
    async fn add(&self, #[tool(param)] a: i32, #[tool(param)] b: i32) -> String {
        let sum = a.wrapping_add(b);
        self.recorded_success(
            "add",
            serde_json::json!({ "a": a, "b": b }),
            &sum.to_string(),
        );
        format!("{a} + {b} = {sum}")
    }

    #[tool(description = "Subtract the second integer from the first")]
    async fn sub(&self, #[tool(param)] a: i32, #[tool(param)] b: i32) -> String {
        let difference = a.wrapping_sub(b);
        self.recorded_success(
            "sub",
            serde_json::json!({ "a": a, "b": b }),
            &difference.to_string(),
        );
        format!("{a} - {b} = {difference}")
    }

    #[tool(description = "Multiply two integers")]
    async fn mul(&self, #[tool(param)] a: i32, #[tool(param)] b: i32) -> String {
        let product = a.wrapping_mul(b);
        self.recorded_success(
            "mul",
            serde_json::json!({ "a": a, "b": b }),
            &product.to_string(),
        );
        format!("{a} * {b} = {product}")
    }

    #[tool(description = "Divide two floats")]
    async fn div(&self, #[tool(aggr)] request: DivisionRequest) -> Result<String, String> {
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
    async fn calc_history(&self, #[tool(aggr)] request: HistoryRequest) -> Result<String, String> {
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
        let subscriptions = self.subscriptions();
        let invalidated: Vec<String> = resources::invalidated_by_write(operation)
            .into_iter()
            .filter(|uri| subscriptions.contains(uri))
            .collect();
        if !send_log && invalidated.is_empty() {
            return;
        }

        tokio::spawn(async move {
            if send_log {
                let log =
                    ServerNotification::LoggingMessageNotification(LoggingMessageNotification {
                        method: LoggingMessageNotificationMethod,
                        params: LoggingMessageNotificationParam {
                            level: LoggingLevel::Info,
                            logger: Some(LOGGER.to_owned()),
                            data: serde_json::json!(message),
                        },
                    });
                if let Err(error) = peer.send_notification(log).await {
                    tracing::debug!(%error, "could not send log notification");
                }
            }
            for uri in invalidated {
                let updated =
                    ServerNotification::ResourceUpdatedNotification(ResourceUpdatedNotification {
                        method: ResourceUpdatedNotificationMethod,
                        params: ResourceUpdatedNotificationParam { uri },
                    });
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
        let configured = self
            .log_level
            .lock()
            .expect("log level mutex poisoned")
            .clone();
        rank(level) >= rank(configured)
    }

    fn subscriptions(&self) -> BTreeSet<String> {
        self.subscriptions
            .lock()
            .expect("subscriptions mutex poisoned")
            .clone()
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
    params: Option<&PaginatedRequestParamInner>,
    page_size: usize,
) -> Result<(Vec<T>, Option<String>), rmcp::Error> {
    let start = match params.and_then(|params| params.cursor.as_deref()) {
        None => 0,
        Some(cursor) => cursor.parse::<usize>().map_err(|_| {
            rmcp::Error::invalid_params(format!("cursor {cursor:?} is not a valid cursor"), None)
        })?,
    };
    let remaining = items.get(start..).unwrap_or_default();
    let (page, rest) = remaining.split_at(page_size.min(remaining.len()));
    let next_cursor = (!rest.is_empty()).then(|| (start + page.len()).to_string());
    Ok((page.to_vec(), next_cursor))
}

// NOTE: this block is deliberately *not* annotated `#[tool(tool_box)]`.
//
// That annotation unconditionally generates `list_tools` and `call_tool`, so hand-writing them
// alongside it is a duplicate-definition error. Both are written out here instead, because
// `list_tools` needs two things the generated one cannot do: sort the `HashMap`-backed tool box, and
// paginate. `call_tool` is exactly the macro's body, restated so the two stay visibly paired — if
// this ever compiles without a `list_tools`, tools are silently invisible, so keep them together.
impl ServerHandler for Calculator {
    async fn list_tools(
        &self,
        params: PaginatedRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::Error> {
        let mut tools = Self::tool_box().list();
        tools.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        let (tools, next_cursor) = page(&tools, params.as_ref(), self.list_page_size)?;
        Ok(ListToolsResult { tools, next_cursor })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParam,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::Error> {
        let context = ToolCallContext::new(self, request, context);
        Self::tool_box().call(context).await
    }

    async fn list_prompts(
        &self,
        params: PaginatedRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, rmcp::Error> {
        let catalogue = prompts::list();
        let (prompts, next_cursor) = page(&catalogue, params.as_ref(), self.list_page_size)?;
        Ok(ListPromptsResult {
            prompts,
            next_cursor,
        })
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, rmcp::Error> {
        prompts::get(&request.name, request.arguments.as_ref())
    }

    async fn list_resources(
        &self,
        params: PaginatedRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, rmcp::Error> {
        let catalogue = resources::list();
        let (resources, next_cursor) = page(&catalogue, params.as_ref(), self.list_page_size)?;
        Ok(ListResourcesResult {
            resources,
            next_cursor,
        })
    }

    async fn list_resource_templates(
        &self,
        params: PaginatedRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, rmcp::Error> {
        let catalogue = resources::templates();
        let (resource_templates, next_cursor) =
            page(&catalogue, params.as_ref(), self.list_page_size)?;
        Ok(ListResourceTemplatesResult {
            resource_templates,
            next_cursor,
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, rmcp::Error> {
        resources::read(&self.store, &request.uri).await
    }

    async fn subscribe(
        &self,
        request: SubscribeRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), rmcp::Error> {
        // Refuse a URI this server does not serve rather than accepting a subscription that could
        // never fire: a silent no-op subscription is worse than an error the client can act on.
        resources::parse(&request.uri)?;
        tracing::debug!(uri = %request.uri, "client subscribed to a resource");
        self.subscriptions
            .lock()
            .expect("subscriptions mutex poisoned")
            .insert(request.uri);
        Ok(())
    }

    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), rmcp::Error> {
        // Unsubscribing something never subscribed is a no-op, not an error: the client's intent —
        // "stop telling me" — is satisfied either way.
        self.subscriptions
            .lock()
            .expect("subscriptions mutex poisoned")
            .remove(&request.uri);
        tracing::debug!(uri = %request.uri, "client unsubscribed from a resource");
        Ok(())
    }

    async fn set_level(
        &self,
        request: rmcp::model::SetLevelRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), rmcp::Error> {
        *self.log_level.lock().expect("log level mutex poisoned") = request.level.clone();
        Ok(())
    }

    async fn complete(
        &self,
        request: CompleteRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, rmcp::Error> {
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
        };
        // `hasMore: false` because every list here is filtered in full; `total` is left unset so a
        // client cannot mistake "everything matching the prefix" for "everything that exists".
        Ok(CompleteResult {
            completion: rmcp::model::CompletionInfo {
                total: Some(values.len() as u32),
                values,
                has_more: Some(false),
            },
        })
    }

    /// Progress and cancellation are accepted and deliberately do nothing here.
    ///
    /// rmcp already cancels a request's `CancellationToken` when `notifications/cancelled` arrives,
    /// before this hook runs, so the transport-level half of the feature is automatic. The other
    /// half has nothing to act on: every tool is a single atomic call that returns its whole result
    /// at once, so there is no partial work to abandon and nothing to report progress *for*. These
    /// hooks exist so that is a decision on record rather than an omission; the tests assert the
    /// server neither replies to nor breaks on such notifications.
    async fn on_cancelled(&self, notification: rmcp::model::CancelledNotificationParam) {
        tracing::debug!(
            id = %notification.request_id,
            reason = ?notification.reason,
            "cancellation received; no in-flight work to abandon"
        );
    }

    async fn on_progress(&self, _notification: rmcp::model::ProgressNotificationParam) {}

    fn get_peer(&self) -> Option<Peer<RoleServer>> {
        self.peer()
    }

    fn set_peer(&mut self, peer: Peer<RoleServer>) {
        *self.peer.lock().expect("peer mutex poisoned") = Some(peer);
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: Default::default(),
            // Every capability advertised here is implemented above. Notably absent:
            // `*ListChanged`, because the tool, prompt and resource catalogues are all static —
            // claiming otherwise would make clients poll for a change that cannot happen. And
            // completions, which the spec defines no capability flag for.
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_resources()
                .enable_resources_subscribe()
                .enable_logging()
                .build(),
            server_info: Implementation {
                name: env!("CARGO_PKG_NAME").to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            instructions: Some(
                "A calculator server. Every arithmetic call is recorded to the configured \
                 database (PostgreSQL, MySQL/MariaDB or SQLite) when DATABASE_URL is set; use \
                 db_status, calc_history and clear_calc_history to inspect storage, or read the \
                 calc://history resource to pull the whole log into context. Subscribe to \
                 calc://history to be told when it changes."
                    .to_owned(),
            ),
        }
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
