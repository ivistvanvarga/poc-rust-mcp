use std::{
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions, PgSslMode},
};
use tokio::sync::OnceCell;
use tracing::{debug, warn};

/// Number of pooled connections kept open for the MCP server.
const MAX_CONNECTIONS: u32 = 5;

/// How long a storage operation may wait for a connection before it is reported as failed.
///
/// `connect_lazy` never fails, so without this bound every tool call would block on TCP connect
/// for ~30s whenever PostgreSQL is down — exactly the situation graceful degradation exists for.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(3);

/// Consecutive connection failures after which storage is skipped without trying to connect.
const BREAKER_THRESHOLD: u32 = 2;

/// How long storage stays skipped after tripping the circuit breaker.
const BREAKER_COOLDOWN: Duration = Duration::from_secs(30);

/// Whether a sqlx error means "the database is unreachable" as opposed to "the query was bad".
/// Only the former should trip the breaker; a bad query must not disable storage.
const fn is_connectivity_error(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::Io(_)
            | sqlx::Error::PoolTimedOut
            | sqlx::Error::PoolClosed
            | sqlx::Error::Configuration(_)
            | sqlx::Error::Tls(_)
    )
}

/// A single recorded calculator call, as returned to MCP clients.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct HistoryEntry {
    pub id: i64,
    pub operation: String,
    pub inputs: serde_json::Value,
    pub result: Option<String>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl HistoryEntry {
    /// Render the row as a single human/LLM readable line.
    #[must_use]
    pub fn to_line(&self) -> String {
        let outcome = match (&self.result, &self.error) {
            (Some(result), _) => result.clone(),
            (None, Some(error)) => format!("error: {error}"),
            (None, None) => "unknown outcome".to_owned(),
        };
        format!(
            "#{} {} {}{} -> {}",
            self.id,
            self.created_at.to_rfc3339(),
            self.operation,
            self.inputs,
            outcome
        )
    }
}

/// Optional Postgres-backed storage for calculator history.
///
/// A `Store` is always constructible: when `DATABASE_URL` is missing or unusable the pool is
/// `None` and every storage operation returns an error instead of panicking, so the MCP server
/// still starts and serves the pure-computation tools.
///
/// When the database is configured but unreachable, a circuit breaker keeps the arithmetic tools
/// fast: after [`BREAKER_THRESHOLD`] connection failures, storage is skipped for
/// [`BREAKER_COOLDOWN`] instead of making every tool call wait out [`ACQUIRE_TIMEOUT`].
#[derive(Debug, Clone, Default)]
pub struct Store {
    pool: Option<PgPool>,
    schema: OnceCell<()>,
    breaker: Arc<Mutex<Breaker>>,
}

/// Connection-failure bookkeeping backing the graceful-degradation behaviour.
#[derive(Debug, Default)]
struct Breaker {
    failures: u32,
    retry_after: Option<Instant>,
}

