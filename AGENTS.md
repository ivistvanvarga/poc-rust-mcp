# AGENTS.md

## Project

Crate `poc-rust-mcp` (edition 2024) with **both** a lib and a bin target:

- `src/lib.rs` — exposes `pub mod config`, `db`, `prompts`, `resources` and `server`, so every test in
  `tests/` can drive the server's public surface directly.
- `src/main.rs` — thin transport wiring (tracing, stdio/SSE), imports the lib rather than declaring
  `mod` items itself; all argument parsing lives in `config`.
- `src/config.rs` — layered configuration, `src/server.rs` — the `Calculator`: tools, `ServerHandler`,
  peer/subscriptions/log level/page size. `src/prompts.rs` — the prompt catalogue.
  `src/resources.rs` — resource catalogue, URI routing, reads from the store.
  `src/db.rs` — all database access, `migrations/{postgres,mysql,sqlite}/` — per-dialect sqlx
  migrations, `config.example.toml` — the documented example (copy to the git-ignored `config.toml`).

## The `ServerHandler` impl block is NOT annotated `#[tool(tool_box)]`

Everything else in this file follows from this, so read it before touching `src/server.rs`.

`#[tool(tool_box)]` on an `impl ServerHandler` block **unconditionally generates `list_tools` and
`call_tool`**. It does not check whether they already exist, so hand-writing them alongside it is a
duplicate-definition error. The inherent `impl Calculator` block keeps the annotation (that is what
builds the static `tool_box()`), and the handler block does not:

```rust
#[tool(tool_box)]
impl Calculator { /* tools; generates fn tool_box() */ }

impl ServerHandler for Calculator {
    async fn list_tools(&self, params, context) -> … { /* written out: sorts + paginates */ }
    async fn call_tool(&self, request, context) -> … { /* written out: the macro's body */ }
}
```

Both are written out because `list_tools` needs two things the generated one cannot do: sort the
`HashMap`-backed tool box so clients see a stable order, and paginate. `call_tool` is exactly the
macro's body, restated so the pair stays visibly together — **if this ever compiles without a
`list_tools`, every tool is silently invisible.** Keep them adjacent and keep them both.

## Server capabilities: advertise only what is implemented

`get_info` enables `tools`, `prompts`, `resources`, `resources_subscribe` and `logging`, and nothing
else. `ServerCapabilities::builder()` will happily build flags for features this crate does not have,
so the list is a claim, and a false one costs a client a feature that then answers `method_not_found`:

- **No `*ListChanged`.** The tool, prompt and resource catalogues are all static. Advertising
  `listChanged` would make clients poll for a change that cannot happen.
- **No `sampling`, `roots` or `elicitation`.** Those are *client* capabilities.
- **Completions have no capability flag at all** in the spec, so there is nothing to set.

`tests/stdio_protocol.rs` asserts both directions: each expected key is present, and
`tools.listChanged` is absent.

## rmcp 0.1.5 bugs and API limits you will rediscover the hard way

- **`ResourceContents` serialises `mime_type`, not `mimeType`.** Its `rename_all = "camelCase"` sits on
  the *enum*, which renames the variants but not the fields inside them. So any media type set on a
  `resources/read` result goes on the wire under a key that is not in the MCP schema. The server
  therefore omits the field (it is optional in the spec) instead of emitting it wrongly;
  `resources/list` still carries a correct `mimeType`, because `RawResource` is a *struct*, whose
  `rename_all` does apply. `tests/mcp_features.rs` pins this so an rmcp upgrade flags it.
- **Tool annotations, `outputSchema`/`structuredContent`, icons and resource links do not exist on
  this pin** (they are 2025-03-26 / 2025-06-18). Do not add code that expects them.
- **rmcp cancels the request `CancellationToken` itself** when `notifications/cancelled` arrives
  (`service.rs`, before the handler hook), so `on_cancelled` needs to do nothing. `on_progress` has
  nothing to report either: every tool is one atomic call returning its whole result. Both are
  implemented as explicit no-ops so the decision is on record, and the tests assert the server does
  not reply to or break on such notifications.
