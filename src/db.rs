use std::{
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use sqlx::{
    MySqlPool, PgPool, SqlitePool,
    migrate::Migrator,
    mysql::{MySqlConnectOptions, MySqlPoolOptions},
    postgres::{PgConnectOptions, PgPoolOptions},
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use tokio::sync::OnceCell;
use tracing::{debug, warn};

use crate::config::StorageConfig;

/// Migrations are per-dialect: there is no spelling of "auto-incrementing key, JSON column,
/// descending index" that all three backends accept.
///
/// One embedded set per backend rather than one shared file, chosen by the URL's scheme at runtime.
/// `migrate!` still reads the SQL at *compile* time, so there is no filesystem access at runtime and
/// no `DATABASE_URL`-dependent build step.
///
/// Every dialect's `0001_calc_history.sql` describes the same table. Adding migration `0002_*` means
/// adding it to all three directories, and never editing an applied one (`VersionMismatch`).
static POSTGRES_MIGRATIONS: Migrator = sqlx::migrate!("./migrations/postgres");
static MYSQL_MIGRATIONS: Migrator = sqlx::migrate!("./migrations/mysql");
static SQLITE_MIGRATIONS: Migrator = sqlx::migrate!("./migrations/sqlite");

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

/// Tunable storage limits, normally produced from [`StorageConfig`].
///
/// All of these are backend-independent: each [`Dialect`] builds its own pool from the same
/// [`Limits`], so `acquire_timeout` bounds a MySQL handshake and an SQLite busy wait exactly as it
/// bounds a PostgreSQL connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Maximum pooled connections.
    pub max_connections: u32,
    /// How long an operation may wait for a connection before it is reported as failed.
    ///
    /// The pool is created lazily and never fails to connect, so without this bound every tool
    /// call would block on TCP connect for ~30s whenever the database is down — exactly the
    /// situation graceful degradation exists for.
    pub acquire_timeout: Duration,
    /// Consecutive connection failures after which storage is skipped without trying to connect.
    pub breaker_threshold: u32,
    /// How long storage stays skipped after tripping the circuit breaker.
    pub breaker_cooldown: Duration,
    /// Upper bound on rows returned by `calc_history`.
    pub max_history_rows: i64,
}

impl Default for Limits {
    fn default() -> Self {
        // Mirrors `StorageConfig::default()`, so a `Store` built without configuration behaves
        // exactly like one built from an empty config file.
        Self::from(&StorageConfig::default())
    }
}

impl From<&StorageConfig> for Limits {
    fn from(storage: &StorageConfig) -> Self {
        Self {
            max_connections: storage.max_connections,
            acquire_timeout: storage.acquire_timeout,
            breaker_threshold: storage.breaker_threshold,
            breaker_cooldown: storage.breaker_cooldown,
            max_history_rows: storage.max_history_rows,
        }
    }
}

/// Which database [`Store`] talks to, decided by the URL's scheme.
///
/// These are the three drivers `sqlx` 0.8 implements itself. Each one carries its own SQL dialect
/// and its own migration set; everything else about a backend — pooling, breaker bookkeeping, the
/// tool-facing API — is shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// `postgres://` or `postgresql://`.
    Postgres,
    /// `mysql://` or `mariadb://` (sqlx's MySQL driver speaks to both).
    MySql,
    /// `sqlite://`, including `sqlite::memory:`.
    Sqlite,
}

