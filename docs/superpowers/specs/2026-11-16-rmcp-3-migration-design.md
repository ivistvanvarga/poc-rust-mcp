# rmcp 3.x migration: design

Date: 2026-11-16
Status: awaiting approval
Scope chosen by the maintainer: **rmcp 3.10**, with the MCP protocol ceiling held at **2025-11-25**.

## Intent and why

The security-hardening branch left four GitHub advisories against `rmcp` 0.1.5 open. They are
documented as accepted risk in `SECURITY.md` for one reason: every one lives in an rmcp feature this
crate does not compile, and the only version-level fix is a major upgrade that rewrites the tool and
handler layer. `AGENTS.md` calls that out as a separate project. This design is that project.

It is worth doing now rather than never for three reasons:

- It is the only way the four rmcp alerts close. Reasoning about reachability is honest, but a
  version that no advisory matches is stronger, and Dependabot cannot be silenced from repo code.
- It retires the API debt `AGENTS.md` has accumulated around rmcp 0.1: the `#[tool(tool_box)]`
  inversion, the `ResourceContents` `mime_type` serialisation bug, `LoggingLevel`'s missing `Ord`,
  and the `rmcp`/`paste` audit ignores.
- 3.10 is where upstream lives. A 2.x stop would be redone.

The migration must not change what clients see. Every tool, prompt, resource, page, subscription
announcement, log-level filter and error mapping that works today must work identically afterwards,
and the test suite is the evidence.

## Goals

- `cargo audit` exits 0 with only the `libsqlite3-sys` ignore left.
- `rmcp` 3.10; all four GHSA advisories fixed by version, not by argument.
- The MCP surface is byte-for-byte compatible for existing clients: same tools and input schemas,
  same prompt and resource catalogues, same pagination, same logging filter, same subscription
  announcements, same tool-vs-protocol error split.
- The custom `src/sse.rs` transport is kept, with its validation invariants and its tests intact.
- The server advertises only the protocol versions whose lifecycle it actually implements.

## Non-goals

- **Adopting the 2026-07-28 inline lifecycle (SEP-2567).** That revision removes the `initialize`
  handshake, moves the lifecycle into per-request `_meta` behind a `server/discover` opener, makes
  `ping` and `resources/subscribe`/`unsubscribe` legacy-only, and replaces them with
  `subscriptions/listen`. That is a second rewrite of the handler and the protocol tests, and it is
  a separate project.
- **rmcp's own HTTP transports.** Streamable HTTP and rmcp's SSE server stay unused;
  `src/sse.rs` remains the only network transport.
- **New SDK features.** Tasks, skills, elicitation, OAuth, structured tool output, icons, tool
  annotations and resource links are available in 3.10 and are deliberately not adopted.
- **Any change to storage, configuration layering, or the database schema.**

## Anti-goals

- Advertising 2026-07-28 while its subscription lifecycle and `ping` are unimplemented. That is the
  exact "advertise a capability you do not have" failure `AGENTS.md` warns about.
- Letting the migration change a tool's wire contract: input schema, result shape, or the
  error-vs-`isError` split.
- Keeping an audit ignore whose advisory no longer matches.
- Reaching for `rmcp::model::*` wildcard imports to make the diff look small; the migration should
  be visible.

## Constraints

- Edition 2024. rmcp 3.10 requires Rust >= 1.88; `rust-toolchain.toml` pins `stable` and the local
  toolchain is 1.98.
- Every test stays in `tests/`; no `#[cfg(test)]` module in `src/`.
- The tool list stays sorted by name and paginated; the prompt and resource catalogues keep their
  declaration order. `server.list_page_size` keeps pagination reachable by a test.
- No new runtime dependency beyond what rmcp 3.10 itself pulls in.
- `Cargo.lock` stays committed.

## Approaches considered

Which rmcp to target:

1. **rmcp 2.2.** The smallest version that fixes the advisories. Less churn now, but 2.x is already
   superseded by 3.x, so the same work would very likely be repeated.
