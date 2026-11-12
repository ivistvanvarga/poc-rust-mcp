//! Layered configuration for the MCP server.
//!
//! Four layers, each overriding the previous one:
//!
//! 1. built-in defaults ([`Config::defaults`])
//! 2. an optional TOML file, named explicitly via `--config` or `MCP_CONFIG`
//! 3. environment variables
//! 4. command-line flags
//!
//! **No config file is ever required** — the server runs on defaults alone, which is what keeps a
//! bare `cargo run` working. Nothing is discovered implicitly: an absent file is simply an absent
//! layer, so behaviour never depends on a file that happens to lie in the working directory.
//!
//! The file is deserialized into a sparse mirror (`FileConfig`) where every field is optional, and
//! only fields actually present in the file overwrite what came before. Unknown keys are rejected
//! rather than ignored, so a typo is reported at startup instead of silently doing nothing.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fmt,
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use serde::Deserialize;
use tracing_subscriber::EnvFilter;

use http::uri::{Authority, Uri};

/// Default bind address for `--sse`.
pub const DEFAULT_SSE_ADDRESS: &str = "127.0.0.1:8000";

/// Default cap on simultaneous SSE sessions.
pub const DEFAULT_SSE_MAX_SESSIONS: usize = 64;

/// Default `tracing` filter when `RUST_LOG` is unset.
pub const DEFAULT_LOG_FILTER: &str = "info";

/// Environment variable names recognised by the env layer.
pub mod env_keys {
    /// Path to the TOML config file.
    pub const CONFIG: &str = "MCP_CONFIG";
    /// Database URL; unchanged from before config files existed. The scheme picks the backend:
    /// `postgres`, `postgresql`, `mysql`, `mariadb` or `sqlite`.
    pub const DATABASE_URL: &str = "DATABASE_URL";
    /// Bind address for the SSE transport.
    pub const SSE_ADDRESS: &str = "MCP_SSE_ADDRESS";
    /// `tracing` filter.
    pub const LOG_FILTER: &str = "RUST_LOG";
    /// Rows per page for the MCP `*/list` methods.
    pub const LIST_PAGE_SIZE: &str = "MCP_LIST_PAGE_SIZE";
    /// Maximum pooled connections.
    pub const STORAGE_MAX_CONNECTIONS: &str = "MCP_STORAGE_MAX_CONNECTIONS";
    /// Connection-acquire budget, in milliseconds.
    pub const STORAGE_ACQUIRE_TIMEOUT_MS: &str = "MCP_STORAGE_ACQUIRE_TIMEOUT_MS";
    /// Consecutive connectivity failures that open the circuit breaker.
    pub const STORAGE_BREAKER_THRESHOLD: &str = "MCP_STORAGE_BREAKER_THRESHOLD";
    /// How long the breaker stays open, in milliseconds.
    pub const STORAGE_BREAKER_COOLDOWN_MS: &str = "MCP_STORAGE_BREAKER_COOLDOWN_MS";
    /// Upper bound on rows returned by `calc_history`.
    pub const STORAGE_MAX_HISTORY_ROWS: &str = "MCP_STORAGE_MAX_HISTORY_ROWS";
    /// Comma-separated `Host` authorities the SSE transport accepts.
    pub const SSE_ALLOWED_HOSTS: &str = "MCP_SSE_ALLOWED_HOSTS";
    /// Comma-separated browser origins the SSE transport accepts.
    pub const SSE_ALLOWED_ORIGINS: &str = "MCP_SSE_ALLOWED_ORIGINS";
    /// Bearer token required on every SSE request.
    pub const SSE_TOKEN: &str = "MCP_SSE_TOKEN";
    /// Maximum simultaneous SSE sessions.
    pub const SSE_MAX_SESSIONS: &str = "MCP_SSE_MAX_SESSIONS";
}

/// Everything the server needs, after all four layers have been merged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub server: ServerConfig,
    pub storage: StorageConfig,
}

