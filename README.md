# rs-canvas

A local-first browser drawing tool for freehand sketching and GCP / cloud architecture diagrams, in a hand-drawn style.

Rust core compiled to WebAssembly owns the document model, interaction logic and Canvas2D rendering; a thin vanilla-TypeScript shell provides the browser UI.

## Layout

- `crates/canvas-core` — pure-Rust document model, geometry, rendering pipeline (no browser dependencies)
- `crates/canvas-wasm` — wasm-bindgen façade (`Editor`) and the Canvas2D renderer
- `web/` — the front end: **plain ES modules, no build step** (like DHH's no-build setups)

## Development

Prerequisites: Rust with the `wasm32-unknown-unknown` target, `wasm-bindgen-cli`, Node 24+ (only for the Playwright test runner).

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli

cd web
npm install
npm run dev        # build wasm once, watch Rust, serve plain files on :5173
npm run build      # build wasm + stage web/dist for deployment (a file copy, no bundler)
npm run test:e2e   # build wasm + run Playwright against the source files
```

The app is served as-is: `index.html` loads `/src/main.js` as an ES module, the wasm-bindgen `--target web` output is imported directly, and CSS/fonts are plain files. There is no Vite, no TypeScript and no bundler in the app — `npm run dev` also watches Rust sources and rebuilds the wasm for you (reload the page after a rebuild).

Core tests run natively:

```sh
cargo test --workspace
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
```

## Licensing

MIT (see `LICENSE`). GCP product names are used nominatively; the shape library contains original artwork only.