2. **rmcp 3.10 (chosen).** Latest release, matching the current SDK and the current spec. One pass
   clears the advisories and the 0.1-era API debt, and there is no second migration behind it.
3. **Do not migrate.** Keep relying on the accepted-risk rationale in `SECURITY.md`; the vulnerable
   features are not compiled. No code changes, but the alerts stay and can only be dismissed by
   reasoning.

Within 3.10, the protocol ceiling:

- **2025-11-25 (chosen).** `ProtocolVersion::LATEST_WITH_INITIALIZE` — the newest revision that
  still has the `initialize` handshake. This crate's handler is written against the session model
  (`initialize` → `notifications/initialized`, a per-session `Peer`, per-session subscriptions and
  log level). Capping here keeps that model, keeps the existing protocol tests meaningful, and lets
  the server advertise exactly what it implements.
- **2026-07-28.** The newest spec. Requires the SEP-2567 inline lifecycle, `subscriptions/listen`,
  and dropping `ping` — a lifecycle rewrite on top of the SDK migration.

All three versions clear the four advisories; the ceiling only decides how much of the new lifecycle
to adopt.

## Design

### 1. Dependency and audit posture

`Cargo.toml`:

```toml
rmcp = { version = "3.10", features = ["server", "transport-io"] }
```

The default features add `base64` and `macros`; both are wanted. No other dependency changes.

`.cargo/audit.toml` loses two of its three ignores:

- **RUSTSEC-2026-0189** (rmcp Streamable HTTP server transport) no longer matches — 3.10 is past the
  advisory's patched version.
- **RUSTSEC-2024-0436** (`paste` unmaintained) no longer matches — rmcp 3.x depends on `pastey`, not
  `paste`.
- **RUSTSEC-2026-0459** (`libsqlite3-sys`) stays: sqlx 0.9 still pins `libsqlite3-sys <0.38.0`.

`SECURITY.md`: the four rmcp GHSAs leave "Accepted risks" and become a line stating they are fixed
by the 3.10 upgrade. The `libsqlite3-sys`, loopback-without-token, and plaintext-HTTP risks stay.

Acceptance: `cargo audit` exits 0, and an ignore that no longer matches is a failure, not a warning
to leave in place.

### 2. Protocol version ceiling

`Calculator` overrides two things:

- `supported_protocol_versions()` returns `Cow::Borrowed(ProtocolVersion::known_up_to(&ProtocolVersion::V_2025_11_25))`
  — `2024-11-05`, `2025-03-26`, `2025-06-18`, `2025-11-25`.
- `get_info().protocol_version` is `V_2025_11_25` rather than the SDK default (`LATEST`).

Negotiation then behaves correctly for every client:

- A client that asks for a version in the list keeps it. In particular the existing tests, which
  send `protocolVersion: "2024-11-05"`, still receive `2024-11-05` — no protocol-version literal in
  the test suite has to change for the sake of the migration.
- A client that asks for `2026-07-28` (or anything newer) over `initialize` falls back to
  `2025-11-25` through the SDK's `negotiate_protocol_version`.
- A client that opens with `server/discover` (2026-07-28 only) is rejected cleanly with
  `unsupported_protocol_version`, rather than being handed a session whose lifecycle the server does
  not implement.

### 3. Tool layer

- `#[tool(tool_box)] impl Calculator` becomes `#[tool_router] impl Calculator`, and `Calculator`
  gains a `tool_router: ToolRouter<Self>` field. The macro also emits `fn tool_router()`, but that
  rebuilds the router on every call, so the constructors store one instance and the handler uses it.
- Per-argument extraction is gone. `#[tool(param)]` and `#[tool(aggr)]` are replaced by
  `Parameters<T>`, where `T` derives `serde::Deserialize` and `schemars::JsonSchema`:
  - `div` and `calc_history` already have a request struct (`DivisionRequest`, `HistoryRequest`).
  - `add`, `sub` and `mul` gain a two-field request struct each.
  The generated input schema is unchanged — an object with the same named properties — so the wire
  contract holds. This is asserted by a test, not assumed.
