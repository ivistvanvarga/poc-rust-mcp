# Security hardening: design

Date: 2026-11-12
Status: awaiting approval
Scope chosen by the maintainer: **4. Everything** — hardening, dependency updates, and a
validated SSE transport.

## Intent and why

`poc-rust-mcp` is a local-first MCP calculator with optional database-backed history. Its
default transport is stdio, which is process-local and not network-reachable. It also offers
`--sse`, and that is the part that turns an otherwise local tool into a network service.

The audit found one high-severity exposure (rmcp 0.1.5's SSE server has no authentication and
no `Host`/`Origin` validation, so a malicious web page can reach a loopback-bound server through
DNS rebinding), a set of unbounded-resource paths, a few places where credentials or raw sqlx
errors can escape into logs or MCP responses, and a default SQLite file mode of `0644` under a
typical umask. Four dependency advisories also apply; one (`rsa` via `sqlx-mysql`) is fixed by
moving to sqlx 0.9, and the others are either unreachable through this crate's usage or blocked
by sqlx's own dependency ranges.

The goal is to close what can be closed without rewriting the tool layer, and to make every
remaining risk explicit, documented, and tested.

## Goals

- DNS rebinding against SSE is blocked by `Host` and `Origin` validation on every request.
- A non-loopback SSE bind requires a bearer token; loopback may run without one for local
  development, but still gets `Host`/`Origin` validation.
- Credentials never appear in logs, warnings, errors, or MCP tool results.
- Client-visible storage errors are coarse and stable; full detail stays in server logs.
- Resource use is bounded: SSE sessions, subscriptions, pending history writes, and stored
  history rows.
- SQLite history files are created owner-only on Unix.
- `cargo audit` is clean except for documented, reviewed ignores with written rationale.
- The existing behaviour and public MCP surface stay intact: all tools, prompts, resources,
  pagination, logging, and subscriptions keep working.

## Non-goals

- Rewriting the tool layer for rmcp 3.x. `AGENTS.md` documents why: rmcp 3.x replaces the
  `#[tool]`/`#[tool_box]` model and would be a separate project.
- Adding OAuth, multi-user identity, or per-user authorization. This remains a single-user
  calculator.
- End-to-end encryption of the SSE transport. Operators who need TLS terminate it in a reverse
  proxy; this crate stays plaintext HTTP.
- Changing the database schema or the migration checksums.

## Anti-goals

- A "security" switch that silently weakens validation when set.
- Wildcard `Host`/`Origin` entries (`*`) or a query-string token, both of which would recreate
  the DNS-rebinding exposure and leak secrets into logs.
- Blocking arithmetic tools on database writes or on the new bounds.
- Returning raw driver text to MCP clients in the name of "debuggability".

## Constraints

- rmcp stays pinned to 0.1.5; the existing `#[tool]` + hand-written `list_tools`/`call_tool`
  arrangement must not change.
- sqlx 0.9.0 is the newest release compatible with the current backend set. It still requires
  `libsqlite3-sys <0.38.0`, so the bundled-SQLite advisory cannot be fixed here.
- All tests stay in `tests/`; no `#[cfg(test)]` modules in `src/`.
- The layered configuration contract stays: flag > environment > file > default, unknown keys
  rejected, no implicit config-file discovery.
- No new runtime service: the SSE transport is implemented in-process with axum, not delegated
  to rmcp's vulnerable server module.

## Approaches considered

1. **Focused hardening only.** Redact credentials, sanitize client-facing errors, bound
   subscriptions/tasks/history, fix log injection, warn on non-loopback SSE, rewrite
   `SECURITY.md`. Smallest change, but leaves the high-severity transport exposure in place and
   leaves the `rsa` advisory unfixed.
2. **Hardening + sqlx 0.9.** Fixes `rsa` and updates the storage layer, but still leaves DNS
   rebinding against SSE unaddressed.
3. **Hardening + a validating SSE layer.** Fixes the transport exposure but leaves the `rsa`
   advisory and the dependency story untouched.
4. **Everything (chosen).** Combines 2 and 3. It is the only option that actually closes the
   high-severity finding instead of documenting it. The cost is a new transport module, a
   dependency bump with a small API migration, and new tests.

