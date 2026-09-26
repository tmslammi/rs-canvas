#!/usr/bin/env bash
# Serve the app as-is: no bundler, no transpiler, plain ES modules.
set -euo pipefail
cd "$(dirname "$0")/.."

bash scripts/watch-wasm.sh &
WATCH_PID=$!
trap 'kill "$WATCH_PID" 2>/dev/null || true' EXIT

echo "rs-canvas: http://localhost:5173 (plain files; reload after Rust changes)"
python3 -m http.server 5173 --bind 127.0.0.1
