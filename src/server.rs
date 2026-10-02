use rmcp::{
    ServerHandler,
    model::{Implementation, ServerCapabilities, ServerInfo},
    schemars, tool,
};

use crate::db::Store;

/// Upper bound on rows returned by the history tools, so a client cannot ask for everything.
const MAX_HISTORY_ROWS: i64 = 100;

/// Clamp a client-supplied history limit into `1..=MAX_HISTORY_ROWS`.
#[must_use]
pub fn clamp_history_limit(limit: i64) -> i64 {
    limit.clamp(1, MAX_HISTORY_ROWS)
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
    #[schemars(description = "How many of the most recent calculations to return (1-100)")]
    pub limit: i64,
}

#[derive(Debug, Clone)]
pub struct Calculator {
    store: Store,
}

#[tool(tool_box)]
impl Calculator {
    #[must_use]
    pub const fn new(store: Store) -> Self {
        Self { store }
    }

    /// Names of every tool this server exposes, sorted.
    ///
    /// The underlying registry is a `HashMap`, so this is the only supported way to enumerate
    /// tools — [`ToolBox::list`](rmcp::handler::server::tool::ToolBox::list) has no defined order.
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
        self.store.record_success(
            "add",
            serde_json::json!({ "a": a, "b": b }),
            &sum.to_string(),
        );
        format!("{a} + {b} = {sum}")
    }

    #[tool(description = "Subtract the second integer from the first")]
    async fn sub(&self, #[tool(param)] a: i32, #[tool(param)] b: i32) -> String {
        let difference = a.wrapping_sub(b);
        self.store.record_success(
            "sub",
            serde_json::json!({ "a": a, "b": b }),
            &difference.to_string(),
        );
        format!("{a} - {b} = {difference}")
    }

    #[tool(description = "Multiply two integers")]
    async fn mul(&self, #[tool(param)] a: i32, #[tool(param)] b: i32) -> String {
        let product = a.wrapping_mul(b);
        self.store.record_success(
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
            self.store
                .record_failure("div", inputs.clone(), "division by zero");
            return Err("division by zero".to_owned());
        }
        let quotient = request.dividend / request.divisor;
        self.store
            .record_success("div", inputs, &quotient.to_string());
        Ok(format!(
            "{} / {} = {}",
            request.dividend, request.divisor, quotient
        ))
    }

    #[tool(description = "Report whether PostgreSQL storage is configured and reachable")]
    async fn db_status(&self) -> Result<String, String> {
        if !self.store.is_configured() {
            return Err(Store::DISABLED.to_owned());
        }
        self.store.status().await
    }

    #[tool(description = "List previously recorded calculations, most recent first")]
    async fn calc_history(&self, #[tool(aggr)] request: HistoryRequest) -> Result<String, String> {
        let limit = clamp_history_limit(request.limit);
        let entries = self.store.list(limit).await?;
        if entries.is_empty() {
            return Ok("no calculations recorded yet".to_owned());
        }
        Ok(entries
            .iter()
            .map(crate::db::HistoryEntry::to_line)
            .collect::<Vec<_>>()
            .join("\n"))
    }

    #[tool(description = "Delete all recorded calculations from PostgreSQL")]
    async fn clear_calc_history(&self) -> Result<String, String> {
        let removed = self.store.clear().await?;
        Ok(format!("deleted {removed} recorded calculations"))
    }
}

#[tool(tool_box)]
impl ServerHandler for Calculator {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: Default::default(),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: env!("CARGO_PKG_NAME").to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            instructions: Some(
                "A calculator server. Every arithmetic call is recorded to PostgreSQL when \
                 DATABASE_URL is configured; use db_status, calc_history and clear_calc_history \
                 to inspect storage."
                    .to_owned(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calculator() -> Calculator {
        Calculator::new(Store::disabled())
    }

    // The tool functions themselves are private (rmcp's macro keeps them so), so these unit tests
    // and tests/stdio_protocol.rs are complementary: these are fast and precise, the integration
    // test drives the same tools through the real JSON-RPC protocol.

    #[tokio::test]
    async fn arithmetic_tools_report_results() {
        let calculator = calculator();
        assert_eq!(calculator.add(2, 3).await, "2 + 3 = 5");
        assert_eq!(calculator.sub(7, 2).await, "7 - 2 = 5");
        assert_eq!(calculator.mul(7, 2).await, "7 * 2 = 14");
    }

    #[tokio::test]
    async fn div_guards_against_zero_divisor() {
        let calculator = calculator();
        let ok = calculator
            .div(DivisionRequest {
                dividend: 9.0,
                divisor: 2.0,
            })
            .await;
        assert_eq!(ok, Ok("9 / 2 = 4.5".to_owned()));
        let division_by_zero = calculator
            .div(DivisionRequest {
                dividend: 9.0,
                divisor: 0.0,
            })
            .await;
        assert_eq!(division_by_zero, Err("division by zero".to_owned()));
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
    fn server_info_advertises_tools_capability() {
        let info = calculator().get_info();
        assert!(info.capabilities.tools.is_some());
        assert_eq!(info.server_info.name, "poc-rust-mcp");
        assert!(info.instructions.is_some());
    }
}
