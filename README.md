# rs-canvas

A local-first browser drawing tool for freehand sketching and GCP / cloud architecture diagrams, in a hand-drawn style.

Rust core compiled to WebAssembly owns the document model, interaction logic and Canvas2D rendering; a thin vanilla-TypeScript shell provides the browser UI.

## Layout

- `crates/canvas-core` — pure-Rust document model, geometry, rendering pipeline (no browser dependencies)
- `crates/canvas-wasm` — wasm-bindgen façade (`Editor`) and the Canvas2D renderer
- `web/` — Vite + vanilla-TypeScript shell

## Development

Prerequisites: Rust with the `wasm32-unknown-unknown` target, `wasm-bindgen-cli`, Node 24+.

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli

cd web
npm install
npm run dev        # build wasm, start Vite dev server
npm run build      # build wasm + production bundle
npm run test:e2e   # build and run Playwright tests
```

Core tests run natively:

```sh
cargo test --workspace
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
```

## Licensing

MIT (see `LICENSE`). GCP product names are used nominatively; the shape library contains original artwork only.
