#!/usr/bin/env bash
# Manual verification helper: drive the real binary over stdio against a live database.
#
#   ./scripts/verify-backend.sh 'mysql://mcp:mcp@127.0.0.1:3306/mcp'
#
# Not part of `cargo test` — see AGENTS.md: PostgreSQL and MySQL need running services, so they are
# verified against the devcontainer by hand rather than in the suite.
set -euo pipefail

URL="${1:?usage: verify-backend.sh <database-url>}"
BIN=target/debug/poc-rust-mcp

[ -x "$BIN" ] || { echo "run 'cargo build' first" >&2; exit 1; }

{
  printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"verify","version":"0.0.1"}}}'
  printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  printf '%s\n' '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"add","arguments":{"a":2,"b":40}}}'
  printf '%s\n' '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"db_status","arguments":{}}}'
  printf '%s\n' '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"calc_history","arguments":{"limit":10}}}'
  printf '%s\n' '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"div","arguments":{"dividend":9.0,"divisor":0.0}}}'
  printf '%s\n' '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"clear_calc_history","arguments":{}}}'
  printf '%s\n' '{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"db_status","arguments":{}}}'
  sleep 2
} | RUST_LOG=warn "$BIN" --db-url "$URL"
