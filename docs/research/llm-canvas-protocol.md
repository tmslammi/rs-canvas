# An efficient agent ↔ canvas protocol — investigation

Status: investigation, not a decision. Written 2026-10-06 against the code at
`main` and a frozen copy of the live document (2,958,196 bytes: 2,632
elements, 770 live, 1,862 tombstones). External facts are cited in
[`protocol-sources.md`](./protocol-sources.md); repo facts reference file and
line. A scratch probe at `crates/canvas-core/examples/protocol_probe.rs`
reproduces the Rust-side numbers in §3; the read-format and compression
figures were computed from the same frozen sample and are described in the
appendix.

> **Update (same day):** stages 1–2 below are implemented — patch frames,
  stable handles, compact reads — with before/after measurements in
  [`protocol-results.md`](./protocol-results.md). Stages 3–4 remain open.

## 1. What "efficient" means here

Two channels with different budgets:

- **Page ↔ server (bytes and CPU).** The page owns the interactive document,
  the server holds the mirror the MCP tools read and write (ADR-0005). Every
  edit must reach the other side, converge, and not stall interaction. The
  budget is O(changed elements), not O(document).
- **Agent ↔ server (tokens and round trips).** The agent pays for every byte
  it reads and writes, and every tool call is a full model step. The budget is
  O(what the agent does not already know), and as few calls as possible.
- **Convergence is not negotiable.** Whatever goes on the wire must still obey
  the merge rules of ADR-0002 (whole-element LWW, tombstones, stable IDs).

The document format serves both budgets; view state stays out of both
(ADR-0006).

## 2. Where the protocol stands today

```
agent ──MCP tool call──▶ server mirror ──snapshot frame──▶ page
                          (doc_ops/verbs)   full doc JSON     parse + reconcile
                                │                             + refresh_scene
                                ▼
                          .rs-canvas/current.json

page gesture ──Effects──▶ IndexedDB autosave ──snapshot frame──▶ server
                                                              parse + reconcile
                                                              + persist + echo
                                                              snapshot to ALL pages
```

- The wire carries one document frame type, `snapshot`, both ways
  (`crates/canvas-server/src/ws.rs:70`, `web/src/agentBridge.js:29`). It is
  the **entire document**, embedded as an escaped JSON *string*.
- `Delta` already exists as the only way a document changes
  (`crates/canvas-core/src/delta.rs`) and history stores gesture-sized
  transactions of deltas (`crates/canvas-core/src/history.rs:117`), but deltas
  never leave the page. ADR-0005 says edits flow "through the existing
  Delta/merge seam" — today they do not; snapshots do.
- On the server, every edit clones the document, reconciles the whole
  document, diffs the whole document for the change log, broadcasts a full
  snapshot to every page (including the origin page), and writes the file
  (`crates/canvas-server/src/state.rs:295-357`).
- The agent reads a text scene graph (`doc_ops::describe`,
  `crates/canvas-server/src/doc_ops.rs:397`) or a change-log cursor
  (`canvas_read(since)`, `mcp.rs:440`). Writes are one element per call
  (`canvas_draw`, `canvas_pen`) or a presentation-coupled beat list
  (`canvas_script`, `script.rs`).

## 3. Measured costs

Frozen sample unless noted; timings are release-mode (average of 3 runs),
single machine.

| Cost | Value |
| --- | --- |
| Document on disk / per snapshot frame | 2,958,196 B |
| Snapshot frame as escaped string vs embedded object | +350,197 B (+11.8%) |
| `to_json` / `from_json` | 5.9 ms / 23.6 ms |
| `clone` (server "before" copy) | 0.6 ms |
| `reconcile` no-op / 1 element changed | 14.3 ms / 14.5 ms |
| One freehand element as a patch frame | 1,289 B (**2,295× smaller** than the snapshot) |
| Freehand payload, 9,847 points, current JSON | 493,683 B |
| — flat f64 JSON array | 351,833 B |
| — quantized 0.1 px int JSON | 109,062 B |
| — quantized 0.01 px int JSON | 128,756 B |
| — varint deltas + u8 pressure, binary | 42,054 B (0.1 px) / 50,578 B (0.01 px) |
| gzip -9 / zstd -19 on the whole document | 772,552 B (3.8×) / 555,430 B (5.3×) |
| `scene.rebuild_all` at 10k / 100k elements | 21.5 ms / 321 ms |
| `scene.update` for 1 changed element | 0.002 ms |
| `doc_ops::describe` scene-graph read, 770 live | 72,361 chars; UUIDs 38.3% of chars |
| — estimated tokens (heuristic) | ~25,000 |
| — with `e1..eN` aliases | 47,613 chars, ~12,600 est. tokens |
| — with aliases + compact notation | 24,186 chars, ~6,800 est. tokens |
| `describe` sorting overhead (O(n²), 770 live) | ~112 ms, grows quadratically |
| 10k fixture: serialize / parse / reconcile | 3.88 MB, 6.6 ms / 29 ms / 19.7 ms |

