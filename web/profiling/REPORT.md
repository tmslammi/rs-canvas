# rs-canvas performance profile

This report has two clearly separated parts:

- **Current master** — re-run on 2026-10-24 from `chore/36-profiling-harness`
  at base commit `7959dae`. Evidence is committed under
  [`evidence/master-7959dae/`](./evidence/master-7959dae/).
- **Historical v1 baseline** — gathered 2026-10-07 on branch `perf/profiling`
  at base commit `5b9b3ae`. Those are the numbers behind the original
  performance tickets and are **not** current-master measurements. Evidence is
  committed under [`evidence/v1-5b9b3ae/`](./evidence/v1-5b9b3ae/).

Both parts use the same rig (`profile.spec.ts`) and the same synthetic
fixtures. Browser numbers are headless Chromium via Playwright on macOS,
release wasm (`opt-level = "s"`, LTO), `wasm-bindgen --target web`. Native
numbers are release Rust. Absolute wall times move under machine load; compare
scenario shapes and ratios, not isolated milliseconds.

## How the data was gathered

- `web/profiling/profile.spec.ts` drives the real app over CDP: V8 CPU profiles
  (250 µs sampling), `HeapProfiler.startSampling`, heap snapshots,
  `Performance` metrics, a `WebAssembly.Memory` hook (bytes + grow calls) and a
  long-task observer.
- `npx playwright test --config profiling/playwright.profiling.config.ts`
  serves the worktree's app on `:4180` with `--no-sync`, one worker. The full
  suite is five tests and took 47 s on the current run.
- Native probes:
  `cargo run --release --example mem -p canvas-core` and
  `cargo run --release --example delete_scaling -p canvas-core`.
- The repo's own frame budgets (`web/e2e/perf.spec.ts`,
  `web/e2e/scale.spec.ts`) were the historical cross-check; this report does
  not rerun them because they are not part of the profiling config.

Artifacts per scenario are `<name>.cpuprofile`, `<name>.allocprofile.json`,
`<name>.json` (wall time, metric deltas, wasm bytes/grows, long tasks), plus
`memory-churn.txt`, `run-summary.json` and an untracked raw heap snapshot.
The adjacent `<name>.analysis.txt` files are the analyzer output for the
committed profile. See the [harness README](./README.md) for commands.

## Current master (`7959dae`, 2026-10-24)

### Headline results

| Scenario | Wall | App work | Wasm memory |
|---|---|---|---|
| boot | 50 ms | 43 ms task time | 1.1 MB |
| load 10k mixed elements | 42 ms (26 ms in call) | 42 ms task time, no long task | 12.3 MB (overview) |
| save JSON 10k (6.45 MB) | 25 ms (21 ms in call) | 25 ms | 28.6 MB |
| reload JSON 10k | 109 ms (84 ms in call) | 1 long task 102 ms | 41.6 MB |
| load 100k-node connected board (199,684 elements) | 454 ms (431 ms in call) | 1 long task 436 ms | 205.1 MB (overview) |
| drag a connected node | 2.68 s wall | wasm/script 173 ms | 205.1 MB |
| pan | 1.34 s wall | wasm/script 50 ms | 205.1 MB |
| zoom | 1.02 s wall | wasm/script 33 ms | 205.1 MB |
| select-all + delete + undo (10k) | 1.91 s | **delete 1.81 s**, undo 79 ms; 1 long task 1.89 s | 60.3 MB |
| freehand stroke, 300 points (10k) | 5.04 s wall | wasm/script 83 ms | 80.8 MB |
| five delete/undo cycles (10k) | — | — | 12.3 → 220.4 MB |

The 100k-node board is 199,684 elements (one connector per node). Playwright
input dispatch dominates drag/pan/zoom wall time (`(idle)`-heavy profiles);
the app's own wasm work there stays in the tens to low hundreds of ms.

### Bottlenecks against the open tickets

#### #29 — undo history and removal clones still grow wasm memory

Evidence: [`memory-churn.txt`](./evidence/master-7959dae/memory-churn.txt).

Repeated select-all → delete → undo on the same 10k board:

```text
loaded: 12.3MB
delete/undo 1: wasm 60.3MB, jsHeap 1.57MB
delete/undo 2: wasm 97.7MB, jsHeap 1.58MB
delete/undo 3: wasm 138.6MB, jsHeap 1.58MB
delete/undo 4: wasm 138.6MB, jsHeap 1.59MB
delete/undo 5: wasm 220.4MB, jsHeap 1.60MB
```

JS heap is flat; wasm linear memory grows by ~26–42 MB per cycle and never
comes back. The historical v1 run showed the same shape (34.4 → 219.9 MB), so
the lazy scene build changed the starting point but not the leak. The causes
listed in #29 (unbounded history, two clones per removal, inverse deltas holding
full elements) remain the things to fix.

#### #30 — mass delete/restore is still quadratic

Evidence: [`select-all-delete-undo.analysis.txt`](./evidence/master-7959dae/select-all-delete-undo.analysis.txt),
[`native-delete-scaling.txt`](./evidence/master-7959dae/native-delete-scaling.txt).

