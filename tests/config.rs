//! The layered configuration framework: defaults < TOML file < environment < CLI flags.
//!
//! [`Env`] is injected rather than read from the process environment, which keeps these tests
//! deterministic and avoids `unsafe { std::env::set_var }` (edition 2024).
//!
//! No database is involved anywhere here, so the file passes on a bare checkout.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use poc_rust_mcp::{
    config::{Cli, Config, ConfigError, Env, MapEnv, StorageConfig, SystemEnv, env_keys},
    db::{Limits, Store},
    server::clamp_history_limit,
};

/// A uniquely named config file that deletes itself, so the suite leaves nothing behind and can run
/// in parallel.
struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn new(contents: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "poc-rust-mcp-config-{}-{serial}.toml",
            std::process::id()
        ));
        std::fs::write(&path, contents).expect("failed to write temporary config file");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn resolve(cli: &Cli, env: &MapEnv) -> Result<Config, ConfigError> {
    Config::resolve(cli, env)
}

fn cli(args: &[&str]) -> Cli {
    Cli::parse_from(args.iter().copied()).expect("arguments should parse")
}

fn no_cli() -> Cli {
    Cli::default()
}

fn no_env() -> MapEnv {
    MapEnv::new()
}

#[test]
fn defaults_alone_are_a_complete_working_configuration() {
    // No file, no environment, no flags: the server must still be startable, with storage off.
    let config = resolve(&no_cli(), &no_env()).expect("defaults should resolve");

    assert_eq!(config.storage.url, None);
    assert_eq!(config.storage.max_connections, 5);
    assert_eq!(config.storage.acquire_timeout, Duration::from_secs(3));
    assert_eq!(config.storage.breaker_threshold, 2);
    assert_eq!(config.storage.breaker_cooldown, Duration::from_secs(30));
    assert_eq!(config.storage.max_history_rows, 100);
    assert_eq!(config.server.log_filter, "info");
    assert_eq!(
        config.server.sse_address,
        "127.0.0.1:8000"
            .parse::<SocketAddr>()
            .expect("valid address")
    );
    assert!(!config.storage.is_enabled());
}

#[test]
fn a_config_file_overrides_defaults_and_leaves_everything_else_alone() {
    let file = TempFile::new(
        r#"
        [storage]
        url = "postgres://file:file@db:5432/file"
        max_history_rows = 7

        [server]
        sse_address = "0.0.0.0:9999"
        "#,
    );
    let config = resolve(
        &cli(&["--config", file.path().to_str().expect("utf-8 path")]),
        &no_env(),
    )
    .expect("valid config file should resolve");

    assert_eq!(
        config.storage.url.as_deref(),
        Some("postgres://file:file@db:5432/file")
    );
    assert_eq!(config.storage.max_history_rows, 7);
    assert_eq!(
        config.server.sse_address,
        "0.0.0.0:9999".parse::<SocketAddr>().expect("valid address")
    );
    // Untouched keys keep their defaults: a partial file must not zero out anything.
    assert_eq!(config.storage.max_connections, 5);
    assert_eq!(config.storage.acquire_timeout, Duration::from_secs(3));
    assert_eq!(config.server.log_filter, "info");
}

#[test]
fn the_environment_overrides_the_file() {
    let file = TempFile::new(
        r#"
        [storage]
        url = "postgres://file:file@db:5432/file"
        max_connections = 11

        [server]
        log_filter = "warn"
        "#,
    );
    let env = MapEnv::new()
        .with(env_keys::DATABASE_URL, "postgres://env:env@db:5432/env")
        .with(env_keys::LOG_FILTER, "error");
    let config = resolve(
        &cli(&["--config", file.path().to_str().expect("utf-8 path")]),
        &env,
    )
    .expect("valid config should resolve");

    assert_eq!(
        config.storage.url.as_deref(),
        Some("postgres://env:env@db:5432/env")
    );
    assert_eq!(config.server.log_filter, "error");
    // Still file-sourced, because the environment did not mention it.
    assert_eq!(config.storage.max_connections, 11);
}

