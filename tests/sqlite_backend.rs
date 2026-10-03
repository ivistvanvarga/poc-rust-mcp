//! The storage layer against a **real** database.
//!
//! `Store` dispatches every query to whichever of the three sqlx backends the URL selected, and only
//! SQLite can be exercised here without a container: it is an embedded, in-process database, so the
//! whole round trip — migrations, insert, read-back of a JSON column and a timestamp, `DELETE` row
//! count — runs on a bare checkout with nothing else running.
//!
//! That makes this file the regression guard for the dialect plumbing itself: placeholders,
//! migrations and column decoding are chosen per backend, and a mistake in any of them would fail
//! here rather than only against the devcontainer. PostgreSQL and MySQL need containers, so they
//! are verified manually; what this file proves is that the *shared* code path is correct.
//!
//! Writes go through the public fire-and-forget [`Store::record_success`]/[`record_failure`], exactly
//! as the tools use them, so these tests also pin down that path rather than a private shortcut.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use poc_rust_mcp::db::{Dialect, Store};

/// Generous ceiling for a background write to land. SQLite is in-process, so this is only ever hit
/// if something actually blocks.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Polling interval while waiting for a detached write to land.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// An in-memory database, private to this store and needing nothing installed.
const IN_MEMORY: &str = "sqlite::memory:";

/// A database file that deletes itself, so the suite leaves nothing behind and can run in parallel.
struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new() -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "poc-rust-mcp-test-{}-{serial}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        Self { path }
    }

    fn url(&self) -> String {
        format!("sqlite://{}", self.path.display())
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        // SQLite in WAL mode would leave sidecar files; this store does not enable WAL, but a
        // leftover must not outlive the test either way.
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path().display())));
        }
    }
}

