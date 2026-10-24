# rs-canvas

A local whiteboard for hand-drawn architecture diagrams, with an agent that can draw on the same canvas you do.

![A hand-drawn GCP architecture diagram in rs-canvas](docs/screenshot.png)

Ask for a diagram and the agent reads what is on the canvas, then adds and revises elements while you watch. Nothing it draws is special: drag it, relabel it or delete it like anything else. You can draw on the canvas yourself. The shape library has GCP icons and generic glyphs, and everything renders loose and hand-drawn.

## Install

macOS 12 or newer; Apple Silicon and Intel builds are published. The installer checks the release checksum and puts the binary in `~/.local/bin`. It carries the front end, so the installing machine needs no Rust, Node or checkout.

```sh
curl -fsSL https://raw.githubusercontent.com/tmslammi/rs-canvas/master/install.sh | bash
~/.local/bin/rs-canvas
```

The full path is for the first launch: the current shell may not have `~/.local/bin` on its PATH yet, but a new terminal will. The app opens at <http://127.0.0.1:5173>, and drawings live in `~/.rs-canvas`.

Linux and Windows are not supported yet.

To pin or roll back to a specific alpha, see [`docs/releases.md`](docs/releases.md).

## Agent bridge

The app serves MCP at <http://127.0.0.1:5173/mcp>, with a WebSocket to the open page behind it. An agent can read the drawing and your selection, add or change elements, point the camera at what it is talking about, take screenshots, open a workspace for the session, and record decisions and open questions.

The page owns the live document. The agent works on a mirrored copy and its edits cross over through the same deltas the browser uses, so there is no second document to reconcile. Camera moves are transient; any user input takes the view back. The server binds to localhost only.

Point any MCP client at the URL above; the checked-in `opencode.jsonc` wires up opencode. The design is in [`docs/adr/0005-agent-drives-canvas.md`](docs/adr/0005-agent-drives-canvas.md).

## Build from source

You need Rust with the `wasm32-unknown-unknown` target, `wasm-bindgen-cli` matching `Cargo.lock` (0.2.129), and Node 24+.

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.129
cd web
npm install
npm run dev
```

`npm run dev` builds the wasm bundle, watches Rust sources and serves the app at <http://localhost:5173>. Reload the page after a Rust rebuild.

## Controls

Space-drag, middle-drag or scroll to pan; Ctrl+wheel or pinch to zoom. `Shift+1` fits the drawing to the window and `Shift+0` resets the view. `G` toggles the grid, `Shift+G` toggles snapping, and Escape cancels the current gesture.

Double-click a shape, connector or frame to label it. Drag from a connection node to draw a bound arrow. Double-click a connector bend to remove it.

Tool keys and the rest of the shortcuts are in [`docs/controls.md`](docs/controls.md).

## Repo

`crates/canvas-core` holds the document model, the shared command layer, geometry, history, merge logic, glyph library and Canvas2D display list, with no browser dependencies. `crates/canvas-wasm` is the wasm-bindgen façade and renderer; `crates/canvas-server` serves the files, live sync, MCP tools and workspace storage; `web/` is the front end in plain ES modules.

Design decisions live in [`docs/adr/`](docs/adr/), and the domain vocabulary is in [`CONTEXT.md`](CONTEXT.md).

## Tests

```sh
cargo test --workspace
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cd web && npm run typecheck && npm run test:e2e
```

Set `RS_CANVAS_TEST_PORT=4273` to run the Playwright suite on a different port range.

Benchmarks are local-only: `cargo run --release --example perf -p canvas-core` and `cd web && npx playwright test massive-bench`.

## License

MIT, see [`LICENSE`](LICENSE). GCP product names are used nominatively; the shape library contains original artwork only.