- **`LoggingLevel` derives `PartialEq` but not `Ord`**, so `server.rs` has a `rank()` helper to order
  them. Do not try to derive it on the SDK type.

## Pushed notifications cannot be asserted with a timeout

Two things make this subtle, and both bit while writing `tests/mcp_features.rs`:

1. **A notification can arrive before the response that caused it.** It is sent from a detached task,
   so a reader waiting for `id = N` may read it first. `tests/support/mod.rs` **buffers**
   notifications rather than skipping them (`Server::pending`, `buffered`, `notification_containing`).
   Skipping them loses messages and makes tests wait for one that already arrived.
2. **No single notification's arrival proves the stream is drained.** Each write announces from its own
   detached task, so two writes interleave freely and a whole batch can still be unscheduled. A test
   that checks "nothing was pushed" right after a write therefore passes even when something *was* —
   it was merely late. `Server::quiesce` drives harmless round-trips until the server stops pushing.

Ordering *within* one `announce_write` is fixed (log line, then each invalidated resource, on one FIFO
sink), and only that ordering may be relied on. Never assert an order *across* writes.

Each assertion like this was checked against a deliberately broken implementation. The four that
matter: dropping the subscription filter, announcing a resource the write did not invalidate, ignoring
`logging/setLevel`, and announcing regardless of subscription.

## Pagination needs a configurable page size to be testable

Every catalogue here fits on a default page, so a cursor bug would be unreachable by a test. Hence
`server.list_page_size` (`MCP_LIST_PAGE_SIZE`, default 20, must be ≥ 1) and
`tests/mcp_features.rs` setting it to 3. `server.rs::page` holds the rule: the cursor is the index of
the page's first item, decimal-encoded; an unparsable cursor is `invalid_params` rather than a silent
full list, and a cursor past the end is an empty final page rather than an error, so a plain loop
terminates.

## One URL, three backends

The URL's **scheme** picks the backend and nothing else is configured:

| Scheme | `Dialect` | sqlx driver |
| --- | --- | --- |
| `postgres://`, `postgresql://` | `Dialect::Postgres` | `PgPool` |
| `mysql://`, `mariadb://` | `Dialect::MySql` | `MySqlPool` |
| `sqlite://`, `sqlite::memory:` | `Dialect::Sqlite` | `SqlitePool` |

### Do NOT reach for `sqlx::Any`

The obvious way to hold "whichever pool" is `AnyPool`, and it does not work: `AnyValueKind` only
covers null/bool/smallint/integer/bigint/real/double/text/blob, and `Any`'s Postgres backend maps
`PgType` onto those kinds with a `match` — `Json`, `Jsonb` and every timestamp type hit the catch-all
arm and error out (`Any driver does not support the Postgres type …`). `HistoryEntry` has a JSON
column and a `TIMESTAMPTZ`, so it cannot be read back through `Any` at all.

Hence the private `enum Pool` plus the `with_pool!` macro in `db.rs`: three concrete pool types, one
block of query code instantiated three times. Only the *pool type* is abstracted; everything that
actually differs between backends (SQL text, migrations) is chosen from the `Dialect` **before** the
macro, so nothing inside a block branches on the backend again.

Two consequences to remember:

- Each arm needs one return type, and `PgQueryResult`/`MySqlQueryResult`/`SqliteQueryResult` are three
  unrelated types. Normalise inside the block (`.map(|r| r.rows_affected())`, `.map(|_| ())`).
- Placeholders are the only SQL syntax that differs, and `Dialect::placeholder` owns that: `$1`..`$5`
  for PostgreSQL, `?` for MySQL and SQLite. `Dialect::insert_sql`/`list_sql` build the text with
  `format!`; the remaining SQL is backend-independent.
- `with_pool!` matches `|$pool:ident| $body:block` — a `block` fragment is the only one that may be
  repeated, which is the whole trick. A bare expression does not match; wrap it in braces.

