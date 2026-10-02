//! Storage behaviour that must hold **without a live PostgreSQL**.
//!
//! `Store` is designed so that a missing or unreachable database degrades instead of breaking the
//! server, and this is the behaviour worth pinning down. Tests that need a real database are
//! deliberately absent: `cargo test` must pass on a bare checkout, so anything requiring Postgres
//! is verified manually against the devcontainer instead.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use poc_rust_mcp::{
    db::{HistoryEntry, Store},
    server::clamp_history_limit,
};

/// Nothing ever listens on port 1, so this URL always fails to connect.
const DEAD_URL: &str = "postgres://mcp:mcp@127.0.0.1:1/mcp";

/// Short enough to keep the suite fast, long enough to be a realistic timeout.
const FAST: Duration = Duration::from_millis(250);

#[tokio::test]
async fn disabled_store_is_configured_nowhere_and_every_operation_says_so() {
    let store = Store::disabled();
    assert!(!store.is_configured());

    assert_eq!(store.status().await.unwrap_err(), Store::DISABLED);
    assert_eq!(store.list(10).await.unwrap_err(), Store::DISABLED);
    assert_eq!(store.clear().await.unwrap_err(), Store::DISABLED);
    assert!(Store::DISABLED.contains("DATABASE_URL"));
}

#[tokio::test]
async fn unparsable_url_degrades_to_disabled_storage() {
    let store = Store::connect("postgres://this is not a url");
    assert!(
        !store.is_configured(),
        "an unusable URL must fall back to disabled storage"
    );
}

#[tokio::test]
async fn unreachable_database_stays_configured_but_fails_within_the_timeout() {
    let store = Store::connect_with_timeout(DEAD_URL, FAST);
    // "Configured but down" is distinct from "not configured": the server still reports storage
    // as available, and each operation fails on its own terms.
    assert!(store.is_configured());

    let started = Instant::now();
    assert!(store.status().await.is_err());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "status() was not bounded by the acquire timeout: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn repeated_connection_failures_open_the_circuit_breaker() {
    let store = Store::connect_with_timeout(DEAD_URL, FAST);

    // The threshold is deliberately small, so this must trip within a few attempts. Once open,
    // calls are rejected without attempting a connection and therefore return immediately.
    let mut opened = false;
    for _ in 0..5 {
        let started = Instant::now();
        let error = store.status().await.unwrap_err();
        if error.contains("temporarily disabled") {
            assert!(
                started.elapsed() < Duration::from_millis(50),
                "an open breaker still waited {:?} before failing",
                started.elapsed()
            );
            opened = true;
            break;
        }
    }
    assert!(
        opened,
        "circuit breaker never opened after repeated failures"
    );
}

#[tokio::test]
async fn history_limit_is_clamped_to_a_sane_range() {
    assert_eq!(clamp_history_limit(i64::MIN), 1);
    assert_eq!(clamp_history_limit(0), 1);
    assert_eq!(clamp_history_limit(1), 1);
    assert_eq!(clamp_history_limit(42), 42);
    assert_eq!(clamp_history_limit(i64::MAX), 100);
}

#[tokio::test]
async fn recording_history_is_fire_and_forget() {
    for store in [
        Store::disabled(),
        Store::connect_with_timeout(DEAD_URL, FAST),
    ] {
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