- Tool return values keep working unchanged: `String`, `Result<String, String>` (the `Err` arm
  becomes `isError: true`), and anything implementing `IntoCallToolResult`.

### 4. Handler layer

`impl ServerHandler for Calculator` stays hand-written, and the 0.1 hazard is gone: 3.x's
`#[tool_handler]` only generates the methods the block does not already define (it checks
`list_tools`, `call_tool`, `get_tool` and `get_info` before emitting anything). So:

- The hand-written `list_tools` (sorted, paginated) and `call_tool` stay. Adding `#[tool_handler]`
  to the block generates only `get_tool`, which rmcp's tool validation needs. This is the inverse of
  the 0.1 rule `AGENTS.md` documents, and the migration updates that rule.
- `list_tools` becomes `(&self, Option<PaginatedRequestParams>, RequestContext<RoleServer>) -> Result<ListToolsResult, ErrorData>`.
  `ListToolsResult` (and its prompt/resource siblings) now also carry `result_type`, `meta`,
  `ttl_ms` and `cache_scope`. The SDK clears the 2026-07-28-only fields when serialising to an
  older peer, so the wire output for our capped versions is unchanged. The `page()` helper keeps its
  cursor rule (index of the page's first item, decimal-encoded; an unparsable cursor is
  `invalid_params`; a past-the-end cursor is an empty final page).
- `rmcp::Error` becomes `rmcp::ErrorData` (aliased `McpError`) throughout, including `page()` and
  the `subscribe` cap.
- `get_info()` returns `ServerConfig` (the alias for `InitializeResult`) instead of `ServerInfo`;
  `server_info` is built with `Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))`;
  the capability builder is unchanged (`enable_tools`, `enable_prompts`, `enable_resources`,
  `enable_resources_subscribe`, `enable_logging`). `logging` is deprecated by SEP-2577 but is still
  the mechanism for `logging/setLevel`, so it stays with a comment naming the deprecation.
- **Peer plumbing changes.** `ServerHandler::get_peer`/`set_peer` no longer exist; a request's peer
  is reached through `RequestContext::peer`. The detached `announce_write` task still needs a
  `Peer<RoleServer>` after the request returns. Two shapes work; the builder picks one:
  - a recording tool declares a `Peer<RoleServer>` parameter (rmcp extracts it via
    `FromContextPart`) and threads it into `recorded_success`/`recorded_failure`/`announce_write`,
    removing the mutex; or
  - the hand-written `call_tool` stores `context.peer.clone()` into `self.peer` before dispatching
    to the router, keeping the current `Arc<Mutex<Option<Peer<RoleServer>>>>` shape and the
    smallest diff.

  Either way, `Calculator` no longer implements `get_peer`/`set_peer`.
- `subscribe`/`unsubscribe` are `#[deprecated]` in 3.10 but are still dispatched for a 2025-11-25
  session. They stay, under `#[allow(deprecated)]`, with a comment naming `subscriptions/listen` as
  their 2026-07-28 replacement. `set_level`, `complete`, `on_cancelled` and `on_progress` keep their
  current shapes.
- `ServerHandler` no longer requires `Clone`, so `Calculator` does not need `#[derive(Clone)]`; the
  per-session factory in `src/sse.rs` and `src/main.rs` is unchanged either way.

### 5. Custom SSE transport (`src/sse.rs`)

The MCP framing and every validation invariant are untouched: the `endpoint` event,
`POST /message?sessionId=…`, the validation order (authority → `Host` → `Origin` → token), the
status-code mappings (`403`/`401`/`415`/`413`/`429`/`410`/`503`), the session registry and the two
cleanup guards.

