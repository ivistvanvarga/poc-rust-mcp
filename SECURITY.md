# Security Policy

## Supported versions

Only the latest `0.x` release is supported with security updates. This is a proof of concept with a
single maintainer; there are no long-term branches and no backports.

## Reporting a vulnerability

Report privately through GitHub's [private security advisories][advisories]:

<https://github.com/ivistvanvarga/poc-rust-mcp/security/advisories/new>

Please include the version, the transport in use (`stdio` or SSE), the configuration relevant to the
issue (bind address, whether a token and allowlists are set), and a reproduction. Do not open a
public issue for anything that could be exploited before a fix ships.

[advisories]: https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability

## Threat model

- **`stdio` is local and trusted.** A client spawns the process as a subprocess; anything that can
  reach its stdin already runs as the same user and can run arbitrary code. No authentication is
  applied on this transport, and none would mean anything.
- **SSE is network-exposed.** The default bind is `127.0.0.1:8000`, but `--sse`/`MCP_SSE_ADDRESS`
  can put it on a LAN or the internet. The threat the transport is hardened against is a **remote
  web page** reaching a loopback- or LAN-bound server, classically through **DNS rebinding**.
- **Loopback without a token is local-trust.** Any local process can call the tools. That is the
  intended local-development mode; the optional token is enforced when set, even on loopback.
- **Non-loopback requires a token and an explicit host list.** The server refuses to serve beyond
  loopback without `MCP_SSE_TOKEN` and `MCP_SSE_ALLOWED_HOSTS`, because it cannot know which names or
  addresses remote clients will use and guessing would defeat the check.
- **TLS is not terminated here.** The transport is plaintext HTTP; operators who need encryption put
  a TLS-terminating reverse proxy in front and forward the original `Host` header.

## What is enforced

The SSE transport (`src/sse.rs`) replaces rmcp 0.1.5's own, which performs no validation at all
(see [RUSTSEC-2026-0189][rsec-0189] below). Every request on both routes — `GET /sse` and
`POST /message` — passes the same chain before a handler runs:

- **`Host` allowlist.** The request authority is taken from the `Host` header, or from the URI
  authority (HTTP/2 `:authority`) when the header is absent. Hosts are compared after lowercasing and
  stripping one trailing dot; userinfo and zone IDs are rejected. An entry with a port matches only
  that port; an entry without one matches any port on that host. A missing, malformed, or unlisted
  authority is `403`. A loopback bind defaults to `localhost`, `127.0.0.1` and `[::1]` pinned to the
  bound port.
- **`Origin` allowlist (restriction only).** An absent `Origin` is normal for CLI clients and is
  allowed; `Origin: null` is rejected. With no `sse_allowed_origins`, an origin is accepted only when
  its host equals the already-validated request host, and its effective port matches when the request
  authority carried one — which keeps a TLS proxy working without letting a malicious page match. The
  server emits **no CORS headers** and answers **no preflight**; the list is not a CORS policy.
- **Header-only bearer token.** `Authorization: Bearer <token>` is required on both routes when
  `MCP_SSE_TOKEN` is set, compared in **constant time** with the length folded into the comparison.
  The token is never accepted from a query string, and there is deliberately no CLI flag for it. A
  missing or wrong token is `401` with `WWW-Authenticate: Bearer`.
- **Bounded resources.** Concurrent SSE sessions (default 64), each session's inbound queue (32
  messages), and request bodies (256 KiB) are capped; a subscription set is capped at 256 per client;
  pending fire-and-forget history writes are capped at 64; and stored history is pruned to
  `max_history_rows` after every insert.
- **Owner-only SQLite files.** On Unix, a history file this process creates is mode `0600`. Existing
  files are never chmodded — tighten one yourself with `chmod 600 path/to.db`.
- **Coarse, non-leaking errors.** Client-facing storage errors never echo the URL, credentials, host,
  or driver text; the full error stays in the server log. Database URLs are redacted before logging.

## Accepted risks

Each is a deliberate, documented trade-off rather than an oversight.

- **rmcp 0.1.5 — [RUSTSEC-2026-0189][rsec-0189].** The vulnerable component is the Streamable HTTP
  *server* transport, which this crate does not use; the replacement in `src/sse.rs` performs the
  `Host`/`Origin`/token validation rmcp omits. Upgrading rmcp would rewrite the tool layer, which is
  out of scope.
- **Bundled SQLite — [RUSTSEC-2026-0459][rsec-0459].** Requires a crafted database plus WAL file that
  the victim opens, and is local-only. The fix ships in `libsqlite3-sys 0.39.0`, but sqlx 0.9 requires
  `>= 0.30.1, < 0.38.0` (the lock resolves `0.37.0`), so no compatible fixed release exists. Operators
  who need the fix sooner can build against a patched system SQLite with `sqlite-unbundled`.
- **`paste` — [RUSTSEC-2024-0436][rsec-0436].** Unmaintained, not vulnerable: a build-time macro
  dependency of rmcp 0.1.5, with no runtime component.
- **Loopback SSE without a token.** Any local process can call the tools. Intended for local
  development; the remote-web-page threat is what the `Host`/`Origin` checks and the optional token
  address.
- **Plaintext HTTP.** TLS is delegated to a reverse proxy, so the token and JSON-RPC bodies are not
  encrypted on the wire by this crate.

## Dependency audit

`cargo audit` must exit 0. Reviewed advisory ignores live in [`.cargo/audit.toml`](.cargo/audit.toml),
each with a written rationale, and are limited to the accepted risks above. Adding an ignore requires
a reason in that file and in `AGENTS.md` — an unreviewed silence is not an option.

[rsec-0189]: https://rustsec.org/advisories/RUSTSEC-2026-0189.html
[rsec-0459]: https://rustsec.org/advisories/RUSTSEC-2026-0459.html
[rsec-0436]: https://rustsec.org/advisories/RUSTSEC-2024-0436.html
