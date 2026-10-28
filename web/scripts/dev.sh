#!/usr/bin/env bash
# Serve the app, the WebSocket agent bridge and the MCP endpoint from the Rust
# server. No bundler or transpiler: plain ES modules plus the wasm bundle.
#
# The server sends Cache-Control: no-store, so a reload picks up a rebuilt
# wasm bundle.
set -euo pipefail
cd "$(dirname "$0")/.."

bash scripts/watch-wasm.sh &
WATCH_PID=$!
trap 'kill "$WATCH_PID" 2>/dev/null || true' EXIT

echo "rs-canvas: http://localhost:5173 (plain files; reload after Rust changes)"
cargo run --manifest-path ../Cargo.toml -p canvas-server -- \
  --web . --port 5173 --doc ../.rs-canvas/current.rsc
