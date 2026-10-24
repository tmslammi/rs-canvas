import { expect, test, type CDPSession, type Page } from "@playwright/test";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

// CDP profiling harness. Each `capture` records one scenario:
//   <name>.cpuprofile        V8 CPU profile (250 µs samples)
//   <name>.allocprofile.json HeapProfiler sampling profile
//   <name>.json              wall time, Performance metrics, wasm memory,
//                            long tasks and the scenario's own result
// The raw output directory is generated and gitignored; curated evidence is
// copied to profiling/evidence/ by hand (see README).
const OUT = path.join(path.dirname(fileURLToPath(import.meta.url)), "out");
fs.mkdirSync(OUT, { recursive: true });

const INIT_SCRIPT = `(() => {
  const rs = (window.__rsProfile = { grows: [], longTasks: [], memories: [] });
  const capture = (result) => {
    try {
      const instance = result && result.instance ? result.instance : result;
      if (instance && instance.exports) {
        for (const value of Object.values(instance.exports)) {
          if (value instanceof WebAssembly.Memory) rs.memories.push(value);
        }
      }
    } catch (error) {}
  };
  const wrap = (fn) => function (...args) {
    const result = fn.apply(this, args);
    if (result && typeof result.then === "function") {
      return result.then((value) => { capture(value); return value; });
    }
    capture(result);
    return result;
  };
  if (typeof WebAssembly.instantiateStreaming === "function") {
    WebAssembly.instantiateStreaming = wrap(WebAssembly.instantiateStreaming.bind(WebAssembly));
  }
  WebAssembly.instantiate = wrap(WebAssembly.instantiate.bind(WebAssembly));
  const grow = WebAssembly.Memory.prototype.grow;
  WebAssembly.Memory.prototype.grow = function (delta) {
    const result = grow.call(this, delta);
    try { rs.grows.push({ t: performance.now(), delta, bytes: this.buffer.byteLength }); } catch (error) {}
    return result;
  };
  try {
    new PerformanceObserver((list) => {
      for (const entry of list.getEntries()) {
        rs.longTasks.push({ start: entry.startTime, duration: entry.duration });
      }
    }).observe({ type: "longtask", buffered: true });
  } catch (error) {}
})();`;

declare global {
  interface Window {
    __rsProfile?: {
      memories: WebAssembly.Memory[];
      grows: { t: number; delta: number; bytes: number }[];
      longTasks: { start: number; duration: number }[];
    };
  }
}

type MetricsMap = Record<string, number>;
type WasmSnapshot = {
  bytes: number;
  memories: number;
  grows: { t: number; delta: number; bytes: number }[];
};
type Summary = Record<string, unknown> & {
  name: string;
  actionError: string | null;
  result: unknown;
  durationMs: number;
  metrics: MetricsMap;
  wasm: { bytes: number; memories: number; grows: number; growBytes: number };
  longTasks: { count: number; totalMs: number; maxMs: number };
};

const summaries: Summary[] = [];

const CUMULATIVE = new Set([
  "TaskDuration",
  "ScriptDuration",
  "LayoutDuration",
  "RecalcStyleDuration",
  "LayoutCount",
  "RecalcStyleCount",
]);

async function readMetrics(cdp: CDPSession): Promise<MetricsMap> {
  const { metrics } = await cdp.send("Performance.getMetrics");
  const map: MetricsMap = {};
  for (const metric of metrics) map[metric.name] = metric.value;
  return map;
}

function metricDelta(before: MetricsMap, after: MetricsMap): MetricsMap {
  const delta: MetricsMap = {};
  for (const key of Object.keys(after)) {
    if (CUMULATIVE.has(key)) {
      delta[key] = +(after[key] - (before[key] ?? 0)).toFixed(6);
    }
  }
  for (const key of ["JSHeapUsedSize", "JSHeapTotalSize", "Nodes", "Documents", "Frames", "JSEventListeners"]) {
    delta[key] = after[key] ?? 0;
  }
  return delta;
}

async function wasmSnapshot(page: Page): Promise<WasmSnapshot> {
  return page.evaluate(() => {
    const rs = window.__rsProfile as unknown as {
      memories: WebAssembly.Memory[];
      grows: { t: number; delta: number; bytes: number }[];
    };
    const memory = rs.memories[0];
    return { bytes: memory ? memory.buffer.byteLength : 0, memories: rs.memories.length, grows: rs.grows.slice() };
  });
}