impl Dialect {
    /// Every scheme [`Self::from_url`] accepts, for error messages and documentation.
    pub const SCHEMES: [&'static str; 5] = ["postgres", "postgresql", "mysql", "mariadb", "sqlite"];

    /// Name of the sqlx driver behind this dialect, reported by `db_status`.
    ///
    /// A `postgresql://` or `mariadb://` URL reports as `postgres`/`mysql`: that is the driver that
    /// actually handles the connection, which is the useful thing to see in a log line.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::MySql => "mysql",
            Self::Sqlite => "sqlite",
        }
    }

    /// Recognise a backend from its URL scheme.
    ///
    /// # Errors
    ///
    /// Returns a message naming the accepted schemes if the URL has no scheme or one this build has
    /// no driver for. Reporting it beats guessing: a typo'd scheme must not look like "storage is
    /// merely unconfigured".
    pub fn from_url(url: &str) -> Result<Self, String> {
        let Some((scheme, _)) = url.split_once(':') else {
            return Err(format!(
                "expected a database URL starting with one of {}: (for example \
                 postgres://mcp:mcp@127.0.0.1:5432/mcp), found {url:?}",
                Self::SCHEMES.join(", ")
            ));
        };
        match scheme.trim().to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" => Ok(Self::Postgres),
            "mysql" | "mariadb" => Ok(Self::MySql),
            "sqlite" => Ok(Self::Sqlite),
            _ => Err(format!(
                "unsupported database URL scheme {scheme:?}; expected one of {}",
                Self::SCHEMES.join(", ")
            )),
        }
    }

    /// The migrations to apply against this backend.
    const fn migrations(self) -> &'static Migrator {
        match self {
            Self::Postgres => &POSTGRES_MIGRATIONS,
            Self::MySql => &MYSQL_MIGRATIONS,
            Self::Sqlite => &SQLITE_MIGRATIONS,
        }
    }

    /// The bind marker for the `index`-th (1-based) argument.
    ///
    /// This is the *only* SQL syntax that differs between these three backends: PostgreSQL numbers
    /// its placeholders, MySQL and SQLite both use bare `?`. Writing the queries in one dialect's
    /// spelling and rewriting the markers here is what keeps a single copy of the SQL text.
    const fn placeholder(self, index: usize) -> &'static str {
        match self {
            Self::Postgres => match index {
                1 => "$1",
                2 => "$2",
                3 => "$3",
                4 => "$4",
                5 => "$5",
                // Every call site passes a literal index, so this cannot be reached; panicking in a
                // `const fn` keeps a future sixth argument from compiling into a broken query.
                _ => panic!("PostgreSQL placeholders are limited to $1..=$5"),
            },
            Self::MySql | Self::Sqlite => "?",
        }
    }

    /// Append one call.
    ///
    /// `created_at` is written by the server instead of being left to the column default so that all
    /// three backends store the same instant, in one format, from one clock. That matters where the
    /// defaults disagree: SQLite's `CURRENT_TIMESTAMP` is a `YYYY-MM-DD HH:MM:SS` string that does
    /// not sort against the RFC 3339 text `DateTime<Utc>` writes, and MySQL's `TIMESTAMP` silently
    /// shifts by the session time zone. See the migration files.
    fn insert_sql(self) -> String {
        format!(
            "INSERT INTO calc_history (operation, inputs, result, error, created_at) \
             VALUES ({}, {}, {}, {}, {})",
            self.placeholder(1),
            self.placeholder(2),
            self.placeholder(3),
            self.placeholder(4),
            self.placeholder(5),
        )
    }

    /// Most recent entries first. `id` breaks ties, which are real: the SQLite and MySQL defaults
    /// only have second resolution.
    fn list_sql(self) -> String {
        format!(
            "SELECT id, operation, inputs, result, error, created_at \
             FROM calc_history \
             ORDER BY created_at DESC, id DESC \
             LIMIT {}",
            self.placeholder(1),
        )
    }

    fn delete_sql(self) -> &'static str {
        "DELETE FROM calc_history"
    }

    fn count_sql(self) -> &'static str {
        "SELECT count(*) FROM calc_history"
    }
}

/// The pool for whichever backend the URL selected.
///
/// An enum rather than `sqlx::Any`, which would be the obvious runtime-generic choice. `Any` erases
/// rows to a fixed set of scalar kinds (null, bool, smallint, integer, bigint, real, double, text,
/// blob) and rejects anything else, so neither a `JSON`/`JSONB` column nor *any* timestamp type can
/// be read back through it — `HistoryEntry` would be unconstructible. Keeping the concrete pools
/// costs one `match` in four methods and buys real JSON and timestamp columns on every backend.
#[derive(Debug, Clone)]
enum Pool {
    Postgres(PgPool),
    MySql(MySqlPool),
    Sqlite(SqlitePool),
}