/// Transport and logging settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// Address `--sse` binds to.
    pub sse_address: SocketAddr,
    /// `tracing` filter directive.
    pub log_filter: String,
    /// Rows per page for the MCP `*/list` methods.
    ///
    /// Every catalogue here is small, so this only matters to a client that pages deliberately —
    /// or to a test, which sets it low to walk a cursor. It exists because pagination that cannot
    /// be exercised is pagination that rots.
    pub list_page_size: usize,
    /// Host authorities accepted by the SSE transport, from configuration.
    ///
    /// Entries are `host` or `host:port`; an entry without a port matches any port on that host.
    /// Empty means "loopback defaults" when the listener is on a loopback address, and is a
    /// startup error otherwise (see [`crate::sse::Settings`]).
    pub sse_allowed_hosts: Vec<String>,
    /// Browser `Origin` values accepted by the SSE transport.
    ///
    /// This is a *restriction* list, not a CORS configuration: matching requests are allowed, but
    /// no `Access-Control-Allow-*` headers are ever emitted, and preflights are not answered.
    pub sse_allowed_origins: Vec<String>,
    /// Bearer token required by the SSE transport. `None` means no token, which is only allowed on
    /// a loopback listener.
    pub sse_token: Option<Secret>,
    /// Maximum number of simultaneous SSE sessions.
    pub sse_max_sessions: usize,
}

/// A string that must not be printed.
///
/// `ServerConfig` derives `Debug`, and every layer of configuration is routinely logged at debug
/// level. A token in that log line would be the whole secret, so the only formatting this type
/// allows is the redacted one.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret itself. Call this only where the value is actually used.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// One entry in `sse_allowed_hosts`: a host, optionally pinned to a port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostPattern {
    pub(crate) host: String,
    pub(crate) port: Option<u16>,
}

/// One entry in `sse_allowed_origins`: scheme, host, and optional pinned port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OriginPattern {
    pub(crate) scheme: String,
    pub(crate) host: String,
    pub(crate) port: Option<u16>,
}

/// Lowercase a host for comparison, rejecting values that could never be a host.
///
/// Brackets around an IPv6 literal are stripped, as is one trailing root dot; both are spelling
/// differences for the same host, and a comparison that missed them would be a security bug in
/// whichever direction it failed.
pub(crate) fn normalize_host(host: &str) -> Option<String> {
    let host = host.trim();
    if host.is_empty() || host.contains('%') || host.contains('*') {
        return None;
    }
    if host.chars().any(char::is_control) {
        return None;
    }
    let host = host.strip_prefix('[').unwrap_or(host);
    let host = host.strip_suffix(']').unwrap_or(host);
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

/// Parse one `host` or `host:port` allowlist entry.
///
/// Anything with a path, scheme, userinfo, wildcard or port 0 is rejected: this is an allowlist, and
/// an entry that could match more than the operator intended has no place in one.
pub(crate) fn parse_host_pattern(value: &str) -> Option<HostPattern> {
    let value = value.trim();
    if value.is_empty() || value.contains('@') {
        return None;
    }
    let authority: Authority = value.parse().ok()?;
    if authority.port_u16() == Some(0) {
        return None;
    }
    Some(HostPattern {
        host: normalize_host(authority.host())?,
        port: authority.port_u16(),
    })
}

/// Parse one browser-origin allowlist entry.
///
/// Only an origin (`scheme://host[:port]`) is accepted: a path or query would suggest that the
/// allowlist is a URL filter, and a wildcard would defeat its purpose.
pub(crate) fn parse_origin_pattern(value: &str) -> Option<OriginPattern> {
    let value = value.trim();
    if value.is_empty() || value.contains('*') || value.contains('@') {
        return None;
    }
    let uri: Uri = value.parse().ok()?;
    let scheme = uri.scheme_str()?.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority = uri.authority()?;
    if authority.port_u16() == Some(0) {
        return None;
    }
    if let Some(path_and_query) = uri.path_and_query() {
        let path = path_and_query.path();
        if !path.is_empty() && path != "/" {
            return None;
        }
        if path_and_query.query().is_some() {
            return None;
        }
    }
    Some(OriginPattern {
        scheme,
        host: normalize_host(authority.host())?,
        port: authority.port_u16(),
    })
}

/// Calculation-history settings. `url: None` means storage is disabled.
///
/// Which backend that URL selects is not configured here: the scheme decides it, and
/// [`crate::db::Dialect`] owns the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageConfig {
    /// Database URL; `None` disables storage entirely.
    pub url: Option<String>,
    /// Maximum pooled connections.
    pub max_connections: u32,
    /// How long an operation may wait for a connection before it counts as failed.
    pub acquire_timeout: Duration,
    /// Consecutive connectivity failures after which storage is skipped.
    pub breaker_threshold: u32,
    /// How long storage stays skipped once the breaker opens.
    pub breaker_cooldown: Duration,
    /// Upper bound on rows `calc_history` will return.
    pub max_history_rows: i64,
}

