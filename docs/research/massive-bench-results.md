# Massive board benchmark results

Outcome of the spatial-index / preview-move / LOD pass, measured with
[`massive-bench.spec.ts`](../../web/e2e/massive-bench.spec.ts) — the same
fixture family as the drag-lag complaint that prompted it: connected boards
(nodes with one bound arrow each, so N nodes ≈ 2N elements), zoomed-out
overview, sustained pan/zoom, and all-elements drift.

Reproduce with:

```sh
cd web && npx playwright test massive-bench
```

(skipped in CI; deterministic connected fixture via
`load_connected_fixture`)

Hardware: Apple M-series laptop, headless Chromium, 1280x800 viewport,
2026-10-17.

## App CPU vs frame pacing

Two clocks measured separately: `appCpuMs` around each editor call
(JS + WASM + Canvas2D submit) and `frameMs` (real rAF intervals including
raster/composite; headless Chromium paces at 60Hz, so `missed60` is the
meaningful frame-deadline column).

| Elements | Load | Fit | Render p50/p95 | Tick+render p50 | Pan appCPU p50 | Pan frames missed60 |
| -------: | ---: | --: | --- | ---: | ---: | ---: |
| 1k | 8ms | 0.3ms | 0.10 / 0.20 ms | 0.00 ms | 0.00 ms | 0.0% |
| 10k | 39ms | 0.6ms | 0.70 / 1.10 ms | 0.10 ms | 0.30 ms | 0.0% |
| 50k | 113ms | 1.6ms | 1.80 / 2.80 ms | 0.50 ms | 1.30 ms | 0.0% |
| 100k | 189ms | 2.8ms | 3.00 / 5.80 ms | 2.70 ms | 2.50 ms | 0.0% |
| 200k | 376ms | 5.6ms | 5.50 / 9.40 ms | 5.80 ms | 5.30 ms | 0.0% |

All five sizes stay under the 16.7 ms (60 Hz) deadline with zero missed
frames; 50k and under also sit under the 4.17 ms (240 Hz) ceiling for every
sampled operation. Heap stays flat (~10 MB) across sizes — the packed SoA
bounds + uniform grid and per-layer ops caches pay for themselves.

Per-size details (p50/p95/p99/max in ms):

```
[1k]   elements=978    render p50/p95/p99/max=0.10/0.20/0.60/0.60  tick+render=0.00/0.10/0.10/0.10  pan=0.00/0.20/0.70
[10k]  elements=9929   render p50/p95/p99/max=0.70/1.10/1.20/1.20  tick+render=0.10/0.20/0.20/0.20  pan=0.30/0.50/0.50
[50k]  elements=49842  render p50/p95/p99/max=1.80/2.80/3.20/3.20  tick+render=0.50/0.60/0.70/0.70  pan=1.30/1.80/1.90
[100k] elements=99776  render p50/p95/p99/max=3.00/5.80/6.40/6.40  tick+render=2.70/2.80/2.80/2.80  pan=2.50/3.70/3.70
[200k] elements=199684 render p50/p95/p99/max=5.50/9.40/11.50/11.50 tick+render=5.80/5.90/5.90/5.90  pan=5.30/7.30/7.40
```

## Bulk drag (the original complaint)

[`select-drag.spec.ts`](../../web/e2e/select-drag.spec.ts) drags a
marquee-selected 50k board in the real browser with coalesced pointer
input and samples frames across the gesture; selection bounds must land
where dragged and element count must survive. It logs its own frame
pacing each run; run it with `npx playwright test select-drag`.

## Known ceiling

Zoom is nearly free through the LOD path (0.4 ms p50 at 200k); the all-
elements-moving tick+render and render paths are the residual costs at
200k (5.8 ms and 9.4 ms p95 respectively) — still 60Hz-safe, but the next
raw-material limit if a bigger board is demanded.