rmcp 3.x was rejected as an approach: its SSE server is gone, its Streamable HTTP transport is
the replacement, and its tool API is different. That is a rewrite, not hardening.

## Design

### 1. Dependencies and audit posture

`Cargo.toml` changes:

- `sqlx` 0.8 → **0.9.0**, `default-features = false`, features:
  `runtime-tokio`, `postgres`, `mysql`, `mysql-rsa`, `sqlite-bundled`, `macros`, `migrate`,
  `json`, `chrono`.
  - `sqlite-bundled` is used instead of `sqlite`: in 0.9 the plain `sqlite` feature also enables
    `sqlite-deserialize`, `sqlite-load-extension`, and `sqlite-unlock-notify`, which this crate
    does not need and does not want to expose.
  - `mysql-rsa` is required for MySQL's default `caching_sha2_password` authentication over a
    plaintext local connection. It also moves `rsa` from 0.9 to 0.10, fixing
    RUSTSEC-2023-0071.
- `rmcp` loses `transport-sse-server`; it keeps `server` and `transport-io`. The `paste`
  advisory is a build-time unmaintained-macro notice from rmcp 0.1.5 and is not fixable without
  the rewrites ruled out above.
- New runtime dependencies:
  - `axum = "0.8"` (already transitive via rmcp; used directly by the new transport),
  - `futures = "0.3"`,
  - `http = "1"` (authority/URI parsing in config and transport),
  - `tokio-stream = "0.1"`,
  - `tokio-util = "0.7"` (promoted from dev-dependency; `CancellationToken`, `PollSender`),
  - `getrandom = "0.3"` (session IDs; already transitive).
- No new dev-dependency: the SSE tests speak HTTP/1.0 over `tokio::net::TcpStream`, so a full
  HTTP client and its TLS stack are not pulled in for tests.

sqlx 0.9 API migration:

- `sqlx::query`/`query_as` now take `impl SqlSafeStr`, implemented natively only for
  `&'static str`. Every SQL string built by `Dialect::*_sql()` is generated from static text and
  bind placeholders, never from user input, so dynamic strings are wrapped in
  `sqlx::AssertSqlSafe(...)`. The migration SQL stays static per dialect.
- Verify every `sqlx::Error` variant used by `is_connectivity_error` still exists in 0.9
  (`Io`, `PoolTimedOut`, `PoolClosed`, `Configuration`, `Tls`).
- The `_sqlx_migrations` table schema is unchanged between 0.8 and 0.9, so existing databases do
  not need a migration. The migration checksum test remains valid.

`.cargo/audit.toml` records reviewed ignores with rationale:

```toml
[advisories]
ignore = [
  # Streamable HTTP server transport only; this crate has its own SSE transport.
  "RUSTSEC-2026-0189",
  # Bundled SQLite CVE-2026-119375; fixed in libsqlite3-sys 0.39, which sqlx 0.9 excludes.
  # Local attack requiring a crafted database + WAL pair.
  "RUSTSEC-2026-0459",
  # paste is unmaintained, not vulnerable; build-time macro reachable only via rmcp 0.1.5.
  "RUSTSEC-2024-0436",
]
```

Acceptance: `cargo audit` exits 0.

### 2. Credential redaction and client-facing errors

A new `db::redact_url(url) -> String` keeps the scheme, host, port, and database name, but
replaces userinfo with `***` and drops the query string (which can carry `password=`,
`sslpassword=`, or tokens). It never panics on malformed input and falls back to the scheme name
or `"<redacted>"`.

Used in:

- `Dialect::from_url` error text: name the accepted schemes and, when a scheme was present,
  only the scheme — never the URL.
- `Store::connect_with_limits` warning: log the redacted URL, not the raw one.
- The error returned by the lazy pool construction if sqlx rejects the URL: log a scrubbed
  message (raw URL substring replaced with the redacted form); the store then degrades to
  disabled as it does today.

Client-facing storage errors become coarse. `Store::finish` logs the full error at `warn!`
internally, then returns one of:

- connectivity failure → `"{context}: database unreachable"`,
- any `sqlx::Error::Database` → `"{context}: database rejected the request"`,
- anything else → `"{context}: internal storage error"`.

