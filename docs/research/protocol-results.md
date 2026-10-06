# Protocol results — patches, handles, compact reads

Outcome of implementing stages 1–2 of
[`llm-canvas-protocol.md`](./llm-canvas-protocol.md), measured with
[`protocol_bench`](../../crates/canvas-core/examples/protocol_bench.rs)
against the same fixture as the baseline: 10,000 live elements (7,000
freehand strokes, 280,603 points) + 2,000 tombstones, 20.29 MB of JSON.

Raw reports: [`protocol-baseline.json`](./protocol-baseline.json) (before)
and [`protocol-after.json`](./protocol-after.json) (after). Reproduce with:

```sh
cargo run --release --example protocol_bench -p canvas-core -- \
  --elements 10000 --tombstones 2000 --out /tmp/report.json
```

## Before / after

| Metric | Before | After | Change |
| --- | --- | --- | --- |
| Wire per user edit | 20.3 MB snapshot | 3.3 KB patch | ~6,000× smaller |
| Page CPU per edit | 436 ms (parse + reconcile + scene rebuild) | 0.08 ms (parse + merge + repaint changed) | ~5,700× |
| Server CPU per user edit | clone + reconcile + diff whole doc | O(changed) merge + log | O(document) → O(changed) |
| Disk writes | 20 MB per edit | debounced (500 ms) + flush on shutdown | bursts coalesce |
| Read: describe time | 6,277 ms (O(n²)) | 4.7 ms | ~1,300× |
| Read: characters | 1,109,183 | 902,239 raw ids / **528,976 handles** | 1.2× / 2.1× |
| Read: estimated tokens | 367,296 | 315,560 raw ids / **144,468 handles** | 1.2× / 2.5× |
| Snapshot frame (resync) | 22.11 MB as escaped string | 20.29 MB as object | 8.2% smaller |
| Snapshot resync CPU | 439 ms | 436 ms | unchanged (rare path) |

Token figures are heuristics (ids at ~2 chars/token, prose at ~4); the
direction is what matters. The single biggest token win is handles: about a
third of the old read was 36-char UUIDs (32.5% of characters on this
fixture, 38% on the live document), and every follow-up tool call now carries
`e12` instead.

Agent edits (`canvas_draw`, `canvas_pen`, `canvas_script`, the verbs) still
clone the mirror and diff it for the change log — O(document) at ~3 ms per
10k elements. Only the page-edit path is O(changed); making agent edits
report their touched ids is a follow-up.

The snapshot row is the connect/resync path, deliberately unchanged in cost;
what changed is that it is no longer the steady state. A gesture now sends
one element-granular patch (3.3 KB for a typical 40-sample stroke), merges
per element, and repaints only the changed layers.

## What was built

- **Patch frames both ways** (`crates/canvas-core/src/patch.rs`): a patch is
  full elements — tombstones included — merged per element with the existing
  LWW rules; idempotent and order-independent. `Purge` (undo of a creation)
  becomes a synthesized tombstone.
- **Deltas leave the page**: `History` keeps an outbox of every applied delta
  (commit, undo, redo); the wasm façade drains it as one
  `{"type":"patch","put":[...]}` frame per gesture
  (`web/src/agentBridge.js`, `web/src/main.js`). The bridge drains even while
  disconnected; the snapshot sent on reconnect catches the server up.
- **Server merges patches** (`crates/canvas-server/src/state.rs`): changed
  elements broadcast to other pages, winners echoed back when a concurrent
  edit beat the sender, revision recorded, disk write debounced and flushed
  on SIGTERM. Broadcast lag falls back to a full snapshot
  (`crates/canvas-server/src/ws.rs`).
- **Snapshot frames are objects**, not escaped strings: 8.2% fewer bytes and
  one parse instead of two.
- **Stable handles** (`crates/canvas-server/src/aliases.rs`): `canvas_read`
  prints `e1`, `e2`, …; every id-taking tool accepts handles or raw ids;
  draw/pen results echo the handle and compact line.
- **Compact read format** (`crates/canvas-core/src/describe.rs`):
  `<handle> <kind> <x>,<y> <w>x<h>[ in=…][ label]`; frame children are
  derivable from `in=`, and the accidental O(n²) is gone.
- **Tests**: unit tests for patch projection/merge, history outbox, aliases,
  compact format, server absorb/broadcast; the full Playwright suite (144
  tests) passes, including undo-as-deletion, reconnect merge and restart
  durability.

## Still open (stages 3–4)

- **Schema v2**: flat quantized points shrink the document 3.8× (15.4 MB →
  4.2 MB JSON on the fixture; 1.8 MB as varint deltas) and style interning
  removes another ~16% of live bytes. Needs the 0.01 px vs 0.1 px decision.
- **Tombstone compaction**: tombstones are merge-required, but a waterline
  policy would cut the 2,000-tombstone overhead in snapshots and disk.
- **Field-level diffs** in `canvas_read(since)`, a `canvas_read(id)` detail
  view, and a `canvas_update` / batch `canvas_apply` for existing elements
  (today the agent cannot move, resize or relabel without delete + redraw);
  tool results also don't yet suggest free space.
- **Agent-edit server cost**: `apply_edit` still clones and diffs the mirror;
  edit functions should report touched ids.
- **Binary patch codec / version vectors** once profiling says JSON parse
  cost matters or multiplayer arrives.