impl Store {
    /// Storage that is permanently off; every storage call fails fast with [`Self::DISABLED`].
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            pool: None,
            schema: OnceCell::const_new(),
            breaker: Arc::default(),
        }
    }

    /// Message returned when storage is not configured.
    pub const DISABLED: &'static str =
        "storage unavailable: DATABASE_URL is not set to a usable PostgreSQL URL";

    /// Build a store from `DATABASE_URL`, degrading to disabled storage when it is absent.
    #[must_use]
    pub fn from_env() -> Self {
        let url = std::env::var("DATABASE_URL")
            .ok()
            .filter(|u| !u.trim().is_empty());
        match url {
            Some(url) => Self::connect(&url),
            None => {
                warn!(
                    "{}; serving calculator tools without history",
                    Self::DISABLED
                );
                Self::disabled()
            }
        }
    }

    /// Build a lazily-connected store for `url`. The pool is **not** contacted here, so this
    /// never fails on an unreachable database.
    #[must_use]
    pub fn connect(url: &str) -> Self {
        Self::connect_with_timeout(url, ACQUIRE_TIMEOUT)
    }

    /// As [`Self::connect`], with an explicit bound on how long operations wait for a connection.
    /// Tests use a millisecond-scale timeout to stay fast.
    #[must_use]
    pub fn connect_with_timeout(url: &str, acquire_timeout: Duration) -> Self {
        match Self::try_connect(url, acquire_timeout) {
            Ok(store) => store,
            Err(error) => {
                warn!(%error, "invalid DATABASE_URL; serving calculator tools without history");
                Self::disabled()
            }
        }
    }

    fn try_connect(url: &str, acquire_timeout: Duration) -> Result<Self, sqlx::Error> {
        // The devcontainer database speaks plaintext TCP; being explicit avoids depending on the
        // TLS feature set of sqlx and avoids a pointless SSL negotiation attempt.
        let options = PgConnectOptions::from_str(url)?.ssl_mode(PgSslMode::Disable);
        let pool = PgPoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            .acquire_timeout(acquire_timeout)
            .connect_lazy_with(options);
        Ok(Self {
            pool: Some(pool),
            schema: OnceCell::new(),
            breaker: Arc::default(),
        })
    }

    /// True when a database is configured, whether or not it is reachable right now.
    #[must_use]
    pub const fn is_configured(&self) -> bool {
        self.pool.is_some()
    }

    /// Reject the call immediately while the breaker is open, so a downed database costs no
    /// wall-clock time per tool call.
    fn check_breaker(&self) -> Result<(), String> {
        let breaker = self.breaker.lock().expect("breaker mutex poisoned");
        match breaker.retry_after {
            Some(retry_after) if Instant::now() < retry_after => {
                let seconds = retry_after
                    .saturating_duration_since(Instant::now())
                    .as_secs();
                Err(format!(
                    "storage temporarily disabled after repeated connection failures, retrying in {seconds}s"
                ))
            }
            _ => Ok(()),
        }
    }

    /// Reset the breaker after a successful round-trip.
    fn note_reachable(&self) {
        if let Ok(mut breaker) = self.breaker.lock() {
            breaker.failures = 0;
            breaker.retry_after = None;
        }
    }

    /// Count a connectivity failure and open the breaker once it trips. Non-connectivity errors
    /// (bad query, constraint violation, …) are ignored on purpose.
    fn note_failure(&self, error: &sqlx::Error) {
        if !is_connectivity_error(error) {
            return;
        }
        let Ok(mut breaker) = self.breaker.lock() else {
            return;
        };
        breaker.failures += 1;
        if breaker.failures >= BREAKER_THRESHOLD {
            breaker.retry_after = Some(Instant::now() + BREAKER_COOLDOWN);
            warn!(
                failures = breaker.failures,
                "storage circuit breaker open; skipping database for {BREAKER_COOLDOWN:?}"
            );
        }
    }

    /// Turn a sqlx result into our string errors, feeding the breaker along the way.
    fn finish<T>(&self, result: Result<T, sqlx::Error>, context: &str) -> Result<T, String> {
        match result {
            Ok(value) => {
                self.note_reachable();
                Ok(value)
            }
            Err(error) => {
                self.note_failure(&error);
                Err(format!("{context}: {error}"))
            }
        }
    }

    /// Apply pending migrations once per process, retrying on the next call if the database is
    /// not up yet. This is why startup never blocks on Postgres.
    async fn ensure_schema(&self) -> Result<&PgPool, String> {
        let pool = self.pool.as_ref().ok_or(Self::DISABLED)?;
        self.check_breaker()?;
        self.schema
            .get_or_try_init(|| async {
                // Callers queue behind this init, so re-check the breaker here: once it opens,
                // the rest of the queue fails instantly instead of each retrying the connection.
                self.check_breaker()?;
                debug!("applying database migrations");
                // MigrateError wraps the underlying sqlx error, so unwrap it for the breaker:
                // a missing database must trip it, a bad migration file must not.
                match sqlx::migrate!("./migrations").run(pool).await {
                    Ok(()) => {
                        self.note_reachable();
                        Ok(())
                    }
                    Err(error) => {
                        match &error {
                            sqlx::migrate::MigrateError::Execute(inner)
                            | sqlx::migrate::MigrateError::ExecuteMigration(inner, _) => {
                                self.note_failure(inner);
                            }
                            _ => {}
                        }
                        Err(format!("failed to apply migrations: {error}"))
                    }
                }
            })
            .await?;
        Ok(pool)
    }

    /// Append a successful call, without blocking the caller.
    ///
    /// This is a detached task on purpose: the calculator tools must answer even when the database
    /// is down, and awaiting the insert here would make every arithmetic call pay the acquire
    /// timeout. The trade-off is that a record can be lost if the process exits immediately after
    /// the tool returns — acceptable for a best-effort history log.
    pub fn record_success(&self, operation: &str, inputs: serde_json::Value, result: &str) {
        self.spawn_insert(operation, inputs, Some(result.to_owned()), None);
    }

    /// Append a failed call, without blocking the caller. See [`Self::record_success`].
    pub fn record_failure(&self, operation: &str, inputs: serde_json::Value, error: &str) {
        self.spawn_insert(operation, inputs, None, Some(error.to_owned()));
    }

    fn spawn_insert(
        &self,
        operation: &str,
        inputs: serde_json::Value,
        result: Option<String>,
        error: Option<String>,
    ) {
        let store = self.clone();
        let operation = operation.to_owned();
        tokio::spawn(async move {
            if let Err(insert_error) = store.insert(&operation, inputs, result, error).await {
                debug!(
                    %insert_error,
                    %operation,
                    "calculation not persisted; storage degraded"
                );
            }
        });
    }

    async fn insert(
        &self,
        operation: &str,
        inputs: serde_json::Value,
        result: Option<String>,
        error: Option<String>,
    ) -> Result<(), String> {
        let pool = self.ensure_schema().await?;
        let executed = sqlx::query(
            "INSERT INTO calc_history (operation, inputs, result, error) VALUES ($1, $2, $3, $4)",
        )
        .bind(operation)
        .bind(inputs)
        .bind(result)
        .bind(error)
        .execute(pool)
        .await;
        self.finish(executed, "failed to insert history")
            .map(|_| ())
    }

    /// Most recent entries first.
    pub async fn list(&self, limit: i64) -> Result<Vec<HistoryEntry>, String> {
        let pool = self.ensure_schema().await?;
        let fetched = sqlx::query_as::<_, HistoryEntry>(
            "SELECT id, operation, inputs, result, error, created_at
             FROM calc_history
             ORDER BY created_at DESC, id DESC
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(pool)
        .await;
        self.finish(fetched, "failed to read history")
    }

    /// Drop all history and report how many rows were removed.
    ///
    /// `DELETE` rather than the faster `TRUNCATE`: PostgreSQL's `TRUNCATE` command tag carries no row
    /// count, so `rows_affected` would always be 0 and the tool would report a lie.
    pub async fn clear(&self) -> Result<u64, String> {
        let pool = self.ensure_schema().await?;
        let result = sqlx::query("DELETE FROM calc_history").execute(pool).await;
        self.finish(result, "failed to clear history")
            .map(|result| result.rows_affected())
    }

    /// Verify the database answers, then report the stored row count.
    pub async fn status(&self) -> Result<String, String> {
        let pool = self.ensure_schema().await?;
        let counted = sqlx::query_as::<_, (i64,)>("SELECT count(*) FROM calc_history")
            .fetch_one(pool)
            .await;
        let (rows,) = self.finish(counted, "database unreachable")?;
        Ok(format!("database reachable, {rows} stored calculations"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unreachable port with a timeout short enough to keep the suite fast.
    const DEAD_URL: &str = "postgres://mcp:mcp@127.0.0.1:1/mcp";
    const FAST: Duration = Duration::from_millis(200);

    #[test]
    fn disabled_store_reports_itself_as_unconfigured() {
        assert!(!Store::disabled().is_configured());
    }

    #[tokio::test]
    async fn disabled_store_fails_every_operation_with_the_same_message() {
        let store = Store::disabled();
        assert_eq!(store.status().await.unwrap_err(), Store::DISABLED);
        assert_eq!(store.list(10).await.unwrap_err(), Store::DISABLED);
        assert_eq!(store.clear().await.unwrap_err(), Store::DISABLED);
    }

    #[tokio::test]
    async fn unparsable_url_degrades_to_disabled_storage() {
        assert!(!Store::connect("not-a-postgres-url").is_configured());
    }

    #[tokio::test]
    async fn unreachable_database_is_configured_but_fails_fast() {
        let store = Store::connect_with_timeout(DEAD_URL, FAST);
        assert!(store.is_configured());
        // Bounded by the acquire timeout — this must not block for sqlx's default ~30s.
        assert!(store.status().await.is_err());
    }

    #[tokio::test]
    async fn breaker_opens_after_repeated_failures_and_fails_without_touching_the_pool() {
        let store = Store::connect_with_timeout(DEAD_URL, FAST);
        assert!(store.status().await.is_err());
        assert!(store.status().await.is_err());

        // Once tripped, further calls are rejected without attempting a connection, so they are
        // effectively instant even though the acquire timeout is still armed.
        let started = Instant::now();
        let error = store.status().await.unwrap_err();
        assert!(
            started.elapsed() < FAST,
            "breaker did not short-circuit: took {started:?}"
        );
        assert!(
            error.contains("temporarily disabled"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn successful_call_resets_the_breaker() {
        let store = Store::disabled();
        store.note_failure(&sqlx::Error::PoolClosed);
        store.note_failure(&sqlx::Error::PoolClosed);
        assert_eq!(store.breaker.lock().unwrap().failures, 2);
        assert!(store.check_breaker().is_err(), "breaker should be open");

        store.note_reachable();
        let breaker = store.breaker.lock().unwrap();
        assert_eq!(breaker.failures, 0);
        assert!(breaker.retry_after.is_none());
    }

    #[test]
    fn breaker_does_not_trip_on_query_level_errors() {
        let store = Store::disabled();
        store.note_failure(&sqlx::Error::RowNotFound);
        assert_eq!(store.breaker.lock().unwrap().failures, 0);
    }

    #[tokio::test]
    async fn recording_is_fire_and_forget_and_never_blocks() {
        for store in [
            Store::disabled(),
            Store::connect_with_timeout(DEAD_URL, FAST),
        ] {
            let started = Instant::now();
            store.record_success("add", serde_json::json!({ "a": 1, "b": 2 }), "3");
            store.record_failure("div", serde_json::json!({}), "division by zero");
            assert!(
                started.elapsed() < FAST,
                "recording blocked the calculator for {started:?}"
            );
        }
    }

    #[test]
    fn history_entry_prefers_result_and_falls_back_to_error() {
        let entry = |result, error| HistoryEntry {
            id: 7,
            operation: "add".to_owned(),
            inputs: serde_json::json!({ "a": 1, "b": 1 }),
            result,
            error,
            created_at: DateTime::<Utc>::UNIX_EPOCH,
        };
        assert!(
            entry(Some("2".to_owned()), None)
                .to_line()
                .ends_with("-> 2")
        );
        assert!(
            entry(None, Some("division by zero".to_owned()))
                .to_line()
                .ends_with("-> error: division by zero")
        );
    }
}
