#!/usr/bin/env bash
# Rebuild the wasm bundle whenever Rust sources change (dev only).
set -uo pipefail
cd "$(dirname "$0")/../.."

MARKER=target/.wasm-watch-marker
mkdir -p target
[ -f "$MARKER" ] || touch "$MARKER"

while true; do
  changed=$(find crates Cargo.toml -type f \( -name '*.rs' -o -name 'Cargo.toml' \) -newer "$MARKER" -print -quit 2>/dev/null || true)
  if [ -n "$changed" ]; then
    echo "[watch-wasm] rebuilding wasm after $changed"
    bash web/scripts/build-wasm.sh
    status=$?
    touch "$MARKER"
    if [ "$status" -ne 0 ]; then
      echo "[watch-wasm] build failed — save again to retry"
    else
      echo "[watch-wasm] wasm rebuilt; reload the page"
    fi
  fi
  sleep 1
done
