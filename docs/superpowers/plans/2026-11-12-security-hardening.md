# Security Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the audit findings in `poc-rust-mcp`: harden credentials and errors, bound every unbounded resource, create SQLite history files owner-only, replace rmcp's unprotected SSE server with a validating transport, upgrade sqlx to 0.9, and document the remaining accepted risks.

**Architecture:** Eight tasks, each with its own test cycle and commit. Dependency work comes first because it changes every storage call; then storage hardening; then the new `src/sse.rs` transport; then documentation. The public MCP tool layer and the `#[tool]`/`#[tool_box]` arrangement stay untouched.

**Tech Stack:** Rust 2024, rmcp 0.1.5 (pinned), sqlx 0.9.0, axum 0.8, tokio, tokio-util, http, getrandom, SQLite/PostgreSQL/MySQL.

**Spec:** `docs/superpowers/specs/2026-11-12-security-hardening-design.md`

## Global Constraints

- rmcp stays at `"0.1"` (resolves to 0.1.5); do not rewrite tools for rmcp 3.x.
- sqlx is exactly `0.9.0` with `runtime-tokio`, `postgres`, `mysql`, `mysql-rsa`, `sqlite-bundled`, `macros`, `migrate`, `json`, `chrono`.
- No `unsafe`. No new CLI token flag. No wildcard `Host`/`Origin` entries.
- Bearer token is header-only (`Authorization: Bearer …`); never in a query string.
- Non-loopback SSE bind requires a token **and** an explicit `sse_allowed_hosts` list.
- Every test lives in `tests/`; no `#[cfg(test)]` in `src/`.
- `src/main.rs::calculator` remains the only place a `Calculator` is built; the SSE path builds one per session with the same page size.
- Limits: message body 256 KiB, session queue 32, sessions 64 by default, subscriptions 256, pending writes 64.
- SQLite files created by this process are mode `0600` on Unix; existing files are never chmodded.
- Final verification must run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo build`, `cargo audit` and all must exit 0.

## Review Focus

These are the failure modes a user will hit that no single existing test exercises. Each has a named test in the task that owns the code.

1. **DNS rebinding** — a browser page opens SSE with its own `Host`/`Origin`. Test: `tests/sse.rs::foreign_host_and_origin_are_rejected`.
2. **Reverse proxy** — a TLS proxy forwards `Host: mcp.example` while the browser sends `Origin: https://mcp.example`. Must be accepted. Test: `tests/sse.rs::proxy_style_origin_matches_a_host_without_a_port`.
3. **Malformed/oversized POST** — `POST /message` with invalid JSON or a 300 KiB body must return a 4xx, not panic or desync. Tests: `tests/sse.rs::post_rejections_are_explicit`.
4. **Existing SQLite file** — connecting must not chmod a file the operator already had. Test: `tests/sqlite_backend.rs::existing_sqlite_file_permissions_are_left_alone`.
5. **Credential leak while degraded** — a URL with a password against a dead host must not echo the password, host, or driver text to the client. Test: `tests/store_offline.rs::client_facing_errors_never_echo_the_url_or_driver_text`.

---

## File Structure

Create:

- `src/sse.rs` — SSE transport, validation middleware, session registry, `Settings`.
- `tests/sse.rs` — in-process SSE server tests with an HTTP/1.0 client.
- `.cargo/audit.toml` — reviewed advisory ignores.
- `docs/superpowers/specs/2026-11-12-security-hardening-design.md` — already written.
- `docs/superpowers/plans/2026-11-12-security-hardening.md` — this plan.

Modify:

- `Cargo.toml`, `Cargo.lock` — dependency changes.
- `src/db.rs` — sqlx 0.9, redaction, sanitized errors, bounds, retention, SQLite mode.
- `src/config.rs` — SSE settings, `Secret`, host/origin parsing and validation, usage text.
- `src/lib.rs` — export `sse`.
- `src/main.rs` — wire the new transport.
- `src/server.rs` — subscription cap, no whole-set clone, Debug-format user input.
- `config.example.toml`, `README.md`, `AGENTS.md`, `SECURITY.md` — documentation.
- `tests/db.rs`, `tests/store_offline.rs`, `tests/sqlite_backend.rs`, `tests/server.rs`, `tests/config.rs` — new coverage.

---

### Task 1: Upgrade sqlx to 0.9 and record audit posture

**Files:**
- Modify: `Cargo.toml`, `Cargo.lock`
- Modify: `src/db.rs`
- Create: `.cargo/audit.toml`

**Interfaces:**
- Produces: a workspace where `sqlx::query`/`query_as` take `SqlSafeStr`; dynamic SQL is wrapped as `sqlx::AssertSqlSafe(...)`.

- [ ] **Step 1: Change `Cargo.toml`**

Replace the sqlx dependency and add the audit config directory:

```toml
sqlx = { version = "=0.9.0", default-features = false, features = [
    "runtime-tokio",
    "postgres",
    "mysql",
    "mysql-rsa",
    "sqlite-bundled",
    "macros",
    "migrate",
    "json",
    "chrono",
] }
```

Remove `transport-sse-server` from rmcp's feature list, leaving `["server", "transport-io"]`.

- [ ] **Step 2: Refresh the lockfile and see what breaks**

Run: `cargo update -p sqlx --precise 0.9.0` then `cargo check --all-targets`
Expected: compile errors only where a `String` is passed to `sqlx::query`/`query_as`.

- [ ] **Step 3: Wrap dynamic SQL**

In `src/db.rs`, every SQL value from `Dialect::insert_sql()` / `list_sql()` / `prune_sql()` is `String`. Wrap each at the call site:

```rust
sqlx::query(sqlx::AssertSqlSafe(sql))
```

and

```rust
sqlx::query_as::<_, HistoryEntry>(sqlx::AssertSqlSafe(sql))
```

