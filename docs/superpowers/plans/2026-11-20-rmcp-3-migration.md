# rmcp 3.10 migration — implementation plan

Date: 2026-11-20
Spec: `docs/superpowers/specs/2026-11-16-rmcp-3-migration-design.md` (approved)
Branch: `rmcp-3-migration` (in place)

## Decisions fixed by this plan (spec left these to the builder)

- **Peer plumbing:** keep `peer: Arc<Mutex<Option<Peer<RoleServer>>>>` and the `peer()`/`announce_write()`
  helpers unchanged. The hand-written `call_tool` stores `context.peer.clone()` into `self.peer`
  before dispatching to the router. Only `get_peer`/`set_peer` leave the `ServerHandler` impl.
  Smallest diff; no tool signature changes; direct-handler tests keep working.
- **Request structs:** `AddRequest`, `SubRequest`, `MulRequest` (fields `a`, `b`, `i32`), each local to
  `src/server.rs`. Keep `DivisionRequest`/`HistoryRequest` names.
- **`Calculator` keeps `#[derive(Clone)]`** — harmless, and the SSE/main session factories may clone.
- **`supported_protocol_versions()`** returns
  `Cow::Borrowed(ProtocolVersion::known_up_to(&ProtocolVersion::V_2025_11_25))`. Do not override
  `discover`; the blanket `discover` already rejects a `server/discover` opener for a version we do
  not advertise (`unsupported_protocol_version`).
- **Deprecations:** `#[allow(deprecated)]` with an explanatory comment at module scope in
  `src/server.rs` (LoggingLevel, logging capability, subscribe/unsubscribe are deprecated by
  SEP-2577 but are the live 2025-11-25 mechanisms), and at the two `tests/support`/`tests/server`
  spots that name `LoggingLevel`. Clippy runs `-D warnings`.
- **Commits:** one migration commit (dependency + code + tests), one docs commit. `AGENTS.md` currently
  has an uncommitted 0.1-era rewrite; fold its 3.x form into the docs commit.

## Task 1 — Pin the wire contract with tests (red first)

Files: `tests/mcp_features.rs`, `tests/server.rs`, `tests/support/mod.rs`.

1. In `reading_the_history_resource_degrades_when_storage_is_off`, flip the assertion from "mimeType
   absent" to "`content["mimeType"] == "text/plain"` and no `mime_type` key". Update the comment to
   say rmcp 3.x fixes the enum-level `rename_all` bug and this pins the correct direction. **Run
   `cargo test --test mcp_features` and watch it fail** — this is the RED.
2. Add `tool_input_schemas_are_unchanged` to `tests/server.rs`: via `Calculator::get_tool(name)` from
   `ServerHandler`, assert each of `add`, `sub`, `mul`, `div`, `calc_history` has an object input
   schema with the exact expected property names and JSON types (`integer` for i32/i64, `number`
   for f64) and the expected `required` list. It should pass against 0.1 already (characterization
   test), proving the `Parameters<T>` rewrite did not move the wire contract.
3. Do **not** yet change type names or helpers — those move with the dependency in Task 2.

## Task 2 — Dependency and audit posture

Files: `Cargo.toml`, `Cargo.lock`, `.cargo/audit.toml`.

- `rmcp = { version = "3.10", features = ["server", "transport-io"] }` (defaults `base64`/`macros`
  are wanted). No other dependency changes.
- Remove ignores `RUSTSEC-2026-0189` and `RUSTSEC-2024-0436`; keep `RUSTSEC-2026-0459` verbatim.
- Run `cargo update -p rmcp` (regenerate `Cargo.lock`). Expect the workspace not to compile until
  Task 3 lands — that is normal for a major dependency bump.

## Task 3 — Tool and handler layer (`src/server.rs`)

Exact edits:

- Imports: `ErrorData as McpError`, `tool_router`, `tool_handler`, `handler::server::wrapper::Parameters`,
  `model::{...}` with renamed params types, `service::{Peer, RequestContext, RoleServer}`,
  `model::{ProtocolVersion, ServerConfig}`. Keep explicit imports (no `model::*`).
- `#[tool(tool_box)] impl Calculator` → `#[tool_router] impl Calculator`. Add field
  `tool_router: ToolRouter<Self>`; both constructors set `Self::tool_router()`.
- `tool_names()` → `self.tool_router.list_all()`.
- `add`/`sub`/`mul`: `Parameters<AddRequest>` etc., destructure `Parameters(AddRequest { a, b })`.
  `div`/`calc_history`: `Parameters<DivisionRequest>` / `Parameters<HistoryRequest>`.
- `impl ServerHandler for Calculator` gets `#[tool_handler]` (generates only `get_tool`, since the
  block hand-writes `list_tools`, `call_tool`, `get_info`).
- `page<T>(items, params: Option<&PaginatedRequestParams>, page_size) -> Result<_, McpError>` with
  `McpError::invalid_params(...)`.
- `list_tools(&self, Option<PaginatedRequestParams>, RequestContext<RoleServer>) -> Result<ListToolsResult, McpError>`:
  `ListToolsResult::with_all_items(tools)`, set `next_cursor`.
- `call_tool(&self, CallToolRequestParams, RequestContext<RoleServer>) -> Result<CallToolResponse, McpError>`:
  clone `context.peer` into `self.peer` via `call_tool` before building `ToolCallContext`, then
  `self.tool_router.call(...)`.