`select-all-delete-undo.cpuprofile` totals 1.91 s of samples, 98.9% wasm. Top
self time:

| Function | Self | Share |
|---|---:|---:|
| `hashbrown::HashMap::insert` | 354 ms | 18.5% |
| `memcmp` | 320 ms | 16.7% |
| `canvas_core::scene::Scene::reindex_from` | 247 ms | 12.9% |
| `core::hash::sip::Hasher::write` | 235 ms | 12.3% |
| `core::hash::BuildHasher::hash_one` | 211 ms | 11.0% |
| `RawVecInner::deallocate` | 153 ms | 8.0% |
| `dlmalloc::malloc` | 118 ms | 6.2% |
| `Vec::remove` | 68 ms | 3.6% |

The per-id delete path removes one layer at a time and reindexes the
`id → layer position` map after every removal (`Scene::remove_layer` →
`reindex_from`), so a select-all delete pays an O(n) map rebuild per element.
The native probe confirms the quadratic shape (three runs on a shared machine;
absolute times vary, ratios are stable):

| Elements | delete + scene | undo + scene |
|---:|---:|---:|
| 2,500 | 87–107 ms | 9–12 ms |
| 5,000 | 321–389 ms | 18–20 ms |
| 10,000 | 1.29–1.38 s | 37–43 ms |
| 20,000 | 5.6–8.3 s | 80–107 ms |

Each doubling of delete work costs roughly 3.5–4×. Undo does not hit the same
path because re-insertion is already indexed by the document order. #30's
suggested bulk path (rebuild the layer vector once when the changed set is a
large fraction of layers, or keep a position map that is not rebuilt per
removal) is still the right fix. The historical v1 profile was also dominated
by `memcmp` + `Vec::remove`; the current position map moved much of that cost
into hash-table maintenance without removing the quadratic behaviour.

#### #31 — fixtures still generate UUIDs they immediately discard

Evidence: [`load-100k-uuid-attribution.txt`](./evidence/master-7959dae/load-100k-uuid-attribution.txt),
[`load-100k-connected.analysis.txt`](./evidence/master-7959dae/load-100k-connected.analysis.txt).

`benchmark_fixture`/`connected_fixture` build `Element::new(...).with_id(...)`;
`Element::new` still calls `uuid::v4::new_v4`, crossing to
`crypto.getRandomValues` on wasm. In the current 455 ms load profile the
uuid-related frames total 7.2 ms (1.6%):

```text
2.63ms uuid::v4::new_v4
1.42ms uuid::fmt::Hyphenated::fmt
1.28ms canvas_core::model::Element::new
0.95ms __wbg_getRandomValues
...
```

That is much smaller than the ~64 ms of JS self time recorded on v1 because
the surrounding load path is faster, but it is still avoidable work on every
fixture/import element. The rest of the load profile is dominated by allocation
(`dlmalloc::alloc` 54 ms, `malloc` 25 ms), BTree key search (`find_key_index`
100 ms across three symbols), order-key validation (4 ms) and the wasm-bindgen
glue (61 ms). #31 remains valid; it is no longer the headline load cost.

#### #33 — JSON load still parses twice

Evidence: [`reload-json-10k.analysis.txt`](./evidence/master-7959dae/reload-json-10k.analysis.txt),
[`reload-json-10k.json`](./evidence/master-7959dae/reload-json-10k.json).

10k save is 25 ms for 6.45 MB; reload is 109 ms (84 ms in the editor call, one
102 ms long task). The profile shows `passStringToWasm0` 10.4 ms and
`serde_json::Value::deserialize` 2.2 ms around BTree insertion/free work.
`from_json` still builds a full `serde_json::Value` tree and then deserializes
that into `Document` (`crates/canvas-core/src/serialization.rs`), so the
double-parse in #33 is unchanged. The historical v1 run was 149 ms; current
JSON is faster, but still ~4.4× the save time and the message-copy cost is
visible on the JS side.

### Scene memory on current master

Evidence: [`native-mem.txt`](./evidence/master-7959dae/native-mem.txt).

`Scene::rebuild_all` now stores approximate bounds only; rough geometry is
built on demand. For the 100k-node connected board (199,684 elements):

| State | Tracked bytes | Wasm observed |
|---|---:|---:|
| overview (no draw ops) | 128.5 MB | 205.1 MB after load |
| full detail (all ops built) | 451.4 MB | — |

Full detail holds 637,426 draw ops with 318.6 MB of points, the same point
payload as v1 (73% of tracked bytes). Loading is now much cheaper because the
overview never builds those ops, but any workflow that needs all detail still
pays the full 451 MB. This is why the load memory dropped from v1's 504 MB to
205 MB while the detail ceiling is unchanged.

### Minor observations (current)

- Drag/pan/zoom wall time is dominated by Playwright input dispatch. The app
  side stays at 33–173 ms of script/wasm work for 160–40 input events.
