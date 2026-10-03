// SPDX-License-Identifier: BSD-3-Clause
//! `db` internals that no public API can reach.
//!
//! Relocated here from `src/db.rs`. Everything here goes through `db::testing`, the
//! `#[doc(hidden)]` shim module that exists solely so the tests can live in `tests/`. If a test in
//! this file can be written against the public `Store`/`Dialect` surface instead, it belongs in
//! `tests/store_offline.rs` or `tests/sqlite_backend.rs` — do not grow this file for that.
//!
//! Two of these guard bugs that no amount of end-to-end testing would have caught, so they are worth
//! keeping precise: `clones_share_one_schema_cell` and `breaker_does_not_trip_on_query_level_errors`.

use poc_rust_mcp::db::{Dialect, Limits, Store, testing};
use std::time::Duration;

#[test]
fn successful_call_resets_the_breaker() {
    let store = Store::disabled();
    testing::note_failure(&store, &sqlx::Error::PoolClosed);
    testing::note_failure(&store, &sqlx::Error::PoolClosed);
    assert_eq!(testing::failure_count(&store), 2);
    assert!(
        testing::breaker_is_open(&store),
        "the breaker should be open at the default threshold of 2"
    );

    testing::note_reachable(&store);
    assert_eq!(testing::failure_count(&store), 0);
    assert!(!testing::breaker_is_open(&store));
}

#[test]
fn breaker_does_not_trip_on_query_level_errors() {
    // A bad query must not be able to disable storage, however many of them a client sends.
    let store = Store::disabled();
    testing::note_failure(&store, &sqlx::Error::RowNotFound);
    assert_eq!(testing::failure_count(&store), 0);
    assert!(!testing::breaker_is_open(&store));
}

#[tokio::test]
async fn clones_share_one_schema_cell() {
    // `tokio::sync::OnceCell` clones to a fresh, *empty* cell when it has no value yet, and every
    // history write hands a clone to a spawned task before any migration has run. With a per-store
    // cell that task migrates on its own, racing the caller's: harmless behind PostgreSQL's advisory
    // lock, a UNIQUE violation on `_sqlx_migrations.version` on SQLite, which has no migration lock.
    //
    // The clone is taken *before* the cell is set on purpose: cloning afterwards copies the value and
    // would pass either way.
    let store = Store::connect("sqlite::memory:");
    let detached = store.clone();

    testing::mark_schema_initialised(&store).await;
    assert!(
        testing::schema_initialised(&detached),
        "a clone taken before the first migration must see that migration"
    );
}

#[test]
fn a_url_without_a_supported_scheme_is_rejected_by_name() {
    // The message has to name the alternatives, otherwise a typo looks like "storage off".
    let no_scheme = Dialect::from_url("mcp.db").unwrap_err();
    assert!(no_scheme.contains("sqlite"), "{no_scheme}");

    let typo = Dialect::from_url("postgresqls://db/mcp").unwrap_err();
    assert!(typo.contains("postgresqls"), "{typo}");
    for scheme in Dialect::SCHEMES {
        assert!(typo.contains(scheme), "{typo} should list {scheme}");
    }
}

#[test]
fn each_backend_gets_its_own_placeholder_spelling() {
    assert_eq!(
        testing::insert_sql(Dialect::Postgres),
        "INSERT INTO calc_history (operation, inputs, result, error, created_at) \
         VALUES ($1, $2, $3, $4, $5)"
    );
    assert_eq!(
        testing::insert_sql(Dialect::MySql),
        "INSERT INTO calc_history (operation, inputs, result, error, created_at) \
         VALUES (?, ?, ?, ?, ?)"
    );
    assert_eq!(
        testing::list_sql(Dialect::Sqlite),
        testing::list_sql(Dialect::MySql),
        "MySQL and SQLite share the '?' placeholder style"
    );
    assert!(testing::list_sql(Dialect::Postgres).contains("LIMIT $1"));
}

#[test]
fn each_backend_has_its_own_migration_set() {
    // Each set is embedded from its own directory, so they cannot silently drift into one shared
    // file. Version 1 must exist in all three or a backend would migrate to nothing.
    for dialect in [Dialect::Postgres, Dialect::MySql, Dialect::Sqlite] {
        let versions: Vec<i64> = testing::migrations(dialect)
            .iter()
            .map(|migration| migration.version)
            .collect();
        assert_eq!(
            versions,
            [1],
            "{} should have exactly migration 1",
            dialect.label()
        );
    }
}

#[test]
fn the_postgres_migration_is_unchanged_from_when_it_was_the_only_one() {
    // sqlx stores a migration's SHA-384 as its checksum, so even a comment edit here would make every
    // existing PostgreSQL deployment fail with `VersionMismatch` on the next storage call.
    assert_eq!(
        testing::postgres_migration_sql().as_deref(),
        Some(include_str!("../migrations/postgres/0001_calc_history.sql")),
    );
}

#[test]
fn sqlite_urls_are_split_like_sqlx_splits_them() {
    // Mirrors sqlx's own parser, so these assertions are about *staying in step* with it rather than
    // about this crate's idea of a SQLite URL.
    assert_eq!(
        testing::sqlite_url_parts("sqlite::memory:"),
        (":memory:", None)
    );
    assert_eq!(
        testing::sqlite_url_parts("sqlite://mcp.db"),
        ("mcp.db", None)
    );
    assert_eq!(
        testing::sqlite_url_parts("sqlite:///var/lib/mcp.db"),
        ("/var/lib/mcp.db", None)
    );
    assert_eq!(
        testing::sqlite_url_parts("sqlite://mcp.db?mode=ro"),
        ("mcp.db", Some("mode=ro"))
    );
}

#[test]
fn sqlite_query_parameters_are_read_without_percent_decoding() {
    // No parameter this crate inspects needs percent-decoding, and pretending otherwise would be a
    // claim about sqlx's behaviour rather than about this code.
    let params = Some("mode=rwc&cache=shared");
    assert_eq!(testing::sqlite_param(params, "mode"), Some("rwc"));
    assert_eq!(testing::sqlite_param(params, "cache"), Some("shared"));
    assert_eq!(testing::sqlite_param(params, "immutable"), None);
    assert_eq!(testing::sqlite_param(None, "mode"), None);
}

#[test]
fn in_memory_sqlite_is_recognised_in_both_spellings() {
    // An in-memory store is forced to one pooled connection, so this recognition changes behaviour
    // rather than merely parsing.
    assert!(testing::sqlite_is_in_memory(":memory:", None));
    assert!(testing::sqlite_is_in_memory("", Some("mode=memory")));
    assert!(!testing::sqlite_is_in_memory("mcp.db", Some("mode=rwc")));
}

#[test]
fn limits_still_default_to_the_empty_configuration() {
    // A convenience worth pinning here rather than only in tests/config.rs: every default in
    // `Limits` is mirrored from `StorageConfig`, so the two can drift silently.
    assert_eq!(Limits::default(), Limits::from(&Default::default()));
    assert_eq!(Limits::default().acquire_timeout, Duration::from_secs(3));
}