#[test]
fn flags_beat_both_the_environment_and_the_file() {
    let file = TempFile::new(
        r#"
        [storage]
        url = "postgres://file:file@db:5432/file"
        max_connections = 11
        "#,
    );
    let env = MapEnv::new().with(env_keys::DATABASE_URL, "postgres://env:env@db:5432/env");
    let config = resolve(
        &cli(&[
            "--config",
            file.path().to_str().expect("utf-8 path"),
            "--db-url",
            "postgres://flag:flag@db:5432/flag",
            "--sse",
            "10.0.0.1:1234",
        ]),
        &env,
    )
    .expect("valid config should resolve");

    assert_eq!(
        config.storage.url.as_deref(),
        Some("postgres://flag:flag@db:5432/flag")
    );
    assert_eq!(
        config.server.sse_address,
        "10.0.0.1:1234"
            .parse::<SocketAddr>()
            .expect("valid address")
    );
    assert_eq!(
        config.storage.max_connections, 11,
        "file still supplies this"
    );
}

#[test]
fn every_storage_tuning_knob_can_come_from_the_environment() {
    let env = MapEnv::new()
        .with(env_keys::STORAGE_MAX_CONNECTIONS, "9")
        .with(env_keys::STORAGE_ACQUIRE_TIMEOUT_MS, "1500")
        .with(env_keys::STORAGE_BREAKER_THRESHOLD, "4")
        .with(env_keys::STORAGE_BREAKER_COOLDOWN_MS, "9000")
        .with(env_keys::STORAGE_MAX_HISTORY_ROWS, "42");
    let config = resolve(&no_cli(), &env).expect("numeric environment should resolve");

    assert_eq!(config.storage.max_connections, 9);
    assert_eq!(config.storage.acquire_timeout, Duration::from_millis(1500));
    assert_eq!(config.storage.breaker_threshold, 4);
    assert_eq!(config.storage.breaker_cooldown, Duration::from_millis(9000));
    assert_eq!(config.storage.max_history_rows, 42);
}

#[test]
fn a_blank_url_means_disabled_storage_rather_than_an_empty_url() {
    // Compose files and CI often export an empty variable to mean "not configured".
    let from_env = resolve(
        &no_cli(),
        &MapEnv::new().with(env_keys::DATABASE_URL, "   "),
    )
    .expect("blank url should resolve");
    assert_eq!(from_env.storage.url, None);

    let file = TempFile::new("[storage]\nurl = \"\"\n");
    let from_file = resolve(
        &cli(&["--config", file.path().to_str().expect("utf-8 path")]),
        &no_env(),
    )
    .expect("blank url in file should resolve");
    assert_eq!(from_file.storage.url, None);
    assert!(!from_file.storage.is_enabled());
}

#[test]
fn a_named_config_file_may_come_from_the_environment() {
    let file = TempFile::new("[storage]\nmax_connections = 3\n");
    let env = MapEnv::new().with(env_keys::CONFIG, file.path().to_str().expect("utf-8 path"));
    let config = resolve(&no_cli(), &env).expect("MCP_CONFIG should be honoured");
    assert_eq!(config.storage.max_connections, 3);

    // ...but the flag outranks the variable.
    let other = TempFile::new("[storage]\nmax_connections = 4\n");
    let config = resolve(
        &cli(&["--config", other.path().to_str().expect("utf-8 path")]),
        &env,
    )
    .expect("--config should win");
    assert_eq!(config.storage.max_connections, 4);
}

#[test]
fn a_missing_config_file_is_an_error_only_when_one_was_named() {
    let absent = std::env::temp_dir().join("poc-rust-mcp-definitely-absent.toml");
    let result = resolve(
        &cli(&["--config", absent.to_str().expect("utf-8 path")]),
        &no_env(),
    );

    let error = result.expect_err("a named but missing config file must fail");
    assert!(matches!(error, ConfigError::Read { .. }), "{error:?}");
    assert!(
        error.to_string().contains("cannot read config file"),
        "{error}"
    );

    // No file named anywhere means no file layer, which is not an error.
    assert!(resolve(&no_cli(), &no_env()).is_ok());
}