impl Pool {
    const fn dialect(&self) -> Dialect {
        match self {
            Self::Postgres(_) => Dialect::Postgres,
            Self::MySql(_) => Dialect::MySql,
            Self::Sqlite(_) => Dialect::Sqlite,
        }
    }

    /// Build a pool for `url` **without dialling it**, so an unreachable or wrong database cannot
    /// fail here — that is the whole point of lazy, degrading storage.
    ///
    /// TLS is left entirely up to the URL and the driver's own default. sqlx is built without a TLS
    /// feature in this crate, and every driver treats "no TLS compiled in" as "stay on plaintext",
    /// so a local container keeps working while enabling a TLS feature later upgrades automatically.
    /// A managed database that *requires* TLS is reached by putting `?sslmode=require` (PostgreSQL)
    /// or `?ssl-mode=REQUIRED` (MySQL) in the URL.
    fn connect(url: &str, dialect: Dialect, limits: Limits) -> Result<Self, String> {
        match dialect {
            Dialect::Postgres => {
                let options = PgConnectOptions::from_str(url).map_err(|error| error.to_string())?;
                Ok(Self::Postgres(
                    PgPoolOptions::new()
                        .max_connections(limits.max_connections)
                        .acquire_timeout(limits.acquire_timeout)
                        .connect_lazy_with(options),
                ))
            }
            Dialect::MySql => {
                let options =
                    MySqlConnectOptions::from_str(url).map_err(|error| error.to_string())?;
                Ok(Self::MySql(
                    MySqlPoolOptions::new()
                        .max_connections(limits.max_connections)
                        .acquire_timeout(limits.acquire_timeout)
                        .connect_lazy_with(options),
                ))
            }
            Dialect::Sqlite => {
                let (database, params) = sqlite_url_parts(url);
                let mut options =
                    SqliteConnectOptions::from_str(url).map_err(|error| error.to_string())?;
                // Create the file if it is missing, so `sqlite://mcp.db` works on a fresh checkout.
                // A URL pinned to `mode=ro`/`mode=rw` means "this file must already exist".
                if !matches!(sqlite_param(params, "mode"), Some("ro" | "rw")) {
                    options = options.create_if_missing(true);
                }
                // sqlx's own busy timeout is 5s, which would outlast the `acquire_timeout` this
                // store promises callers, so tie the two together.
                options = options.pragma(
                    "busy_timeout",
                    limits.acquire_timeout.as_millis().to_string(),
                );
                // An in-memory database is private to this process, and SQLite's shared-cache mode
                // takes *table*-level locks that the busy handler cannot wait out. One connection
                // serialises it, which turns a contended write into a slow one rather than a failed
                // one; a file-backed database keeps the configured pool size.
                let max_connections = if sqlite_is_in_memory(database, params) {
                    1
                } else {
                    limits.max_connections
                };
                Ok(Self::Sqlite(
                    SqlitePoolOptions::new()
                        .max_connections(max_connections)
                        .acquire_timeout(limits.acquire_timeout)
                        .connect_lazy_with(options),
                ))
            }
        }
    }
}

/// Run one block against whichever pool the store holds.
///
/// The three pools are the same API with three different Rust types, so the block is written once
/// and instantiated per backend. Everything that genuinely differs between backends — the SQL text
/// and the migration set — is chosen from the [`Dialect`] *before* this point, so nothing inside a
/// block needs to branch on the backend again.
macro_rules! with_pool {
    ($backend:expr, |$pool:ident| $body:block) => {
        match $backend {
            Pool::Postgres($pool) => $body,
            Pool::MySql($pool) => $body,
            Pool::Sqlite($pool) => $body,
        }
    };
}