Token figures are estimates (IDs at ~2 chars/token, prose at ~4); they are
directionally right but should be re-measured with the client's tokenizer
before they are quoted in a decision.

## 4. Findings

**F1 — Every edit pays for the whole document, twice.** A one-stroke gesture
ships ~2.9 MB each way, runs parse + full reconcile + full scene rebuild on
the page (~40 ms at this size, ~70 ms at 10k), and parse + reconcile +
persist on the server. The origin page even receives its own change echoed
back. `scene.update(changed)` costs 0.002 ms for one element, so the payload,
not the rendering, is the problem.

**F2 — The snapshot frame is a string inside JSON.** `snapshot_frame`
stringifies the document and embeds it as an escaped string
(`state.rs:348`): +11.8% bytes and a double parse on every frame. Embedding
the document as an object removes both.

**F3 — Freehand points are the bulk of live content and are encoded
naively.** Point objects with full f64 precision and parallel pressures are
64.6% of all live element bytes (16.7% of the whole file once tombstones are
counted). Flat + quantized JSON is 3.8–4.5× smaller; varint deltas + u8
pressure are ~10–12× smaller (per sources, Google Polyline and Mapbox vector
tiles use exactly this family of encodings; Excalidraw stores raw arrays too,
so this is greenfield for us).

**F4 — Full-precision floats are printed as noise.** Coordinates serialize as
`2368.6666666666665`; nothing rounds at write time (Excalidraw likewise never
rounds on save; its SVG export uses 2 decimals — see sources). Quantizing at
creation is free correctness-wise if the editor's epsilon matches.

**F5 — Tombstones are 71% of the sample document.** 1,862 tombstones vs 770
live elements, and live elements are only 25.8% of the file's bytes. They are
merge-required, but they ride every snapshot, every reconcile, and every disk
write. There is no compaction policy at all.

**F6 — Style is repeated per element.** 16.4% of live element bytes is style
objects; a document with a few palettes repeats the same eight-field object
hundreds of times. Interning styles per document would shrink both disk and
wire.

**F7 — The agent read is bigger and slower than it needs to be.** UUIDs are
38% of the read text; there is no alias layer, so every later tool call also
carries 36-char IDs. `describe_element` calls `ordered_live()` once per
element — O(n²) time and allocations (measured ~112 ms of pure sorting for
770 live elements, quadratic growth). `canvas_read(since)` re-describes whole
elements and, if `since` is older than the 200-entry change log, silently
reports a partial history (`state.rs:337`, `mcp.rs:446`).

**F8 — The agent surface has no update path.** There is no tool to move,
resize, restyle or relabel an existing element; the batch tool
(`canvas_script`) batches draw/connect/annotate/emphasis/tidy/clear but is
presentation-shaped, and every other write is one element per round trip.
Fixing a label costs a model step per element, and there is no way to fix
geometry at all except delete + redraw.

**F9 — Compression cannot be delegated to the WebSocket stack.** Browsers
offer `permessage-deflate`, but tungstenite/tokio-tungstenite/axum have no
support (sources §4). If snapshots ever remain on the wire, compression must
be app-level; `DecompressionStream` supports gzip, deflate and — with varying
support — zstd/brotli, so gzip/deflate is the portable baseline and zstd the
better ratio (MDN, linked in sources §4).

## 5. Target design

### 5.1 Patch sync (page ↔ server) — the core change

Replace steady-state snapshots with element-granular patches built from the
deltas that already exist:

```json
{"type":"patch","rev":431,"put":[<element>, ...]}
```

