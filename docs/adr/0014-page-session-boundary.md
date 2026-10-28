# The page session is browser-free; the wasm façade is its adapter

The browser page had accumulated sync-facing state inside `canvas-wasm`:
the base revision, the purge ledger, frames deferred during a gesture, the
outbound delta projection and agent presentation pacing. That state is not
about canvases or DOM events, and testing it required a browser. It moves into
`canvas-core::page::PageSession`, a browser-free collaborator with a frame API:

- **Inbound** — `apply_snapshot` / `apply_patch` decode a binary frame, apply
  it through the editor's merge entry points, and report the ids that changed
  (or that nothing did). A frame that arrives while a gesture is in flight is
  deferred instead; `drain_pending` / `apply_pending` return the deferred
  frames in application order at gesture end, after the local edit has
  committed (ADR-0011).
- **Outbound** — `take_outbound_patch` projects drained deltas into one patch
  and records purges in the ledger; `take_outbound_frame` adds the binary
  header. `snapshot_frame` folds the ledger into a full snapshot.
  `synced_document` is the document autosaves and snapshots persist.
- **Playback** — `start_presentation`, `advance_presentation` and
  `cancel_presentation` own the reveal frontier, caption, hold timing and
  outcome queue. Camera and highlight cues are returned to the caller; the
  session never touches a camera.

`canvas-wasm` remains the browser adapter: it owns the two canvases, input,
rendering, IndexedDB, the text overlay and screenshot capture. It delegates
the frame methods to `PageSession` and keeps the public JS API stable. The
live `Document` stays with `Editor` (ADR-0013); `PageSession` takes the editor
as a parameter and applies through its public merge/patch entry points rather
than becoming a second document owner.

`web/src/main.js` is now a composition root. `workspaceSession` owns the
workspace URL, switching, leave/checkpoint and rename/delete handoff;
`autosave` owns the debounce, unload checkpoint and IndexedDB save/load. The
editor input surface stays in `main.js`, which passes a `prepare` callback so
a workspace switch still commits the text edit and cancels gestures before the
outgoing document is stored.

## Considered options

- **Keep the state in `canvas-wasm` and test it through wasm-bindgen** —
  rejected: the sync rules are exactly the ones that need native unit tests,
  and a browser test cannot cover mid-gesture deferral cheaply.
- **Put `PageSession` in a new crate** — rejected: it depends only on
  `canvas-core` types (`Document`, `Patch`, `PurgeLedger`, `wire`) and a new
  crate would add a dependency edge without an independent boundary.
- **Move screenshot capture into the session** — rejected: a screenshot is a
  Canvas2D render of the live viewport; its request ids already live in the
  server's `PageHub` (ADR-0012). The browser adapter is the honest owner.
- **Let the session own the live document** — rejected: it would duplicate the
  committed-document owner, break `Editor`'s single-mutator invariant, and put
  scene invalidation on the wrong side of the boundary.