/// The database path and query string of a SQLite URL, i.e. what follows the scheme.
///
/// This mirrors sqlx's own parser because [`SqliteConnectOptions`] does not expose whether it was
/// handed an in-memory database or a read-only file, and both change how the pool is built.
fn sqlite_url_parts(url: &str) -> (&str, Option<&str>) {
    let rest = url
        .trim_start_matches("sqlite://")
        .trim_start_matches("sqlite:");
    rest.split_once('?')
        .map_or((rest, None), |(database, params)| (database, Some(params)))
}

/// One `key=value` pair from a SQLite URL's query string.
///
/// Percent-decoding is skipped on purpose: no parameter inspected here is one that needs it.
fn sqlite_param<'a>(params: Option<&'a str>, key: &str) -> Option<&'a str> {
    params?.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then_some(value)
    })
}

/// Whether a SQLite URL names a private in-memory database rather than a file on disk.
fn sqlite_is_in_memory(database: &str, params: Option<&str>) -> bool {
    database == ":memory:" || matches!(sqlite_param(params, "mode"), Some("memory"))
}

/// Optional database-backed storage for calculator history.
///
/// A `Store` is always constructible: when no usable URL is configured the backend is `None` and
/// every storage operation returns an error instead of panicking, so the MCP server still starts and
/// serves the pure-computation tools.
///
/// Which backend is in use follows from the URL's scheme (see [`Dialect`]); nothing else has to be
/// configured. When the database is configured but unreachable, a circuit breaker keeps the
/// arithmetic tools fast: after [`Limits::breaker_threshold`] connection failures, storage is
/// skipped for [`Limits::breaker_cooldown`] instead of making every tool call wait out
/// [`Limits::acquire_timeout`].
#[derive(Debug, Clone)]
pub struct Store {
    backend: Option<Pool>,
    schema: Arc<OnceCell<()>>,
    breaker: Arc<Mutex<Breaker>>,
    limits: Limits,
}