async function capture(
  page: Page,
  cdp: CDPSession,
  name: string,
  action: () => Promise<unknown>,
): Promise<Summary> {
  const before = await readMetrics(cdp);
  const startMs = await page.evaluate(() => performance.now());
  await cdp.send("Profiler.start");
  let allocStarted = false;
  try {
    await cdp.send("HeapProfiler.enable");
    await cdp.send("HeapProfiler.startSampling", { samplingInterval: 32768, stackDepth: 32 });
    allocStarted = true;
  } catch {
    // Allocation sampling is not available in every build.
  }
  let actionResult: unknown = null;
  let actionError: string | null = null;
  try {
    actionResult = await action();
  } catch (error) {
    actionError = String(error);
  }
  const endMs = await page.evaluate(() => performance.now());
  let allocProfile: unknown = null;
  if (allocStarted) {
    try {
      allocProfile = await cdp.send("HeapProfiler.stopSampling");
    } catch {
      // Keep the CPU profile even if allocation sampling failed to stop.
    }
  }
  const { profile } = await cdp.send("Profiler.stop");
  const after = await readMetrics(cdp);
  const wasm = await wasmSnapshot(page);
  const longTasks = await page.evaluate(() => {
    const rs = window.__rsProfile as unknown as {
      longTasks: { start: number; duration: number }[];
    };
    return rs.longTasks.splice(0);
  });
  const windowTasks = longTasks.filter((task) => task.start >= startMs && task.start <= endMs);
  const windowGrows = wasm.grows.filter((grow) => grow.t >= startMs && grow.t <= endMs);

  fs.writeFileSync(path.join(OUT, `${name}.cpuprofile`), JSON.stringify(profile));
  if (allocProfile) {
    fs.writeFileSync(path.join(OUT, `${name}.allocprofile.json`), JSON.stringify(allocProfile));
  }
  const summary: Summary = {
    name,
    actionError,
    result: actionResult,
    durationMs: +(endMs - startMs).toFixed(3),
    metrics: metricDelta(before, after),
    wasm: {
      bytes: wasm.bytes,
      memories: wasm.memories,
      grows: windowGrows.length,
      growBytes: windowGrows.length ? windowGrows[windowGrows.length - 1].bytes : wasm.bytes,
    },
    longTasks: {
      count: windowTasks.length,
      totalMs: +windowTasks.reduce((sum, task) => sum + task.duration, 0).toFixed(3),
      maxMs: windowTasks.length ? +Math.max(...windowTasks.map((task) => task.duration)).toFixed(3) : 0,
    },
  };
  fs.writeFileSync(path.join(OUT, `${name}.json`), JSON.stringify(summary, null, 2));
  summaries.push(summary);
  console.log(
    `\n[profile] ${name}: ${summary.durationMs}ms`,
    JSON.stringify(summary.metrics),
    JSON.stringify(summary.wasm),
    JSON.stringify(summary.longTasks),
  );
  if (actionError) throw new Error(`${name}: scenario action failed: ${actionError}`);
  return summary;
}

async function takeHeapSnapshot(cdp: CDPSession, file: string): Promise<void> {
  const chunks: string[] = [];
  const onChunk = (event: { chunk: string }) => chunks.push(event.chunk);
  cdp.on("HeapProfiler.addHeapSnapshotChunk", onChunk);
  await cdp.send("HeapProfiler.enable");
  await cdp.send("HeapProfiler.collectGarbage");
  await cdp.send("HeapProfiler.takeHeapSnapshot", { reportProgress: false, captureNumericValue: false });
  cdp.off("HeapProfiler.addHeapSnapshotChunk", onChunk);
  fs.writeFileSync(file, chunks.join(""));
}

async function boot(page: Page): Promise<CDPSession> {
  await page.addInitScript(INIT_SCRIPT);
  const cdp = await page.context().newCDPSession(page);
  await cdp.send("Performance.enable");
  await cdp.send("Profiler.enable");
  await cdp.send("Profiler.setSamplingInterval", { interval: 250 });
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true, undefined, { timeout: 120_000 });
  await page.evaluate(() =>
    (window as unknown as { __rsPauseAutosave?: (p: boolean) => void }).__rsPauseAutosave?.(true),
  );
  return cdp;
}

test.afterAll(() => {
  fs.writeFileSync(path.join(OUT, "run-summary.json"), JSON.stringify(summaries, null, 2));
});