The static `&'static str` queries (`delete_sql`, `count_sql`) stay unwrapped. Add a short comment above the first wrap explaining that the text is generated from static fragments plus placeholders, never user input.

- [ ] **Step 4: Confirm sqlx error variants and migrations**

Run: `cargo check --all-targets`
Expected: clean. If `sqlx::Error::Tls` or another variant in `is_connectivity_error` moved, adjust the `matches!` to the 0.9 variant. Do not weaken the set.

- [ ] **Step 5: Run the storage tests**

Run: `cargo test --test sqlite_backend --test db --test store_offline`
Expected: PASS with no database container.

- [ ] **Step 6: Write `.cargo/audit.toml`**

```toml
[advisories]
ignore = [
    # rmcp's Streamable HTTP server transport. This crate does not use it; src/sse.rs is the
    # only network transport and performs Host/Origin/token validation itself.
    "RUSTSEC-2026-0189",
    # Bundled SQLite CVE-2026-119375. Fixed only in libsqlite3-sys >=0.39, which sqlx 0.9
    # excludes. Local attack: a crafted database plus WAL pair the victim opens.
    "RUSTSEC-2026-0459",
    # paste is unmaintained, not vulnerable. Build-time macro reachable only via rmcp 0.1.5.
    "RUSTSEC-2024-0436",
]
```

- [ ] **Step 7: Verify the audit**

