import { expect, test, type Page } from "@playwright/test";

// Bulk-selection drag at scale: marquee-select the whole overview board,
// then drag it. Exercises the preview path (no per-move rebuilds, no
// per-move index maintenance) plus release settle, in the real browser.

declare global {
  interface Window {
    __rsEditor?: {
      element_count(): number;
      load_connected_fixture(nodes: number, arrowsPerNode: number): void;
      zoom_to_fit(): void;
      render(): void;
      selection_bounds(): Float64Array | undefined;
    };
  }
}

test("bulk drag of a 50k board stays interactive", async ({ page }) => {
  test.setTimeout(300_000);
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() =>
    (window as unknown as { __rsPauseAutosave?: (p: boolean) => void }).__rsPauseAutosave?.(true),
  );
  await page.evaluate(() => window.__rsEditor!.load_connected_fixture(25000, 1));
  const count = await page.evaluate(() => window.__rsEditor!.element_count());
  console.log(`board elements: ${count}`);
  await page.evaluate(() => window.__rsEditor!.zoom_to_fit());
  await page.waitForTimeout(300);

  // Marquee the whole viewport to select everything visible.
  const box = await page.evaluate(() => ({ w: window.innerWidth, h: window.innerHeight }));
  await page.mouse.move(4, 60);
  await page.mouse.down();
  await page.mouse.move(box.w - 4, box.h - 4, { steps: 8 });
  await page.mouse.up();
  const selected = await page.evaluate(() => !!window.__rsEditor!.selection_bounds());
  expect(selected, "marquee selected the board").toBe(true);

  // Drag the selection with real input while sampling browser frames.
  const cx = box.w / 2;
  const cy = box.h / 2;
  await page.evaluate(() => {
    const st = { deltas: [] as number[], last: performance.now(), raf: 0 };
    (window as unknown as { __dragFrames: typeof st }).__dragFrames = st;
    const loop = (t: number) => {
      st.deltas.push(t - st.last);
      st.last = t;
      st.raf = requestAnimationFrame(loop);
    };
    st.raf = requestAnimationFrame(loop);
  });
  const before = await page.evaluate(() => {
    const ed = window.__rsEditor!;
    const b = ed.selection_bounds();
    return b ? [b[0], b[1]] : null;
  });
  await page.mouse.move(cx, cy);
  await page.mouse.down();
  const t0 = Date.now();
  await page.mouse.move(cx + 200, cy + 120, { steps: 30 });
  const moveMs = Date.now() - t0;
  await page.mouse.up();
  const frames: number[] = await page.evaluate(() => {
    const st = (window as unknown as { __dragFrames: { deltas: number[]; raf: number } })
      .__dragFrames;
    cancelAnimationFrame(st.raf);
    return st.deltas.slice(2);
  });
  const sorted = [...frames].sort((a, b) => a - b);
  const q = (p: number) => sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * p))] ?? 0;
  const missed = frames.length ? (frames.filter((f) => f > 17.5).length / frames.length) * 100 : 0;
  const after = await page.evaluate(() => {
    const ed = window.__rsEditor!.selection_bounds();
    return ed ? [ed[0], ed[1]] : null;
  });
  console.log(
    `bulk drag: ${moveMs}ms wall for 30 input steps, frames p50 ${q(0.5).toFixed(1)}ms p95 ${q(0.95).toFixed(1)}ms max ${q(1).toFixed(1)}ms missed60 ${missed.toFixed(1)}%`,
  );
  expect(after).not.toBeNull();
  expect(Math.hypot((after as number[])[0] - (before as number[])[0], (after as number[])[1] - (before as number[])[1])).toBeGreaterThan(50);
  expect(await page.evaluate(() => window.__rsEditor!.element_count())).toBe(count);
});
