import { expect, test } from "@playwright/test";

const count = (page: import("@playwright/test").Page): Promise<number> =>
  page.evaluate(() => window.__rsEditor!.element_count());

const percentile = (values: number[], fraction: number): number => {
  const sorted = [...values].sort((a, b) => a - b);
  const index = Math.min(sorted.length - 1, Math.floor(sorted.length * fraction));
  return sorted[index];
};

/** Wasm linear memory may grow under a bounded history's peak, but five
 * delete/undo cycles must not retain anything like the old two-clone
 * tombstones (which reached ~220MB on this fixture). */
const WASM_MEMORY_BUDGET = 64 * 1024 * 1024;

const wasmMemoryBytes = (page: import("@playwright/test").Page): Promise<number> =>
  page.evaluate(() => {
    const memories =
      (window as unknown as { __rsWasmMemories?: WebAssembly.Memory[] })
        .__rsWasmMemories ?? [];
    return memories.reduce(
      (largest, memory) => Math.max(largest, memory.buffer.byteLength),
      0,
    );
  });

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

test("delete/undo cycles keep wasm memory bounded", async ({ page }) => {
  test.setTimeout(60_000);
  // Capture every wasm memory this page creates. The generated glue
  // instantiates through one of these two entry points.
  await page.addInitScript(() => {
    const memories: WebAssembly.Memory[] = [];
    (
      window as unknown as { __rsWasmMemories: WebAssembly.Memory[] }
    ).__rsWasmMemories = memories;
    const record = (instance: WebAssembly.Instance) => {
      const memory = instance.exports.memory;
      if (memory instanceof WebAssembly.Memory) memories.push(memory);
    };

    const instantiate = WebAssembly.instantiate;
    WebAssembly.instantiate = function (...args: unknown[]) {
      return (instantiate as (...callArgs: unknown[]) => Promise<unknown>)
        .apply(WebAssembly, args)
        .then((result: unknown) => {
          const instantiated = result as
            | WebAssembly.Instance
            | { instance: WebAssembly.Instance };
          record(
            instantiated instanceof WebAssembly.Instance
              ? instantiated
              : instantiated.instance,
          );
          return result;
        });
    } as typeof WebAssembly.instantiate;

    const instantiateStreaming = WebAssembly.instantiateStreaming;
    WebAssembly.instantiateStreaming = function (...args: unknown[]) {
      return (instantiateStreaming as (...callArgs: unknown[]) => Promise<unknown>)
        .apply(WebAssembly, args)
        .then((result: unknown) => {
          record((result as { instance: WebAssembly.Instance }).instance);
          return result;
        });
    } as typeof WebAssembly.instantiateStreaming;
  });

  const cdp = await page.context().newCDPSession(page);
  await cdp.send("HeapProfiler.enable");
  const collectGarbage = async () => {
    await cdp.send("HeapProfiler.collectGarbage");
    await cdp.send("HeapProfiler.collectGarbage");
  };

  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.load_benchmark_fixture(10000));
  expect(await count(page)).toBe(10000);
  // Let the debounced autosave settle before taking the baseline.
  await page.waitForTimeout(800);
  await collectGarbage();
  const loadedBytes = await wasmMemoryBytes(page);

  const cycles: number[] = [];
  for (let cycle = 0; cycle < 5; cycle++) {
    await page.keyboard.press("ControlOrMeta+a");
    await page.keyboard.press("Delete");
    expect(await count(page), `cycle ${cycle + 1}: delete-all`).toBe(0);
    await page.keyboard.press("ControlOrMeta+z");
    expect(await count(page), `cycle ${cycle + 1}: undo restores all`).toBe(10000);
    await collectGarbage();
    cycles.push(await wasmMemoryBytes(page));
  }
  // Settle the autosave the last undo scheduled, then measure again.
  await page.waitForTimeout(800);
  await collectGarbage();
  const finalBytes = await wasmMemoryBytes(page);

  const mb = (bytes: number) => `${(bytes / (1024 * 1024)).toFixed(1)}MB`;
  console.log(
    `history memory: post-load ${mb(loadedBytes)}; cycles ${cycles.map(mb).join(", ")}; final ${mb(finalBytes)}; growth ${mb(finalBytes - loadedBytes)}`,
  );

  expect(loadedBytes, "the wasm heap was captured").toBeGreaterThan(0);
  expect(finalBytes - loadedBytes).toBeLessThanOrEqual(WASM_MEMORY_BUDGET);
});