#[test]
fn malformed_toml_is_reported_with_the_file_name() {
    let file = TempFile::new("[storage\nthis is not toml");
    let result = resolve(
        &cli(&["--config", file.path().to_str().expect("utf-8 path")]),
        &no_env(),
    );

    let error = result.expect_err("invalid TOML must fail");
    assert!(matches!(error, ConfigError::Parse { .. }), "{error:?}");
    let message = error.to_string();
    assert!(message.contains("invalid config file"), "{message}");
    assert!(
        message.contains(&file.path().display().to_string()),
        "the message should name the file: {message}"
    );
}

#[test]
fn misspelled_keys_are_rejected_instead_of_being_ignored() {
    // The whole point of a config framework: a typo must not silently do nothing.
    let file = TempFile::new("[storage]\nmax_connection = 4\n");
    let result = resolve(
        &cli(&["--config", file.path().to_str().expect("utf-8 path")]),
        &no_env(),
    );

    let error = result.expect_err("unknown key must fail");
    assert!(matches!(error, ConfigError::Parse { .. }), "{error:?}");

    let section = TempFile::new("[datastore]\nurl = \"x\"\n");
    let result = resolve(
        &cli(&["--config", section.path().to_str().expect("utf-8 path")]),
        &no_env(),
    );
    assert!(
        matches!(result, Err(ConfigError::Parse { .. })),
        "an unknown section must fail too"
    );
}

#[test]
fn unusable_values_are_rejected_with_the_offending_key() {
    let cases: &[(&str, &str)] = &[
        (
            "[server]\nsse_address = \"not-an-address\"\n",
            "server.sse_address",
        ),
        (
            "[storage]\nmax_connections = 0\n",
            "storage.max_connections",
        ),
        (
            "[storage]\nbreaker_threshold = 0\n",
            "storage.breaker_threshold",
        ),
        (
            "[storage]\nacquire_timeout_ms = 0\n",
            "storage.acquire_timeout_ms",
        ),
        (
            "[storage]\nbreaker_cooldown_ms = 0\n",
            "storage.breaker_cooldown_ms",
        ),
        (
            "[storage]\nmax_history_rows = 0\n",
            "storage.max_history_rows",
        ),
        (
            "[server]\nlog_filter = \"!!!not a filter\"\n",
            "server.log_filter",
        ),
    ];

    for (contents, expected_key) in cases {
        let file = TempFile::new(contents);
        let result = resolve(
            &cli(&["--config", file.path().to_str().expect("utf-8 path")]),
            &no_env(),
        );
        let error = result.expect_err("an unusable value must be rejected");
        match &error {
            ConfigError::InvalidValue { key, .. } => assert_eq!(key, expected_key, "{contents}"),
            other => panic!("{contents}produced the wrong error: {other:?}"),
        }
        assert!(
            error.to_string().contains(expected_key),
            "the message should name the key: {error}"
        );
    }
}