Run: `cargo audit`
Expected: exit 0. `rsa` must no longer be reported; confirm with `cargo tree -i rsa` showing `rsa v0.10.*` under `sqlx-mysql`.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock src/db.rs .cargo/audit.toml
git commit -m "build: upgrade sqlx to 0.9 and record audit ignores"
```

---

### Task 2: Redact credentials, sanitize client errors, fix log injection

**Files:**
- Modify: `src/db.rs`, `src/server.rs`
- Test: `tests/db.rs`, `tests/store_offline.rs`

**Interfaces:**
- Produces: `pub fn redact_url(url: &str) -> String` in `src/db.rs`; `db::testing::redact_url`.
- Produces: `Store::finish` and migration errors return one of the stable messages:
  `"<context>: database unreachable"`, `"<context>: database rejected the request"`,
  `"<context>: internal storage error"`, or `"failed to apply migrations: migration failed"` /
  `"failed to apply migrations: migration checksum mismatch"`.

- [ ] **Step 1: Write the failing redaction test**

Add to `tests/db.rs`:

```rust
#[test]
fn redacted_urls_keep_the_backend_but_never_the_credentials() {
    let cases = [
        (
            "postgres://mcp:supersecret@example.com:5432/mcp?password=alsosecret",
            "postgres://***@example.com:5432/mcp",
        ),
        (
            "mysql://user@[::1]:3306/db?ssl-mode=REQUIRED",
            "mysql://***@[::1]:3306/db",
        ),
        ("postgres://example.com/mcp", "postgres://example.com/mcp"),
        ("not-a-url", "<redacted>"),
    ];
    for (input, expected) in cases {
        assert_eq!(poc_rust_mcp::db::testing::redact_url(input), expected, "{input}");
    }
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --test db redacted_urls_keep_the_backend -- --exact`
Expected: FAIL, item `redact_url` not found.

- [ ] **Step 3: Implement `redact_url`**

In `src/db.rs`, above `Dialect`:

```rust
/// Keep the scheme, authority host and path; replace userinfo and drop the query/fragment.
#[must_use]
pub fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return "<redacted>".to_owned();
    };
    if scheme.is_empty() {
        return "<redacted>".to_owned();
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let path = rest[authority_end..]
        .split(['?', '#'])
        .next()
        .unwrap_or_default();
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let userinfo = if authority.contains('@') { "***@" } else { "" };
    format!("{scheme}://{userinfo}{host}{path}")
}
```

Add to `db::testing`:

```rust
pub use super::redact_url;
```

- [ ] **Step 4: Run the test**

Run: `cargo test --test db redacted_urls_keep_the_backend -- --exact`
Expected: PASS.

- [ ] **Step 5: Use redaction on the URL error paths**

In `Dialect::from_url`, remove `found {url:?}` from the no-scheme message. Keep the accepted-scheme list. In `Store::connect_with_limits`, log:

```rust
warn!(
    url = %redact_url(url),
    error = %error,
    "unusable database URL; serving calculator tools without history"
);
```

In `Pool::connect`, when a driver's `FromStr` fails, return a generic message naming only the backend, e.g. `"database URL is not valid for the postgres backend"`, and log the raw error only through the redacted path above. Do not include the URL in the returned string.

- [ ] **Step 6: Sanitize client-facing errors**

In `Store::finish`, replace `format!("{context}: {error}")` with a `warn!` of the full error plus a categorized message:

```rust
fn describe_storage_error(error: &sqlx::Error) -> &'static str {
    if is_connectivity_error(error) {
        "database unreachable"
    } else if matches!(error, sqlx::Error::Database(_)) {
        "database rejected the request"
    } else {
        "internal storage error"
    }
}
```

Then `Err(format!("{context}: {}", describe_storage_error(&error)))`.

In `ensure_schema`, map `MigrateError`:

- `VersionMismatch(_)` → `"failed to apply migrations: migration checksum mismatch"`
- `VersionMissing(_) | VersionTooOld(_, _) | VersionTooNew(_, _) | VersionNotPresent(_)` →
  `"failed to apply migrations: migration history does not match the embedded migrations"`
- everything else → `"failed to apply migrations: migration failed"`

Keep the full `MigrateError` in a `warn!` and keep the existing breaker bookkeeping for `Execute`/`ExecuteMigration`.

- [ ] **Step 7: Write the failing leak test**

Add to `tests/store_offline.rs`:

```rust
#[tokio::test]
async fn client_facing_errors_never_echo_the_url_or_driver_text() {
    let store = Store::connect_with_timeout(
        "postgres://mcp:supersecret@127.0.0.1:1/mcp",
        FAST,
    );
    let error = store.status().await.expect_err("the database is down");
    assert!(!error.contains("supersecret"), "{error}");
    assert!(!error.contains("127.0.0.1"), "{error}");
    assert!(!error.contains("connection refused"), "{error}");
    assert!(!error.contains("sqlx"), "{error}");
    assert!(
        error.contains("database unreachable") || error.contains("migration failed"),
        "{error}"
    );
}
```

- [ ] **Step 8: Run the leak test**

Run: `cargo test --test store_offline client_facing_errors_never_echo -- --exact`
Expected: PASS. If the migration path reports `"migration failed"`, that is acceptable; the assertion permits both stable forms.

- [ ] **Step 9: Fix log injection**

In `src/server.rs`, change the two user-controlled `SubscribeRequestParam`/`UnsubscribeRequestParam` log lines from `%request.uri` to `?request.uri`. Search for other `%`-formatted client strings with `grep -n 'uri = %\|%request\.' src/` and convert any remaining ones to `?`.

- [ ] **Step 10: Run the affected suites and commit**

Run: `cargo test --test db --test store_offline --test server`
Then:

```bash
git add src/db.rs src/server.rs tests/db.rs tests/store_offline.rs
git commit -m "fix: redact database credentials and sanitize storage errors"
```

---

### Task 3: Bound subscriptions and detached history writes

**Files:**
- Modify: `src/server.rs`, `src/db.rs`
- Test: `tests/server.rs`, `tests/db.rs`

**Interfaces:**
- Produces: `Calculator::with_subscription_limit(self, limit: usize) -> Calculator`.
- Produces: `pub const MAX_SUBSCRIPTIONS: usize = 256;` in `src/server.rs`.
- Produces: `pub const MAX_PENDING_WRITES: usize = 64;` in `src/db.rs`.
- Produces: `db::testing::take_write_permits(store) -> Vec<OwnedSemaphorePermit>` and
  `db::testing::dropped_writes(store) -> usize`.

- [ ] **Step 1: Write the failing subscription-cap test**

Add to `tests/server.rs`:

```rust
#[tokio::test]
async fn subscriptions_are_capped_per_client() {
    let calculator = Calculator::new(Store::disabled()).with_subscription_limit(2);
    for uri in [resources::HISTORY_URI, "calc://history/1"] {
        calculator
            .subscribe(
                SubscribeRequestParam { uri: uri.to_owned() },
                request_context(),
            )
            .await
            .expect("within the cap");
    }
    // Re-subscribing to the same URI is free.
    calculator
        .subscribe(
            SubscribeRequestParam { uri: resources::HISTORY_URI.to_owned() },
            request_context(),
        )
        .await
        .expect("duplicate subscription");
    let error = calculator
        .subscribe(
            SubscribeRequestParam { uri: "calc://history/2".to_owned() },
            request_context(),
        )
        .await
        .expect_err("cap exceeded");
    assert!(error.message.contains("too many"), "{error}");
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --test server subscriptions_are_capped -- --exact`
Expected: FAIL, no `with_subscription_limit`.

- [ ] **Step 3: Implement the cap**

In `src/server.rs`:

```rust
pub const MAX_SUBSCRIPTIONS: usize = 256;
```

Add `subscription_limit: usize` to `Calculator`; set it to `MAX_SUBSCRIPTIONS` in `with_list_page_size`; add:

```rust
#[must_use]
pub fn with_subscription_limit(mut self, limit: usize) -> Self {
    self.subscription_limit = limit.max(1);
    self
}
```

In `subscribe`, lock once:

```rust
let mut subscriptions = self.subscriptions.lock().expect("subscriptions mutex poisoned");
if !subscriptions.contains(&request.uri) && subscriptions.len() >= self.subscription_limit {
    return Err(McpError::invalid_params("too many resource subscriptions", None));
}
subscriptions.insert(request.uri);
```

`McpError` is already the alias used by the file (`rmcp::Error`); use the same `invalid_params` call shape as `resources::parse`.

- [ ] **Step 4: Remove the whole-set clone in `announce_write`**

Replace `self.subscriptions()` usage inside `announce_write` with a scoped lock that collects only matching URIs:

```rust
let invalidated: Vec<String> = {
    let subscriptions = self.subscriptions.lock().expect("subscriptions mutex poisoned");
    resources::invalidated_by_write(operation)
        .into_iter()
        .filter(|uri| subscriptions.contains(uri))
        .collect()
};
```

Delete the now-unused `Calculator::subscriptions` helper.

- [ ] **Step 5: Write the failing pending-write test**

Add to `tests/db.rs`:

```rust
#[tokio::test]
async fn saturated_write_slots_drop_records_instead_of_blocking_the_caller() {
    let store = Store::connect_with_timeout("postgres://mcp:mcp@127.0.0.1:1/mcp", FAST);
    let held = poc_rust_mcp::db::testing::take_write_permits(&store);
    assert_eq!(held.len(), poc_rust_mcp::db::MAX_PENDING_WRITES);
    let started = std::time::Instant::now();
    store.record_success("add", serde_json::json!({ "a": 1, "b": 1 }), "2");
    assert_eq!(poc_rust_mcp::db::testing::dropped_writes(&store), 1);
    assert!(started.elapsed() < std::time::Duration::from_millis(50));
    drop(held);
}
```

`FAST` is the existing 250 ms constant in that file.

- [ ] **Step 6: Run it and watch it fail**

Run: `cargo test --test db saturated_write_slots -- --exact`
Expected: FAIL, missing testing item / limit.

- [ ] **Step 7: Implement the semaphore**

In `src/db.rs`:

```rust
pub const MAX_PENDING_WRITES: usize = 64;
```

Add to `Store`:

```rust
writes: Arc<tokio::sync::Semaphore>,
dropped: Arc<std::sync::atomic::AtomicUsize>,
```

Initialise in `with_backend` with `Arc::new(Semaphore::new(MAX_PENDING_WRITES))` and `Arc::default()`.

In `spawn_insert`, before cloning/spawning:

```rust
let Ok(permit) = self.writes.clone().try_acquire_owned() else {
    self.dropped.fetch_add(1, Ordering::Relaxed);
    debug!(%operation, "history write dropped: too many pending writes");
    return;
};
tokio::spawn(async move {
    let _permit = permit;
    // existing insert logic
});
```

Add to `db::testing`:

```rust
pub fn take_write_permits(store: &Store) -> Vec<tokio::sync::OwnedSemaphorePermit> {
    let mut permits = Vec::new();
    while let Ok(permit) = store.writes.clone().try_acquire_owned() {
        permits.push(permit);
    }
    permits
}

pub fn dropped_writes(store: &Store) -> usize {
    store.dropped.load(std::sync::atomic::Ordering::Relaxed)
}
```

- [ ] **Step 8: Run the tests and commit**

Run: `cargo test --test server --test db --test mcp_features`
Then:

```bash
git add src/server.rs src/db.rs tests/server.rs tests/db.rs
git commit -m "fix: bound subscriptions and pending history writes"
```

---

### Task 4: Retain only the newest history rows and clamp reads

**Files:**
- Modify: `src/db.rs`
- Test: `tests/sqlite_backend.rs`, `tests/db.rs`

**Interfaces:**
- Produces: `Dialect::prune_sql(self) -> String`; `db::testing::prune_sql(dialect) -> String`.
- Produces: `Store::list` clamps its argument to `1..=limits.max_history_rows`.

- [ ] **Step 1: Write the failing retention test**

Add to `tests/sqlite_backend.rs`:

```rust
#[tokio::test]
async fn history_is_pruned_to_the_configured_row_cap() {
    let store = Store::connect_with_limits(
        IN_MEMORY,
        poc_rust_mcp::db::Limits {
            max_history_rows: 5,
            ..poc_rust_mcp::db::Limits::default()
        },
    );
    for index in 0..9 {
        store.record_success(
            "add",
            serde_json::json!({ "index": index }),
            &index.to_string(),
        );
    }
    let entries = wait_for_history(&store, |entries| entries.len() == 5).await;
    let results: Vec<&str> = entries.iter().filter_map(|e| e.result.as_deref()).collect();
    assert_eq!(results, ["8", "7", "6", "5", "4"], "only the newest survive");
    // A direct oversized read cannot escape the cap either.
    let clamped = store.list(1000).await.expect("listing should succeed");
    assert_eq!(clamped.len(), 5);
    // Zero and negative limits clamp to one row, not to "unlimited".
    assert_eq!(store.list(0).await.expect("listing").len(), 1);
    assert_eq!(store.list(-5).await.expect("listing").len(), 1);
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --test sqlite_backend history_is_pruned -- --exact`
Expected: FAIL; nine rows survive and `list(1000)` returns nine.

- [ ] **Step 3: Add the prune SQL**

In `Dialect`:

```rust
fn prune_sql(self) -> String {
    match self {
        Self::Postgres => format!(
            "DELETE FROM calc_history WHERE id NOT IN \
             (SELECT id FROM calc_history ORDER BY created_at DESC, id DESC LIMIT {})",
            self.placeholder(1)
        ),
        Self::Sqlite => format!(
            "DELETE FROM calc_history WHERE id NOT IN \
             (SELECT id FROM calc_history ORDER BY created_at DESC, id DESC LIMIT {})",
            self.placeholder(1)
        ),
        Self::MySql => format!(
            "DELETE h FROM calc_history h LEFT JOIN \
             (SELECT id FROM calc_history ORDER BY created_at DESC, id DESC LIMIT {}) keep \
             ON h.id = keep.id WHERE keep.id IS NULL",
            self.placeholder(1)
        ),
    }
}
```

- [ ] **Step 4: Prune after each insert**

In `Store::insert`, after the insert succeeds, call a new private method:

```rust
async fn prune(&self, backend: &Pool) -> Result<(), String> {
    let sql = backend.dialect().prune_sql();
    let executed = with_pool!(backend, |pool| {
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(self.limits.max_history_rows)
            .execute(pool)
            .await
            .map(|_| ())
    });
    self.finish(executed, "failed to prune history")
}
```

Call it from `insert` with `if let Err(error) = self.prune(&backend).await { debug!(%error, "history pruning failed"); }`. Do not let a prune failure turn a successful insert into a client-visible error; the insert already returned `Ok`.

- [ ] **Step 5: Clamp `Store::list`**

At the top of `list`:

```rust
let limit = limit.clamp(1, self.limits.max_history_rows.max(1));
```

- [ ] **Step 6: Add the prune-SQL test**

Add to `tests/db.rs`:

```rust
#[test]
fn prune_sql_uses_the_dialect_placeholder_and_mysql_derived_table() {
    use poc_rust_mcp::db::{Dialect, testing};
    assert!(testing::prune_sql(Dialect::Postgres).contains("$1"));
    assert!(testing::prune_sql(Dialect::Sqlite).contains('?'));
    let mysql = testing::prune_sql(Dialect::MySql);
    assert!(mysql.contains("DELETE h FROM"), "{mysql}");
    assert!(mysql.contains("LEFT JOIN"), "{mysql}");
}
```

Expose `prune_sql` through `db::testing`.

- [ ] **Step 7: Run the tests and commit**

Run: `cargo test --test sqlite_backend --test db --test server`
Then:

```bash
git add src/db.rs tests/sqlite_backend.rs tests/db.rs
git commit -m "fix: retain only the newest history rows and clamp reads"
```

---

### Task 5: Create SQLite history files owner-only

**Files:**
- Modify: `src/db.rs`
- Test: `tests/sqlite_backend.rs`

**Interfaces:**
- Produces: `Pool::connect` calls `prepare_sqlite_file(url) -> Result<(), String>` before building the pool; failure degrades storage.

- [ ] **Step 1: Write the failing permission tests**

Add to `tests/sqlite_backend.rs`:

```rust
#[cfg(unix)]
#[tokio::test]
async fn a_new_sqlite_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let path = TempDb::new();
    let store = Store::connect(&path.url());
    store.status().await.expect("the database should be usable");
    let mode = std::fs::metadata(path.path())
        .expect("the file should exist")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "new history files must not be group/world readable");
}