### Migrations are per dialect, embedded at compile time

`migrations/0001_calc_history.sql` became `migrations/postgres/0001_calc_history.sql` **byte for
byte**, because sqlx stores that file's SHA-384 as the migration checksum. Verify before touching it:

```bash
sha384sum migrations/postgres/0001_calc_history.sql
podman exec <pg-container> psql -U mcp -d mcp -tAc \
  "SELECT encode(checksum,'hex') FROM _sqlx_migrations WHERE version = 1"
```

Adding migration `0002_*` means adding it to **all three** directories. `Migrator` is *not* generic
in sqlx 0.8.6 — dispatch is via the `Migrate`/`MigrateDatabase` traits — which is what lets one `db.rs`
carry three sets:

```rust
static POSTGRES_MIGRATIONS: Migrator = sqlx::migrate!("./migrations/postgres");
```

The macros embed the SQL with `include_str!` at build time, so there is still no filesystem access at
runtime and no `DATABASE_URL`-dependent build step. `migrations/` must contain **no** top-level
`.sql`: `sqlx::migrate!("./migrations")` would silently resolve to zero migrations.

MySQL notes: 8.0.16+/MariaDB 10.2.1+ are required, for the enforced `CHECK` constraint and for `JSON`
columns. MySQL has no `CREATE INDEX IF NOT EXISTS` and its DDL is non-transactional, so a partial
failure is recovered by the `CREATE TABLE IF NOT EXISTS` on the retry being a no-op.

### `created_at` is written by the server, on purpose

Every `0001_*` migration has `created_at NOT NULL`, but the `INSERT` binds `Utc::now()` rather than
relying on the column default, and SQLite has **no** `DEFAULT` at all. This is not redundancy:

- SQLite's `CURRENT_TIMESTAMP` writes `2026-10-03 12:00:00`, whose space separator sorts **before** the
  `T` of RFC 3339. Mixing the two silently corrupts `ORDER BY created_at DESC`, because SQLite
  compares TEXT lexicographically.
- MySQL's `TIMESTAMP` converts between the session time zone and UTC on the way in *and* out.
  `DATETIME` does not, so the schema uses `DATETIME(6)`.

Ties are real (both defaults have second resolution), which is why the query is
`ORDER BY created_at DESC, id DESC` — keep both keys.

### SQLite specifics worth knowing

- `SqliteConnectOptions` exposes no getter for "in-memory" or "read-only", so `sqlite_url_parts` /
  `sqlite_param` re-parse the URL exactly as sqlx does. Both are unit-tested in `db.rs`.
- An **in-memory** store is forced to `max_connections(1)`. SQLite's shared-cache mode takes
  *table*-level locks that `busy_timeout` cannot wait out, so a larger pool turns a contended write
  into a failed one. A *file* store keeps the configured pool size — which also means the migration
  race described below only reproduces on a file database.
- `create_if_missing(true)` unless the URL pins `mode=ro`/`mode=rw`, so `sqlite://mcp.db` works on a
  fresh checkout but a read-only URL never conjures a database behind the user's back.
- `busy_timeout` is tied to `acquire_timeout`; sqlx's own default is 5s, which would outlast the
  budget this store promises callers.

### TLS

`Pool::connect` deliberately does **not** touch `ssl_mode` on any driver — the URL and the driver's
own default decide. sqlx is built here without a TLS feature, and both `PgSslMode::Prefer` and
`MySqlSslMode::Preferred` check `tls::available()` first and stay on plaintext when it is false (see
`sqlx-postgres/src/connection/tls.rs`, `sqlx-mysql/src/connection/tls.rs`), so local containers keep
working and enabling a TLS feature later upgrades automatically. A managed database that *requires*
TLS needs `?sslmode=require` / `?ssl-mode=REQUIRED` in the URL.

### `Store::schema` MUST be `Arc<OnceCell<()>>`

