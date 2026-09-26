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

  expect(p95).toBeLessThan(50);
  expect(max).toBeLessThan(150);

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
