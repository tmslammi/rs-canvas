# Paint orchestration lives in core; the shell owns the surface

`canvas-wasm` held the scene's paint policy: the full-detail and LOD paths,
dirty-rect merging and clipping, the all-visible fast path, and the
camera-snapshot guard that keeps a pan from composing onto stale pixels. That
logic is browser-independent and was only testable through Playwright. It now
lives in `canvas-core::paint`:

- **`Surface`** carries the few surface facts orchestration needs: device pixel
  ratio, viewport size in CSS pixels and the background color. It has no
  Canvas2D or DOM types.
- **`render_full` / `render_lod` / `render`** choose the all-visible fast path
  or visible layers, skip hidden presentation elements and the text element
  under the live editor, and apply the camera + DPR transform. LOD groups
  bounds by palette index and submits one batched fill per color.
- **`paint_dirty`** intersects dirty rects with the viewport, merges them,
  clears the merged device rects, clips the redraw to the padded cleared
  region, and redraws only layers intersecting the merged rects. The
  higher-layer-over-lower-layer rule is therefore enforced by the same code
  that runs in the page.
- **`merge_rects`**, **`ViewSnapshot`** and **`guard_snapshot`** own the
  deterministic merge and the scroll/zoom last-paint guard.
- **`RecordingRenderer`** implements `Renderer` natively and records every
  call in order, so paint policy has native tests instead of pixel probes.

The `Renderer` seam gained one primitive: `fill_rects(&[Rect], color)`. LOD
used to reach through `Canvas2dRenderer::ctx` to build a Canvas2D path; now
each backend implements the batch. Canvas2D keeps the same one-path/one-fill
submission and the same 0.5-unit hairline minimum, and the recorder sees the
batch as one call.

`canvas-wasm` remains the browser adapter: it owns the canvases, input,
Canvas2D state cache, text measurement, IndexedDB and screenshot capture. Grid
and overlay painting stay there because they are inherently Canvas2D. The pan
pixel-shift fast path also stays because it calls `drawImage`; core still owns
the snapshot type and guard it consults.

## Consequences

- The page and native tests run the same paint policy; a dirty-rect or LOD
  regression can be asserted by call order instead of reading canvas pixels.
- The LOD path no longer clones the palette strings per frame: it borrows the
  scene palette and reuses one rect scratch buffer across colors, so there is
  no per-rect allocation.
- Canvas2D behavior is unchanged: same transform, clear, clip, palette order,
  fill alpha and hairline sizes.
- A backend that cannot batch rects can still implement `fill_rects` by
  issuing one fill per rect; the seam does not require Canvas2D.

## Considered options

- **Keep orchestration in `canvas-wasm` and test it through Playwright** —
  rejected: the rules that fail most visibly (clip padding, layer
  intersection, snapshot invalidation) are cheap to test natively and
  expensive to diagnose from pixels.
- **Pass Canvas2D contexts into core behind a trait with `ctx` escape
  hatches** — rejected: it recreates the browser dependency behind a different
  name and lets backends bypass the display-list seam.
- **Move grid, overlay and pan pixel-shifting through the seam too** —
  rejected: they are canvas operations, not scene paint policy; forcing them
  through a browser-free interface would add abstractions with no native use.
- **Make `fill_rects` take an iterator** — rejected: a slice plus one reused
  scratch buffer keeps the trait object-safe and the per-frame allocation
  count at zero for the Canvas2D path.