A real bug that stayed latent while storage was PostgreSQL-only, so it is easy to reintroduce.
`tokio::sync::OnceCell` implements `Clone` by **copying the value if set and otherwise producing a
fresh, empty cell**. `record_success` hands `self.clone()` to a `tokio::spawn`, so with a per-store
cell the detached task ran its *own* migration alongside the caller's. PostgreSQL hid this behind
`Migrator::lock`'s advisory lock; SQLite's `Migrate::lock` is a literal no-op, so both runs read an
empty `_sqlx_migrations` and the loser failed with `UNIQUE constraint failed: _sqlx_migrations.version`
— surfacing as a silently dropped history row.

`db::tests::clones_share_one_schema_cell` guards it, and it clones *before* the first migration on
purpose: a clone taken afterwards copies the value and passes either way. Keep the `Arc` for the same
reason `breaker` has one.

## rmcp is pinned to 0.1 — the 0.1 API is not the 3.x API

`rmcp = "0.1"` resolves to **0.1.5**. Do not copy 3.x examples (docs.rs "latest" shows 3.5):

- 0.1 has **no `#[tool_router]` / `#[tool_handler]` macros** and no `ToolRouter` type.
- Tools are declared with `#[tool]` + a `#[tool_box]` impl block. On the **inherent** `impl Calculator`
  that annotation builds the static tool box; on `impl ServerHandler for Calculator` it would generate
  `list_tools`/`call_tool` — but this crate writes those out instead, see above.
- Per-arg schema comes from `#[tool(param)]`; complex input from `#[tool(aggr)]` on a single
  struct deriving `serde::Deserialize` + `schemars::JsonSchema` (via `rmcp::schemars`).
- A tool returns anything implementing `IntoContents` (`String`, `Content`, `()`), or
  `Result<T, E>` with both sides `IntoContents` — `Err` becomes `isError: true`.

## stdout is the protocol channel

`stdout` carries JSON-RPC in stdio mode. `tracing_subscriber::fmt()` **defaults to stdout** and
silently corrupts the stream, so `init_tracing` must keep `.with_writer(std::io::stderr)`.
Everything else (help text, diagnostics) must stay off stdout in stdio mode. The same applies to
the *client* side of a hand-rolled smoke test: one stray non-JSON line on the server's stdin
desynchronises the stream and silently drops responses.

## Run modes

```bash
cargo run                      # stdio (default) — what MCP clients spawn
cargo run -- --sse 127.0.0.1:8000   # HTTP+SSE, endpoints GET /sse, POST /message?sessionId=…
RUST_LOG=debug cargo run        # logs go to stderr; default filter is info
cargo run -- --config config.toml     # TOML config file (also MCP_CONFIG)
cargo run -- --db-url postgres://…    # beats DATABASE_URL
cargo run -- --db-url mysql://…       # same flag, different backend
cargo run -- --db-url sqlite://mcp.db # …including SQLite, which needs no server
```

`--help` prints the full flag/environment/config-file list. Tracing is initialised *after*
`Config::resolve`, because the filter is configuration; `main` prints failures with `eprintln!` and
the whole `anyhow` error chain, since `Display` alone is only the outermost context.

Smoke-test stdio without a client. **Build first, then keep stdin open ~1s**: a cold `cargo run`
compiles while stdin is already at EOF, and rmcp 0.1.5 shuts down and drops in-flight responses.

```bash
cargo build
{ printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"smoke","version":"0.0.1"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'; sleep 1; } \
  | RUST_LOG=warn ./target/debug/poc-rust-mcp
```

`scripts/verify-backend.sh <url>` is the same idea plus `db_status`/`calc_history`/
`clear_calc_history`, for the manual checks a live PostgreSQL or MySQL needs. Responses may arrive
**out of id order** — history writes are fire-and-forget, so a tool result can beat a later tool
result. That is not a bug; do not "fix" it by awaiting the write.

## ServerInfo defaults are wrong for us

