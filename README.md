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
npm run dev        # build wasm once, watch Rust, serve the app + agent bridge on :5173
npm run build      # build wasm + stage web/dist for deployment (a file copy, no bundler)
npm run test:e2e   # build wasm + run Playwright against the source files
```

The dev command runs the Rust server in `crates/canvas-server`: it serves the
plain files, hosts the WebSocket bridge for the open page, and exposes the
agent's MCP endpoint at `http://127.0.0.1:5173/mcp`. The project's
`opencode.jsonc` points opencode at that endpoint; start the dev command before
using the canvas tools. The server binds to localhost only.

## Agent (MCP)

With `npm run dev` running, opencode can draw, explain and look at the canvas:
`canvas_read` (scene graph with revision cursors), `canvas_draw` and
`canvas_pen` (shapes, and freehand strokes with per-point pressure and ink
style — width, colour, roughness, dash, opacity, fill/shading),
`canvas_script` (ordered beats with handles, automatic placement, captions and
camera focus — presented beat by beat on the user's canvas),
`canvas_screenshot`, `canvas_focus`/`canvas_point`/`canvas_zoom`,
`canvas_annotate`/`canvas_emphasis`/`canvas_tidy`/`canvas_clear`, and
`canvas_status` (revision plus transient view hints). The page and the server
share one document through the merge-ready model, and the mirror persists to
`.rs-canvas/current.json`.

The toolbar's ✉ button opens an in-canvas chat backed by the local OpenCode
service: `canvas-server` discovers it from its registration file, creates one
session at the project root — so it inherits the canvas MCP config — and the
answers appear in the drawer, pinned to DeepSeek 4.1 Flash for now. The ◎
button opens live agent preferences the server enforces: camera moves
(off / gentle / full — focus skips targets that are already visible and never
zooms past 2× on gentle) and presentation pacing, persisted to
`.rs-canvas/settings.json`. Everything is local-only.

The app is served as-is: `index.html` loads `/src/main.js` as an ES module, the wasm-bindgen `--target web` output is imported directly, and CSS/fonts are plain files. There is no Vite, no TypeScript and no bundler in the app — `npm run dev` also watches Rust sources and rebuilds the wasm for you (reload the page after a rebuild).

Core tests run natively:

```sh
cargo test --workspace
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
```

## Licensing

MIT (see `LICENSE`). GCP product names are used nominatively; the shape library contains original artwork only.