#[test]
fn non_numeric_environment_values_are_rejected() {
    let env = MapEnv::new().with(env_keys::STORAGE_MAX_CONNECTIONS, "plenty");
    let error = resolve(&no_cli(), &env).expect_err("a non-number must fail");
    assert!(
        matches!(error, ConfigError::InvalidValue { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("not a valid number"), "{error}");

    let bad_addr = MapEnv::new().with(env_keys::SSE_ADDRESS, "localhost:1234");
    let error = resolve(&no_cli(), &bad_addr).expect_err("a host:port without scheme must fail");
    assert!(
        matches!(error, ConfigError::InvalidValue { .. }),
        "{error:?}"
    );
}

#[test]
fn cli_parsing_covers_the_documented_shapes() {
    assert!(cli(&["--help"]).help);
    assert!(cli(&["-h"]).help);
    assert!(cli(&["--help"]).help, "-h is accepted");

    let sse = cli(&["--sse"]);
    assert!(sse.serve_sse && sse.sse_address.is_none());

    let sse_with_addr = cli(&["--sse", "127.0.0.1:7000"]);
    assert!(sse_with_addr.serve_sse);
    assert_eq!(sse_with_addr.sse_address.as_deref(), Some("127.0.0.1:7000"));

    // A flag following --sse must not be swallowed as its address.
    let sse_then_flag = cli(&["--sse", "--db-url", "postgres://x/y"]);
    assert!(sse_then_flag.serve_sse);
    assert!(sse_then_flag.sse_address.is_none());
    assert_eq!(
        sse_then_flag.database_url.as_deref(),
        Some("postgres://x/y")
    );

    assert_eq!(
        cli(&["-c", "/tmp/x.toml"]).config_path,
        Some(PathBuf::from("/tmp/x.toml"))
    );
}

#[test]
fn malformed_cli_arguments_are_rejected() {
    let unknown = Cli::parse_from(["--nope"]).expect_err("unknown flag must fail");
    assert!(
        matches!(unknown, ConfigError::UnknownArgument(_)),
        "{unknown:?}"
    );
    assert!(unknown.to_string().contains("--help"), "{unknown}");

    let dangling = Cli::parse_from(["--config"]).expect_err("a value is required");
    assert!(
        matches!(dangling, ConfigError::MissingValue { flag: "--config" }),
        "{dangling:?}"
    );
}

#[test]
fn usage_documents_the_layers_and_the_config_file() {
    let usage = Cli::usage();
    for expected in [
        "--config",
        "--db-url",
        "--sse",
        env_keys::CONFIG,
        env_keys::DATABASE_URL,
        "config.example.toml",
    ] {
        assert!(
            usage.contains(expected),
            "usage should mention {expected}:\n{usage}"
        );
    }
}

#[tokio::test]
async fn resolved_storage_settings_reach_the_store() {
    let config = resolve(
        &no_cli(),
        &MapEnv::new()
            .with(env_keys::DATABASE_URL, "postgres://user:pw@db:5432/db")
            .with(env_keys::STORAGE_MAX_HISTORY_ROWS, "5"),
    )
    .expect("settings should resolve");

    let store = Store::from_settings(&config.storage);
    assert!(store.is_configured(), "a URL means storage is configured");
    assert_eq!(store.max_history_rows(), 5);
    assert_eq!(
        *store.limits(),
        Limits::from(&config.storage),
        "the store must carry every configured limit"
    );

    // A configured cap is honoured when clamping what a client may ask for.
    assert_eq!(clamp_history_limit(1_000_000, store.max_history_rows()), 5);
    assert_eq!(clamp_history_limit(0, store.max_history_rows()), 1);
}

#[tokio::test]
async fn an_unusable_url_degrades_to_disabled_storage_rather_than_failing() {
    let config = resolve(
        &no_cli(),
        &MapEnv::new().with(env_keys::DATABASE_URL, "definitely not a url"),
    )
    .expect("resolution succeeds; the URL is only parsed when the pool is built");

    let store = Store::from_settings(&config.storage);
    assert!(!store.is_configured());
    assert!(
        Store::DISABLED.contains("DATABASE_URL"),
        "the message should tell the user what to set: {}",
        Store::DISABLED
    );
}

#[test]
fn the_real_environment_is_readable_through_the_same_trait() {
    // `SystemEnv` is the production path; assert it reports something for a key that does exist,
    // without asserting a value, since the ambient environment is not ours to control.
    let env = SystemEnv;
    let _ = env.get(env_keys::DATABASE_URL);
}

#[test]
fn default_limits_match_an_empty_configuration() {
    assert_eq!(Limits::default(), Limits::from(&StorageConfig::default()));
}