- The 10k load no longer produces a long task (42 ms total), but JSON reload
  still does (102 ms).
- `Element::new` / `Element::bounds()` still show up on edit paths; caching
  bounds per changed element remains worthwhile but is not the main cost here.
- The JS heap outside wasm stays ~1.6 MB and does not grow with the document;
  the memory problem is wasm-side.

## Historical v1 baseline (`5b9b3ae`, 2026-10-07)

Everything in this section describes commit `5b9b3ae` (v1), not current
master. It is kept because #29/#30/#31/#33 were filed against these numbers.
Evidence: [`evidence/v1-5b9b3ae/`](./evidence/v1-5b9b3ae/).

### Headline results

| Scenario | Wall | App work | Wasm memory |
|---|---|---|---|
| boot | 53 ms | ~all | 1.1 MB |
| load 10k mixed elements | 86 ms | 86 ms (1 long task 71 ms) | 34.4 MB |
| load 200k connected elements | 1.32–1.52 s | 1.32 s (1 long task 1.3 s) | 504 MB |
| drag a connected node (100k board) | 2.7 s | wasm 115 ms (0.7 ms/move) | 504 MB |
| pan / zoom (100k board) | 1.34 / 1.01 s | wasm 36 / 26 ms | 504 MB |
| freehand stroke, 300 points (10k) | 5.0 s | wasm 29 ms | 34 MB |
| select-all + delete + undo (10k) | 352 ms | 352 ms (1 long task 335 ms) | 34 → 62 MB |
| save_json 10k (6.5 MB) | 27 ms | 21 ms | — |
| load_json 10k | 149 ms | 129 ms wasm | 53 → 66 MB |

The app's own frame budgets held 60 fps for 100k drag/pan/zoom (median
16.7 ms, p95 16.8 ms, one 33 ms frame); interaction was not the bottleneck.

### Historical bottlenecks, as reported at the time

1. **Unbounded undo history + double clone per removal (#29).**
   `memory-churn.txt` recorded 34.4 → 61.9 → 97.2 → 138.1 → 138.1 → 219.9 MB
   across five select-all/delete/undo cycles, with JS heap flat at ~1.5 MB.
   `History` had no cap; `Transaction::remove` cloned the element twice
   (`before` and `tombstoned`); inverse deltas retained full `Element` clones
   until dropped.

2. **Scene ops dominated memory (#not separately ticketed).**
   `mem.rs` accounted ~434 MB tracked for `connected_fixture(100_000)`, with
   637,426 draw ops and 318.6 MB of points (73%), plus 98.2 MB of scene-layer
   vectors, 80.7 MB of elements and 4.8 MB of strings. Every element got
   precomputed rough geometry at load, including off-screen ones.

3. **Load was an allocation storm (#31 and general load).**
   `load-100k-connected.cpuprofile`: `dlmalloc::alloc` 756 ms self (49.7%),
   `malloc` 67 ms, `free` 16 ms; `RawVecInner::try_allocate_in` 454 ms,
   `finish_grow` 89 ms. `uuid::v4`/`getRandomValues` cost ~64 ms of JS self
   time for ids the fixtures immediately overwrote. `sin`/`rem_pio2`/`cos`
   added 84 ms for rough geometry; `memcmp` 71 ms for BTree keys.

4. **Mass delete/undo was quadratic in `Scene::update` (#30).**
   10k select-all/delete/undo was a 335 ms long task: `memcmp` 165 ms (47%,
   attributed to `Scene::update`) and `Vec::remove` 57 ms (16%). Native
   scaling: 2.5k 7/14 ms → 5k 33/37 ms → 10k 92/111 ms → 20k 340/374 ms
   (delete/undo), ~3.5× per doubling. `remove_layer` linear-scanned then
   `Vec::remove`d per changed id; re-insertion scanned again.

5. **JSON load parsed twice (#33).**
   10k `load_json` 149 ms (129 ms wasm) with `Value::deserialize`, escaping and
   BTree inserts hot; `save_json` 21 ms / 6.45 MB. JS `passStringToWasm0`
   added 11.7 ms. `from_json` built a full `serde_json::Value` tree and then
   converted it to `Document`.

Historical caveats: headless Chromium rasterizes Canvas2D in software; all
wasm/JS numbers are unaffected. `load_connected_fixture` and
`load_benchmark_fixture` are synthetic load paths, so the uuid-boundary cost
was on those paths, not on user drawing (one id per gesture) or `load_json`
(ids come from JSON). xctrace symbolication was partial; the native example
timings and the two probes were the native evidence.

## Reproduce

```sh
cd web
npm ci && npm run build:wasm
npm run profile
node profiling/analyze-cpu.mjs profiling/out/load-100k-connected.cpuprofile
node profiling/analyze-memory.mjs heap profiling/out/load-10k.heapsnapshot

cd ..
cargo run --release --example mem -p canvas-core
cargo run --release --example delete_scaling -p canvas-core
```

The raw heap snapshot is generated but intentionally not committed (4.6 MB);
the committed `*-heap.analysis.txt` is its small text summary.