`rmcp`'s `ServerInfo::default()` reports `serverInfo` as `rmcp` + the *SDK's* version and
advertises **no** capabilities (`"capabilities":{}`), so a client sees zero tools. `get_info` in
`src/server.rs` must set `server_info` (from `CARGO_PKG_NAME`/`CARGO_PKG_VERSION`) and the
capabilities explicitly — and see "Server capabilities: advertise only what is implemented" for why
that list is short and deliberate. `instructions` is where the client learns that history exists;
`tests/stdio_protocol.rs` asserts it still mentions `DATABASE_URL`.

## Configuration is layered, and the file layer is optional

Precedence is **flag > environment > file > default**, resolved once at startup by
`Config::resolve`. Full key/env/flag table in the README and in `config.example.toml`.

- **A config file is never required and never discovered.** It is read only when `--config` or
  `MCP_CONFIG` names it. Do not add implicit search paths: behaviour that depends on a stray file in
  the working directory is not debuggable.
- The file is deserialised into a *sparse* mirror where every field is `Option`, and only keys
  actually present overwrite the layer below. That is why a partial file does not zero anything out.
- `#[serde(deny_unknown_fields)]` is load-bearing: a misspelled key must fail at startup instead of
  silently doing nothing. Keep it.
- Durations are integer **milliseconds** (`acquire_timeout_ms`, `breaker_cooldown_ms`) because TOML
  has no duration type.
- `Store::from_settings(&config.storage)` is the only way the binary builds a `Store`;
  `Store::from_env` is gone, but `DATABASE_URL` still works because it is an env-layer key.
- Bad values are rejected in `Config::validate` — bad address, bad filter, zero timeouts, zero
  thresholds — so a typo fails before the transport opens rather than halfway through serving.
- Tests must **not** use `unsafe { std::env::set_var }` (edition 2024). `Config::resolve` takes
  `&dyn Env`, and `MapEnv` injects values for tests. That is why the trait exists.

## Storage

All SQL lives in `src/db.rs`; tools never touch sqlx types. The limits are no longer constants —
they come from `config::StorageConfig` via `db::Limits`.

- **Missing/unparsable/unsupported URL ⇒ storage disabled.** The server still starts and every
  arithmetic tool still works; storage tools return an error content. This is deliberate. An unknown
  *scheme* is not a guess: `Dialect::from_url` names the accepted schemes in the warning, so
  `mysqls://` reads as a typo rather than as "storage is merely off".
- **Migrations are applied lazily**, on the first storage call, and retried until they succeed —
  never at startup. Add a new numbered file to **all three** `migrations/*/` directories; never edit
  an applied one (`VersionMismatch`, checked by SHA-384).
- **History writes are fire-and-forget** (`tokio::spawn` in `record_success`/`record_failure`) so
  arithmetic tools never block on the database. Consequence: a row can be lost if the process exits
  immediately after the tool returns. Do not "fix" this by awaiting the insert.
- **Circuit breaker** (`Store::breaker`): after 2 *connectivity* failures storage is skipped for
  30s, and each attempt is bounded by a 3s acquire timeout. These are `Limits` fields sourced from
  configuration, not constants. Only connectivity errors may trip it — `is_connectivity_error`
  deliberately excludes
  query-level errors so a bad query cannot disable storage. The check is repeated *inside* the
  migration `OnceCell` so callers already queued behind it fail fast instead of each retrying.
- `clear_calc_history` uses `DELETE`, **not** `TRUNCATE`: `TRUNCATE` reports no row count on
  PostgreSQL or MySQL, so `rows_affected` would always be 0 and the tool would report a lie.
- sqlx runs with `default-features = false` and **no compile-time checked queries** — `query_as` plus
  `#[derive(FromRow)]`. That is intentional: `query!` would require `DATABASE_URL` or a committed
  `.sqlx` offline cache at build time. Adding a new sqlx capability may mean enabling another
  feature (`json` is required for `serde_json::Value` columns — including on SQLite, where JSON is
  just TEXT; `chrono` for `DateTime<Utc>`; `sqlite` is **bundled**, so it builds SQLite from C source
  and needs a C compiler).

## Devcontainer (podman-compose)

