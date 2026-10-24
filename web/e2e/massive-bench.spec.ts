import { expect, test, type Page } from "@playwright/test";

// Massive-diagram benchmark: deterministic connected boards, zoomed-out
// rendering, pan/zoom, and (once exposed) bulk-element motion.
//
// Measures TWO clocks separately:
//  - appCpuMs: performance.now() around the editor call (simulation +
//    visibility/scene prep + draw submission, i.e. JS+WASM+Canvas2D submit)
//  - frameMs: rAF intervals during the workload (browser frame pacing,
//    includes raster/composite/GPU, reported against the 4.17ms @240Hz
//    deadline as well as 16.7ms @60Hz)
//
// Scene sizes are totals: nodes N with 1 arrow each ~= 2N elements.

type CpuStats = { p50: number; p95: number; p99: number; max: number; n: number };
type FrameStats = { frames: number; p50: number; p95: number; p99: number; max: number; missed240: number; missed60: number };

const q = (sorted: number[], p: number) =>
  sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * p))] ?? 0;

const cpuStats = (v: number[]): CpuStats => {
  const s = [...v].sort((a, b) => a - b);
  return { p50: q(s, 0.5), p95: q(s, 0.95), p99: q(s, 0.99), max: s[s.length - 1] ?? 0, n: s.length };
};

declare global {
  interface Window {
    __rsEditor?: {
      element_count(): number;
      load_connected_fixture(nodes: number, arrowsPerNode: number): void;
      zoom_to_fit(): void;
      zoom(): number;
      wheel(x: number, y: number, dx: number, dy: number, ctrl: boolean): void;
      render(): void;
      benchmark_tick(dx: number, dy: number): void;
    };
  }
}

async function appCpu(page: Page, fnName: string, iters: number, args: unknown[] = []): Promise<number[]> {
  return page.evaluate(
    ([fn, n, a]) => {
      const ed = window.__rsEditor!;
      const out: number[] = [];
      for (let i = 0; i < (n as number); i++) {
        const t0 = performance.now();
        (ed[fn as keyof typeof ed] as (...x: unknown[]) => void)(...(a as unknown[]));
        out.push(performance.now() - t0);
      }
      return out;
    },
    [fnName, iters, args] as const,
  );
}

async function frameIntervals(page: Page, action: () => Promise<void>): Promise<number[]> {
  await page.evaluate(() => {
    const st = { deltas: [] as number[], last: 0, raf: 0, running: true };
    (window as unknown as { __bs: typeof st }).__bs = st;
    st.last = performance.now();
    const loop = (t: number) => {
      if (!st.running) return;
      st.deltas.push(t - st.last);
      st.last = t;
      st.raf = requestAnimationFrame(loop);
    };
    st.raf = requestAnimationFrame(loop);
  });
  await action();
  return page.evaluate(() => {
    const st = (window as unknown as { __bs: { deltas: number[]; raf: number; running: boolean } }).__bs;
    st.running = false;
    cancelAnimationFrame(st.raf);
    return st.deltas.slice(2);
  });
}

const frameStats = (deltas: number[]): FrameStats => {
  const s = [...deltas].sort((a, b) => a - b);
  const missed240 = s.filter((v) => v > 4.17).length;
  const missed60 = s.filter((v) => v > 17.5).length;
  return {
    frames: s.length,
    p50: q(s, 0.5),
    p95: q(s, 0.95),
    p99: q(s, 0.99),
    max: s[s.length - 1] ?? 0,
    missed240: s.length ? missed240 / s.length : 0,
    missed60: s.length ? missed60 / s.length : 0,
  };
};

async function memMB(page: Page): Promise<number | null> {
  return page.evaluate(() => {
    const m = (performance as unknown as { memory?: { usedJSHeapSize: number } }).memory;
    return m ? m.usedJSHeapSize / 1048576 : null;
  });
}

const SIZES: Array<{ label: string; nodes: number }> = [
  { label: "1k", nodes: 500 },
  { label: "10k", nodes: 5000 },
  { label: "50k", nodes: 25000 },
  { label: "100k", nodes: 50000 },
  { label: "200k", nodes: 100000 },
];

// Benchmark, not a correctness check: CI runners are noisy and would only
// burn ten minutes producing meaningless numbers. Run it locally:
//   npx playwright test massive-bench
test.skip(process.env.CI, "benchmark runs locally, not in CI");

