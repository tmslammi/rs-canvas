import { expect, test, type Page } from "@playwright/test";

// The heavy board: 100k nodes wired by bound connectors. Dragging, panning
// and zooming must hold roughly 60fps (16.7ms frames) on a normal machine.

const NODES = 100_000;
const ARROWS_PER_NODE = 1;

type FrameStats = { frames: number; median: number; p95: number; max: number };

async function measureFrames(page: Page, action: () => Promise<void>): Promise<FrameStats> {
  await page.evaluate(() => {
    const state = {
      deltas: [] as number[],
      last: performance.now(),
      raf: 0,
    };
    (window as unknown as { __frameState: typeof state }).__frameState = state;
    const loop = (t: number) => {
      state.deltas.push(t - state.last);
      state.last = t;
      state.raf = requestAnimationFrame(loop);
    };
    state.raf = requestAnimationFrame(loop);
  });
  await action();
  return page.evaluate(() => {
    const state = (
      window as unknown as {
        __frameState: { deltas: number[]; raf: number };
      }
    ).__frameState;
    cancelAnimationFrame(state.raf);
    const deltas = state.deltas.slice(2).sort((a, b) => a - b);
    const quantile = (p: number) =>
      deltas[Math.min(deltas.length - 1, Math.floor(deltas.length * p))] ?? 0;
    return {
      frames: deltas.length,
      median: quantile(0.5),
      p95: quantile(0.95),
      max: deltas[deltas.length - 1] ?? 0,
    };
  });
}

function report(label: string, stats: FrameStats): void {
  console.log(
    `${label}: ${stats.frames} frames, median ${stats.median.toFixed(1)}ms, ` +
      `p95 ${stats.p95.toFixed(1)}ms, max ${stats.max.toFixed(1)}ms`,
  );
}

test("100k-node connected board holds 60fps while dragging, panning and zooming", async ({
  page,
}) => {
  test.setTimeout(600_000);
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() =>
    (window as unknown as { __rsPauseAutosave?: (p: boolean) => void }).__rsPauseAutosave?.(true),
  );

  const loadMs = await page.evaluate(
    ([nodes, arrows]) => {
      const start = performance.now();
      window.__rsEditor!.load_connected_fixture(nodes, arrows);
      return performance.now() - start;
    },
    [NODES, ARROWS_PER_NODE],
  );
  const count = await page.evaluate(() => window.__rsEditor!.element_count());
  console.log(`loaded ${count} elements in ${Math.round(loadMs)}ms`);
  expect(count, "fixture loaded").toBeGreaterThan(190_000);

  // Give the browser a couple of quiet frames after the load.
  await page.waitForTimeout(200);

  // Drag a node that carries bound connectors.
  const drag = await measureFrames(page, async () => {
    await page.mouse.move(90, 65);
    await page.mouse.down();
    await page.mouse.move(290, 265, { steps: 160 });
    await page.mouse.up();
  });
  report("drag", drag);

  const pan = await measureFrames(page, async () => {
    await page.mouse.move(640, 400);
    for (let i = 0; i < 40; i++) {
      await page.mouse.wheel(0, 12);
    }
  });
  report("pan", pan);

  const zoom = await measureFrames(page, async () => {
    await page.mouse.move(640, 400);
    await page.keyboard.down("Control");
    for (let i = 0; i < 30; i++) {
      await page.mouse.wheel(0, -12);
    }
    await page.keyboard.up("Control");
  });
  report("zoom", zoom);

  // 16.67ms is the ideal 60fps frame; allow a hair of timer slack on the
  // median and one dropped frame (33.3ms) at the 95th percentile. Shared CI
  // runners are noisier, so they get a looser budget.
  const medianBudget = process.env.CI ? 20 : 17.5;
  const p95Budget = process.env.CI ? 50 : 40;
  for (const [label, stats] of [
    ["drag", drag],
    ["pan", pan],
    ["zoom", zoom],
  ] as const) {
    expect(stats.frames, `${label} produced frames`).toBeGreaterThan(5);
    expect(stats.median, `${label} median frame time`).toBeLessThanOrEqual(medianBudget);
    expect(stats.p95, `${label} p95 frame time`).toBeLessThanOrEqual(p95Budget);
  }
});
