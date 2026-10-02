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
//! The file is deserialized into a sparse mirror ([`FileConfig`]) where every field is optional, and
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

/// Default bind address for `--sse`.
pub const DEFAULT_SSE_ADDRESS: &str = "127.0.0.1:8000";

/// Default `tracing` filter when `RUST_LOG` is unset.
pub const DEFAULT_LOG_FILTER: &str = "info";

/// Environment variable names recognised by the env layer.
pub mod env_keys {
    /// Path to the TOML config file.
    pub const CONFIG: &str = "MCP_CONFIG";
    /// PostgreSQL URL; unchanged from before config files existed.
    pub const DATABASE_URL: &str = "DATABASE_URL";
    /// Bind address for the SSE transport.
    pub const SSE_ADDRESS: &str = "MCP_SSE_ADDRESS";
    /// `tracing` filter.
    pub const LOG_FILTER: &str = "RUST_LOG";
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
}

/// PostgreSQL history settings. `url: None` means storage is disabled.
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
    /// Whether a URL was configured at all. An unparsable URL still counts as configured here;
    /// [`crate::db::Store`] is what decides it is unusable.
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
poc-rust-mcp — MCP calculator server with PostgreSQL-backed history

Usage:
  poc-rust-mcp                Serve MCP over stdio (default, for client subprocesses)
  poc-rust-mcp --sse [ADDR]   Serve MCP over SSE on ADDR (default 127.0.0.1:8000)
  poc-rust-mcp --help         Show this message

Configuration, highest precedence first:
  1. command-line flags
       --config <PATH>  TOML config file (also MCP_CONFIG)
       --db-url <URL>   PostgreSQL URL (also DATABASE_URL)
       --sse [ADDR]     bind address for SSE (also MCP_SSE_ADDRESS)
  2. environment variables
  3. the TOML config file, if one was named
  4. built-in defaults

  No config file is required. Precedence for a setting is
  flag > environment > file > default.

Environment:
  DATABASE_URL   PostgreSQL URL for calculation history, e.g.
                 postgres://mcp:mcp@127.0.0.1:5432/mcp
                 Unset or unreachable = storage disabled, calculator tools still work
  RUST_LOG       Log filter, e.g. RUST_LOG=debug (logs go to stderr)

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