#[cfg(unix)]
#[tokio::test]
async fn existing_sqlite_file_permissions_are_left_alone() {
    use std::os::unix::fs::PermissionsExt;
    let path = TempDb::new();
    std::fs::write(path.path(), b"").expect("create the file");
    std::fs::set_permissions(path.path(), std::fs::Permissions::from_mode(0o644))
        .expect("set a shared mode");
    let store = Store::connect(&path.url());
    store.status().await.expect("the database should be usable");
    let mode = std::fs::metadata(path.path())
        .expect("the file should exist")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o644, "an existing file must keep its mode");
}

#[tokio::test]
async fn an_uncreatable_sqlite_path_degrades_to_disabled_storage() {
    let store = Store::connect("sqlite:///definitely/missing/poc-rust-mcp/db.sqlite");
    assert!(!store.is_configured(), "a bad path must not look healthy");
}
```

- [ ] **Step 2: Run them and watch the permission tests fail**

Run: `cargo test --test sqlite_backend sqlite_file -- --exact`
Expected: the new-file test fails with mode `0644` under the usual umask; the bad-path test fails because storage is still configured.

- [ ] **Step 3: Implement file preparation**

In `src/db.rs`:

```rust
fn prepare_sqlite_file(url: &str) -> Result<(), String> {
    use std::str::FromStr;
    let (database, params) = sqlite_url_parts(url);
    if sqlite_is_in_memory(database, params)
        || matches!(sqlite_param(params, "mode"), Some("ro" | "rw"))
    {
        return Ok(());
    }
    let options = sqlx::sqlite::SqliteConnectOptions::from_str(url)
        .map_err(|_| "database URL is not valid for the sqlite backend".to_owned())?;
    let path = options.get_filename();
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }
    match open.open(path) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::metadata(path).is_ok_and(|meta| meta.is_file()) {
                Ok(())
            } else {
                Err("sqlite database path exists but is not a regular file".to_owned())
            }
        }
        Err(error) => Err(format!("cannot create the sqlite database file: {error}")),
    }
}
```

Call it in `Pool::connect`'s SQLite arm before parsing options. Return errors with the generic message, not the path.

- [ ] **Step 4: Run the tests**

Run: `cargo test --test sqlite_backend --test store_offline`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/db.rs tests/sqlite_backend.rs
git commit -m "fix: create sqlite history files owner-only"
```