```bash
podman-compose -f .devcontainer/compose.yaml up -d      # db + mysql + app
podman-compose -f .devcontainer/compose.yaml exec app cargo test
podman-compose -f .devcontainer/compose.yaml exec db psql -U mcp -d mcp
podman-compose -f .devcontainer/compose.yaml exec mysql mysql -u mcp -pmcp mcp
podman-compose -f .devcontainer/compose.yaml down -v     # -v also drops the pgdata/mysqldata volumes
```

- `db` is `mcp`/`mcp`/`mcp` on host port 5432 (data in `pgdata`); `mysql` is the same on host port
  3306 (data in `mysqldata`). SQLite needs no service at all.
- `depends_on` does **not** wait for either database to accept connections — podman-compose does not
  honour health conditions, and the app must not block on startup anyway (see lazy migration).
- `.devcontainer/Dockerfile` exists for exactly one reason: the base Rust image sets `PATH` via
  ENV, but Debian's `/etc/profile` reassigns `PATH` for login shells, so `cargo` disappears in
  `bash -l` and in devcontainer terminals. The fix must live in `/etc/profile.d`.
- The `:z` on the `/workspace` bind mount is required on SELinux hosts; without it the container is
  denied access to the mounted repo.
- The compose `app` service sets `DATABASE_URL` for `db`, which still works: it is an env-layer key
  with no config file involved. Edit that one value to point at `mysql://mcp:mcp@mysql:3306/mcp`.
- `rust-toolchain.toml` pins `stable`, so the container rustup-downloads the current stable on
  first use even though the image ships 1.98.1. Version drift between host and container is
  expected; both build clean.

## Tests

**Every test lives in `tests/`. There is no `#[cfg(test)]` module in `src/`** — do not add one back.
Each file is one layer, and each assertion lives in exactly one of them; do not duplicate.

- `tests/stdio_protocol.rs` spawns the **real binary** over stdio and speaks raw JSON-RPC. Protocol
  mechanics: capabilities (and the ones deliberately *not* advertised), `ping`, `tools/list`, argument
  validation, tool errors vs JSON-RPC errors, stdout purity, and that progress/cancellation
  notifications leave the server working.
- `tests/mcp_features.rs` covers every feature *beyond* tools through the same transport: the prompt
  catalogue and its validation, resource routing and degradation, subscription announcements, the
  logging level filter, completion, and a cursor walk over all four paginated methods. A handler can be
  perfectly correct and never routed, which is why these are not unit tests.
- `tests/server.rs` drives the `Calculator` and its `ServerHandler` hooks directly, where a failure
  points at one function. Tools are called through the **public `ServerHandler::call_tool`**, because
  rmcp's `#[tool]` macro keeps them private and `server::testing` deliberately does not widen that.
- `tests/prompts.rs` / `tests/resources.rs` treat the two catalogues as pure functions: rendering,
  argument validation, URI routing, and reads against an in-memory SQLite store.
- `tests/store_offline.rs` exercises the public `Store`/`HistoryEntry` surface **without** a live
  database: scheme→backend selection, disabled storage, unparsable and unsupported URLs, bounded
  failure, breaker tripping, fire-and-forget writes. Runs against a dead PostgreSQL *and* a dead
  MySQL URL, so the degradation guarantees are not proved for one driver only.
- `tests/sqlite_backend.rs` is the only layer that needs a **real, working** database, and SQLite is
  the only one that can be: embedded, in-process, nothing to install. It covers migrations, a JSON
  and a timestamp column surviving a round trip, newest-first ordering, `DELETE` row counts, and a
  file-backed database persisting across two stores.
- `tests/config.rs` pins the layering: precedence, defaults with no file at all, blank-value handling,
  rejected unknown keys and out-of-range values, CLI parsing, and that resolved settings and the
  resolved URL's backend reach the `Store`.
- `tests/db.rs` holds the few behaviours no public API can reach — the circuit breaker's bookkeeping,
  the shared schema cell, the per-dialect SQL and migration sets, SQLite URL parsing.

### `db::testing` and `server::testing` exist for `tests/`