Migration failures are similarly classified: `VersionMismatch`, `VersionMissing`,
`VersionTooOld`, `VersionTooNew`, and `VersionNotPresent` are actionable and safe to name;
everything else becomes `"failed to apply migrations: migration failed"`. The full
`MigrateError` is logged.

This deliberately trades some client-side debuggability for not echoing SQL text, values, host
names, or driver internals to an LLM. The server log remains the debugging channel.

### 3. Resource bounds

- **Subscriptions.** `Calculator` gains a per-instance cap (default 256), configurable in tests
  through a builder. `subscribe` returns `McpError::invalid_params` when adding a new URI would
  exceed the cap. Re-subscribing to an existing URI stays free.
- **No whole-set clone on writes.** `announce_write` locks the subscription set, collects only
  the invalidated URIs that are actually subscribed, drops the guard, and then spawns the
  notification task.
- **Detached history writes.** `Store` owns an `Arc<Semaphore>` with 64 permits. A write uses
  `try_acquire_owned` before spawning; if none is available, the record is dropped with a
  `debug!` line. This keeps arithmetic tools non-blocking and stops an unreachable database from
  accumulating unbounded tasks. Losing a best-effort history row under load is already an
  accepted property of fire-and-forget writes.
- **Read bound.** `Store::list` clamps internally to `1..=limits.max_history_rows`, so even a
  caller that bypasses `server::clamp_history_limit` cannot ask for more.
- **Retention.** After a successful insert, the same detached task prunes the table to the newest
  `max_history_rows` rows. Per dialect:
  - PostgreSQL/SQLite: `DELETE FROM calc_history WHERE id NOT IN
    (SELECT id FROM calc_history ORDER BY created_at DESC, id DESC LIMIT <placeholder>)`.
  - MySQL: a multi-table `DELETE` with a materialised derived table, because its `DELETE`
    cannot reference the target table directly in a subquery:
    `DELETE h FROM calc_history h LEFT JOIN (SELECT id FROM calc_history ORDER BY created_at
    DESC, id DESC LIMIT ?) keep ON h.id = keep.id WHERE keep.id IS NULL`.
  Pruning on every write doubles the query count but is idempotent and keeps disk use bounded;
  at this crate's scale that is the right trade. Errors are logged, not surfaced, and the insert
  is already durable.
- **SSE sessions.** The transport caps concurrent sessions (default 64, settable through config)
  and refuses new ones with `503` plus `Retry-After`. A session's inbound channel is bounded
  (32 messages), so a client cannot queue unbounded work. Request bodies on `/message` are
  limited to 256 KiB.

### 4. Custom SSE transport (`src/sse.rs`)

rmcp's `transport-sse-server` is removed and replaced by a module that speaks the same wire
protocol:

- `GET /sse` opens the event stream. The first event is
  `event: endpoint` with `data: /message?sessionId=<id>`.
- `POST /message?sessionId=<id>` accepts one JSON-RPC message and answers `202 Accepted`; an
  unknown session is `404`; a send failure is `410`; a full inbound queue is `429` with
  `Retry-After: 1`. The handler uses `try_send`, so it never blocks a request task on a slow
  session.
- A body over 256 KiB is `413`. Malformed JSON is `400`; a non-JSON content type is `415`.
  Both rejections are mapped explicitly rather than inheriting axum's extractor defaults, so the
  contract is stable and testable.
- The event stream sends `event: message` with the serialised server message, and a keep-alive
  comment every 15 seconds.
- The origin allowlist is a **restriction only**. The server emits no CORS headers and answers
  no preflight requests. A browser client needs a same-origin proxy that adds the bearer token;
  `sse_allowed_origins` exists for non-browser clients that happen to send `Origin` and for
  deployments where a proxy forwards it. It does not make cross-origin `EventSource` work, and
  the docs say so.

Every request, on both routes, passes the same validation chain before reaching a handler:

1. **Host allowlist.** The request authority is taken from the `Host` header, or from
   `request.uri().authority()` when the header is absent (HTTP/2 `:authority`). At least one
   must be present. It is parsed as an `http::uri::Authority`; userinfo and zone IDs are
   rejected. Hosts are compared after lowercasing and stripping a single trailing dot (DNS root
   notation); IPv6 hosts are compared without brackets. A missing, malformed, or unlisted
   authority is `403`.
   - An allowlist entry with an explicit port matches only that port. An entry without a port
     matches any port on that host: the listener is already fixed, and the port in `Host` is
     only ever the listener's or a proxy's advertised port.
   - When `sse_allowed_hosts` is set, it is the complete list.
   - When it is unset and the bind is loopback, the default list is `localhost`, `127.0.0.1`,
     and `[::1]`, each pinned to the actual listener port.
   - When the bind is non-loopback, the allowlist must be set explicitly: the server cannot know
     which LAN name or address clients will use, and guessing would defeat the check.
2. **Origin allowlist.** An absent `Origin` is allowed only after the Host and token checks; it
   is the normal case for CLI clients. `Origin: null` is rejected. A present origin must parse
   as `scheme://authority` with no path, userinfo, or query.
   - When `sse_allowed_origins` is set, the origin must match one entry after the same host
     normalisation; a pinned entry port must match, an entry without a port matches any port on
     that host.
   - When it is unset, the origin is accepted only when its host matches the already-validated
     request host, and — if the request authority carried an explicit port — its effective port
     (80 for `http`, 443 for `https`) matches that port. This keeps a TLS-terminating proxy
     working (`Origin: https://mcp.example.com`, `Host: mcp.example.com`) without weakening the
     check: a malicious page cannot make its Origin host equal the target host.
3. **Bearer token.** When a token is configured, `Authorization: Bearer <token>` is required on
   both routes. Comparison is constant-time over equal-length byte slices, and the length check
   is folded into the same comparison so a wrong length does not short-circuit. Missing or wrong
   credentials are `401` with `WWW-Authenticate: Bearer`.
   - A non-loopback bind refuses to start without a token. Loopback may run without one for
     local development; if one is set, it is enforced there too.

Token delivery is header-only. A browser `EventSource` cannot set an `Authorization` header,
which is a feature here: a malicious page cannot supply the token even if it somehow passes the
Host and Origin checks. Query-string tokens are not supported because they leak into access
logs and process listings. A browser UI needs a same-origin proxy that adds the header.

Sessions:

- IDs are 32 bytes from the OS CSPRNG (`getrandom`), hex-encoded, and never reused.
- The registry is a `tokio::sync::Mutex<HashMap<...>>`; capacity check and insert happen under
  one lock.
- Each session has a `CancellationToken`. Dropping the stream removes the map entry and cancels
  the session's `Calculator`, so disconnects do not leak tasks.
- A fresh `Calculator` per session preserves the current behaviour: subscriptions and log level
  are per client.
- On process shutdown the parent token cancels every session and then the axum graceful
  shutdown completes; open SSE bodies do not keep the process alive.

The actual listener address is authoritative. `Settings::from_config(&ServerConfig)` validates
syntax and early-rejects a non-loopback configured address without a token or explicit hosts, but
`serve` re-derives the default host list and repeats the non-loopback requirement from
`listener.local_addr()`. That lets tests bind `127.0.0.1:0` and still get correct defaults for
the selected port, and it means a caller cannot pair a non-loopback listener with loopback
defaults. A configured/listener mismatch is not an error by itself; the listener wins.

The module's public surface is deliberately small:

- `pub struct Settings` and `Settings::from_config(&ServerConfig) -> Result<Settings,
  SseConfigError>`, which computes the defaults and rejects a non-loopback bind without a token
  or an explicit host list.
- `pub async fn serve<F, S>(listener: TcpListener, settings: Settings, service: F,
  cancel: CancellationToken) -> io::Result<()>`.
- `#[doc(hidden)] pub mod testing` for tests that need internals (URL/authority normalisation,
  constant-time comparison, session-cap construction), following the existing `db::testing` and
  `server::testing` precedent.

`main.rs` binds the listener itself, logs the bind address and the security mode at `info!`, and
passes `Calculator::with_list_page_size(store.clone(), page_size)` per session. The stdio path is
unchanged.

