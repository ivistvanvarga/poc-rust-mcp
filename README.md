# poc-rust-mcp

A [Model Context Protocol](https://modelcontextprotocol.io) server that does arithmetic, built with
[`rmcp`](https://crates.io/crates/rmcp) and Tokio, with an **optional** PostgreSQL-backed history of
every calculation.

The point of the PoC is the storage story: the calculator tools must answer instantly whether or not
a database exists, is reachable, or is misconfigured. Storage degrades; it never blocks.

| Tool | Arguments | Returns |
| --- | --- | --- |
| `add` | `a: i32`, `b: i32` | `"2 + 40 = 42"` |
| `sub` | `a: i32`, `b: i32` | `"7 - 2 = 5"` |
| `mul` | `a: i32`, `b: i32` | `"6 * 7 = 42"` |
| `div` | `dividend: f64`, `divisor: f64` | `"9 / 2 = 4.5"`, or a tool error `division by zero` |
| `db_status` | — | `"database reachable, 128 stored calculations"` |
| `calc_history` | `limit: i64` (clamped to `1..=storage.max_history_rows`) | one line per entry, most recent first |
| `clear_calc_history` | — | `"deleted 128 recorded calculations"` |

## Requirements

- Rust `stable` ≥ 1.85 (edition 2024). A `rust-toolchain.toml` pins stable with `clippy` and
  `rustfmt`, so nothing needs installing manually.
- PostgreSQL is **optional**. Everything except the three history tools works without it.
- `podman` + `podman-compose` only if you want the devcontainer.

## Quick start

Without a database — the server still starts and all arithmetic works:

```bash
cargo run                     # stdio (default)
cargo run -- --help
```

With the devcontainer (app + PostgreSQL, `DATABASE_URL` wired to the `db` service):

```bash
podman-compose -f .devcontainer/compose.yaml up -d --build
podman-compose -f .devcontainer/compose.yaml exec app cargo run
```

PostgreSQL is published on host port `5432` as `mcp` / `mcp` / `mcp`, so `cargo run` from your host
works too:

```bash
export DATABASE_URL=postgres://mcp:mcp@127.0.0.1:5432/mcp
cargo run
```

## Configuration

Settings come from four layers. Each one overrides the one before it:

```
command-line flag  >  environment variable  >  config file  >  built-in default
```

**No config file is required.** A missing file is simply an absent layer, so a bare `cargo run`
behaves exactly as before. Nothing is discovered implicitly: to use a file you name it with
`--config` or `MCP_CONFIG`, so behaviour never depends on a file that happens to be lying around in
the working directory.

```bash
cargo run -- --config config.example.toml   # explicit
MCP_CONFIG=config.example.toml cargo run    # via environment
```

Copy `config.example.toml` to `config.toml` (git-ignored) and edit it. Every key is optional, unknown
keys are rejected rather than ignored, and durations are integer milliseconds because TOML has no
duration type.

### Settings

| Setting | Flag | Environment | Default | Meaning |
| --- | --- | --- | --- | --- |
| — | `--config <PATH>` | `MCP_CONFIG` | none | TOML config file to load |
| `server.sse_address` | `--sse [ADDR]` | `MCP_SSE_ADDRESS` | `127.0.0.1:8000` | Bind address for the SSE transport |
| `server.log_filter` | — | `RUST_LOG` | `info` | `tracing` filter; logs always go to **stderr** |
| `storage.url` | `--db-url <URL>` | `DATABASE_URL` | unset | PostgreSQL URL. Unset, blank, or unparsable ⇒ storage disabled |
| `storage.max_connections` | — | `MCP_STORAGE_MAX_CONNECTIONS` | `5` | Pool size |
| `storage.acquire_timeout_ms` | — | `MCP_STORAGE_ACQUIRE_TIMEOUT_MS` | `3000` | How long an operation may wait for a connection |
| `storage.breaker_threshold` | — | `MCP_STORAGE_BREAKER_THRESHOLD` | `2` | Connectivity failures before storage is skipped |
| `storage.breaker_cooldown_ms` | — | `MCP_STORAGE_BREAKER_COOLDOWN_MS` | `30000` | How long the breaker stays open |
| `storage.max_history_rows` | — | `MCP_STORAGE_MAX_HISTORY_ROWS` | `100` | Upper bound on rows `calc_history` returns |

A blank value counts as unset, so an empty `DATABASE_URL` exported by a compose file or CI job means
"not configured" rather than "empty URL".

Bad configuration is rejected at startup, before the transport opens, with the offending key named:

```
$ poc-rust-mcp --config bad.toml
poc-rust-mcp: invalid config file bad.toml: TOML parse error at line 2, column 1
  |
2 | max_connection = 4
  | ^^^^^^^^^^^^^^
unknown field `max_connection`, expected one of `url`, `max_connections`, …
```

### Precedence example

```bash
# config.toml says max_connections = 11 and log_filter = "warn"
export MCP_STORAGE_MAX_CONNECTIONS=9   # beats the file
cargo run -- --sse 0.0.0.0:9000        # beats MCP_SSE_ADDRESS and the file
# -> max_connections = 9, sse_address = 0.0.0.0:9000, log_filter = "warn"
```

## Running

### stdio (default)

This is what MCP clients spawn as a subprocess. JSON-RPC is read from stdin and written to stdout —
nothing else may ever be printed there.

```bash
cargo run
```

Typical MCP client configuration:

```json
{
  "mcpServers": {
    "poc-rust-mcp": {
      "command": "cargo",
      "args": ["run", "--quiet", "--manifest-path", "/path/to/poc-rust-mcp/Cargo.toml"],
      "env": { "DATABASE_URL": "postgres://mcp:mcp@127.0.0.1:5432/mcp" }
    }
  }
}
```

### SSE

```bash
cargo run -- --sse                  # 127.0.0.1:8000
cargo run -- --sse 0.0.0.0:9000     # explicit bind address
```

- `GET /sse` — long-lived event stream; the first event is `event: endpoint` with the session URL.
- `POST /message?sessionId=…` — returns `202 Accepted`; the JSON-RPC response arrives on that
  session's SSE stream.

The client handshake over SSE is the usual sequence: open `GET /sse`, POST `initialize`, POST
`notifications/initialized`, then POST tool calls.

### Smoke test without a client

Build first, then keep stdin open — a cold `cargo run` compiles while stdin is already at EOF, and
`rmcp` 0.1.5 shuts down and drops in-flight responses.

```bash
cargo build
{ printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"smoke","version":"0.0.1"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"add","arguments":{"a":2,"b":40}}}'; sleep 1; } \
  | RUST_LOG=warn ./target/debug/poc-rust-mcp
```

## How storage behaves

Everything here is configurable (see [Configuration](#configuration)); with no configuration at all,
these are the defaults.

- **Missing or unparsable URL ⇒ storage disabled.** The server starts normally, every arithmetic tool
  works, and the three history tools return an explanatory error.
- **Migrations are applied lazily**, on the first storage call, not at startup, and retried until
  they succeed. Add a new numbered file in `migrations/`; never edit an applied one.
- **History writes are fire-and-forget** (`tokio::spawn`), so a tool never waits for the database.
  The trade-off: a row can be lost if the process exits immediately after the tool returns.
- **Every storage attempt is bounded** by a 3 s acquire timeout, and a circuit breaker skips storage
  entirely for 30 s after 2 *connectivity* failures. Query-level errors never trip it, so a bad
  query cannot disable storage.
- **Queries are not checked at compile time** (`query_as` + `#[derive(FromRow)]`), so building
  without a database works. The trade-off: a typo in SQL is a runtime error, not a build error.
- `clear_calc_history` uses `DELETE`, not `TRUNCATE`, because `TRUNCATE` reports no row count and
  the tool would claim it deleted nothing.

Migration `0001_calc_history.sql` stores `operation`, the raw JSON `inputs`, and exactly one of
`result` or `error` (enforced by a check constraint), indexed newest-first.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build
```

Tests are hermetic: `cargo test` passes on a bare checkout with **no PostgreSQL running**.

| Layer | File | What it proves |
| --- | --- | --- |
| Protocol | `tests/stdio_protocol.rs` | Spawns the real binary and speaks raw JSON-RPC: capabilities, `tools/list`, argument validation, tool errors vs JSON-RPC errors, stdout purity, and that arithmetic does not block on a dead database. |
| Public API | `tests/store_offline.rs` | The `Store`/`HistoryEntry` surface without a database: disabled storage, unparsable URLs, bounded failure, breaker tripping, fire-and-forget writes. |
| Configuration | `tests/config.rs` | Layer precedence, defaults, blank-value handling, rejected keys and out-of-range values, CLI parsing, and that resolved settings reach the `Store`. |
| Private | `src/**/mod tests` | Only what needs private access — `rmcp`'s macro makes tool functions private, and breaker internals are reachable only inside `db.rs`. |

Anything that needs a live database (real inserts, `DELETE` row counts, migration
`VersionMismatch`, recovering after the 30 s breaker cooldown) is verified manually against the
devcontainer rather than in `cargo test`.

### Layout

```
src/lib.rs                     library target: exposes config + db + server
src/config.rs                  layered configuration (defaults/file/env/flags)
src/main.rs                    CLI, tracing, stdio/SSE wiring
src/server.rs                  Calculator server, tool definitions, ServerInfo
src/db.rs                      all PostgreSQL access, migrations, circuit breaker
migrations/                    sqlx migrations
config.example.toml            documented example config (copy to config.toml)
tests/                         integration tests
.devcontainer/                 podman-compose stack (app + postgres:16)
AGENTS.md                      notes and gotchas for coding agents
```

The crate ships both a lib and a bin, so the server can be embedded in another Rust program:

```rust
use poc_rust_mcp::{
    config::{Cli, Config, SystemEnv},
    db::Store,
    server::Calculator,
};

let cli = Cli::parse_from(std::env::args().skip(1))?;
let config = Config::resolve(&cli, &SystemEnv)?;
let calculator = Calculator::new(Store::from_settings(&config.storage));
```

## Known limitations

- Arithmetic uses wrapping semantics for `i32`, so `add`/`sub`/`mul` wrap on overflow rather than
  returning an error.
- Fire-and-forget writes can lose the last row if the process exits immediately after a tool returns.
- `rmcp` is pinned to `0.1`, whose API differs substantially from 3.x — see `AGENTS.md`.
- `ToolBox::list()` is backed by a `HashMap`, so tool order is not deterministic (`tool_names()`
  sorts).
- There is no config-file discovery: a file is only read when `--config` or `MCP_CONFIG` names it.
  Longest-prefix or per-directory search would be the next step if that becomes annoying.