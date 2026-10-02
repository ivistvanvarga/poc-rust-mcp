# AGENTS.md

## Project

Single binary crate `poc-rust-mcp` (edition 2024). `src/main.rs` = CLI + transport wiring,
`src/server.rs` = the `Calculator` server and all tools, `src/db.rs` = all PostgreSQL access,
`migrations/` = sqlx migrations. There is **no lib target**, so `tests/*.rs` cannot
`use poc_rust_mcp::…` — keep tests in `#[cfg(test)]` modules.

## rmcp is pinned to 0.1 — the 0.1 API is not the 3.x API

`rmcp = "0.1"` resolves to **0.1.5**. Do not copy 3.x examples (docs.rs "latest" shows 3.5):

- 0.1 has **no `#[tool_router]` / `#[tool_handler]` macros** and no `ToolRouter` type.
- Tools are declared with `#[tool]` + a `#[tool_box]` impl block, and `ServerHandler` must be
  annotated **twice** — once on the inherent `impl Calculator` (builds the static tool box) and
  once on `impl ServerHandler for Calculator` (generates `list_tools` / `call_tool`).
  Forgetting the second one silently leaves `tools/list` empty.
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
```

Smoke-test stdio without a client. **Build first, then keep stdin open ~1s**: a cold `cargo run`
compiles while stdin is already at EOF, and rmcp 0.1.5 shuts down and drops in-flight responses.

```bash
cargo build
{ printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"smoke","version":"0.0.1"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'; sleep 1; } \
  | RUST_LOG=warn ./target/debug/poc-rust-mcp
```

## ServerInfo defaults are wrong for us

`rmcp`'s `ServerInfo::default()` reports `serverInfo` as `rmcp` + the *SDK's* version and
advertises **no** capabilities (`"capabilities":{}`), so a client sees zero tools. `get_info` in
`src/server.rs` must set `server_info` (from `CARGO_PKG_NAME`/`CARGO_PKG_VERSION`) and
`capabilities: ServerCapabilities::builder().enable_tools().build()` explicitly.

## Postgres storage

`DATABASE_URL` is the only knob. All SQL lives in `src/db.rs`; tools never touch sqlx types.

- **Missing/unparsable `DATABASE_URL` ⇒ storage disabled.** The server still starts and every
  arithmetic tool still works; storage tools return an error content. This is deliberate.
- **Migrations are applied lazily**, on the first storage call, and retried until they succeed —
  never at startup. Add a new numbered file in `migrations/`; never edit an applied one
  (`VersionMismatch`).
- **History writes are fire-and-forget** (`tokio::spawn` in `record_success`/`record_failure`) so
  arithmetic tools never block on the database. Consequence: a row can be lost if the process exits
  immediately after the tool returns. Do not "fix" this by awaiting the insert.
- **Circuit breaker** (`Store::breaker`): after 2 *connectivity* failures storage is skipped for
  30s, and each attempt is bounded by a 3s acquire timeout. Constants live at the top of
  `src/db.rs`. Only connectivity errors may trip it — `is_connectivity_error` deliberately excludes
  query-level errors so a bad query cannot disable storage. The check is repeated *inside* the
  migration `OnceCell` so callers already queued behind it fail fast instead of each retrying.
- `clear_calc_history` uses `DELETE`, **not** `TRUNCATE`: Postgres' `TRUNCATE` command tag carries
  no row count, so `rows_affected` is always 0 and the tool would report a lie.
- sqlx runs with `default-features = false` and **no compile-time checked queries** — `query_as` plus
  `#[derive(FromRow)]`. That is intentional: `query!` would require `DATABASE_URL` or a committed
  `.sqlx` offline cache at build time. Adding a new sqlx capability may mean enabling another
  feature (`json` is required for `serde_json::Value` columns).

## Devcontainer (podman-compose)

```bash
podman-compose -f .devcontainer/compose.yaml up -d      # db + app
podman-compose -f .devcontainer/compose.yaml exec app cargo test
podman-compose -f .devcontainer/compose.yaml exec db psql -U mcp -d mcp
podman-compose -f .devcontainer/compose.yaml down -v     # -v also drops the pgdata volume
```

- DB is `mcp`/`mcp`/`mcp`, published on host port 5432, data in the `pgdata` volume.
- `depends_on` does **not** wait for Postgres to accept connections — podman-compose does not
  honour health conditions, and the app must not block on startup anyway (see lazy migration).
- `.devcontainer/Dockerfile` exists for exactly one reason: the base Rust image sets `PATH` via
  ENV, but Debian's `/etc/profile` reassigns `PATH` for login shells, so `cargo` disappears in
  `bash -l` and in devcontainer terminals. The fix must live in `/etc/profile.d`.
- The `:z` on the `/workspace` bind mount is required on SELinux hosts; without it the container is
  denied access to the mounted repo.
- `rust-toolchain.toml` pins `stable`, so the container rustup-downloads the current stable on
  first use even though the image ships 1.98.1. Version drift between host and container is
  expected; both build clean.

## Tests

`ToolBox::list()` is backed by a `HashMap`, so tool order is **not deterministic** — sort before
asserting. Run one test with `cargo test tool_box_exposes_every_tool`. DB tests must not require a
live database: point at a dead port with `Store::connect_with_timeout(url, FAST)` and assert
bounded failure instead. Recovering from a tripped breaker takes 30s by design, so it is verified
manually, not in the suite.

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
- `Cargo.lock` is **committed** (only `/target` is ignored). Commit it whenever deps change.