test("massive benchmark: zoomed-out render + pan/zoom appCPU vs frame intervals", async ({ page }) => {
  test.setTimeout(600_000);
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() =>
    (window as unknown as { __rsPauseAutosave?: (p: boolean) => void }).__rsPauseAutosave?.(true),
  );

  console.log("hardware: see outer log (Apple M4, 10c, 16GB, Chrome 155, 1280x800 viewport)");
  for (const { label, nodes } of SIZES) {
    const loadMs = await page.evaluate(([n]) => {
      const t0 = performance.now();
      window.__rsEditor!.load_connected_fixture(n, 1);
      return performance.now() - t0;
    }, [nodes] as const);
    const count = await page.evaluate(() => window.__rsEditor!.element_count());
    const heap0 = await memMB(page);

    // Zoomed-out: fit whole board (clamped by MIN_ZOOM on baseline).
    const fitMs = await page.evaluate(() => {
      const t0 = performance.now();
      window.__rsEditor!.zoom_to_fit();
      return performance.now() - t0;
    });
    const zoom = await page.evaluate(() => window.__rsEditor!.zoom());
    await page.waitForTimeout(150);

    // App-side CPU: full render submit, pan step, zoom step.
    const renderRaw = await appCpu(page, "render", 30);
    const renderCpu = cpuStats(renderRaw);
    const panRaw = await appCpu(page, "wheel", 60, [640, 400, 0, 12, false]);
    const panCpu = cpuStats(panRaw);
    const zoomRaw = await appCpu(page, "wheel", 30, [640, 400, 0, -12, true]);
    const zoomCpu = cpuStats(zoomRaw);
    // All-elements-moving: uniform drift tick (simulation+scene prep, no
    // draw) and tick+render (full app-side frame cost).
    const tickRaw = await appCpu(page, "benchmark_tick", 30, [2, 1.5]);
    const tickCpu = cpuStats(tickRaw);
    const tickRenderRaw: number[] = await page.evaluate(() => {
      const ed = window.__rsEditor!;
      const out: number[] = [];
      // Warmup: settle wasm/canvas caches before timing.
      for (let i = 0; i < 5; i++) {
        ed.benchmark_tick!(2, 1.5);
        ed.render();
      }
      for (let i = 0; i < 30; i++) {
        const t0 = performance.now();
        ed.benchmark_tick!(2, 1.5);
        ed.render();
        out.push(performance.now() - t0);
      }
      return out;
    });
    const tickRenderCpu = cpuStats(tickRenderRaw);

    // Browser frame intervals during sustained pan and zoom.
    const panFrames = frameStats(
      await frameIntervals(page, async () => {
        for (let i = 0; i < 40; i++) await page.mouse.wheel(0, 12);
      }),
    );
    const zoomFrames = frameStats(
      await frameIntervals(page, async () => {
        await page.keyboard.down("Control");
        for (let i = 0; i < 30; i++) await page.mouse.wheel(0, -12);
        await page.keyboard.up("Control");
      }),
    );
    const heap1 = await memMB(page);
    const over3 = (v: number[]) => (v.length ? (v.filter((x) => x > 3).length / v.length) * 100 : 0);
    console.log(
      `[${label}] elements=${count} load=${loadMs.toFixed(0)}ms fit=${fitMs.toFixed(2)}ms zoom=${zoom.toFixed(4)} heap=${heap0?.toFixed(0) ?? "?"}->${heap1?.toFixed(0) ?? "?"}MB | ` +
        `renderCpu p50/p95/p99/max=${renderCpu.p50.toFixed(2)}/${renderCpu.p95.toFixed(2)}/${renderCpu.p99.toFixed(2)}/${renderCpu.max.toFixed(2)}ms over3=${over3(renderRaw).toFixed(0)}% | ` +
        `tick p50/p95/p99=${tickCpu.p50.toFixed(2)}/${tickCpu.p95.toFixed(2)}/${tickCpu.p99.toFixed(2)}ms tick+render p50/p95/p99/max=${tickRenderCpu.p50.toFixed(2)}/${tickRenderCpu.p95.toFixed(2)}/${tickRenderCpu.p99.toFixed(2)}/${tickRenderCpu.max.toFixed(2)}ms over3=${over3(tickRenderRaw).toFixed(0)}% | ` +
        `panCpu p50/p95/p99=${panCpu.p50.toFixed(2)}/${panCpu.p95.toFixed(2)}/${panCpu.p99.toFixed(2)}ms over3=${over3(panRaw).toFixed(0)}% frames p50/p95/p99/max=${panFrames.p50.toFixed(2)}/${panFrames.p95.toFixed(2)}/${panFrames.p99.toFixed(2)}/${panFrames.max.toFixed(2)}ms missed240=${(panFrames.missed240 * 100).toFixed(1)}% missed60=${(panFrames.missed60 * 100).toFixed(1)}% | ` +
        `zoomCpu p50/p95=${zoomCpu.p50.toFixed(2)}/${zoomCpu.p95.toFixed(2)}ms frames p50/p95/max=${zoomFrames.p50.toFixed(2)}/${zoomFrames.p95.toFixed(2)}/${zoomFrames.max.toFixed(2)}ms missed240=${(zoomFrames.missed240 * 100).toFixed(1)}%`,
    );
    expect(count).toBeGreaterThan(0);
  }
});