test("boot: wasm compile and first paint", async ({ page }) => {
  test.setTimeout(300_000);
  await page.addInitScript(INIT_SCRIPT);
  const cdp = await page.context().newCDPSession(page);
  await cdp.send("Performance.enable");
  await cdp.send("Profiler.enable");
  await cdp.send("Profiler.setSamplingInterval", { interval: 250 });
  const before = await readMetrics(cdp);
  const startMs = await page.evaluate(() => performance.now());
  await cdp.send("Profiler.start");
  const start = Date.now();
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true, undefined, { timeout: 120_000 });
  const bootMs = Date.now() - start;
  const endMs = await page.evaluate(() => performance.now());
  const { profile } = await cdp.send("Profiler.stop");
  const after = await readMetrics(cdp);
  const wasm = await wasmSnapshot(page);
  const longTasks = await page.evaluate(
    ([from, to]) => {
      const rs = window.__rsProfile as unknown as {
        longTasks: { start: number; duration: number }[];
      };
      return rs.longTasks.splice(0).filter((task) => task.start >= from && task.start <= to);
    },
    [startMs, endMs] as const,
  );
  fs.writeFileSync(path.join(OUT, "boot.cpuprofile"), JSON.stringify(profile));
  const windowGrows = wasm.grows.filter((grow) => grow.t >= startMs && grow.t <= endMs);
  const summary: Summary = {
    name: "boot",
    actionError: null,
    result: { bootMs },
    durationMs: +(endMs - startMs).toFixed(3),
    metrics: metricDelta(before, after),
    wasm: {
      bytes: wasm.bytes,
      memories: wasm.memories,
      grows: windowGrows.length,
      growBytes: windowGrows.length ? windowGrows[windowGrows.length - 1].bytes : wasm.bytes,
    },
    longTasks: {
      count: longTasks.length,
      totalMs: +longTasks.reduce((sum, task) => sum + task.duration, 0).toFixed(3),
      maxMs: longTasks.length ? +Math.max(...longTasks.map((task) => task.duration)).toFixed(3) : 0,
    },
    bootMs,
  };
  fs.writeFileSync(path.join(OUT, "boot.json"), JSON.stringify(summary, null, 2));
  summaries.push(summary);
  console.log(`\n[profile] boot: ${bootMs}ms`, JSON.stringify(summary.metrics), JSON.stringify(summary.wasm));
});

test("load + save 10k mixed elements", async ({ page }) => {
  test.setTimeout(300_000);
  const cdp = await boot(page);
  await capture(page, cdp, "load-10k", () =>
    page.evaluate(() => {
      const editor = window.__rsEditor!;
      const start = performance.now();
      editor.load_benchmark_fixture(10000);
      return { loadMs: +(performance.now() - start).toFixed(3), count: editor.element_count() };
    }),
  );
  await capture(page, cdp, "save-json-10k", () =>
    page.evaluate(() => {
      const editor = window.__rsEditor!;
      const start = performance.now();
      const json = editor.save_json();
      return { saveMs: +(performance.now() - start).toFixed(3), bytes: json.length };
    }),
  );
  await capture(page, cdp, "reload-json-10k", () =>
    page.evaluate(() => {
      const editor = window.__rsEditor!;
      const json = editor.save_json();
      const start = performance.now();
      editor.load_json(json);
      return { loadMs: +(performance.now() - start).toFixed(3), count: editor.element_count() };
    }),
  );
  await takeHeapSnapshot(cdp, path.join(OUT, "load-10k.heapsnapshot"));
});

test("100k-node connected board: load, drag, pan, zoom", async ({ page }) => {
  test.setTimeout(900_000);
  const cdp = await boot(page);
  await capture(page, cdp, "load-100k-connected", () =>
    page.evaluate(() => {
      const editor = window.__rsEditor!;
      const start = performance.now();
      editor.load_connected_fixture(100_000, 1);
      return { loadMs: +(performance.now() - start).toFixed(3), count: editor.element_count() };
    }),
  );
  await page.waitForTimeout(500);
  await capture(page, cdp, "drag-node-100k", async () => {
    await page.mouse.move(90, 65);
    await page.mouse.down();
    await page.mouse.move(290, 265, { steps: 160 });
    await page.mouse.up();
  });
  await capture(page, cdp, "pan-100k", async () => {
    await page.mouse.move(640, 400);
    for (let i = 0; i < 40; i++) await page.mouse.wheel(0, 12);
  });
  await capture(page, cdp, "zoom-100k", async () => {
    await page.mouse.move(640, 400);
    await page.keyboard.down("Control");
    for (let i = 0; i < 30; i++) await page.mouse.wheel(0, -12);
    await page.keyboard.up("Control");
  });
  await cdp.send("HeapProfiler.collectGarbage");
  const metrics = await readMetrics(cdp);
  console.log(`\n[profile] 100k after GC: JSHeapUsed=${(metrics.JSHeapUsedSize / 1048576).toFixed(1)}MB`);
});

