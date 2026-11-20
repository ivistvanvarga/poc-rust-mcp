# AGENTS.md

Crate `poc-rust-mcp` (edition 2024, lib + bin). `src/main.rs` only wires transports; the real server
is the lib modules (`config`, `db`, `prompts`, `resources`, `server`, `sse`). Every test lives in
`tests/` — do not add `#[cfg(test)]` back into `src/`.

`README.md` is the source of truth for commands, the config table, MCP surface, storage behaviour
and the test-layer table; `SECURITY.md` holds the threat model and accepted risks. This file is only
the traps an agent would otherwise reintroduce.

## rmcp 3.10 is pinned; protocol ceiling is 2025-11-25

- Tools are `#[tool]` methods in a `#[tool_router]` inherent `impl Calculator`; scalars and structs
  alike are wrapped in `Parameters<T>` (`T: Deserialize + schemars::JsonSchema`). A tool returns
  `IntoCallToolResult` (`String`, `Content`, `()`) or `Result<T, E>`; `Err` becomes `isError: true`.
- **`impl ServerHandler for Calculator` carries `#[tool_handler]`.** It generates only the methods
  the block does *not* define (here, `get_tool`), so the hand-written `list_tools`, `call_tool` and
  `get_info` stay. Keep `list_tools`: it sorts the `HashMap`-backed router and paginates. Keep
  `call_tool`: it stores `context.peer.clone()` for detached notification tasks, and pre-validates
  arguments through `validate_tool_arguments` so a wrong JSON type stays a JSON-RPC
  `invalid_params` instead of becoming a tool error (3.x's router rewrites it otherwise).
- Errors are `rmcp::ErrorData` (aliased `McpError`) — there is no crate-root `rmcp::McpError`. Many
  result types are `#[non_exhaustive]`: use builders (`ListToolsResult::with_all_items`,
  `ReadResourceResult::new`, `GetPromptResult::new`, `CompleteResult::new`, `ServerConfig::new`).
  `CallToolResponse`/`GetPromptResponse`/`ReadResourceResponse` are wrapper enums; tests unwrap
  `Complete` before serialising.
- `get_info` builds `ServerConfig` explicitly with `ProtocolVersion::V_2025_11_25`, and
  `supported_protocol_versions()` caps negotiation at `known_up_to(V_2025_11_25)`. 2026-07-28
  removes `initialize`/`ping` and changes subscriptions — do not advertise it. The blanket
  `discover` already rejects a `server/discover` opener for a version we do not advertise, so no
  override is needed. Advertise only what exists: no `*ListChanged`, no
  `sampling`/`roots`/`elicitation` (client capabilities); completions have no flag at all.
  `tests/stdio_protocol.rs` asserts both presence and the absence of `tools.listChanged`. The test
  request version stays `2024-11-05`; the server negotiates it from the advertised range.
- `ResourceContents::text(...)` now serialises `mimeType` correctly (the 0.1 enum-level `rename_all`
  bug is gone); the server emits it on reads and `tests/mcp_features.rs` pins the wire key.
- `LoggingLevel` is `Copy` and `PartialEq` but still not `Ord` — use `server.rs::rank()`, do not
  derive. It, the logging capability and `subscribe`/`unsubscribe` are deprecated by SEP-2577 and
  SEP-2567 but are the live mechanisms at this protocol ceiling: the module-level
  `#[allow(deprecated)]` in `src/server.rs` (and in the two tests naming them) is deliberate.
  `on_cancelled` and `on_progress` take a `NotificationContext<RoleServer>` and are deliberate
  no-ops (rmcp cancels the token before the hook; tools are atomic); tests assert the server neither
  replies to nor breaks on those notifications.
- Tool annotations, `outputSchema`/`structuredContent`, icons and resource links exist in 3.10 but
  are deliberately unused — the wire contract stays the 0.1 shape. Do not add them without a spec
  change.

## stdout is the protocol channel

`tracing_subscriber::fmt()` defaults to stdout; `init_tracing` must keep `.with_writer(std::io::stderr)`.
Nothing else may print to stdout in stdio mode, including the client side of a smoke test — one stray
line desynchronises the stream. Build first and keep stdin open ~1 s: a cold `cargo run` compiles past
EOF and rmcp drops in-flight responses. `scripts/verify-backend.sh` is the live-database variant.
Its responses can arrive out of id order because history writes are fire-and-forget; do not "fix"
that by awaiting them.

## Storage: three concrete pools, never `sqlx::Any`

- `Any` cannot carry `HistoryEntry`: its Postgres mapping of `Json`/`Jsonb` and every timestamp hits
  a catch-all and errors. `db.rs` uses a private `enum Pool` plus `with_pool!` to instantiate one
  query block for `PgPool`/`MySqlPool`/`SqlitePool`. Choose SQL and migrations from `Dialect`
  *before* the macro; inside it, normalise the three unrelated `QueryResult` types
  (`rows_affected()`, `()`) and get placeholders from `Dialect::placeholder`. `with_pool!` matches
  `|$pool:ident| $body:block` — pass a block, not a bare expression.
- `Store::schema` **must** be `Arc<OnceCell<()>>`. `tokio::sync::OnceCell::clone` yields a fresh empty
  cell when unset, and `record_success` clones the store into a spawned task; a per-store cell makes
  that task run its own migration. PostgreSQL hides this behind its advisory lock, SQLite has none,
  and the loser fails with `UNIQUE constraint failed: _sqlx_migrations.version` — a silently dropped
  row. `tests/db.rs::clones_share_one_schema_cell` clones *before* the first migration on purpose.
- Every SQL string from `Dialect::*_sql()` needs `sqlx::AssertSqlSafe(...)` at the call site (sqlx
  0.9's `SqlSafeStr` is native only for `&'static str`). No `query!` — it would need `DATABASE_URL`
  at build time. Features are deliberate: `json` even on SQLite, `chrono`, `sqlite-bundled` (not
  `sqlite`).
- `Pool::connect` deliberately never sets `ssl_mode`; the URL and driver default decide, and sqlx is
  built without TLS. Do not "help" by forcing one.
- `created_at` is written by the server (`Utc::now()`), never by a column default; SQLite has no
  `DEFAULT` at all. `CURRENT_TIMESTAMP`'s space separator sorts before RFC 3339's `T`, and MySQL
  `TIMESTAMP` shifts with the session zone (`DATETIME(6)` does not). Keep
  `ORDER BY created_at DESC, id DESC`.
- SQLite has no getter for in-memory/read-only, so `sqlite_url_parts`/`sqlite_param` re-parse the URL
  the way sqlx does. In-memory stores are forced to `max_connections(1)` (shared-cache table locks
  ignore `busy_timeout`); file stores keep the configured pool, which is why the migration race only
  shows up on a file. `busy_timeout` is tied to `acquire_timeout`.
- Migrations are embedded per dialect. Keep `migrations/` free of top-level `.sql`
  (`sqlx::migrate!("./migrations")` would silently embed zero), add every new numbered file to all
  three directories, and never edit an applied one or add even a comment header — the SHA-384
  checksum breaks deployed databases with `VersionMismatch`. MySQL has no `CREATE INDEX IF NOT
  EXISTS` and its DDL is non-transactional; the retry's `CREATE TABLE IF NOT EXISTS` is the recovery.
- `is_connectivity_error` deliberately excludes query-level errors so a bad query cannot trip the
  breaker. All limits come from `StorageConfig`, not constants.

## Config and SSE traps

- `Config::resolve(&cli, &SystemEnv)` runs once at startup; tests inject via `MapEnv` through
  `&dyn Env` — never `unsafe { std::env::set_var }` (edition 2024). The file layer is a sparse mirror
  of `Option`s with `#[serde(deny_unknown_fields)]`: keep both, so a partial file cannot zero settings
  and a typo fails at startup. No implicit config-file discovery. `Store::from_settings` is the
  binary's only `Store` constructor; `DATABASE_URL` works only because it is an env-layer key. Trace
  init comes after resolve; `EnvFilter::try_new` rejects bad filters at startup.
- `src/sse.rs` replaces rmcp's transports, which do no `Host`/`Origin`/token validation. Validation
  order is authority → `Host` → `Origin` → token (403 for host/origin, 401 for token); the token is
  header-only and constant-time, with deliberately no CLI flag. A non-loopback bind needs both a
  token and explicit `sse_allowed_hosts`; `serve` rechecks against `listener.local_addr()` because it
  is authoritative.
  `Origin` is a restriction, not CORS — with no allowlist it must match the validated `Host` (and the
  effective port when the authority carried one). Two independent `SessionCleanup` guards (stream
  and transport) make removal/cancellation prompt and idempotent; keep them separate. The full-queue
  `429` branch is testable only through `sse::testing` because the service drains the queue as fast
  as it fills.
- `tests/sse.rs` uses a hand-rolled HTTP/1.0 client over `TcpStream` so bodies end at EOF (no chunked
  parser, no HTTP dev-dependency). Keep it that way.

## Writing tests

- Protocol tests share the harness in `tests/support/mod.rs` (`#![allow(dead_code)]`, since each
  protocol binary uses only part of it). Spawned servers must use
  `env!("CARGO_BIN_EXE_poc-rust-mcp")`, `env_remove` every ambient setting under test
  (`DATABASE_URL` at minimum) and `kill_on_drop(true)`; read each response before closing stdin.
- Pushed notifications arrive from detached tasks. The harness **buffers** them (`buffered`,
  `buffered_notifications`, `notification_containing`) instead of skipping, and `quiesce` drives
  harmless round-trips until pushing stops: a timeout cannot prove "nothing was pushed", and one
  notification cannot prove the stream drained. Only ordering within one `announce_write` is fixed
  (log line, then each invalidated resource); never assert across writes. `SETTLE` is 20 ms.
- Budgets must be tighter than the behaviour: `FAST` (250 ms) in `tests/store_offline.rs` against
  the 3 s acquire timeout. A looser budget silently passes when fire-and-forget becomes blocking.
  Tests that need a row poll via `wait_for_history`; do not make inserts awaitable for a test.
- Every catalogue fits a default page, so `tests/mcp_features.rs` sets `MCP_LIST_PAGE_SIZE=3`.
  `page()` takes a decimal index cursor: an unparsable cursor is `invalid_params`, a past-the-end
  cursor is an empty final page. `ToolRouter::list_all()` is `HashMap`-backed, so tools are sorted
  server-side (and `list_tools` sorts again to be certain).
- `db::testing` and `server::testing` are `#[doc(hidden)]` shims over still-private `fn`s — add a
  shim rather than widening visibility. Public-surface tests belong in `store_offline.rs` /
  `sqlite_backend.rs`, not `db.rs`. rmcp keeps tool methods private, so `tests/server.rs` calls them
  through `ServerHandler::call_tool`.
- `cargo test` must pass with no database running (dead ports on `127.0.0.1:1`, for both PostgreSQL
  and MySQL). Live-backend checks — migration `VersionMismatch`, TLS, 30 s breaker recovery — are
  manual via `scripts/verify-backend.sh`.
- Focus commands: `cargo test --test server`, `cargo test --test mcp_features pages_without_gaps`,
  `cargo test --test sqlite_backend`.

## Repo hygiene

- `depends_on` does not wait for the databases (podman-compose ignores health conditions) and the
  app must not block on startup. `:z` on the `/workspace` bind is required on SELinux hosts. The
  `.devcontainer/Dockerfile` exists only to restore `PATH` in login shells via `/etc/profile.d`.
  `rust-toolchain.toml` pins `stable`, so the container downloads it on first use; drift from the
  image's Rust is expected.
- Verify before finishing: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test`, `cargo build`, `cargo audit`. No CI and no `[lints]` table. One reviewed ignore lives
  in `.cargo/audit.toml`; GHSA-only advisories are a SUPERSET of RustSec and belong in `SECURITY.md`
  instead. Add an ignore only with a written rationale.
- `Cargo.lock` is committed. Every Rust file and the verification script carries
  `SPDX-License-Identifier: BSD-3-Clause` on its first line (after the shebang in the script); keep
  it in step with `Cargo.toml`, `LICENSE` and README's License section. Never add a header — not even
  a comment — to migration SQL (checksum).
