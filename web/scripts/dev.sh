#!/usr/bin/env bash
# Serve the app, the WebSocket agent bridge and the MCP endpoint from the Rust
# server: no bundler, no transpiler, plain ES modules.
#
# The server sets Cache-Control: no-store on every response, because a stale
# main.js or wasm in the browser looks exactly like "the new feature isn't
# there" — reloads then appear to do nothing until a hard refresh.
set -euo pipefail
cd "$(dirname "$0")/.."

bash scripts/watch-wasm.sh &
WATCH_PID=$!
trap 'kill "$WATCH_PID" 2>/dev/null || true' EXIT

echo "rs-canvas: http://localhost:5173 (plain files; reload after Rust changes)"
cargo run --manifest-path ../Cargo.toml -p canvas-server -- \
  --web . --port 5173 --doc ../.rs-canvas/current.json
