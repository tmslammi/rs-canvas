import { expect, test } from "@playwright/test";

const count = (page: import("@playwright/test").Page): Promise<number> =>
  page.evaluate(() => window.__rsEditor!.element_count());

const percentile = (values: number[], fraction: number): number => {
  const sorted = [...values].sort((a, b) => a - b);
  const index = Math.min(sorted.length - 1, Math.floor(sorted.length * fraction));
  return sorted[index];
};

test("10,000 elements stay interactive", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);

  const loadMs = await page.evaluate(() => {
    const start = performance.now();
    window.__rsEditor!.load_benchmark_fixture(10000);
    return performance.now() - start;
  });
  expect(await count(page)).toBe(10000);

  const frameTimes = await page.evaluate(() => {
    const editor = window.__rsEditor!;
    const times: number[] = [];
    for (let i = 0; i < 120; i++) {
      const direction = i % 2 === 0 ? 1 : -1;
      const start = performance.now();
      editor.wheel(400, 300, direction * 6, direction * 4, false);
      times.push(performance.now() - start);
    }
    return times;
  });

  const p50 = percentile(frameTimes, 0.5);
  const p95 = percentile(frameTimes, 0.95);
  const max = Math.max(...frameTimes);
  console.log(
    `scale: load 10k in ${loadMs.toFixed(0)}ms; pan frame p50 ${p50.toFixed(1)}ms, p95 ${p95.toFixed(1)}ms, max ${max.toFixed(1)}ms`,
  );

  // Panning must stay inside a 60fps frame: it shifts existing pixels and
  // repaints only the newly exposed strips.
  expect(p95).toBeLessThan(8);
  expect(max).toBeLessThan(60);

  // Editing stays responsive: pan back, draw one shape, undo.
  await page.keyboard.press("r");
  await page.mouse.move(300, 300);
  await page.mouse.down();
  await page.mouse.move(400, 360, { steps: 4 });
  await page.mouse.up();
  expect(await count(page)).toBe(10001);
  await page.keyboard.press("ControlOrMeta+z");
  expect(await count(page)).toBe(10000);
});

test("mass delete and undo scale linearly", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);

  // App work for one select-all -> delete -> undo cycle: the editor command
  // plus the scene/index rebuild and repaint it triggers. Three samples,
  // median reported, so one GC pause cannot fail the scaling assertion.
  const measure = async (n: number) => {
    expect(
      await page.evaluate((elements) => {
        window.__rsEditor!.load_benchmark_fixture(elements);
        return window.__rsEditor!.element_count();
      }, n),
    ).toBe(n);

    const samples = await page.evaluate(() => {
      const editor = window.__rsEditor!;
      const runs: { delete: number; undo: number; total: number }[] = [];
      for (let i = 0; i < 3; i++) {
        editor.key_down("a", false, false, true, false);
        const deleteStart = performance.now();
        editor.key_down("Delete", false, false, false, false);
        const deleted = performance.now() - deleteStart;
        const undoStart = performance.now();
        editor.undo();
        const undone = performance.now() - undoStart;
        runs.push({ delete: deleted, undo: undone, total: deleted + undone });
      }
      return runs;
    });
    expect(await count(page)).toBe(n);

    const pick = (key: "delete" | "undo" | "total") => {
      const values = samples.map((sample) => sample[key]).sort((a, b) => a - b);
      return values[1];
    };
    return { delete: pick("delete"), undo: pick("undo"), total: pick("total") };
  };

  const small = await measure(10000);
  const large = await measure(20000);
  console.log(
    `scale: mass edit 10k delete ${small.delete.toFixed(1)}ms, undo ${small.undo.toFixed(1)}ms, total ${small.total.toFixed(1)}ms; ` +
      `20k delete ${large.delete.toFixed(1)}ms, undo ${large.undo.toFixed(1)}ms, total ${large.total.toFixed(1)}ms; ` +
      `20k/10k ${(large.total / small.total).toFixed(2)}x`,
  );

  // Target budget is 100ms of app work per operation. Keep CI headroom for
  // slower wasm/GC rather than asserting the target exactly.
  expect(small.delete).toBeLessThan(200);
  expect(small.undo).toBeLessThan(200);
  // Quadratic growth would be ~4x for a doubled board; allow the linear 2.5x
  // plus a small fixed tolerance for timer and GC noise.
  expect(large.total).toBeLessThan(small.total * 2.5 + 25);

  // Hit-testing still resolves elements restored by the mass undo (first
  // fixture shape sits at world 20,20 at the default camera).
  await page.mouse.click(60, 40);
  expect(
    await page.evaluate(() => window.__rsEditor!.selection_bounds()),
    "restored element is selectable",
  ).toBeDefined();
});