impl StorageConfig {
    /// Whether a URL was configured at all. An unparsable or unsupported URL still counts as
    /// configured here; [`crate::db::Store`] is what decides it is unusable.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.url.is_some()
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            sse_address: SocketAddr::from_str(DEFAULT_SSE_ADDRESS)
                .expect("the built-in default SSE address is a valid socket address"),
            log_filter: DEFAULT_LOG_FILTER.to_owned(),
            list_page_size: crate::server::DEFAULT_LIST_PAGE_SIZE,
            sse_allowed_hosts: Vec::new(),
            sse_allowed_origins: Vec::new(),
            sse_token: None,
            sse_max_sessions: DEFAULT_SSE_MAX_SESSIONS,
        }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        // The acquire timeout is what keeps a down database from stalling every tool call, and the
        // breaker keeps repeated calls from paying it again. Both are configurable but the
        // defaults are the values this server is tuned for.
        Self {
            url: None,
            max_connections: 5,
            acquire_timeout: Duration::from_secs(3),
            breaker_threshold: 2,
            breaker_cooldown: Duration::from_secs(30),
            max_history_rows: 100,
        }
    }
}

/// Source of environment variables, injectable so layering can be tested without touching the
/// process environment (`std::env::set_var` is `unsafe` on edition 2024).
pub trait Env {
    /// Return the value for `key`, or `None` when unset.
    ///
    /// Blank and whitespace-only values count as unset: an empty `DATABASE_URL` in a compose file
    /// or CI job means "not configured", not "empty URL".
    fn get(&self, key: &str) -> Option<String>;
}

/// The real process environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemEnv;

impl Env for SystemEnv {
    fn get(&self, key: &str) -> Option<String> {
        non_blank(std::env::var(key).ok())
    }
}

/// An in-memory environment for tests and embedders.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MapEnv(BTreeMap<String, String>);

impl MapEnv {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder-style [`Self::set`].
    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.set(key, value);
        self
    }

    /// Set a variable, replacing any previous value.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.0.insert(key.into(), value.into());
    }
}

impl Env for MapEnv {
    fn get(&self, key: &str) -> Option<String> {
        non_blank(self.0.get(key).cloned())
    }
}

/// Treat blank and whitespace-only values as unset: an empty `DATABASE_URL` in a compose file or CI
/// job means "not configured", not "empty URL".
fn non_blank(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

/// Command-line flags, which form the highest-precedence layer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cli {
    pub help: bool,
    /// Serve over SSE instead of stdio.
    pub serve_sse: bool,
    /// Address given to `--sse`, if the flag carried one.
    pub sse_address: Option<String>,
    pub config_path: Option<PathBuf>,
    pub database_url: Option<String>,
}

impl Cli {
    /// Parse flags from an iterator of arguments, e.g. `std::env::args().skip(1)`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::UnknownArgument`] or [`ConfigError::MissingValue`] for malformed input.
    pub fn parse_from<I, S>(args: I) -> Result<Self, ConfigError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let mut args = args.into_iter().map(Into::into).peekable();
        let mut cli = Self::default();

        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("-h" | "--help") => cli.help = true,
                Some("-c" | "--config") => {
                    cli.config_path = Some(PathBuf::from(next_value(&mut args, "--config")?));
                }
                Some("--db-url") => {
                    cli.database_url = Some(
                        next_value(&mut args, "--db-url")?
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
                Some("--sse") => {
                    cli.serve_sse = true;
                    // The address is optional, so only consume the next argument when it cannot be
                    // another flag: `--sse 0.0.0.0:9000` works, `--sse --db-url x` does not eat it.
                    if let Some(next) = args.peek().and_then(|arg| arg.as_os_str().to_str())
                        && !next.starts_with('-')
                    {
                        let addr = args.next().expect("peeked argument is still present");
                        cli.sse_address = Some(addr.to_string_lossy().into_owned());
                    }
                }
                _ => {
                    return Err(ConfigError::UnknownArgument(
                        arg.to_string_lossy().into_owned(),
                    ));
                }
            }
        }
        Ok(cli)
    }

    /// Help text, also listing the environment variables and the config file.
    #[must_use]
    pub fn usage() -> &'static str {
        "\
poc-rust-mcp — MCP calculator server with a database-backed history