- Rename all `list_prompts`, `get_prompt`, `list_resources`, `list_resource_templates`,
  `read_resource`, `subscribe`, `unsubscribe`, `set_level`, `complete` param types to `*RequestParams`;
  use `with_all_items` for the three list results; keep the exact `invalid_params` messages and the
  subscription cap. Keep `on_cancelled`/`on_progress` bodies (add `NotificationContext` arg per 3.x).
- Remove `get_peer`/`set_peer`.
- `get_info() -> ServerConfig`: `ServerConfig::new(ServerCapabilities::builder()...build())
  .with_protocol_version(ProtocolVersion::V_2025_11_25)
  .with_server_info(Implementation::new(...))
  .with_instructions("...")` (same string, same capability set). Keep the comment explaining why
  `*ListChanged`, completions and 2026-07-28 features are absent; note logging is deprecated but live.
- Add `supported_protocol_versions()` returning the `Cow` expression above.
- Keep `rank()` unchanged (LoggingLevel still has no `Ord`).

## Task 4 — Resources `mimeType` fix (`src/resources.rs`)

- Import `ErrorData as McpError`.
- `text()` sets `mime_type: Some(MIME_TYPE.to_owned())`; replace the bug-workaround doc comment with
  a short note that rmcp 3.x serialises `mimeType` correctly and `tests/mcp_features.rs` pins it.

## Task 5 — SSE transport refit (`src/sse.rs`) and main

- Only import paths / renamed types; no behaviour, validation order, status mapping or session logic
  changes. Check `RxJsonRpcMessage`, `TxJsonRpcMessage`, `ClientJsonRpcMessage`, `ServiceExt::serve_with_ct`,
  `IntoTransport for (Si, St)` still line up; rename only if the compiler demands it.
- `src/main.rs` needs no change unless a renamed rmcp import appears.

## Task 6 — Test support refit for 3.x

- `tests/support/mod.rs`: rebuild `request_context()`. In 3.10 `Peer::new` is `pub(crate)` and
  `RequestContext` is `#[non_exhaustive]`, so define a minimal `struct Noop; impl ServerHandler for Noop {}`
  inside the helper, spin `serve_directly_with_ct::<RoleServer, Noop, _, _, _>(Noop, tokio::io::duplex(256),
  None, CancellationToken::new())`, clone `running.peer()`, keep the handle alive, and return
  `RequestContext::new(RequestId::Number(0), peer)`. Drop the now-unused `AtomicU32RequestIdProvider`,
  `Peer`, `ClientInfo`, `InitializeRequestParam` imports. Keep `PROTOCOL_VERSION = "2024-11-05"` (the
  server negotiates it for that request).
- `tests/server.rs`: import `CallToolRequestParams`, `CallToolResponse`, `PaginatedRequestParams`,
  `SetLevelRequestParams`, `UnsubscribeRequestParams`, etc. `call_param()` builds
  `CallToolRequestParams { name, arguments, ..Default::default() }` (struct is `#[non_exhaustive]` —
  use a struct-update or constructor; pick whichever compiles). `call_result()` matches
  `CallToolResponse::Complete(inner)` then `serde_json::to_value(inner)`. `cursor()` returns
  `Some(PaginatedRequestParams { cursor: Some(..), ..Default::default() })`.
- `tests/sse.rs`: `ClientJsonRpcMessage` import path stays if valid; run its test to confirm.
- `tests/resources.rs`, `tests/prompts.rs`: model type names check; change only what fails.
- `tests/stdio_protocol.rs`: comment only ("rmcp 0.1.5" → "rmcp 3.10"); assertions unchanged since
  the server negotiates `2024-11-05` for the test's requested version.

## Task 7 — Compile-fix loop and full test pass

- `cargo test` until green. Use `cargo test --test server`, `cargo test --test mcp_features
  pages_without_gaps`, `cargo test --test sqlite_backend` as focus loops.
- Confirm Task 1's flipped mimeType assertion now passes and the new schema test still passes.
- Confirm all existing behavioural assertions (pagination, subscriptions, log filter, completion,
  tool-error-vs-protocol-error split) pass unchanged.

## Task 8 — Documentation

- `AGENTS.md`: replace the 0.1-specific traps with 3.x rules — `#[tool_router]` + `#[tool_handler]`
  (handler fills only missing methods, so hand-written sorted/paginated `list_tools`/`call_tool`
  stay), `Parameters<T>`, protocol ceiling 2025-11-25 and why, peer via `RequestContext`/stored clone,
  `ErrorData`, `ServerConfig`, `mimeType` fixed, `LoggingLevel` still no `Ord`, tool annotations /
  `outputSchema` / icons exist but unused, audit ignores down to one.
- `SECURITY.md`: move the four GHSA advisories + RUSTSEC-2026-0189 + RUSTSEC-2024-0436 out of
  "Accepted risks" into a "fixed by the 3.10 upgrade" note; keep the SQLite, loopback-token and
  plaintext-HTTP risks. Update "Dependency audit" wording accordingly.
- `README.md`: update every "rmcp 0.1.5"/"rmcp 0.1" mention (lines ~27, 235, 257, 327, 332-337,
  472), the worked-around SDK bug section, and add the 2025-11-25 protocol ceiling where features
  are described.
- Fold the already-modified `AGENTS.md` into this commit.

## Task 9 — Verification (before claiming done)

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build
cargo audit
```

All five must exit 0. Fix any warnings rather than suppressing them except the documented
`#[allow(deprecated)]` sites. Manual-only: `scripts/verify-backend.sh` and one `--sse` run with a
token and explicit host list (no live DB in this environment).