Both are `#[doc(hidden)] pub mod` shims wrapping internals that are still declared `fn`. A child
module can already read its parent's private items, so **nothing outside those two modules was made
`pub`** — the rendered API is unchanged, which is why this route was chosen over widening the items
in place.

The rule that follows: if a new test needs private access, add a shim to the relevant `testing` module
rather than adding `pub` to the item. If a test can be written against the public surface, it does not
belong in `tests/db.rs` at all — put it in `tests/store_offline.rs` or `tests/sqlite_backend.rs`.

### `tests/support/mod.rs`

The shared JSON-RPC harness (not a test binary — Cargo compiles it into each of the two protocol-level
files). It carries `#![allow(dead_code)]`, because an item only one of them needs is dead code in the
other. It also exports `request_context()` for calling handlers without a transport. Read it before
changing how tests talk to the server; see "Pushed notifications cannot be asserted with a timeout".

Rules for the suite:

- `cargo test` must pass on a bare checkout with **no database running**. Point at a dead port
  (`postgres://…127.0.0.1:1/mcp`) via `Store::connect_with_timeout(url, FAST)` and assert bounded
  failure. Only the MySQL- and PostgreSQL-specific behaviour (migration `VersionMismatch`, TLS,
  recovering after the 30 s breaker cooldown) is verified manually against the devcontainer, with
  `scripts/verify-backend.sh`.
- A test that wants to see a row must **poll** for it: writes are fire-and-forget, so
  `wait_for_history` in `tests/sqlite_backend.rs` is the sanctioned way to observe one. Do not make
  the insert path awaitable just to make a test simpler.
- Anything spawning the server must set `env!("CARGO_BIN_EXE_poc-rust-mcp")`, `env_remove`
  `DATABASE_URL` so an ambient value cannot change what is under test, and `kill_on_drop(true)`. A
  test that needs another setting must `env_remove` its ambient value too, or the developer's shell
  decides what the test proves.
- Read each JSON-RPC response **before** closing stdin; rmcp 0.1.5 drops in-flight responses at EOF.
- Time budgets must be *tighter* than the behaviour they guard. The acquire timeout is 3s, so the
  "arithmetic never blocks on the database" guard uses a 1s budget — a 5s budget silently passes
  even when the fire-and-forget write has been turned back into a blocking one. The same logic
  applies to `Server::quiesce`, whose settle window (20ms) must be far longer than a scheduling turn.
- `ToolBox::list()` is backed by a `HashMap`, so the tool list is sorted by the server rather than
  reported in map order. The prompt and resource catalogues keep their declaration order, which is
  stable too — but only *within* one list, and never across two writes' notifications.
- Recovering from a tripped breaker takes 30s by design, so it is verified manually, not in the
  suite.

## Verification (run before finishing any change)

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build
```

No CI, no `[lints]` table — clippy defaults only, so `-D warnings` must pass cleanly.

## Toolchain / git

- `rust-toolchain.toml` pins `stable` + `clippy`/`rustfmt`. Edition 2024 needs Rust >= 1.85.
- `tracing-subscriber` needs `features = ["env-filter"]` for `RUST_LOG` support; it is not a default.
  `config.rs` also uses `EnvFilter::try_new` to reject a bad filter at startup.
- `Cargo.lock` is **committed** (only `/target` is ignored). Commit it whenever deps change.
- **Licensing: BSD 2-Clause, and `LICENSE` is the authoritative copy.** There are no per-file
  headers, which is the usual Rust convention — do not add one without being asked. Three places
  mention the licence and must agree: the SPDX identifier `license = "BSD-2-Clause"` in
  `Cargo.toml`, the header in `LICENSE`, and the `## License` section in `README.md`. The `LICENSE`
  text is the SPDX `BSD-2-Clause` template verbatim with only the year and copyright holder
  substituted, so do not reflow it; if the variant ever changes to 3-Clause (which adds a
  non-endorsement clause), change all three and replace the file with the SPDX `BSD-3-Clause` text.