Usage:
  poc-rust-mcp                Serve MCP over stdio (default, for client subprocesses)
  poc-rust-mcp --sse [ADDR]   Serve MCP over SSE on ADDR (default 127.0.0.1:8000)
  poc-rust-mcp --help         Show this message

Configuration, highest precedence first:
  1. command-line flags
       --config <PATH>  TOML config file (also MCP_CONFIG)
       --db-url <URL>   database URL (also DATABASE_URL)
       --sse [ADDR]     bind address for SSE (also MCP_SSE_ADDRESS)
  2. environment variables
  3. the TOML config file, if one was named
  4. built-in defaults

  No config file is required. Precedence for a setting is
  flag > environment > file > default.

Database:
  The URL's scheme picks the backend; nothing else is configured.
    postgres://mcp:mcp@127.0.0.1:5432/mcp   PostgreSQL
    mysql://mcp:mcp@127.0.0.1:3306/mcp      MySQL or MariaDB
    sqlite://mcp.db  (or sqlite::memory:)   SQLite, no server needed
  Unset, unparsable or unreachable = storage disabled, calculator tools still work.
  TLS follows the URL and the driver's own default; add ?sslmode=require (PostgreSQL)
  or ?ssl-mode=REQUIRED (MySQL) if the server demands it.

MCP features:
  tools       add, sub, mul, div, db_status, calc_history, clear_calc_history
  prompts     review_calculation_history, check_storage_health
  resources   calc://history, calc://history/{id}, calc://history/operation/{operation}
              subscribe with resources/subscribe to be told when history changes
  logging     logging/setLevel filters the per-calculation notifications/message
  completion  argument values for prompts and resource templates
  All */list methods are paginated (server.list_page_size).

SSE security:
  A loopback listener (the 127.0.0.1 default) trusts local processes. Anything else —
  --sse 0.0.0.0:9000 included — requires MCP_SSE_TOKEN *and* MCP_SSE_ALLOWED_HOSTS, and
  the server refuses to start without them.
    MCP_SSE_TOKEN          Bearer token; send it as `Authorization: Bearer <token>`.
                           There is deliberately no --sse-token flag: a command line is
                           world-readable through ps and the shell history.
    MCP_SSE_ALLOWED_HOSTS  Comma-separated Host values, host or host:port. A Host without
                           a port matches any port on that host. Non-loopback needs this.
    MCP_SSE_ALLOWED_ORIGINS
                           Comma-separated browser origins (https://ui.example). This is a
                           restriction list, not CORS: no CORS headers are sent and
                           preflights are not answered.
    MCP_SSE_MAX_SESSIONS   Maximum simultaneous sessions (default 64).
  Every request's Host and Origin are checked before its token. Behind a reverse proxy,
  forward the original Host header; X-Forwarded-Host is not trusted.

Environment:
  DATABASE_URL   database URL for calculation history, e.g.
                 postgres://mcp:mcp@127.0.0.1:5432/mcp
                 Unset or unreachable = storage disabled, calculator tools still work
  RUST_LOG       Log filter, e.g. RUST_LOG=debug (logs go to stderr)
  MCP_LIST_PAGE_SIZE
                 Rows per page for the MCP */list methods (default 20)
  MCP_SSE_ADDRESS, MCP_SSE_TOKEN, MCP_SSE_ALLOWED_HOSTS, MCP_SSE_ALLOWED_ORIGINS,
  MCP_SSE_MAX_SESSIONS
                 See "SSE security" above.
  MCP_STORAGE_*  See config.example.toml for the storage tuning knobs.

See config.example.toml for every file setting.
"
    }
}