### 5. Configuration

New `ServerConfig` fields, with the same four-layer precedence as everything else:

| Setting | Environment | File key | Default |
| --- | --- | --- | --- |
| `sse_allowed_hosts: Vec<String>` | `MCP_SSE_ALLOWED_HOSTS` (comma-separated) | `server.sse_allowed_hosts` | loopback names + bind port, or required for non-loopback |
| `sse_allowed_origins: Vec<String>` | `MCP_SSE_ALLOWED_ORIGINS` (comma-separated) | `server.sse_allowed_origins` | empty = same-origin only |
| `sse_token: Option<Secret>` | `MCP_SSE_TOKEN` | `server.sse_token` | `None` on loopback, required otherwise |
| `sse_max_sessions: usize` | `MCP_SSE_MAX_SESSIONS` | `server.sse_max_sessions` | `64` |

Details:

- The token is wrapped in a `Secret` newtype whose `Debug` prints `Secret(***)`; configuration
  can be logged without leaking it. `ServerConfig` keeps `Debug`, `Clone`, `PartialEq`, `Eq`.
- There is **no CLI token flag**: command lines are visible in the process list. `--help`
  documents `MCP_SSE_TOKEN` instead.
- Environment lists split on `,`, trim whitespace around each entry, and reject an empty entry
  (so `a,,b` is an error rather than silently ignored). A blank variable is unset.
- Validation rejects: empty token strings, `*` in either allowlist, malformed authorities,
  origins with a scheme other than `http`/`https`, origins with a path/query/userinfo, and
  `sse_max_sessions == 0`.
- The non-loopback requirements (token and explicit hosts) are checked when the SSE transport
  is constructed, not in `Config::validate`, so a config file that names an address but runs
  stdio does not fail startup.

`config.example.toml` and `Cli::usage()` document the new keys, the header-only token, the
loopback default, and the reverse-proxy note about forwarding the original `Host`.

### 6. Log injection

Client-controlled strings in logs use `?` (Debug) rather than `%` (Display), so newlines,
ANSI escapes, and control characters are escaped. The known sites are `subscribe` and
`unsubscribe` (`request.uri`); the implementation audits the rest of `src/` for the same pattern
before finishing. Session IDs and allowlist decisions are logged structurally.

### 7. SQLite file permissions

The file is created eagerly during `Store::try_connect` (that is, when the store is built at
startup), not on the first storage call: a permission error is a configuration error and should
be visible immediately, while migrations stay lazy as before.

`SqliteConnectOptions::from_str(url).get_filename()` supplies the decoded path, so the code does
not re-parse the URL. If the path does not exist, `Store` creates it with
`OpenOptions::new().write(true).create_new(true).mode(0o600)` on Unix. `create_new` makes the
creation race-safe; an `AlreadyExists` result from a concurrent creator is treated as success.
If the path exists but is not a regular file (directory, broken symlink, device), or creation
fails for any other reason, the store logs a warning naming the redacted URL and degrades to
disabled storage, exactly like an unparsable URL. It does not fail startup. In-memory URLs and
explicit `mode=ro`/`mode=rw` URLs are untouched. Existing regular files keep their permissions:
changing them could surprise an operator who deliberately shared one, and the README documents
how to tighten them.

The `-wal` and `-shm` sidecars SQLite creates inherit the database file's mode on Unix, so the
owner-only guarantee covers them too.

### 8. Documentation

- **`SECURITY.md`**: real supported-version policy (latest 0.x only), GitHub private-advisory
  reporting, a short threat model (stdio trusted/local, SSE network-exposed), what the server
  enforces, and the accepted risks below. The fake `5.1.x`/`4.0.x` table goes away.
- **`README.md`**: SSE security section (token, hosts, origins, loopback defaults, proxy note,
  header-only token), the new config rows, retention semantics (the table is pruned, not just
  the read), SQLite file mode, and updated known limitations.
- **`config.example.toml`**: the four new keys, with warnings and defaults.
- **`AGENTS.md`**: the new module and its invariants (validation order, header-only token,
  no non-loopback without token/hosts, session cleanup), the retention/limit behaviour, the
  `AssertSqlSafe` rule for sqlx 0.9, the audit command and `.cargo/audit.toml`, and the new test
  layers.