---

### Task 6: Add the SSE configuration surface

**Files:**
- Modify: `src/config.rs`, `config.example.toml`
- Test: `tests/config.rs`

**Interfaces:**
- Produces: `pub struct Secret` with `new`, `expose`, redacted `Debug`.
- Produces: `ServerConfig` fields `sse_allowed_hosts: Vec<String>`, `sse_allowed_origins: Vec<String>`, `sse_token: Option<Secret>`, `sse_max_sessions: usize`.
- Produces: `pub(crate) struct HostPattern { host: String, port: Option<u16> }`,
  `pub(crate) struct OriginPattern { scheme: String, host: String, port: Option<u16> }`,
  `pub(crate) fn parse_host_pattern`, `parse_origin_pattern`, `normalize_host`.
- Produces: env keys `MCP_SSE_ALLOWED_HOSTS`, `MCP_SSE_ALLOWED_ORIGINS`, `MCP_SSE_TOKEN`, `MCP_SSE_MAX_SESSIONS`.

- [ ] **Step 1: Write the failing env/layering test**

Add to `tests/config.rs`:

```rust
#[test]
fn sse_security_settings_layer_through_environment_and_file() {
    use poc_rust_mcp::config::{Cli, Config, MapEnv};

    let env = MapEnv::new()
        .with("MCP_SSE_ALLOWED_HOSTS", "localhost:8000, 127.0.0.1")
        .with("MCP_SSE_ALLOWED_ORIGINS", "https://ui.example")
        .with("MCP_SSE_TOKEN", "s3cret")
        .with("MCP_SSE_MAX_SESSIONS", "3");
    let config = Config::resolve(
        &Cli { serve_sse: true, ..Cli::default() },
        &env,
    )
    .expect("valid settings");
    assert_eq!(
        config.server.sse_allowed_hosts,
        ["localhost:8000", "127.0.0.1"]
    );
    assert_eq!(config.server.sse_allowed_origins, ["https://ui.example"]);
    assert_eq!(config.server.sse_token.as_ref().unwrap().expose(), "s3cret");
    assert_eq!(config.server.sse_max_sessions, 3);
    assert!(!format!("{:?}", config.server).contains("s3cret"));
}

#[test]
fn sse_security_settings_reject_malformed_values() {
    use poc_rust_mcp::config::{Cli, Config, MapEnv};
    for (key, value) in [
        ("MCP_SSE_ALLOWED_HOSTS", "https://example.com"),
        ("MCP_SSE_ALLOWED_HOSTS", "*"),
        ("MCP_SSE_ALLOWED_HOSTS", "a,,b"),
        ("MCP_SSE_ALLOWED_ORIGINS", "ftp://example.com"),
        ("MCP_SSE_ALLOWED_ORIGINS", "https://example.com/path"),
        ("MCP_SSE_ALLOWED_ORIGINS", "*"),
        ("MCP_SSE_MAX_SESSIONS", "0"),
    ] {
        let env = MapEnv::new().with(key, value);
        assert!(
            Config::resolve(&Cli::default(), &env).is_err(),
            "{key}={value} must be rejected"
        );
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --test config sse_security -- --exact`
Expected: FAIL, unknown fields/keys.