test("editing ops on 10k: select-all/delete/undo, freehand, connector", async ({ page }) => {
  test.setTimeout(300_000);
  const cdp = await boot(page);
  await page.evaluate(() => window.__rsEditor!.load_benchmark_fixture(10000));
  await capture(page, cdp, "select-all-delete-undo", () =>
    page.evaluate(() => {
      const editor = window.__rsEditor as unknown as {
        key_down(key: string, shift: boolean, alt: boolean, ctrl: boolean, meta: boolean): boolean;
        element_count(): number;
      };
      const t0 = performance.now();
      editor.key_down("a", false, false, false, true);
      const selectMs = performance.now() - t0;
      const afterSelect = editor.element_count();
      const t1 = performance.now();
      editor.key_down("Delete", false, false, false, false);
      const deleteMs = performance.now() - t1;
      const afterDelete = editor.element_count();
      const t2 = performance.now();
      editor.key_down("z", false, false, false, true);
      const undoMs = performance.now() - t2;
      return { selectMs, deleteMs, undoMs, afterSelect, afterDelete, count: editor.element_count() };
    }),
  );
  await capture(page, cdp, "freehand-300-points", async () => {
    await page.evaluate(() => window.__rsEditor!.set_tool("freehand"));
    await page.mouse.move(300, 300);
    await page.mouse.down();
    for (let i = 1; i <= 300; i++) {
      await page.mouse.move(300 + i * 2, 300 + Math.sin(i / 12) * 80);
    }
    await page.mouse.up();
    return { count: await page.evaluate(() => window.__rsEditor!.element_count()) };
  });
  await capture(page, cdp, "connector-drag", async () => {
    await page.evaluate(() => window.__rsEditor!.set_tool("arrow"));
    await page.mouse.move(100, 60);
    await page.mouse.down();
    for (let i = 1; i <= 60; i++) {
      await page.mouse.move(100 + i * 8, 60 + i * 5);
    }
    await page.mouse.up();
  });
  await capture(page, cdp, "eraser-drag", async () => {
    await page.evaluate(() => window.__rsEditor!.set_tool("eraser"));
    await page.mouse.move(120, 60);
    await page.mouse.down();
    for (let i = 1; i <= 50; i++) {
      await page.mouse.move(120 + i * 20, 60);
    }
    await page.mouse.up();
  });
});

test("memory churn: repeated delete/undo cycles and strokes", async ({ page }) => {
  test.setTimeout(300_000);
  const cdp = await boot(page);
  await page.evaluate(() => window.__rsEditor!.load_benchmark_fixture(10000));
  await cdp.send("HeapProfiler.collectGarbage");
  const log: string[] = [];
  const bytes = async () => (await wasmSnapshot(page)).bytes / 1048576;
  log.push(`loaded: ${(await bytes()).toFixed(1)}MB`);
  for (let cycle = 0; cycle < 5; cycle++) {
    await page.evaluate(() => {
      const editor = window.__rsEditor as unknown as {
        key_down(k: string, s: boolean, a: boolean, c: boolean, m: boolean): boolean;
      };
      editor.key_down("a", false, false, false, true);
      editor.key_down("Delete", false, false, false, false);
      editor.key_down("z", false, false, false, true);
    });
    await cdp.send("HeapProfiler.collectGarbage");
    const metrics = await readMetrics(cdp);
    log.push(
      `delete/undo ${cycle + 1}: wasm ${(await bytes()).toFixed(1)}MB, jsHeap ${(metrics.JSHeapUsedSize / 1048576).toFixed(2)}MB, count ${await page.evaluate(() => window.__rsEditor!.element_count())}`,
    );
  }
  await page.evaluate(() => window.__rsEditor!.set_tool("freehand"));
  for (let stroke = 0; stroke < 5; stroke++) {
    await page.mouse.move(300, 300);
    await page.mouse.down();
    for (let i = 1; i <= 100; i++) await page.mouse.move(300 + i * 2, 300 + Math.sin(i / 8) * 50);
    await page.mouse.up();
    log.push(`stroke ${stroke + 1}: wasm ${(await bytes()).toFixed(1)}MB`);
  }
  fs.writeFileSync(path.join(OUT, "memory-churn.txt"), log.join("\n"));
  console.log(`\n[churn]\n${log.join("\n")}`);
  expect(log.length).toBeGreaterThan(5);
});