## Accepted risks

- **rmcp 0.1.5 / RUSTSEC-2026-0189.** The vulnerable component is the Streamable HTTP server
  transport, which this crate does not use. The replacement SSE transport performs the missing
  `Host`/`Origin` validation. Upgrading rmcp would rewrite the tool layer.
- **libsqlite3-sys / RUSTSEC-2026-0459.** Requires a crafted database plus WAL file opened by the
  victim, and is local-only. sqlx 0.9 still pins `libsqlite3-sys <0.38.0`; no compatible fixed
  release exists. Operators can build with `sqlite-unbundled` against a patched system SQLite if
  they need the fix sooner.
- **paste / RUSTSEC-2024-0436.** Unmaintained, build-time macro dependency of rmcp 0.1.5. No
  runtime component and no known vulnerability.
- **Loopback SSE without a token.** Any local process can call the tools. That is the intended
  local-development mode; the threat addressed here is a remote web page, which the Host/Origin
  checks and the optional token stop.
- **Plaintext HTTP.** TLS is delegated to a reverse proxy; the token and JSON-RPC bodies are not
  encrypted on the wire by this crate.

## Tests

New and changed layers:

- **`tests/sse.rs` (new).** In-process server on `127.0.0.1:0`, driven by a small HTTP/1.0
  client over `tokio::net::TcpStream` (HTTP/1.0 closes the stream at EOF, so no chunked-body
  parser is needed):
  - rejects a foreign `Host` on `GET /sse` and `POST /message` with `403`,
  - rejects a foreign `Origin` with `403`, rejects `Origin: null`, allows an absent Origin,
  - requires the bearer token when configured, on both routes (`401`),
  - accepts the correct token and completes a full `initialize` → `notifications/initialized`
    → `tools/call add` handshake over the event stream,
  - returns `404` for an unknown session, `400` for malformed JSON, `413` for an oversized
    body, `429` when the inbound queue is full, and `503` when the session cap is reached,
  - removes a session after its stream closes (poll until the POST is `404`),
  - `Settings::from_config` refuses a non-loopback bind without a token or without explicit
    hosts, accepts a loopback bind without either, and `serve` uses the listener's port for the
    default host list (`127.0.0.1:0` works).
- **`tests/config.rs`.** Layering of all four new settings through the file and environment
  layers, comma-list parsing (including the empty-entry error), validation failures, token
  redaction in `Debug`, and `sse_max_sessions == 0`.
- **`tests/store_offline.rs`.** Dead-database errors no longer contain the URL, password, host,
  port, or driver text; the warning path is exercised through the public API.
- **`tests/sqlite_backend.rs`.** Retention keeps exactly the newest `max_history_rows` rows;
  `Store::list` clamps an oversized limit; a newly created file is mode `0600` on Unix; an
  in-memory store creates no file.
- **`tests/server.rs`.** Subscription cap accepts the first N and rejects N+1 with
  `invalid_params`; re-subscribing is still free.
- **`tests/db.rs`.** `redact_url` cases (credentials, query, malformed, IPv6) through
  `db::testing`; the per-dialect retention SQL contains the expected placeholder.
- **Existing suites.** Update any assertion that depended on raw SQLx error text. The protocol
  and feature suites should otherwise pass unchanged, proving the transport and configuration
  changes did not alter the MCP surface.

## Verification

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build
cargo audit
```

Manual checks after the suite: `scripts/verify-backend.sh` against the devcontainer PostgreSQL and
MySQL (the latter now authenticates through `mysql-rsa`), and one `--sse` run with a token and an
explicit host list to confirm the logged security mode.

## What is left to the builder

- Exact module-private types and naming in `sse.rs`.
- Where the HTTP/1.0 test client helpers live inside `tests/sse.rs` (they stay local to that
  file).
- The precise `SseConfigError` variants, as long as `Display` names the offending setting.
- Whether `redact_url` grows to handle percent-encoded userinfo; the initial implementation
  redacts everything before the last `@` in the authority and drops the query, which covers the
  cases the server itself accepts.
- The wording of the new log lines and security mode banner.