impl Default for Store {
    /// Same as [`Store::disabled`]: storage off, limits at their defaults.
    fn default() -> Self {
        Self::disabled()
    }
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
        Self::with_backend(None, Limits::default())
    }

    /// Message returned when storage is not configured.
    pub const DISABLED: &'static str = "storage unavailable: no usable database URL is \
        configured (set DATABASE_URL, --db-url, or [storage].url in a config file)";

    /// Build a store from resolved [`StorageConfig`].
    ///
    /// This is the normal entry point: the binary resolves configuration once at startup and hands
    /// the storage section over. A missing, unparsable or unsupported URL degrades to disabled
    /// storage with a warning rather than failing.
    #[must_use]
    pub fn from_settings(storage: &StorageConfig) -> Self {
        match &storage.url {
            Some(url) => Self::connect_with_limits(url, Limits::from(storage)),
            None => {
                warn!(
                    "{}; serving calculator tools without history",
                    Self::DISABLED
                );
                Self::disabled()
            }
        }
    }

    /// Build a lazily-connected store for `url` using default limits. The pool is **not** contacted
    /// here, so this never fails on an unreachable database.
    #[must_use]
    pub fn connect(url: &str) -> Self {
        Self::connect_with_limits(url, Limits::default())
    }

    /// As [`Self::connect`], with an explicit bound on how long operations wait for a connection.
    /// Tests use a millisecond-scale timeout to stay fast.
    #[must_use]
    pub fn connect_with_timeout(url: &str, acquire_timeout: Duration) -> Self {
        Self::connect_with_limits(
            url,
            Limits {
                acquire_timeout,
                ..Limits::default()
            },
        )
    }

    /// As [`Self::connect`], with every limit supplied explicitly.
    #[must_use]
    pub fn connect_with_limits(url: &str, limits: Limits) -> Self {
        match Self::try_connect(url, limits) {
            Ok(store) => store,
            Err(error) => {
                warn!(%error, "unusable database URL; serving calculator tools without history");
                Self::with_backend(None, limits)
            }
        }
    }

    fn try_connect(url: &str, limits: Limits) -> Result<Self, String> {
        let dialect = Dialect::from_url(url)?;
        Pool::connect(url, dialect, limits).map(|pool| Self::with_backend(Some(pool), limits))
    }

    fn with_backend(backend: Option<Pool>, limits: Limits) -> Self {
        Self {
            backend,
            // Shared with every clone, like `breaker`. `tokio::sync::OnceCell` clones to a *fresh,
            // empty* cell, and `record_success` hands a clone to a spawned task — so a per-store
            // cell would let that task run its own migration alongside the caller's. PostgreSQL
            // hides that behind its advisory lock; SQLite has no migration lock at all, so both
            // runs read an empty `_sqlx_migrations` and the loser fails with a UNIQUE violation on
            // `version`.
            schema: Arc::new(OnceCell::const_new()),
            breaker: Arc::default(),
            limits,
        }
    }

    /// True when a database is configured, whether or not it is reachable right now.
    #[must_use]
    pub const fn is_configured(&self) -> bool {
        self.backend.is_some()
    }

    /// The backend in use, or `None` when storage is disabled.
    #[must_use]
    pub fn dialect(&self) -> Option<Dialect> {
        self.backend.as_ref().map(Pool::dialect)
    }

    /// The limits this store was built with.
    #[must_use]
    pub const fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Upper bound on rows `calc_history` will return, from configuration.
    #[must_use]
    pub const fn max_history_rows(&self) -> i64 {
        self.limits.max_history_rows
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
        if breaker.failures >= self.limits.breaker_threshold {
            breaker.retry_after = Some(Instant::now() + self.limits.breaker_cooldown);
            warn!(
                failures = breaker.failures,
                cooldown = ?self.limits.breaker_cooldown,
                "storage circuit breaker open; skipping database temporarily"
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
    /// not up yet. This is why startup never blocks on the database.
    async fn ensure_schema(&self) -> Result<&Pool, String> {
        let backend = self.backend.as_ref().ok_or(Self::DISABLED)?;
        let dialect = backend.dialect();
        self.check_breaker()?;
        self.schema
            .get_or_try_init(|| async {
                // Callers queue behind this init, so re-check the breaker here: once it opens,
                // the rest of the queue fails instantly instead of each retrying the connection.
                self.check_breaker()?;
                debug!(dialect = dialect.label(), "applying database migrations");
                // MigrateError wraps the underlying sqlx error, so unwrap it for the breaker:
                // a missing database must trip it, a bad migration file must not.
                match with_pool!(backend, |pool| { dialect.migrations().run(pool).await }) {
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
        Ok(backend)
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
        let backend = self.ensure_schema().await?;
        let dialect = backend.dialect();
        let sql = dialect.insert_sql();
        let created_at = Utc::now();
        // `()` as the normalised result: an insert's row count is not interesting, but the three
        // `QueryResult` types are not the same type, so the arms need a common one.
        let executed = with_pool!(backend, |pool| {
            sqlx::query(&sql)
                .bind(operation)
                .bind(inputs)
                .bind(result)
                .bind(error)
                .bind(created_at)
                .execute(pool)
                .await
                .map(|_| ())
        });
        self.finish(executed, "failed to insert history")
    }

    /// Most recent entries first.
    pub async fn list(&self, limit: i64) -> Result<Vec<HistoryEntry>, String> {
        let backend = self.ensure_schema().await?;
        let sql = backend.dialect().list_sql();
        let fetched = with_pool!(backend, |pool| {
            sqlx::query_as::<_, HistoryEntry>(&sql)
                .bind(limit)
                .fetch_all(pool)
                .await
        });
        self.finish(fetched, "failed to read history")
    }

    /// Drop all history and report how many rows were removed.
    ///
    /// `DELETE` rather than the faster `TRUNCATE`, which exists on all three backends but reports no
    /// row count on PostgreSQL and MySQL, so `rows_affected` would be 0 and the tool would report a
    /// lie.
    pub async fn clear(&self) -> Result<u64, String> {
        let backend = self.ensure_schema().await?;
        let sql = backend.dialect().delete_sql();
        // Each driver has its own `QueryResult` type, so the row count is taken inside the block to
        // give all three arms one common return type.
        let executed = with_pool!(backend, |pool| {
            sqlx::query(sql)
                .execute(pool)
                .await
                .map(|result| result.rows_affected())
        });
        self.finish(executed, "failed to clear history")
    }

    /// Verify the database answers, then report the stored row count and which backend answered.
    pub async fn status(&self) -> Result<String, String> {
        let backend = self.ensure_schema().await?;
        let dialect = backend.dialect();
        let sql = dialect.count_sql();
        let counted = with_pool!(backend, |pool| {
            sqlx::query_as::<_, (i64,)>(sql).fetch_one(pool).await
        });
        let (rows,) = self.finish(counted, "database unreachable")?;
        Ok(format!(
            "{} database reachable, {rows} stored calculations",
            dialect.label()
        ))
    }
}

/// Private access for the relocated unit tests in `tests/db.rs`.
///
/// A child module can already read its parent's private items, so nothing here needs to widen
/// visibility: these are thin shims over internals that stay `fn`, not `pub fn`. `#[doc(hidden)]`
/// keeps them out of the rendered API. This exists so that *every* test lives in `tests/` — see
/// `AGENTS.md`.
#[doc(hidden)]
pub mod testing {
    use super::{
        Dialect, Migrator, POSTGRES_MIGRATIONS, Store, sqlite_is_in_memory as parse_in_memory,
        sqlite_param as parse_param, sqlite_url_parts as parse_url_parts,
    };

    /// Count a failure as the breaker sees it, without needing a real database.
    pub fn note_failure(store: &Store, error: &sqlx::Error) {
        store.note_failure(error);
    }

    /// Reset the breaker as a successful round-trip would.
    pub fn note_reachable(store: &Store) {
        store.note_reachable();
    }

    /// Whether the breaker would currently reject a call.
    pub fn breaker_is_open(store: &Store) -> bool {
        store.check_breaker().is_err()
    }

    /// Consecutive connectivity failures recorded so far.
    pub fn failure_count(store: &Store) -> u32 {
        store
            .breaker
            .lock()
            .expect("breaker mutex poisoned")
            .failures
    }

    /// Pretend migrations have already run, so a clone's view of the cell can be inspected.
    pub async fn mark_schema_initialised(store: &Store) {
        store.schema.get_or_init(|| async {}).await;
    }

    /// Whether *this* store's view of the schema cell is set — the thing that must survive a clone.
    pub fn schema_initialised(store: &Store) -> bool {
        store.schema.initialized()
    }

    /// The `INSERT` a backend would send, placeholders included.
    pub fn insert_sql(dialect: Dialect) -> String {
        dialect.insert_sql()
    }

    /// The `SELECT` a backend would send, placeholders included.
    pub fn list_sql(dialect: Dialect) -> String {
        dialect.list_sql()
    }

    /// The migrations a backend would apply.
    pub fn migrations(dialect: Dialect) -> &'static Migrator {
        dialect.migrations()
    }

    /// The SQL of the PostgreSQL migration that has already shipped, for checksum pinning.
    ///
    /// Owned rather than borrowed because a migration body is a `Cow` and the test only needs to
    /// compare it with the file on disk.
    pub fn postgres_migration_sql() -> Option<String> {
        POSTGRES_MIGRATIONS
            .iter()
            .next()
            .map(|migration| migration.sql.to_string())
    }

    /// See the private `sqlite_url_parts`, which this mirrors.
    pub fn sqlite_url_parts(url: &str) -> (&str, Option<&str>) {
        parse_url_parts(url)
    }

    /// See the private `sqlite_param`, which this mirrors.
    pub fn sqlite_param<'a>(params: Option<&'a str>, key: &str) -> Option<&'a str> {
        parse_param(params, key)
    }

    /// See the private `sqlite_is_in_memory`, which this mirrors.
    pub fn sqlite_is_in_memory(database: &str, params: Option<&str>) -> bool {
        parse_in_memory(database, params)
    }
}