fn next_value<I>(
    args: &mut std::iter::Peekable<I>,
    flag: &'static str,
) -> Result<OsString, ConfigError>
where
    I: Iterator<Item = OsString>,
{
    args.next().ok_or(ConfigError::MissingValue { flag })
}

impl Config {
    /// The bottom layer: built-in defaults, with storage disabled.
    #[must_use]
    pub fn defaults() -> Self {
        Self::default()
    }

    /// Defaults overlaid with the TOML file at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Read`] or [`ConfigError::Parse`] if the file is missing, unreadable,
    /// not valid TOML, or contains unknown keys.
    pub fn from_file(path: &Path) -> Result<Self, ConfigError> {
        let mut config = Self::defaults();
        config.apply_file(path)?;
        config.validate()?;
        Ok(config)
    }

    /// Merge all four layers, the way the binary does it.
    ///
    /// The file layer is skipped entirely when no path is configured, so this is safe to call with
    /// no file present.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if a file cannot be read or parsed, an environment variable cannot
    /// be parsed as its type, or a resolved value is out of range.
    pub fn resolve(cli: &Cli, env: &dyn Env) -> Result<Self, ConfigError> {
        let mut config = Self::defaults();

        // Layer 2: the file. `--config` beats `MCP_CONFIG`; neither means "no file layer".
        let path = cli
            .config_path
            .clone()
            .or_else(|| env.get(env_keys::CONFIG).map(PathBuf::from));
        if let Some(path) = path {
            config.apply_file(&path)?;
        }

        // Layer 3: environment.
        if let Some(url) = non_blank(env.get(env_keys::DATABASE_URL)) {
            config.storage.url = Some(url);
        }
        if let Some(addr) = non_blank(env.get(env_keys::SSE_ADDRESS)) {
            config.server.sse_address = parse_socket_addr(env_keys::SSE_ADDRESS, &addr)?;
        }
        if let Some(filter) = non_blank(env.get(env_keys::LOG_FILTER)) {
            config.server.log_filter = filter;
        }
        if let Some(value) = non_blank(env.get(env_keys::LIST_PAGE_SIZE)) {
            config.server.list_page_size = parse_number(env_keys::LIST_PAGE_SIZE, &value)?;
        }
        if let Some(value) = non_blank(env.get(env_keys::STORAGE_MAX_CONNECTIONS)) {
            config.storage.max_connections =
                parse_number(env_keys::STORAGE_MAX_CONNECTIONS, &value)?;
        }
        if let Some(value) = non_blank(env.get(env_keys::STORAGE_ACQUIRE_TIMEOUT_MS)) {
            config.storage.acquire_timeout =
                Duration::from_millis(parse_number(env_keys::STORAGE_ACQUIRE_TIMEOUT_MS, &value)?);
        }
        if let Some(value) = non_blank(env.get(env_keys::STORAGE_BREAKER_THRESHOLD)) {
            config.storage.breaker_threshold =
                parse_number(env_keys::STORAGE_BREAKER_THRESHOLD, &value)?;
        }
        if let Some(value) = non_blank(env.get(env_keys::STORAGE_BREAKER_COOLDOWN_MS)) {
            config.storage.breaker_cooldown =
                Duration::from_millis(parse_number(env_keys::STORAGE_BREAKER_COOLDOWN_MS, &value)?);
        }
        if let Some(value) = non_blank(env.get(env_keys::STORAGE_MAX_HISTORY_ROWS)) {
            config.storage.max_history_rows =
                parse_number(env_keys::STORAGE_MAX_HISTORY_ROWS, &value)?;
        }
        if let Some(value) = non_blank(env.get(env_keys::SSE_ALLOWED_HOSTS)) {
            config.server.sse_allowed_hosts = parse_list(&value);
        }
        if let Some(value) = non_blank(env.get(env_keys::SSE_ALLOWED_ORIGINS)) {
            config.server.sse_allowed_origins = parse_list(&value);
        }
        if let Some(value) = non_blank(env.get(env_keys::SSE_TOKEN)) {
            config.server.sse_token = Some(Secret::new(value));
        }
        if let Some(value) = non_blank(env.get(env_keys::SSE_MAX_SESSIONS)) {
            config.server.sse_max_sessions = parse_number(env_keys::SSE_MAX_SESSIONS, &value)?;
        }

        // Layer 4: flags.
        if let Some(url) = &cli.database_url {
            config.storage.url = Some(url.clone());
        }
        if let Some(addr) = &cli.sse_address {
            config.server.sse_address = parse_socket_addr("--sse", addr)?;
        }

        config.validate()?;
        Ok(config)
    }

