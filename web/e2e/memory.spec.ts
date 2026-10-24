import { expect, test } from "@playwright/test";

const count = (page: import("@playwright/test").Page): Promise<number> =>
  page.evaluate(() => window.__rsEditor!.element_count());

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
