// SPDX-License-Identifier: BSD-3-Clause
//! Storage behaviour that must hold **without a live database**.
//!
//! `Store` is designed so that a missing or unreachable database degrades instead of breaking the
//! server, and this is the behaviour worth pinning down. Tests that need a real database are split
//! by what they need: SQLite is embedded, so `tests/sqlite_backend.rs` exercises a full round trip
//! here; PostgreSQL and MySQL need containers, so anything specific to them is verified manually
//! against the devcontainer instead.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use poc_rust_mcp::{
    db::{Dialect, HistoryEntry, Limits, Store},
    server::clamp_history_limit,
};

/// Nothing ever listens on port 1, so these URLs always fail to connect.
const DEAD_PG_URL: &str = "postgres://mcp:mcp@127.0.0.1:1/mcp";
const DEAD_MYSQL_URL: &str = "mysql://mcp:mcp@127.0.0.1:1/mcp";

/// Both network drivers, so the degradation guarantees are not proved for PostgreSQL alone.
const DEAD_URLS: [&str; 2] = [DEAD_PG_URL, DEAD_MYSQL_URL];

/// Short enough to keep the suite fast, long enough to be a realistic timeout.
const FAST: Duration = Duration::from_millis(250);

#[tokio::test]
async fn disabled_store_is_configured_nowhere_and_every_operation_says_so() {
    let store = Store::disabled();
    assert!(!store.is_configured());
    assert_eq!(store.dialect(), None);

    assert_eq!(store.status().await.unwrap_err(), Store::DISABLED);
    assert_eq!(store.list(10).await.unwrap_err(), Store::DISABLED);
    assert_eq!(store.clear().await.unwrap_err(), Store::DISABLED);
    assert!(Store::DISABLED.contains("DATABASE_URL"));
    // The message must not name a backend, since no backend is in play.
    assert!(
        !Store::DISABLED.contains("PostgreSQL"),
        "{}",
        Store::DISABLED
    );
}

#[tokio::test]
async fn the_url_scheme_alone_picks_the_backend() {
    // Nothing connects here, so this covers the whole dispatch decision for every supported driver.
    let cases = [
        ("postgres://mcp:mcp@db:5432/mcp", Dialect::Postgres),
        ("postgresql://mcp:mcp@db/mcp", Dialect::Postgres),
        ("mysql://mcp:mcp@db:3306/mcp", Dialect::MySql),
        ("mariadb://mcp:mcp@db/mcp", Dialect::MySql),
        ("sqlite://mcp.db", Dialect::Sqlite),
        ("sqlite::memory:", Dialect::Sqlite),
        // A bare `mysql://` is a valid URL: the driver fills in its own host and user defaults, and
        // whether the resulting database is reachable is a later question, not a configuration one.
        ("mysql://", Dialect::MySql),
    ];

    for (url, expected) in cases {
        let store = Store::connect(url);
        assert!(store.is_configured(), "{url} should be accepted");
        assert_eq!(store.dialect(), Some(expected), "{url}");
        assert_eq!(store.limits(), &Limits::default(), "{url}");
    }
}

#[tokio::test]
async fn unparsable_url_degrades_to_disabled_storage() {
    // The scheme is checked before the driver parses the rest, so a URL that names a supported
    // backend but is malformed still has to degrade rather than fail.
    for url in [
        "postgres://this is not a url",
        "not a url at all",
        "no-scheme-here",
    ] {
        let store = Store::connect(url);
        assert!(
            !store.is_configured(),
            "an unusable URL must fall back to disabled storage: {url}"
        );
    }
}

#[tokio::test]
async fn an_unsupported_scheme_is_rejected_rather_than_guessed() {
    // A typo'd scheme must not read as "storage is merely unconfigured" — the operator needs to be
    // told which schemes exist. A near-miss like `mysqls://` is the case worth catching.
    let store = Store::connect("mysqls://mcp:mcp@127.0.0.1:3306/mcp");
    assert!(!store.is_configured());
    assert_eq!(store.dialect(), None);
}

#[tokio::test]
async fn unreachable_database_stays_configured_but_fails_within_the_timeout() {
    for url in DEAD_URLS {
        let store = Store::connect_with_timeout(url, FAST);
        // "Configured but down" is distinct from "not configured": the server still reports storage
        // as available, and each operation fails on its own terms.
        assert!(store.is_configured(), "{url}");

        let started = Instant::now();
        assert!(store.status().await.is_err(), "{url}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "status() against {url} was not bounded by the acquire timeout: {:?}",
            started.elapsed()
        );
    }
}

#[tokio::test]
async fn repeated_connection_failures_open_the_circuit_breaker() {
    for url in DEAD_URLS {
        let store = Store::connect_with_timeout(url, FAST);

        // The threshold is deliberately small, so this must trip within a few attempts. Once open,
        // calls are rejected without attempting a connection and therefore return immediately.
        let mut opened = false;
        for _ in 0..5 {
            let started = Instant::now();
            let error = store.status().await.unwrap_err();
            if error.contains("temporarily disabled") {
                assert!(
                    started.elapsed() < Duration::from_millis(50),
                    "an open breaker still waited {:?} before failing against {url}",
                    started.elapsed()
                );
                opened = true;
                break;
            }
        }
        assert!(
            opened,
            "circuit breaker never opened after repeated failures against {url}"
        );
    }
}

#[test]
fn history_limit_is_clamped_to_the_configured_maximum() {
    // The cap now comes from configuration, so clamping is exercised against a chosen maximum.
    let max = 100;
    assert_eq!(clamp_history_limit(i64::MIN, max), 1);
    assert_eq!(clamp_history_limit(0, max), 1);
    assert_eq!(clamp_history_limit(1, max), 1);
    assert_eq!(clamp_history_limit(42, max), 42);
    assert_eq!(clamp_history_limit(i64::MAX, max), max);

    // A nonsensical cap must not panic or produce an inverted range.
    assert_eq!(clamp_history_limit(50, 0), 1);
    assert_eq!(clamp_history_limit(50, -5), 1);
}

#[tokio::test]
async fn recording_history_is_fire_and_forget() {
    let mut stores = vec![Store::disabled()];
    stores.extend(DEAD_URLS.map(|url| Store::connect_with_timeout(url, FAST)));

    for store in &stores {
        let started = Instant::now();
        store.record_success("add", serde_json::json!({ "a": 1, "b": 2 }), "3");
        store.record_failure("div", serde_json::json!({}), "division by zero");
        assert!(
            started.elapsed() < FAST,
            "recording blocked the caller for {:?}",
            started.elapsed()
        );
    }
}

#[test]
fn history_entry_renders_result_and_error_rows() {
    let entry = |result: Option<&str>, error: Option<&str>| HistoryEntry {
        id: 7,
        operation: "add".to_owned(),
        inputs: serde_json::json!({ "a": 1, "b": 1 }),
        result: result.map(str::to_owned),
        error: error.map(str::to_owned),
        created_at: DateTime::<Utc>::UNIX_EPOCH,
    };

    let success = entry(Some("2"), None).to_line();
    assert!(success.contains("#7"), "{success}");
    assert!(success.contains("add"), "{success}");
    assert!(success.ends_with("-> 2"), "{success}");

    let failure = entry(None, Some("division by zero")).to_line();
    assert!(failure.ends_with("-> error: division by zero"), "{failure}");
}