- `put` carries full elements **including merge metadata**. Tombstones travel
  as elements with `isDeleted:true` and a bumped version (`Delta::Remove`
  already produces exactly this); `Delta::Purge` (undo of a creation)
  synthesizes a tombstone the way `PageSync::prepare` does today
  (`state.rs:49`).
- Merge is per element with the existing `pick_winner` rules; order does not
  matter, duplicates are harmless, and no full-document reconcile runs
  (idempotence/commutativity is already proven by `merge.rs` tests).
- **Harvesting:** `History` gains an outbox filled on every applied delta —
  `commit`, `undo`, `redo`. The wasm façade drains it as JSON after each
  input batch; `agentBridge` sends one patch per gesture. Transactions are
  already gesture-sized and coalesced (`history.rs:41`), so a drag is one
  patch, not 100.
- **Cursors and resync:** each side tracks the other's `rev`. The server
  keeps a bounded ring of recent patches; a page that reconnects or reports a
  gap (including broadcast lag, `ws.rs:59`) gets replayed patches if the ring
  covers it, otherwise a full `snapshot`. Gap detection must be explicit —
  silently continuing is the one way this design diverges.
- **No echo:** a page's patch is applied by the server and broadcast only to
  *other* pages. Agent edits broadcast to all.
- **In-flight gestures:** keep the existing `pending_remote` deferral
  (`canvas-wasm/src/lib.rs:492`).
- **Server CPU:** drop the per-edit `doc.clone()`, whole-document reconcile
  and whole-document diff; merge the patch, record changed IDs, publish the
  patch. Debounce the disk write; persist on idle and shutdown.

Expected effect at the sample size: per-gesture wire 2.9 MB → ~1–3 KB, page
CPU ~40 ms → well under 1 ms, server CPU ~40 ms → ~1 ms, and the mirror
converges with O(changed) work everywhere.

### 5.2 Snapshot framing and compression

- `snapshot` stays for connect/resync only. Embed the document as an object:
  `{"type":"snapshot","rev":N,"doc":{...}}` — saves 11.8% and one parse
  (F2).
- Compress snapshots app-level: zstd if the browser support matrix allows
  (5.3× measured), gzip/deflate as the portable fallback (3.8×; browser-native
  decode, `flate2` server-side). Patches stay plain JSON: at ~1 KB they are
  cheaper uncompressed than the framing overhead, and plain text is
  debuggable.

### 5.3 Document schema v2 (one format for disk and wire)

ADR-0003 keeps us free to version our own schema; this is a `schemaVersion: 2`
migration, not a new format:

- `points` as flat `[x,y,…]` quantized to 0.01 px (measured 3.8× smaller than
  today; 0.1 px is 4.5× but visible at deep zoom), `pressures` as u8
  (0–255). Binary varint-delta encoding can wait for a binary patch codec;
  the JSON win is most of the benefit and keeps the format inspectable.
- Round all geometry at creation to the same precision.
- A document-level style table with elements referencing it — removes most of
  the repeated 16.4%.
- Tombstone policy: keep them for merge, but (a) exclude tombstones older
  than the oldest connected peer's acked rev from snapshots, (b) compact the
  persisted mirror on a waterline. This needs a short design note of its own;
  it is the riskiest item here.

### 5.4 Agent surface (MCP)

1. **Handles.** `canvas_read` returns short aliases (`e12`) instead of UUIDs;
   every id-taking tool accepts either. The server owns the alias table. This
   alone halves read tokens (F7) and cuts every later tool call that
   references an element.
2. **Compact notation, detail on demand.** Lines like
   `e12 stroke 2368,1194 59x0 n=13` and `e4 rect 10,20 160x100 "Web"` instead
   of the current verbose form; omit defaults, drop the frame `children` list
   (derivable from `in=`), and add `canvas_read(id)` / a `detail` flag for
   full fields. Measured: ~3.7× fewer tokens than today on the sample.
3. **Field-level diffs for `since` reads.** Store the compact previous state
   with each change-log entry and emit `e12 move 2368,1194→2404,1210`,
   `e9 deleted`, `e4 text "Web"→"API"` instead of re-describing elements.
   Also fix the truncation hole: if `since` precedes the retained log, say so
   and require a full read.
