# poc-rust-mcp

A [Model Context Protocol](https://modelcontextprotocol.io) server that does arithmetic, built with
[`rmcp`](https://crates.io/crates/rmcp) and Tokio, with an **optional** database-backed history of
every calculation.

History storage works on **PostgreSQL, MySQL/MariaDB and SQLite**. The URL's scheme picks the
backend; nothing else is configured.

The point of the PoC is the storage story: the calculator tools must answer instantly whether or not
a database exists, is reachable, or is misconfigured. Storage degrades; it never blocks.

## Tools

| Tool | Arguments | Returns |
| --- | --- | --- |
| `add` | `a: i32`, `b: i32` | `"2 + 40 = 42"` |
| `sub` | `a: i32`, `b: i32` | `"7 - 2 = 5"` |
| `mul` | `a: i32`, `b: i32` | `"6 * 7 = 42"` |
| `div` | `dividend: f64`, `divisor: f64` | `"9 / 2 = 4.5"`, or a tool error `division by zero` |
| `db_status` | — | `"mysql database reachable, 128 stored calculations"` |
| `calc_history` | `limit: i64` (clamped to `1..=storage.max_history_rows`) | one line per entry, most recent first |
| `clear_calc_history` | — | `"deleted 128 recorded calculations"` |

## Everything else MCP offers

The server implements every server-side feature rmcp 0.1.5 exposes, and advertises exactly those —
see [MCP features](#mcp-features) for the full map and for what the pinned SDK cannot do.

## Requirements

- Rust `stable` ≥ 1.85 (edition 2024). A `rust-toolchain.toml` pins stable with `clippy` and
  `rustfmt`, so nothing needs installing manually.
- A database is **optional**. Everything except the three history tools works without one.
- `podman` + `podman-compose` only if you want the devcontainer.
- SQLite needs no server at all; PostgreSQL and MySQL are only needed to test against those backends.

## Quick start

Without a database — the server still starts and all arithmetic works:

```bash
cargo run                     # stdio (default)
cargo run -- --help
```

With a database, pick the URL scheme:

```bash
cargo run -- --db-url sqlite://mcp.db          # SQLite file, created if missing
cargo run -- --db-url sqlite::memory:          # SQLite, nothing on disk
cargo run -- --db-url postgres://mcp:mcp@127.0.0.1:5432/mcp
cargo run -- --db-url mysql://mcp:mcp@127.0.0.1:3306/mcp
```

With the devcontainer (app + PostgreSQL + MySQL, `DATABASE_URL` wired to the `db` service):

```bash
podman-compose -f .devcontainer/compose.yaml up -d --build
podman-compose -f .devcontainer/compose.yaml exec app cargo run
```

PostgreSQL is published on host port `5432` and MySQL on `3306`, both as `mcp` / `mcp` / `mcp`, so
`cargo run` from your host works too:

```bash
export DATABASE_URL=postgres://mcp:mcp@127.0.0.1:5432/mcp   # or mysql://…:3306/mcp
cargo run
```

## Backends

| URL scheme | Backend | Reported by `db_status` as |
| --- | --- | --- |
| `postgres://`, `postgresql://` | PostgreSQL | `postgres` |
| `mysql://`, `mariadb://` | MySQL or MariaDB (sqlx's MySQL driver speaks to both) | `mysql` |
| `sqlite://` | SQLite file or in-process | `sqlite` |

`db_status` names the *driver*, not the scheme you typed, so `mariadb://` reports `mysql`.

Notes per backend:

- **MySQL/MariaDB** needs 8.0.16+ / 10.2.1+, the first releases that enforce `CHECK` constraints,
  which the schema relies on to keep `result` and `error` mutually exclusive.
- **SQLite** creates the file if it is missing. A URL pinned to `?mode=ro` or `?mode=rw` means "this
  file must already exist" and is left alone. An in-memory database (`sqlite::memory:`) is capped at a
  single pooled connection, because SQLite's shared-cache mode takes *table* locks that its busy
  handler cannot wait out.
- **TLS** follows the URL and the driver's own default. This crate builds sqlx without a TLS feature,
  and every driver treats "no TLS compiled in" as "stay on plaintext" — so a local container works
  untouched, and enabling a TLS feature later upgrades automatically. For a managed database that
  *requires* TLS, put `?sslmode=require` (PostgreSQL) or `?ssl-mode=REQUIRED` (MySQL) in the URL.

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
| `server.list_page_size` | — | `MCP_LIST_PAGE_SIZE` | `20` | Rows per page for the MCP `*/list` methods |
| `storage.url` | `--db-url <URL>` | `DATABASE_URL` | unset | Database URL; the scheme picks the backend. Unset, blank, unparsable or unsupported ⇒ storage disabled |
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

An unparsable or unsupported *URL* is treated differently from a bad config file: it degrades to
disabled storage with a warning naming the accepted schemes, rather than refusing to start. A server
that cannot use its history is still a working calculator.

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

`scripts/verify-backend.sh <url>` does the same against a live database and additionally exercises
`db_status`, `calc_history` and `clear_calc_history` — the manual check for a backend that needs a
container:

```bash
./scripts/verify-backend.sh 'mysql://mcp:mcp@127.0.0.1:3306/mcp'
```

## MCP features

rmcp 0.1.5 exposes one `ServerHandler` hook per MCP server-side feature, and all of them are
implemented. `initialize` advertises exactly this set — nothing more:

```json
{"tools": {}, "prompts": {}, "resources": {"subscribe": true}, "logging": {}}
```

| Feature | Method(s) | Behaviour |
| --- | --- | --- |
| Tools | `tools/list`, `tools/call` | The seven above, listed sorted, paginated |
| Prompts | `prompts/list`, `prompts/get` | Two prompts; arguments validated, not just described |
| Resources | `resources/list`, `resources/templates/list`, `resources/read` | History as a readable document |
| Subscription | `resources/subscribe`, `resources/unsubscribe`, `notifications/resources/updated` | Announced on every write, per affected resource |
| Logging | `logging/setLevel`, `notifications/message` | An `Info` line per recorded calculation, filtered by the level |
| Completion | `completion/complete` | Prompt arguments and resource-template placeholders |
| Ping | `ping` | Answered |
| Pagination | `params.cursor` on all four `*/list` | Opaque index cursors |
| Progress / cancellation | `notifications/progress`, `notifications/cancelled` | Accepted; see below |

### Prompts

A prompt runs nothing: `prompts/get` returns messages the client hands to a model, which then decides
which tools to call. Both prompts point the model at a tool or resource rather than encoding answers.

| Name | Arguments | Asks the model to |
| --- | --- | --- |
| `review_calculation_history` | `operation` (optional) | read `calc://history` and summarise it, optionally narrowed to one operation |
| `check_storage_health` | — | call `db_status` and explain any degradation |

Validation is strict, because a silently ignored argument would change a prompt's meaning without
telling anyone: an unknown prompt, a misspelled argument, or an `operation` outside
`add`/`sub`/`mul`/`div` is an `invalid_params` error naming what is accepted.

### Resources

```text
calc://history                       every recorded calculation, newest first  (listed)
calc://history/{id}                  one recorded calculation                   (template)
calc://history/operation/{operation} every recorded calculation of one operation (template)
```

The per-id and per-operation URIs are **templates**, not enumerated resources: the set of ids is
unbounded and changes with every call, so listing them would be a lie a client could cache.

Reads degrade exactly like the tools. A disabled or unreachable database is **not** a missing
resource — the resource resolves and its *content* explains why it is empty. Only a URI this server
does not serve, or an id with no row behind it, is `resource_not_found` (`-32002`).

### Subscription and logging

`resources/subscribe` is refused for a URI this server does not serve, rather than accepted into a
subscription that could never fire. A write then announces every subscribed resource it invalidates —
the whole log, and that operation's slice — but never a single row, which does not change once
written. Announcements are sent from a detached task and describe the *attempt* to record, because
the write is fire-and-forget by design; the notification is advisory, so over-announcing is harmless
where a client left waiting is not.

`logging/setLevel` sets a floor. Each recorded calculation emits one `Info` `notifications/message`;
below the floor nothing is sent. The default floor is `Info`, so announcements work without any setup.

### Progress and cancellation

Both notifications are accepted and deliberately do nothing. rmcp already cancels the request's
`CancellationToken` when `notifications/cancelled` arrives, before the handler hook runs, so the
transport half is automatic; and every tool is a single atomic call returning its whole result at
once, so there is no partial work to abandon and nothing to report progress *for*. The tests assert
the server neither replies to nor breaks on such notifications.

### What the pinned SDK cannot do

`rmcp` 0.1 implements MCP 2024-11-05 only, so these are unavailable rather than unimplemented, and no
capability is advertised for them: **tool annotations** (`readOnlyHint`, `destructiveHint`, …),
**structured tool output** (`outputSchema`/`structuredContent`), **icons**, and resource links in tool
results — all of which arrived in 2025-03-26 and 2025-06-18.

One SDK bug is worked around rather than fixed: rmcp serialises `ResourceContents` with an
enum-level `rename_all`, which renames the variants but not the fields inside them, so a `mimeType`
set on a read result goes on the wire as `mime_type` — not a key in the MCP schema. The server
therefore *omits* the optional media type on reads instead of emitting it wrongly; `resources/list`
still carries a correct `mimeType`, because that one comes from a struct whose `rename_all` applies.
`tests/mcp_features.rs` pins this, so an rmcp upgrade will flag it.

## How storage behaves

Everything here is configurable (see [Configuration](#configuration)); with no configuration at all,
these are the defaults.

- **Missing, unparsable or unsupported URL ⇒ storage disabled.** The server starts normally, every
  arithmetic tool works, and the three history tools return an explanatory error.
- **Migrations are applied lazily**, on the first storage call, not at startup, and retried until
  they succeed. Migrations are **per backend** — see below.
- **History writes are fire-and-forget** (`tokio::spawn`), so a tool never waits for the database.
  The trade-off: a row can be lost if the process exits immediately after the tool returns.
- **Every storage attempt is bounded** by a 3 s acquire timeout, and a circuit breaker skips storage
  entirely for 30 s after 2 *connectivity* failures. Query-level errors never trip it, so a bad
  query cannot disable storage.
- **Queries are not checked at compile time** (`query_as` + `#[derive(FromRow)]`), so building
  without a database works. The trade-off: a typo in SQL is a runtime error, not a build error.
- `clear_calc_history` uses `DELETE`, not `TRUNCATE`, because `TRUNCATE` reports no row count on
  PostgreSQL or MySQL and the tool would claim it deleted nothing.

### Migrations are per backend

`migrations/` holds one directory per dialect, and `db.rs` embeds each set at compile time and picks
the set to apply from the URL's scheme:

```
migrations/postgres/0001_calc_history.sql
migrations/mysql/0001_calc_history.sql
migrations/sqlite/0001_calc_history.sql
```

There is no spelling of "auto-incrementing key, JSON column, descending index" that all three
backends accept, so one shared file is not an option. Adding migration `0002_*` means adding it to
all three directories, and never editing an applied one — sqlx records each file's SHA-384 and a
change to an applied file fails with `VersionMismatch`.

Each `0001_calc_history.sql` stores `operation`, the raw JSON `inputs`, exactly one of `result` or
`error` (enforced by a check constraint), and a `created_at` indexed newest-first.

Two deliberate differences from PostgreSQL's defaults, both documented in the migration files:

- **`created_at` is written by the server**, not left to the column default, so all three backends
  store the same instant from one clock. SQLite's `CURRENT_TIMESTAMP` is a `YYYY-MM-DD HH:MM:SS`
  string that does not sort against the RFC 3339 text `DateTime<Utc>` writes, and MySQL's `TIMESTAMP`
  silently shifts by the session time zone (`DATETIME(6)` has no such conversion).
- **SQLite's `created_at` has no `DEFAULT`** for the same reason: mixing two timestamp formats would
  silently corrupt `ORDER BY created_at DESC`.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build
```

Tests are hermetic: `cargo test` passes on a bare checkout with **no database running**.

| Layer | File | What it proves |
| --- | --- | --- |
| Protocol | `tests/stdio_protocol.rs` | Spawns the real binary and speaks raw JSON-RPC: capabilities (and the ones deliberately *not* advertised), `ping`, `tools/list`, argument validation, tool errors vs JSON-RPC errors, stdout purity, that arithmetic does not block on a dead database, and that progress/cancellation notifications leave the server working. |
| MCP features | `tests/mcp_features.rs` | Every feature beyond tools, reachable through the protocol: prompt catalogue and validation, resource routing and degradation, subscription announcements, the logging level filter, completion, and a full cursor walk over all four paginated methods. |
| Handlers | `tests/server.rs` | The `Calculator` and its `ServerHandler` hooks called directly — tools via the public `call_tool` (rmcp keeps them private), capabilities, pagination, subscribe/unsubscribe, completion, and the log-level filter. |
| Catalogue | `tests/prompts.rs`, `tests/resources.rs` | The prompt and resource catalogues as pure functions: rendering, argument validation, URI routing, and reads against an in-memory SQLite store. |
| Internals | `tests/db.rs` | The handful of behaviours no public API can reach — the circuit breaker's bookkeeping, the shared schema cell, the per-dialect SQL and migration sets, and SQLite URL parsing — through the `#[doc(hidden)] db::testing` module. |
| Public API | `tests/store_offline.rs` | The `Store`/`HistoryEntry` surface without a database: scheme→backend selection, disabled storage, unparsable and unsupported URLs, bounded failure, breaker tripping, fire-and-forget writes. |
| Real backend | `tests/sqlite_backend.rs` | A full round trip against SQLite, which is embedded and needs no server: migrations apply, a JSON and a timestamp column survive a write/read, history is newest-first and limited, `clear` reports a real row count, a file-backed database persists across stores. |
| Configuration | `tests/config.rs` | Layer precedence, defaults, blank-value handling, rejected keys and out-of-range values, CLI parsing, and that resolved settings and the resolved URL's backend reach the `Store`. |

Every test lives in `tests/`; there is no `#[cfg(test)]` module left in `src/`. `tests/db.rs` and
`tests/server.rs` reach a few private items through `db::testing` and `server::testing` —
`#[doc(hidden)] pub mod` shims that wrap internals still declared `fn`, so the rendered API is
unchanged. rmcp's macro keeps the tool functions private and `server::testing` deliberately does not
widen that, so `tests/server.rs` calls tools the way a client does.

`tests/support/mod.rs` holds the JSON-RPC harness the two protocol-level files share, plus a
`request_context()` helper for calling handlers directly. One part is worth knowing about:
**notifications are buffered, not skipped.** The server pushes `notifications/message` and
`notifications/resources/updated` from detached tasks, so either can land before the response that
caused it — a reader that skipped it would lose it. `Server::quiesce` then drives harmless round-trips
until the server stops pushing, because no single notification's arrival can prove the stream is
drained: several writes announce from separate tasks and may interleave, and a whole batch can still
be unscheduled. A test that needs a page size other than the default sets `MCP_LIST_PAGE_SIZE`;
pagination is otherwise unreachable, since every catalogue fits on one page.

Anything that needs a live PostgreSQL or MySQL is verified manually against the devcontainer with
`scripts/verify-backend.sh`, rather than in `cargo test`. That includes migration `VersionMismatch`
and recovering after the 30 s breaker cooldown.

### Layout

```
src/lib.rs                     library target: exposes config + db + prompts + resources + server
src/config.rs                  layered configuration (defaults/file/env/flags)
src/main.rs                    CLI, tracing, stdio/SSE wiring
src/server.rs                  Calculator: ServerHandler, peer/subscriptions, page size
src/prompts.rs                 the prompt catalogue, rendering and validation
src/resources.rs               resource catalogue, URI routing, reads from the store
src/db.rs                      all database access, dialects, migrations, circuit breaker
migrations/{postgres,mysql,sqlite}/  per-dialect sqlx migrations
config.example.toml            documented example config (copy to config.toml)
scripts/verify-backend.sh      manual end-to-end check against a live database
tests/                         every test in the crate, one file per layer
tests/support/mod.rs           JSON-RPC harness shared by the protocol-level test files
.devcontainer/                 podman-compose stack (app + postgres + mysql)
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
- `rmcp` is pinned to `0.1`, whose API differs substantially from 3.x — see `AGENTS.md`. Tool
  annotations, structured tool output and icons are unavailable on this pin.
- `ToolBox::list()` is backed by a `HashMap`, so `tools/list` is sorted by the server rather than
  reported in map order; the prompt and resource catalogues keep their declaration order.
- There is no config-file discovery: a file is only read when `--config` or `MCP_CONFIG` names it.
  Longest-prefix or per-directory search would be the next step if this becomes annoying.
- SQL Server is not supported. sqlx itself only implements PostgreSQL, MySQL and SQLite; MSSQL needs
  the third-party `sqlx-mssql` crate.
- SQLite file databases stay in the rollback-journal mode SQLite defaults to. A history log written by
  several processes at once would want WAL, which the URL cannot currently request.

## License

BSD 2-Clause. The full text is in [`LICENSE`](LICENSE); `Cargo.toml` declares the SPDX identifier
`BSD-2-Clause`, and the two must stay in step.