- [ ] **Step 3: Add `Secret` and the fields**

In `src/config.rs`:

```rust
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
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
```

Add the four `ServerConfig` fields. Defaults: empty vectors, `None`, `64`.

- [ ] **Step 4: Add env keys, parsing, and file mirror**

Add the four constants. In `resolve`:

- host/origin values are split on `,`, trimmed, and each entry validated with the parser; an empty entry is an error naming the key.
- `sse_token` uses `non_blank` and `Secret::new`.
- `sse_max_sessions` uses `parse_number`.

In `FileServer`, add `sse_allowed_hosts: Option<Vec<String>>`,
`sse_allowed_origins: Option<Vec<String>>`, `sse_token: Option<String>`,
`sse_max_sessions: Option<usize>`; apply each in `apply_file`.

- [ ] **Step 5: Add parsers and validation**

Implement `normalize_host(host: &str) -> Option<String>`: reject empty, `%`, and control characters; strip a leading `[`/trailing `]`; lowercase; strip one trailing dot.

`parse_host_pattern` parses `http::uri::Authority`, rejects userinfo (`@`), `*`, and port 0; stores the normalised host and `port_u16()`.

`parse_origin_pattern` parses `http::uri::Uri`, requires scheme `http`/`https`, rejects any `path_and_query`, userinfo, `*`, and port 0; stores lowercase scheme, normalised host, and `port_u16()`.

In `validate`, reject `sse_max_sessions == 0`, a token whose trimmed value is empty, and every malformed host/origin entry, using `ConfigError::InvalidValue` with the owning key.

- [ ] **Step 6: Update `Cli::usage` and `config.example.toml`**

Add the four keys under `[server]`, explaining: header-only token (no CLI flag), loopback defaults, non-loopback requirements, comma-separated env lists, and that the origin list is restriction-only (no CORS).

- [ ] **Step 7: Run the config tests and commit**

Run: `cargo test --test config`
Then:

```bash
git add src/config.rs config.example.toml tests/config.rs
git commit -m "feat: add SSE host, origin, token and session settings"
```

---

### Task 7: Replace the SSE transport with a validating one

**Files:**
- Create: `src/sse.rs`
- Modify: `src/lib.rs`, `src/main.rs`, `Cargo.toml`
- Test: `tests/sse.rs` (new)

**Interfaces:**
- Consumes: `config::{HostPattern, OriginPattern, parse_host_pattern, parse_origin_pattern}` and `ServerConfig`'s new fields.
- Produces: `sse::Settings`, `sse::Settings::from_config(&ServerConfig) -> Result<Settings, SseConfigError>`, `Settings::with_keep_alive(self, Duration) -> Settings`.
- Produces: `sse::serve<F, S>(listener: TcpListener, settings: Settings, service: F, cancel: CancellationToken) -> io::Result<()>`.
- Produces: `sse::testing::{try_send_status, normalize_authority, constant_time_eq, parse_origin}`.

- [ ] **Step 1: Add dependencies and export the module**

`Cargo.toml`:

```toml
axum = "0.8"
futures = "0.3"
http = "1"
tokio-stream = "0.1"
tokio-util = "0.7"
getrandom = "0.3"
```

Remove `tokio-util` from `[dev-dependencies]` (it is now a normal dependency). In `src/lib.rs`, add `pub mod sse;`.

- [ ] **Step 2: Write the failing Settings test**

Add to `tests/sse.rs`:

```rust
use poc_rust_mcp::config::{Secret, ServerConfig};
use poc_rust_mcp::sse;

#[test]
fn non_loopback_binds_require_a_token_and_explicit_hosts() {
    let mut config = ServerConfig {
        sse_address: "0.0.0.0:8000".parse().unwrap(),
        ..ServerConfig::default()
    };
    assert!(sse::Settings::from_config(&config).is_err(), "no token, no hosts");

    config.sse_token = Some(Secret::new("t"));
    assert!(sse::Settings::from_config(&config).is_err(), "hosts still missing");

    config.sse_allowed_hosts = vec!["mcp.example".to_owned()];
    sse::Settings::from_config(&config).expect("token + hosts is enough");

    let loopback = ServerConfig::default();
    sse::Settings::from_config(&loopback).expect("loopback needs neither");
}
```

- [ ] **Step 3: Run it and watch it fail**

Run: `cargo test --test sse non_loopback_binds -- --exact`
Expected: FAIL, module/type missing.

- [ ] **Step 4: Implement `Settings` and validation**

In `src/sse.rs`:

```rust
pub const DEFAULT_MAX_SESSIONS: usize = 64;
pub const MESSAGE_BODY_LIMIT: usize = 256 * 1024;
pub const SESSION_QUEUE: usize = 32;
const DEFAULT_KEEP_ALIVE: Duration = Duration::from_secs(15);

#[derive(Debug)]
pub enum SseConfigError {
    NonLoopbackWithoutToken,
    NonLoopbackWithoutHosts,
    InvalidHost(String),
    InvalidOrigin(String),
    InvalidMaxSessions,
}

pub struct Settings {
    configured_addr: SocketAddr,
    allowed_hosts: Option<Vec<HostPattern>>,
    allowed_origins: Option<Vec<OriginPattern>>,
    token: Option<String>,
    max_sessions: usize,
    keep_alive: Duration,
}
```

