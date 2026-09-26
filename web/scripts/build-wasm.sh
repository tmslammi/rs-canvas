#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."

cargo build --release --target wasm32-unknown-unknown -p canvas-wasm
wasm-bindgen --target web --out-dir web/src/wasm \
  target/wasm32-unknown-unknown/release/canvas_wasm.wasm