The 3.x transport layer still accepts a `(Sink, Stream)` pair (`IntoTransport for (Si, St)`), and
`RxJsonRpcMessage`, `TxJsonRpcMessage`, `ClientJsonRpcMessage` and `ServiceExt::serve_with_ct` all
still exist. So the refit is confined to import paths and any renamed model types; `Settings`, the
session types, the handlers and the `serve` signature do not change.

### 6. Resource `mimeType`

rmcp 3.10 fixes the `ResourceContents` enum-level `rename_all` bug that forced this crate to omit the
media type on `resources/read`: `mime_type` now serialises as `mimeType`, and the SDK ships a test
asserting exactly that. `src/resources.rs` therefore stops omitting the field and emits the media
type on reads again. `tests/mcp_features.rs` flips its pin from "the `mimeType` key is absent" to
"the `mimeType` key is present and correct", so the fix is pinned in the direction that matters and a
regression is still caught.

### 7. Tests

The migration changes type paths and a few constructors; it must not change a single behavioural
assertion. Specifically:

- **`tests/support/mod.rs`** — `PROTOCOL_VERSION` stays `"2024-11-05"`. `request_context()` is
  rebuilt for the 3.x `RequestContext`, which now carries `peer`, `id`, `meta`, `ct` and
  `extensions`.
- **`tests/server.rs`** — model names updated (`CallToolRequestParams`, `CallToolResponse`,
  `PaginatedRequestParams`, `SetLevelRequestParams`, `UnsubscribeRequestParams`), and the
  `call_param`/`cursor` helpers follow. Tools are still reached through the public
  `ServerHandler::call_tool`.
- **`tests/stdio_protocol.rs`** — capability assertions keep their intent; the `initialize`
  assertion uses the version the server negotiates for the requested `2024-11-05`.
- **`tests/sse.rs`** — the protocol-version literal and the `ClientJsonRpcMessage` path; the
  transport tests are otherwise unchanged.
- **`tests/prompts.rs`**, **`tests/resources.rs`**, **`tests/mcp_features.rs`** — model type paths,
  plus the `mimeType` flip.
- **New assertion** — a tool's advertised input schema is unchanged from 0.1 (same property names
  and types), proving the `Parameters<T>` rewrite did not move the wire contract.

Every existing assertion about pagination, subscription announcements, the log-level filter,
completion, and the tool-error-vs-protocol-error split must pass unchanged. That is the evidence
that the migration preserved the MCP surface.

### 8. Documentation

- **`AGENTS.md`** — replace the "`ServerHandler` impl block is NOT annotated `#[tool(tool_box)]`"
  section with the 3.x rule: `#[tool_router]` on the inherent block, `#[tool_handler]` on the
  handler block, which fills only the missing methods, so the hand-written sorted/paginated
  `list_tools` and `call_tool` stay. Replace the "rmcp 0.1.5 bugs" list: the `mime_type` bug is
  fixed, `LoggingLevel` still has no `Ord` (the `rank()` helper stays), and tool
  annotations/`outputSchema`/icons now exist but are unused. Replace the "rmcp is pinned to 0.1"
  section with the 3.10 rule, the protocol ceiling and its rationale, and the peer-plumbing change.
- **`SECURITY.md`** — the rmcp accepted-risk entries become "fixed by the rmcp 3.10 upgrade".
- **`README.md`** — any mention of the rmcp version or the MCP protocol revision.

## Verification

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build
cargo audit
```

Plus the manual checks the hardening branch already needed: `scripts/verify-backend.sh` against the
devcontainer PostgreSQL and MySQL, and one `--sse` run with a token and an explicit host list.

## What is left to the builder

- The peer-plumbing shape (a `Peer<RoleServer>` tool parameter versus a stored clone in `call_tool`).
- The request-struct names for `add`/`sub`/`mul`, and whether `DivisionRequest`/`HistoryRequest` are
  renamed for consistency.
- Whether `Calculator` keeps `#[derive(Clone)]`.
- The exact `Cow` expression for `supported_protocol_versions` and whether to narrow `discover`.
- Commit granularity within the branch (one migration commit versus staged commits per layer).
