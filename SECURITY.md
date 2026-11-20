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

The SSE transport (`src/sse.rs`) replaces rmcp's own transports, which perform no
`Host`/`Origin`/token validation. Every request on both routes — `GET /sse` and `POST /message` —
passes the same chain before a handler runs:

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

## Resolved by the rmcp 3.10 upgrade

The crate used to pin rmcp 0.1.5, whose affected features this crate never enabled. The upgrade to
3.10 removes the affected code entirely, so these advisories are no longer accepted risks and no
longer appear in `.cargo/audit.toml`:

- [RUSTSEC-2026-0189][rsec-0189] / [GHSA-9pj6-vhgr-3mwh][ghsa-9pj6] — Streamable HTTP *server*
  transport session-table leak.
- [GHSA-33f5-2c5q-wgwj][ghsa-33f5] and [GHSA-c9xm-49cp-xcr9][ghsa-c9xm] — OAuth protected-resource
  metadata discovery and the OAuth client.
- [GHSA-9g45-5xwm-f3wc][ghsa-9g45] — HTTP client leaking headers on a cross-origin redirect.
- [RUSTSEC-2024-0436][rsec-0436] — `paste`, an unmaintained build-time macro dependency of rmcp 0.1.5
  (replaced by `pastey` in 3.x).

The custom SSE transport stays for defence in depth: it validates `Host`, `Origin` and bearer tokens
that rmcp's transports do not, not because an advisory is outstanding.

## Accepted risks

Each is a deliberate, documented trade-off rather than an oversight.

- **Bundled SQLite — [RUSTSEC-2026-0459][rsec-0459].** Requires a crafted database plus WAL file that
  the victim opens, and is local-only. The fix ships in `libsqlite3-sys 0.39.0`, but sqlx 0.9 requires
  `>= 0.30.1, < 0.38.0` (the lock resolves `0.37.0`), so no compatible fixed release exists. Operators
  who need the fix sooner can build against a patched system SQLite with `sqlite-unbundled`.
- **Loopback SSE without a token.** Any local process can call the tools. Intended for local
  development; the remote-web-page threat is what the `Host`/`Origin` checks and the optional token
  address.
- **Plaintext HTTP.** TLS is delegated to a reverse proxy, so the token and JSON-RPC bodies are not
  encrypted on the wire by this crate.

## Dependency audit

`cargo audit` must exit 0. The one reviewed advisory ignore lives in
[`.cargo/audit.toml`](.cargo/audit.toml) with a written rationale, and covers only the accepted risk
above. Adding an ignore requires a reason in that file and in `AGENTS.md` — an unreviewed silence is
not an option.

GitHub's Dependabot alerts are scored against the GitHub Advisory Database, a superset of RustSec, so
they can list an advisory `cargo audit` never sees. The `rmcp` and `paste` alerts were GitHub-only or
resolved upstream by the 3.10 upgrade (see [Resolved](#resolved-by-the-rmcp-3100-upgrade) above); the
remaining `libsqlite3-sys` alert is [RUSTSEC-2026-0459][rsec-0459], already an ignore.

[rsec-0189]: https://rustsec.org/advisories/RUSTSEC-2026-0189.html
[rsec-0459]: https://rustsec.org/advisories/RUSTSEC-2026-0459.html
[rsec-0436]: https://rustsec.org/advisories/RUSTSEC-2024-0436.html
[ghsa-9pj6]: https://github.com/advisories/GHSA-9pj6-vhgr-3mwh
[ghsa-33f5]: https://github.com/advisories/GHSA-33f5-2c5q-wgwj
[ghsa-c9xm]: https://github.com/advisories/GHSA-c9xm-49cp-xcr9
[ghsa-9g45]: https://github.com/advisories/GHSA-9g45-5xwm-f3wc