`from_config` parses every list entry, stores `Option` (`None` = defaults), and returns
`NonLoopbackWithoutToken` / `NonLoopbackWithoutHosts` when `!configured_addr.ip().is_loopback()`
and the token or host list is missing. `with_keep_alive` is a `#[must_use]` builder with
`#[doc(hidden)]`.

`serve` calls a private `resolve(actual: SocketAddr) -> Result<ResolvedSettings, SseConfigError>`
that repeats the non-loopback check against the real listener, pins the default host list to
`actual.port()`, and returns concrete patterns.

- [ ] **Step 5: Implement the request validation helpers**

Use `http::uri::{Authority, Uri}` and `http::header`.

- `normalize_authority(&Authority) -> Option<(String, Option<u16>)>`: `config::normalize_host` plus `port_u16()`; reject userinfo and port 0.
- `request_authority(&Request) -> Option<(String, Option<u16>)>`: `Host` header first, then `request.uri().authority()`.
- `host_allowed(patterns, host, port)`: host equal; entry port `None` matches any, `Some` requires equality.
- `parse_origin(&str) -> Option<(String, String, u16)>`: `(scheme, host, effective_port)`; reject path/query, non-http(s), port 0.
- `origin_allowed(patterns: Option<&[OriginPattern]>, origin: &str, host: &str, port: Option<u16>)`: configured patterns require scheme + host + (pinned port or effective port); `None` requires host equality and, when `port` is `Some`, an equal effective port.
- `constant_time_eq(a: &[u8], b: &[u8]) -> bool`: iterate `0..max(len)` with `get(i).copied().unwrap_or(0)`, OR the XORs, and fold the length difference into the accumulator.
- `bearer_allowed(expected: &str, headers: &HeaderMap)`: split the header on the first space, compare the scheme case-insensitively to `Bearer`, then `constant_time_eq`.

- [ ] **Step 6: Implement the middleware and routes**

```rust
async fn authorize<F, S>(
    State(app): State<App<F, S>>,
    request: Request,
    next: Next,
) -> Result<Response, Response>;
```

Order: authority presence, host allowlist, `Origin` (absent is fine; `null` is rejected), token. `403` for host/origin, `401` with `WWW-Authenticate: Bearer` for token.

Routes:

```rust
Router::new()
    .route("/sse", get(sse_handler::<F, S>))
    .route("/message", post(post_event_handler::<F, S>))
    .layer(DefaultBodyLimit::max(MESSAGE_BODY_LIMIT))
    .layer(middleware::from_fn_with_state(app.clone(), authorize::<F, S>))
    .with_state(app)
```

- [ ] **Step 7: Implement sessions, the transport, and cleanup**

`App<F, S>` holds `Arc<Mutex<HashMap<String, Session>>>`, `ResolvedSettings`, `Arc<F>`, and `PhantomData<fn() -> S>`.

`Session { tx: mpsc::Sender<ClientJsonRpcMessage>, cancel: CancellationToken }`.

`sse_handler`:

- generate `[u8; 32]` with `getrandom::fill`, hex-encode;
- check capacity and insert under one lock; over capacity returns `503` with `Retry-After: 5`;
- build `from_client`/`to_client` channels of `SESSION_QUEUE`; a `SessionGuard` and `CancellationToken` are attached;
- spawn `(app.service)().serve_with_ct(transport, ct.clone())`, then `running.waiting().await`; log failures at `debug!`;
- return `Sse::new(guarded_stream).keep_alive(KeepAlive::new().interval(settings.keep_alive).text("keep-alive"))`;
- the first event is `event: endpoint`, data `/message?sessionId=<id>`; later events are `event: message`.

`SseTransport` implements `Sink<TxJsonRpcMessage<RoleServer>, Error = io::Error>` over `PollSender` and `Stream<Item = RxJsonRpcMessage<RoleServer>>` over `ReceiverStream`.

`SessionGuard::drop` cancels the token and spawns removal of the map entry. `SseTransport` also keeps a guard, so both a client disconnect and a service end clean up; removal is idempotent.

`serve` builds the router, runs `axum::serve(listener, router).with_graceful_shutdown(...)`, and on cancellation cancels every session token before completing.

- [ ] **Step 8: Implement `POST /message`**

Use `Query<PostEventQuery>` and `Result<Json<ClientJsonRpcMessage>, JsonRejection>`. Map rejections: 413 → 413, 415 → 415, everything else → 400. Look up the session under the lock and clone the sender; missing → 404. Use `try_send_status`:

```rust
fn try_send_status(
    sender: &mpsc::Sender<ClientJsonRpcMessage>,
    message: ClientJsonRpcMessage,
) -> Result<StatusCode, StatusCode> {
    match sender.try_send(message) {
        Ok(()) => Ok(StatusCode::ACCEPTED),
        Err(TrySendError::Full(_)) => Err(StatusCode::TOO_MANY_REQUESTS),
        Err(TrySendError::Closed(_)) => Err(StatusCode::GONE),
    }
}
```

The handler maps 429 to a response with `Retry-After: 1`.

- [ ] **Step 9: Wire `main.rs`**

Replace `SseServer` with:

```rust
let settings = poc_rust_mcp::sse::Settings::from_config(server)?;
let listener = tokio::net::TcpListener::bind(server.sse_address).await?;
let cancel = tokio_util::sync::CancellationToken::new();
let signal = cancel.clone();
tokio::spawn(async move {
    if tokio::signal::ctrl_c().await.is_ok() {
        signal.cancel();
    }
});
let page_size = server.list_page_size;
poc_rust_mcp::sse::serve(
    listener,
    settings,
    move || Calculator::with_list_page_size(store.clone(), page_size),
    cancel,
)
.await?;
```