    fn apply_file(&mut self, path: &Path) -> Result<(), ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let file: FileConfig = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;

        if let Some(addr) = file.server.sse_address {
            self.server.sse_address = parse_socket_addr("server.sse_address", &addr)?;
        }
        if let Some(filter) = file.server.log_filter {
            self.server.log_filter = filter;
        }
        if let Some(value) = file.server.list_page_size {
            self.server.list_page_size = value;
        }
        if let Some(hosts) = file.server.sse_allowed_hosts {
            self.server.sse_allowed_hosts = hosts;
        }
        if let Some(origins) = file.server.sse_allowed_origins {
            self.server.sse_allowed_origins = origins;
        }
        if let Some(token) = file.server.sse_token {
            self.server.sse_token = Some(Secret::new(token));
        }
        if let Some(value) = file.server.sse_max_sessions {
            self.server.sse_max_sessions = value;
        }

        if let Some(url) = file.storage.url {
            // An explicitly blank URL means "disable storage", the same as omitting it.
            self.storage.url = Some(url).filter(|value| !value.trim().is_empty());
        }
        if let Some(value) = file.storage.max_connections {
            self.storage.max_connections = value;
        }
        if let Some(value) = file.storage.acquire_timeout_ms {
            self.storage.acquire_timeout = Duration::from_millis(value);
        }
        if let Some(value) = file.storage.breaker_threshold {
            self.storage.breaker_threshold = value;
        }
        if let Some(value) = file.storage.breaker_cooldown_ms {
            self.storage.breaker_cooldown = Duration::from_millis(value);
        }
        if let Some(value) = file.storage.max_history_rows {
            self.storage.max_history_rows = value;
        }
        Ok(())
    }

    /// Reject values that would silently misbehave later.
    fn validate(&self) -> Result<(), ConfigError> {
        if self.storage.max_connections == 0 {
            return Err(ConfigError::InvalidValue {
                key: "storage.max_connections".to_owned(),
                value: "0".to_owned(),
                reason: "must be at least 1".to_owned(),
            });
        }
        if self.storage.breaker_threshold == 0 {
            return Err(ConfigError::InvalidValue {
                key: "storage.breaker_threshold".to_owned(),
                value: "0".to_owned(),
                reason: "must be at least 1; 0 would disable the breaker entirely".to_owned(),
            });
        }
        if self.storage.acquire_timeout.is_zero() {
            return Err(ConfigError::InvalidValue {
                key: "storage.acquire_timeout_ms".to_owned(),
                value: "0".to_owned(),
                reason: "must be greater than 0; 0 would fail every storage call instantly"
                    .to_owned(),
            });
        }
        if self.storage.breaker_cooldown.is_zero() {
            return Err(ConfigError::InvalidValue {
                key: "storage.breaker_cooldown_ms".to_owned(),
                value: "0".to_owned(),
                reason: "must be greater than 0; 0 would make the breaker unusable".to_owned(),
            });
        }
        if self.storage.max_history_rows < 1 {
            return Err(ConfigError::InvalidValue {
                key: "storage.max_history_rows".to_owned(),
                value: self.storage.max_history_rows.to_string(),
                reason: "must be at least 1".to_owned(),
            });
        }
        if self.server.list_page_size == 0 {
            return Err(ConfigError::InvalidValue {
                key: "server.list_page_size".to_owned(),
                value: "0".to_owned(),
                reason: "must be at least 1; 0 would return an empty page and never advance"
                    .to_owned(),
            });
        }
        if self.server.sse_max_sessions == 0 {
            return Err(ConfigError::InvalidValue {
                key: "server.sse_max_sessions".to_owned(),
                value: "0".to_owned(),
                reason: "must be at least 1; 0 would refuse every SSE session".to_owned(),
            });
        }
        if let Some(token) = &self.server.sse_token
            && token.expose().trim().is_empty()
        {
            return Err(ConfigError::InvalidValue {
                key: "server.sse_token".to_owned(),
                value: String::new(),
                reason: "must not be blank".to_owned(),
            });
        }
        for host in &self.server.sse_allowed_hosts {
            if parse_host_pattern(host).is_none() {
                return Err(ConfigError::InvalidValue {
                    key: "server.sse_allowed_hosts".to_owned(),
                    value: host.clone(),
                    reason: "expected host or host:port, without scheme, path, userinfo or port 0"
                        .to_owned(),
                });
            }
        }
        for origin in &self.server.sse_allowed_origins {
            if parse_origin_pattern(origin).is_none() {
                return Err(ConfigError::InvalidValue {
                    key: "server.sse_allowed_origins".to_owned(),
                    value: origin.clone(),
                    reason: "expected scheme://host[:port] with no path or query".to_owned(),
                });
            }
        }
        // Surface a bad filter now instead of letting `tracing_subscriber` quietly fall back.
        EnvFilter::try_new(&self.server.log_filter).map_err(|error| ConfigError::InvalidValue {
            key: "server.log_filter".to_owned(),
            value: self.server.log_filter.clone(),
            reason: format!("not a valid tracing filter: {error}"),
        })?;
        Ok(())
    }
}

