#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."

expected=$(awk '/^name = "wasm-bindgen"$/{getline; gsub(/version = |"/,""); print; exit}' Cargo.lock)
actual=$(wasm-bindgen --version | awk '{print $2}')
if [ "$actual" != "$expected" ]; then
  echo "wasm-bindgen $actual does not match Cargo.lock ($expected)" >&2
  echo "install it with: cargo install wasm-bindgen-cli --version $expected" >&2
  exit 1
fi

cargo build --release --locked --target wasm32-unknown-unknown -p canvas-wasm
wasm-bindgen --target web --out-dir web/src/wasm \
  target/wasm32-unknown-unknown/release/canvas_wasm.wasm
