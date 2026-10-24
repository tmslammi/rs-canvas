# Profiling harness

Playwright + Chrome DevTools Protocol harness for the real app: it loads large
fixtures through the editor API, records V8 CPU profiles, allocation samples,
heap snapshots, `Performance` metrics, long tasks and `WebAssembly.Memory`
growth, and writes one artifact set per scenario. It is intentionally separate
from the e2e suite (own config, own port, one worker, `--no-sync`) so profile
numbers are not mixed with correctness runs.

The committed [report](./REPORT.md) separates the historical v1 baseline
(commit `5b9b3ae`) from the current-master re-run. Curated evidence lives in
[`evidence/`](./evidence/); see [Reading the evidence](#reading-the-evidence).

## Run

Prerequisites are the normal web prerequisites: Rust with the
`wasm32-unknown-unknown` target, `wasm-bindgen-cli` matching `Cargo.lock`, and
Node 24+.

```sh
cd web
npm ci
npm run build:wasm
npm run profile          # build wasm again, then run the full 5-test suite
```

`npm run profile` is just
`playwright test --config profiling/playwright.profiling.config.ts`. To skip
the rebuild:

```sh
npx playwright test --config profiling/playwright.profiling.config.ts
```

Useful subsets:

```sh
# Fast smoke (boot, 10k load/save/reload, 10k edit, memory churn)
npx playwright test --config profiling/playwright.profiling.config.ts \
  --grep "boot:|load \\+ save 10k|editing ops on 10k|memory churn"

# Only the select-all -> delete -> undo scenario
npx playwright test --config profiling/playwright.profiling.config.ts \
  --grep "editing ops on 10k"
```

The config serves the current worktree's `web/` on
`http://localhost:4180` with `--no-sync` and refuses to reuse a server already
on that port. Set `RS_CANVAS_PROFILE_PORT` only if 4180 is occupied.

## Artifacts

Everything lands in `web/profiling/out/` (gitignored):

| File | Contents |
| --- | --- |
| `<scenario>.cpuprofile` | V8 CPU profile, 250 µs sampling interval |
| `<scenario>.allocprofile.json` | `HeapProfiler.startSampling` allocation profile |
| `<scenario>.json` | Wall duration, `Performance` metric deltas, wasm bytes/grows, long tasks, scenario result |
| `run-summary.json` | All scenario summaries from the last run |
| `load-10k.heapsnapshot` | Full V8 heap snapshot after the 10k load (~4.6 MB, not committed) |
| `memory-churn.txt` | Wasm/JS memory after each repeated delete/undo cycle |
| `test-results/` | Playwright failure context and `.last-run.json` |

Only the small CPU/alloc/JSON artifacts are worth committing; copy the ones a
report cites into `evidence/<label>/` and keep the committed evidence under
~2 MB. Never commit `.last-run.json`, `test-results/`, Playwright HTML
reports, or the raw heap snapshot.

## Read the artifacts

```sh
cd web

# Top self-time functions, wasm/JS split, top scripts
node profiling/analyze-cpu.mjs profiling/out/load-100k-connected.cpuprofile 30

# Sampled allocation sites
node profiling/analyze-memory.mjs alloc profiling/out/load-10k.allocprofile.json

# Heap composition (the raw snapshot stays untracked; the text output is small)
node profiling/analyze-memory.mjs heap profiling/out/load-10k.heapsnapshot
```

`cpuprofile` analysis is by self time; caller/callee attribution for a
specific function can be inspected in Chrome DevTools (Performance panel →
Load profile) or with a small script against the profile's `nodes` array.
Wasm frames appear only if the release wasm carries a name section; the
provided build does.

## Native probes

Two Rust probes cover costs the browser profile cannot isolate. Both are
release-only and need no server:

```sh
cargo run --release --example mem -p canvas-core
cargo run --release --example delete_bench -p canvas-core
```

- `mem` prints document/scene byte accounting for the 10k- and 100k-node
  connected fixtures. It reports the overview state (approximate layer
  bounds, no draw ops) and the full-detail state (all rough geometry built),
  which is the state the historical v1 report measured eagerly.
- `delete_bench` runs select-all → delete → undo for 2.5k through 20k mixed
  elements and prints delete+scene and undo+scene timings. The scene column
  includes the dirty-layer cache rebuild, not Canvas2D paint.

Probe timings are wall-clock and will move under machine load; the scaling
ratio between sizes is the useful signal. On a shared machine, use a private
`CARGO_TARGET_DIR` if other checkouts are building the same `canvas-core`
package at the same time.

## Reading the evidence

- `evidence/v1-5b9b3ae/` — profiles from 2026-10-07 on branch `perf/profiling`,
  base commit `5b9b3ae` (v1). These are the numbers behind the original
  performance tickets (#29, #30, #31, #33) and are **not** current-master
  measurements.
- `evidence/master-7959dae/` — the same harness re-run on `master` at
  `7959dae` (2026-10-24), plus current native probe output. The report labels
  every number with its commit.

Each `.analysis.txt` is the analyzer output for the adjacent profile, so a
report claim can be checked without rerunning the suite.