/// Poll `list` until `predicate` holds, so a detached write can be observed.
///
/// History writes are fire-and-forget by design, so a test that wants to see a row has to wait for
/// the runtime to schedule the spawned task. Failing on the deadline rather than hanging keeps a
/// regression in the insert path visible as a failure instead of a stalled suite.
async fn wait_for_history(
    store: &Store,
    predicate: impl Fn(&[poc_rust_mcp::db::HistoryEntry]) -> bool,
) -> Vec<poc_rust_mcp::db::HistoryEntry> {
    let deadline = Instant::now() + WRITE_TIMEOUT;
    loop {
        let entries = store
            .list(100)
            .await
            .expect("listing history from sqlite should succeed");
        if predicate(&entries) {
            return entries;
        }
        assert!(
            Instant::now() < deadline,
            "history never satisfied the expectation; last seen: {entries:#?}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[tokio::test]
async fn migrations_apply_and_the_empty_database_is_reachable() {
    let store = Store::connect(IN_MEMORY);
    assert_eq!(store.dialect(), Some(Dialect::Sqlite));

    // `status` is the first call in practice, and it is the call that has to apply the migrations.
    assert_eq!(
        store
            .status()
            .await
            .expect("sqlite should migrate and answer"),
        "sqlite database reachable, 0 stored calculations"
    );
}

#[tokio::test]
async fn a_successful_call_round_trips_through_a_real_json_and_timestamp_column() {
    let store = Store::connect(IN_MEMORY);
    store.record_success("add", serde_json::json!({ "a": 2, "b": 40 }), "42");

    let entries = wait_for_history(&store, |entries| entries.len() == 1).await;
    let entry = &entries[0];

    // The JSON column survived, rather than being flattened to text on the way in.
    assert_eq!(entry.operation, "add");
    assert_eq!(entry.inputs, serde_json::json!({ "a": 2, "b": 40 }));
    assert_eq!(entry.result.as_deref(), Some("42"));
    assert_eq!(entry.error, None);

    // The timestamp column survived, and is recent — i.e. the server wrote it rather than relying
    // on a column default, which SQLite cannot express in a format that sorts.
    let age = chrono::Utc::now().signed_duration_since(entry.created_at);
    assert!(
        age < chrono::TimeDelta::seconds(60),
        "created_at is not recent: {age:?}"
    );

    let line = entry.to_line();
    assert!(line.contains("add"), "{line}");
    assert!(line.ends_with("-> 42"), "{line}");
}

#[tokio::test]
async fn a_failed_call_round_trips_through_the_error_column() {
    let store = Store::connect(IN_MEMORY);
    store.record_failure(
        "div",
        serde_json::json!({ "dividend": 9.0 }),
        "division by zero",
    );

    let entries = wait_for_history(&store, |entries| entries.len() == 1).await;
    let entry = &entries[0];

    assert_eq!(entry.operation, "div");
    assert_eq!(entry.result, None);
    assert_eq!(entry.error.as_deref(), Some("division by zero"));
    assert!(entry.to_line().ends_with("-> error: division by zero"));
}

#[tokio::test]
async fn history_comes_back_newest_first_and_honours_the_limit() {
    let store = Store::connect(IN_MEMORY);
    for index in 0..5 {
        store.record_success(
            "add",
            serde_json::json!({ "index": index }),
            &index.to_string(),
        );
    }
    wait_for_history(&store, |entries| entries.len() == 5).await;

    let newest_first = store.list(3).await.expect("listing should succeed");
    assert_eq!(newest_first.len(), 3, "the limit must be honoured");
    // Ties on `created_at` are expected — SQLite's own resolution is a second — which is exactly
    // why `id DESC` is the second sort key rather than an afterthought.
    let results: Vec<&str> = newest_first
        .iter()
        .filter_map(|entry| entry.result.as_deref())
        .collect();
    assert_eq!(results, ["4", "3", "2"], "newest first");

    let ids: Vec<i64> = newest_first.iter().map(|entry| entry.id).collect();
    let mut descending = ids.clone();
    descending.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(
        ids, descending,
        "auto-incrementing ids must descend with the rows"
    );
}

#[tokio::test]
async fn clear_reports_the_number_of_rows_it_removed() {
    // The reason `clear` uses DELETE rather than TRUNCATE: TRUNCATE carries no row count on
    // PostgreSQL or MySQL, so the tool used to be in a position to claim it deleted nothing.
    let store = Store::connect(IN_MEMORY);
    for index in 0..3 {
        store.record_success("add", serde_json::json!({ "index": index }), "3");
    }
    wait_for_history(&store, |entries| entries.len() == 3).await;

    assert_eq!(store.clear().await.expect("clearing should succeed"), 3);
    assert!(
        store
            .list(100)
            .await
            .expect("listing should succeed")
            .is_empty(),
        "history should be empty after a clear"
    );
    // A second clear has nothing to remove, and must say so rather than report stale rows.
    assert_eq!(store.clear().await.expect("clearing should succeed"), 0);
}

#[tokio::test]
async fn a_file_backed_database_is_created_on_demand_and_keeps_its_rows() {
    // `sqlite://path.db` must work on a fresh checkout, so the file is created if it is missing, and
    // its contents must outlive the store that wrote them.
    let path = TempDb::new();
    assert!(
        !path.path().exists(),
        "the fixture should start without a database file"
    );

    let store = Store::connect(&path.url());
    store.record_success("mul", serde_json::json!({ "a": 6, "b": 7 }), "42");
    wait_for_history(&store, |entries| entries.len() == 1).await;

    assert!(
        path.path().exists(),
        "the database file should have been created"
    );

    // A second store over the same file sees the first one's row: the schema is not per-process.
    let reopened = Store::connect(&path.url());
    let entries = wait_for_history(&reopened, |entries| entries.len() == 1).await;
    assert_eq!(entries[0].result.as_deref(), Some("42"));
}

#[tokio::test]
async fn migrations_are_applied_once_and_re_running_them_is_harmless() {
    // sqlx records applied migrations in `_sqlx_migrations`, so a second store against the same
    // database must find the work already done instead of failing on `table already exists`.
    let path = TempDb::new();

    let first = Store::connect(&path.url());
    first.record_success("add", serde_json::json!({ "a": 1, "b": 1 }), "2");
    wait_for_history(&first, |entries| entries.len() == 1).await;

    let second = Store::connect(&path.url());
    assert_eq!(
        second
            .status()
            .await
            .expect("a second store should find the schema ready"),
        "sqlite database reachable, 1 stored calculations"
    );
}

#[tokio::test]
async fn a_detached_write_and_a_concurrent_read_never_migrate_twice() {
    // `record_success` hands a clone of the store to a spawned task, and a `tokio::sync::OnceCell`
    // clones to a fresh empty cell. When the cell was per-store rather than shared, the spawned
    // task ran its *own* migration alongside the caller's: on SQLite, which has no migration lock,
    // both read an empty `_sqlx_migrations` and one lost with a UNIQUE violation on `version`. The
    // symptom was a silently dropped row, so this asserts every row lands.
    //
    // This needs a *file*-backed database. An in-memory store is capped at a single pooled
    // connection, so the two migrations queue on that connection instead of racing and the bug
    // hides itself.
    let path = TempDb::new();
    let store = Store::connect(&path.url());

    // No await between recording and reading, so the spawned task and the caller reach
    // `ensure_schema` together — the race the shared cell closes.
    for index in 0..25 {
        store.record_success(
            "add",
            serde_json::json!({ "index": index }),
            &index.to_string(),
        );
        let _ = store.list(1).await;
    }

    let entries = wait_for_history(&store, |entries| entries.len() == 25).await;
    assert_eq!(
        entries.len(),
        25,
        "every detached write must survive, including the ones that raced a read"
    );
}

#[tokio::test]
async fn a_read_only_url_does_not_create_the_database_behind_the_users_back() {
    // `mode=ro` is the URL's way of saying "this file must already exist". Creating it anyway would
    // turn a typo'd path into a silently empty database that looks healthy.
    let path = TempDb::new();
    let store = Store::connect_with_timeout(
        &format!("{}?mode=ro", path.url()),
        Duration::from_millis(250),
    );

    let error = store.status().await.unwrap_err();
    assert!(
        !path.path().exists(),
        "a read-only URL must not create the file"
    );
    assert!(
        !error.is_empty(),
        "the failure must explain itself: {error}"
    );
}