4. **Batch apply and update.** A pure `canvas_apply` (the script executor
   without presentation, or `canvas_script` with the presentation fields made
   explicitly optional) taking draw/update/connect/delete/move/style ops in
   one call, with handles resolved server-side. Add `canvas_update` for
   existing elements — today the capability is missing entirely (F8), and
   every fix costs a round trip.
5. **Echo state in results.** Tool results already return IDs; add the
   created element's compact line and a free-space hint so the agent rarely
   needs a follow-up read. Every avoided read is ~25k tokens at sample size.
6. **Screenshot discipline.** Keep text-first verification; screenshots are
   the expensive modality (base64 + vision tokens). `scale` exists; a region
   crop would help more.
7. **Fix `describe`'s O(n²)** by hoisting `ordered_live()` out of the loop —
   independent of everything else and cheap.

`structuredContent` (MCP 2025-06-18, sources §1) is available if opencode
wants a machine channel, but returning both text and JSON duplicates tokens;
keep text as the model-facing payload.

### 5.5 Later options, in order of payoff

- **Binary patch codec** (postcard/CBOR with the v2 quantized points): only
  after patches exist and profiling shows JSON parse cost matters.
- **Version vectors per peer** instead of rev counters: causal delivery and
  three-way knowledge, the step before a CRDT.
- **Loro** when multiplayer needs real CRDT semantics (ADR-0002's plan);
  element-granular patches are exactly the migration seam that ADR-0002
  promised would exist.

## 6. Staged plan and expected gains

| Stage | Work | Wire/edit | Page CPU/edit | Read tokens |
| --- | --- | --- | --- | --- |
| 0 (today) | — | ~2.9 MB | ~40 ms | ~25k |
| 1 | Patch frames + outbox + cursor/resync; object snapshots; server O(changed); describe fix | ~1–3 KB | <1 ms | ~25k |
| 2 | Handles + compact read + field diffs + `canvas_apply`/`canvas_update` + echo | ~1–3 KB | <1 ms | ~7–13k |
| 3 | Schema v2: quantized flat points, precision, style table | ~0.3–0.7 KB | <1 ms | ~7–13k |
| 4 | Tombstone compaction; binary codec if profiled | ~0.1–0.3 KB | <1 ms | ~7–13k |

Stage 1 is the single highest-value change and is invisible to the agent and
the user; stages 2–3 are where the token wins land.

## 7. Correctness obligations

- Patches must be idempotent and commutative per element; reuse
  `merge::pick_winner`, do not write a second comparison.
- Deletions must always carry a tombstone with a version that beats the copy
  the peer holds; purge needs the synthesized-tombstone path.
- A missed or lagged patch must be detectable (`rev` gap) and repaired with a
  snapshot; the current `Lagged(_) => continue` (`ws.rs:59`) is safe only
  because frames are full snapshots.
- Inbound patches during a gesture stay deferred.
- View hints, settings and presentations are unchanged (ADR-0006); nothing
  in the document model may depend on them.
- The persisted file stays schema-valid at all times (write atomic, debounced).

## 8. Open decisions

1. Wire-first patches (no schema change) vs waiting for schema v2 and doing
   both at once — recommendation: patch now, v2 next; they are orthogonal.
2. 0.01 px vs 0.1 px quantization, and whether the editor's existing epsilon
   (`Excalidraw`-style `isCloseTo`) defines the floor.
3. Alias lifetime: per MCP session vs per connection vs per revision window.
4. Whether `canvas_apply` absorbs `canvas_script` or the two stay separate
   (presentation vs silent batch).
5. Tombstone waterline policy — needs its own design note before any code.

If a direction is chosen, record it as an ADR (0007) next to ADR-0005, which
this investigation effectively finishes: the bridge was designed around the
Delta seam but currently ships snapshots.

## Appendix — reproduce

```sh
cp .rs-canvas/current.json /tmp/rs-canvas-sample.json
cargo run --release --example protocol_probe -p canvas-core -- /tmp/rs-canvas-sample.json
gzip -9 -c /tmp/rs-canvas-sample.json | wc -c
zstd -19 -c /tmp/rs-canvas-sample.json | wc -c
```

Read-format figures (chars, UUID share, alias and compact variants) were
produced by a one-off replication of `doc_ops::describe_element` over the
same sample; token counts are heuristics (IDs at ~2 chars/token, prose at
~4), not a BPE measurement.

The live document is continuously edited while the app is open; copy it first
so all numbers come from one revision.