fn parse_socket_addr(key: &str, value: &str) -> Result<SocketAddr, ConfigError> {
    SocketAddr::from_str(value.trim()).map_err(|error| ConfigError::InvalidValue {
        key: key.to_owned(),
        value: value.to_owned(),
        reason: format!("not a valid socket address: {error}"),
    })
}

fn parse_number<T>(key: &str, value: &str) -> Result<T, ConfigError>
where
    T: FromStr,
{
    value.trim().parse().map_err(|_| ConfigError::InvalidValue {
        key: key.to_owned(),
        value: value.to_owned(),
        reason: "not a valid number".to_owned(),
    })
}

/// Split a comma-separated env value, keeping empty entries so validation can reject them.
fn parse_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|entry| entry.trim().to_owned())
        .collect()
}

/// Sparse mirror of [`Config`]: every field optional, so only what the file actually sets wins.
///
/// `deny_unknown_fields` turns a misspelled key into a startup error rather than a setting that
/// quietly does nothing.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default)]
    server: FileServer,
    #[serde(default)]
    storage: FileStorage,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileServer {
    sse_address: Option<String>,
    log_filter: Option<String>,
    list_page_size: Option<usize>,
    sse_allowed_hosts: Option<Vec<String>>,
    sse_allowed_origins: Option<Vec<String>>,
    sse_token: Option<String>,
    sse_max_sessions: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileStorage {
    url: Option<String>,
    max_connections: Option<u32>,
    acquire_timeout_ms: Option<u64>,
    breaker_threshold: Option<u32>,
    breaker_cooldown_ms: Option<u64>,
    max_history_rows: Option<i64>,
}

/// Everything that can go wrong while building a [`Config`].
#[derive(Debug)]
pub enum ConfigError {
    UnknownArgument(String),
    MissingValue {
        flag: &'static str,
    },
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    InvalidValue {
        key: String,
        value: String,
        reason: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownArgument(argument) => {
                write!(f, "unknown argument {argument:?}, try --help")
            }
            Self::MissingValue { flag } => write!(f, "{flag} requires a value"),
            Self::Read { path, source } => {
                write!(f, "cannot read config file {}: {source}", path.display())
            }
            Self::Parse { path, source } => {
                write!(f, "invalid config file {}: {source}", path.display())
            }
            Self::InvalidValue { key, value, reason } => {
                write!(f, "invalid value for {key}: {value:?} ({reason})")
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::UnknownArgument(_) | Self::MissingValue { .. } | Self::InvalidValue { .. } => {
                None
            }
        }
    }
}