Log the actual listener address and whether a token is required. Remove the `rmcp::transport::sse_server` import.

- [ ] **Step 10: Build the HTTP/1.0 test client**

In `tests/sse.rs`, add local helpers (do not add a dev-dependency): connect with `TcpStream`, send
requests with `HTTP/1.0`, read the status line and headers, and read SSE events line by line. HTTP/1.0
has no chunked encoding, so the body is read until EOF and no chunk parser is needed.

Helpers:

```rust
async fn read_head(reader: &mut BufReader<TcpStream>) -> (u16, Vec<(String, String)>);
async fn get_sse(addr: SocketAddr, host: &str, origin: Option<&str>, token: Option<&str>)
    -> (u16, BufReader<TcpStream>);
async fn post(addr: SocketAddr, path: &str, host: &str, origin: Option<&str>, token: Option<&str>, body: &str) -> u16;
async fn next_event(reader: &mut BufReader<TcpStream>) -> (Option<String>, Option<String>);
async fn response_for(reader: &mut BufReader<TcpStream>, id: u64) -> serde_json::Value;
```

`response_for` skips notifications and keep-alive comments until a JSON object with the requested
`id` appears. Wrap every read in `tokio::time::timeout(Duration::from_secs(5), ...)`.

- [ ] **Step 11: Write the rejection and handshake tests**

Tests:

- `foreign_host_and_origin_are_rejected`: `Host: evil.example` on GET and POST → 403;
  `Origin: https://evil.example` with a valid Host → 403; `Origin: null` → 403; absent Origin →
  accepted.
- `token_is_required_on_both_routes`: server with `sse_token = Some(Secret::new("t"))`; GET and
  POST without/with a wrong token → 401; with `Bearer t` → accepted.
- `proxy_style_origin_matches_a_host_without_a_port`: `sse_allowed_hosts = ["mcp.example"]`,
  `Host: mcp.example`, `Origin: https://mcp.example` → 200.
- `initialize_and_a_tool_call_work_over_the_event_stream`: open SSE, read the endpoint event, POST
  `initialize` (id 1), read until id 1, POST `notifications/initialized`, POST `tools/call` for
  `add` `{a:2,b:3}` (id 2), assert the result text is `2 + 3 = 5`.
- `sessions_are_capped`: `sse_max_sessions = 1`; first GET 200, second GET 503.
- `a_disconnected_stream_releases_its_session`: keep-alive set to 50 ms via
  `Settings::with_keep_alive`; open SSE, read endpoint, drop the stream, then poll POST to the old
  session id until 404 (deadline 5 s).

- [ ] **Step 12: Run the SSE tests**

Run: `cargo test --test sse`
Expected: PASS. If the handshake stalls, check the endpoint event data and that the POST target is
`/message?sessionId=<id>`.

- [ ] **Step 13: Run the whole protocol suite and commit**

Run: `cargo test --test stdio_protocol --test mcp_features --test sse`
Then:

```bash
git add Cargo.toml Cargo.lock src/sse.rs src/lib.rs src/main.rs tests/sse.rs
git commit -m "feat: serve SSE through a validating transport"
```

---

### Task 8: Documentation and final verification

**Files:**
- Modify: `SECURITY.md`, `README.md`, `AGENTS.md`, `config.example.toml`

**Interfaces:**
- Consumes: everything above.

- [ ] **Step 1: Rewrite `SECURITY.md`**

Replace the GitHub template with:

- supported versions: latest 0.x only;
- reporting: GitHub private security advisories at
  `https://github.com/iv-istvan-varga/poc-rust-mcp/security/advisories/new`;
- threat model: stdio is local/trusted; SSE is network-exposed; loopback without a token is
  local-trust; non-loopback requires token + hosts;
- what is enforced: Host/Origin allowlists, header-only bearer token, constant-time compare,
  bounded sessions/queues/subscriptions/writes/history, owner-only SQLite files;
- accepted risks from the spec, each with its rationale and the mitigation in place;
- the audit-ignore policy and where the file lives.

- [ ] **Step 2: Update `README.md`**

Add an "SSE security" section and update the configuration table:

- `MCP_SSE_TOKEN` / `server.sse_token` — header-only, no CLI flag;
- `MCP_SSE_ALLOWED_HOSTS` / `server.sse_allowed_hosts` — non-loopback requires it; entries without
  a port match any port;
- `MCP_SSE_ALLOWED_ORIGINS` / `server.sse_allowed_origins` — restriction only, no CORS;
- `MCP_SSE_MAX_SESSIONS` / `server.sse_max_sessions`;
- a reverse-proxy note: forward the original `Host` header; do not rely on `X-Forwarded-Host`;
- retention: `max_history_rows` now caps stored rows, not just reads;
- SQLite file mode and how to tighten an existing file (`chmod 600`).

- [ ] **Step 3: Update `AGENTS.md`**

Add:

- `src/sse.rs` and its invariants: actual listener is authoritative; validation order
  Host → Origin → token; header-only token; non-loopback requires token + hosts; session cleanup
  on stream drop; no CORS.
- sqlx 0.9: `AssertSqlSafe` for dynamic SQL; `sqlite-bundled` rather than `sqlite`.
- Retention and the new bounds.
- `cargo audit` and `.cargo/audit.toml`; the three accepted advisories.
- `tests/sse.rs` and the HTTP/1.0 client rationale.

- [ ] **Step 4: Run the full verification**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build
cargo audit
```

Expected: all exit 0. Fix any drift inline.

- [ ] **Step 5: Commit**

```bash
git add SECURITY.md README.md AGENTS.md config.example.toml
git commit -m "docs: document the hardened SSE transport and accepted risks"
```
