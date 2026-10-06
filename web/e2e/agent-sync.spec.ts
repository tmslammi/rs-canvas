import { spawn, type ChildProcess } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { expect, test, type Page } from "@playwright/test";
import { elementCount, mcpClient, toolJson } from "./mcp-client";

// Document sync: the page and the server keep one drawing through the
// merge-ready model. This spec runs against its own server on 4175 (its
// assertions are exact element counts) and in serial.

test.describe.configure({ mode: "serial" });

const BASE = "http://localhost:4175";

async function openPage(page: Page) {
  await page.goto(`${BASE}/`);
  await page.waitForFunction(() => window.__rsReady === true);
  await page.waitForFunction(() => window.__rsAgentConnected === true);
}

async function drawRectangle(page: Page) {
  await page.keyboard.press("r");
  await page.mouse.move(420, 320);
  await page.mouse.down();
  await page.mouse.move(580, 400, { steps: 5 });
  await page.mouse.up();
}

test("a human edit reaches the agent", async ({ page, request }) => {
  const mcp = mcpClient(request, BASE);
  const before = elementCount(await mcp.text("canvas_read", {}));
  await openPage(page);
  await drawRectangle(page);

  await expect
    .poll(async () => elementCount(await mcp.text("canvas_read", {})), {
      timeout: 5_000,
    })
    .toBe(before + 1);
  expect(await mcp.text("canvas_read", {})).toContain("rect");
});

test("undoing a creation propagates as a deletion", async ({
  page,
  request,
}) => {
  await openPage(page);
  const mcp = mcpClient(request, BASE);
  await drawRectangle(page);
  await expect
    .poll(async () => elementCount(await mcp.text("canvas_read", {})), {
      timeout: 5_000,
    })
    .toBe(1);

  // Undo purges the element (no tombstone); it must still disappear from the
  // mirror, or the next agent broadcast resurrects it.
  await page.keyboard.press("ControlOrMeta+z");
  await expect
    .poll(async () => elementCount(await mcp.text("canvas_read", {})), {
      timeout: 5_000,
    })
    .toBe(0);

  await mcp.call("canvas_draw", { kind: "sticky", text: "Titanic" });
  await page.waitForFunction(
    () => window.__rsEditor!.element_count() === 2,
    undefined,
    { timeout: 3_000 },
  );
  const liveRectangle = await page.evaluate(() => {
    const doc = JSON.parse(window.__rsEditor!.save_json());
    return Object.values<Record<string, unknown>>(doc.elements).some(
      (element) => element.type === "rectangle" && !element.isDeleted,
    );
  });
  expect(liveRectangle, "the purged rectangle resurrected").toBe(false);
  const described = await mcp.text("canvas_read", {});
  expect(described).not.toContain(" rect ");
  expect(described).toContain("Titanic");
});

test("an agent draw appears without a reload", async ({ page, request }) => {
  await openPage(page);
  const before = await page.evaluate(() => window.__rsEditor!.element_count());
  const mcp = mcpClient(request, BASE);
  const created = toolJson(
    await mcp.call("canvas_draw", { kind: "sticky", text: "Queue" }),
  );

  // A sticky is two elements: the note and its bound label.
  await page.waitForFunction(
    (count) => window.__rsEditor!.element_count() === count,
    before + 2,
    { timeout: 3_000 },
  );
  expect(typeof created.id).toBe("string");
});

test("a canvas_pen stroke lands as a freehand element", async ({
  page,
  request,
}) => {
  await openPage(page);
  const before = await page.evaluate(() => window.__rsEditor!.element_count());
  const mcp = mcpClient(request, BASE);
  const created = toolJson(
    await mcp.call("canvas_pen", {
      points: [
        { x: 100, y: 100, pressure: 0.2 },
        { x: 160, y: 140, pressure: 1.0 },
        { x: 220, y: 100 },
      ],
      pressure: 0.6,
      stroke_width: 5,
      stroke_color: "#ff8787",
      fill_color: "#ffec99",
      fill_style: "hachure",
    }),
  );

  await page.waitForFunction(
    (count) => window.__rsEditor!.element_count() === count,
    before + 1,
    { timeout: 3_000 },
  );
  expect(typeof created.id).toBe("string");
  expect(await mcp.text("canvas_read", {})).toContain("stroke");

  // The pressure profile and ink style survive the trip to the page.
  const element = await page.evaluate(() => {
    const doc = JSON.parse(window.__rsEditor!.save_json());
    return Object.values<Record<string, any>>(doc.elements).find(
      (e) => e.type === "freehand",
    );
  });
  expect(element.pressures).toEqual([0.2, 1.0, 0.6]);
  expect(element.style.strokeColor).toBe("#ff8787");
  expect(element.style.strokeWidth).toBe(5);
  expect(element.style.fillColor).toBe("#ffec99");
});

test("canvas_script builds a diagram in one atomic call", async ({
  page,
  request,
}) => {
  await openPage(page);
  const before = await page.evaluate(() => window.__rsEditor!.element_count());
  const mcp = mcpClient(request, BASE);
  const created = toolJson(
    await mcp.call("canvas_script", {
      beats: [
        { draw: { as: "client", kind: "sticky", text: "Client" } },
        { draw: { as: "vpc", kind: "frame", name: "VPC", around: ["client"] } },
        { draw: { as: "db", kind: "sticky", text: "DB", near: "vpc" } },
        { connect: { from: "client", to: "db", label: "SQL" } },
      ],
    }),
  );

  // client + label, frame, db + label, arrow + label = 7 elements.
  await page.waitForFunction(
    (count) => window.__rsEditor!.element_count() === count,
    before + 7,
    { timeout: 3_000 },
  );
  expect(created.created.client).toBeTruthy();
  expect(created.created.db).toBeTruthy();
});

test("agent edits stay out of the user's undo stack", async ({
  page,
  request,
}) => {
  await openPage(page);
  expect(await page.evaluate(() => window.__rsEditor!.can_undo())).toBe(false);
  const before = await page.evaluate(() => window.__rsEditor!.element_count());
  const mcp = mcpClient(request, BASE);
  await mcp.call("canvas_draw", { kind: "rectangle", text: "API" });

  await page.waitForFunction(
    (count) => window.__rsEditor!.element_count() === count,
    before + 2,
    { timeout: 3_000 },
  );
  expect(await page.evaluate(() => window.__rsEditor!.can_undo())).toBe(false);
});

test("the server keeps drawings while no page is open", async ({
  context,
  request,
}) => {
  const mcp = mcpClient(request, BASE);
  await mcp.call("canvas_draw", { kind: "ellipse", text: "LB" });

  const page = await context.newPage();
  await openPage(page);
  await page.waitForFunction(
    () => window.__rsEditor!.save_json().includes("LB"),
    undefined,
    { timeout: 3_000 },
  );
  await page.close();
});

test("the agent can see the live viewport", async ({ page, request }) => {
  await openPage(page);
  const mcp = mcpClient(request, BASE);
  await mcp.call("canvas_script", {
    beats: [{ draw: { as: "a", kind: "sticky", text: "Hello" } }],
  });
  const before = await page.evaluate(() => ({
    scroll: Array.from(window.__rsEditor!.scroll()),
    zoom: window.__rsEditor!.zoom(),
  }));

  const result = await mcp.call("canvas_screenshot", {});
  const image = result.content?.find((block) => block.type === "image");
  expect(image?.mimeType).toBe("image/png");
  expect((image?.data ?? "").length).toBeGreaterThan(100);

  const after = await page.evaluate(() => ({
    scroll: Array.from(window.__rsEditor!.scroll()),
    zoom: window.__rsEditor!.zoom(),
  }));
  expect(after).toEqual(before);
});

test("the agent can see without a browser", async ({ request }) => {
  const mcp = mcpClient(request, BASE);
  await mcp.call("canvas_draw", { kind: "rectangle", text: "Offline" });

  const result = await mcp.call("canvas_screenshot", {});
  const image = result.content?.find((block) => block.type === "image");
  expect(image?.mimeType).toBe("image/png");
  expect((image?.data ?? "").length).toBeGreaterThan(100);
});

test("the agent sees the human's view and what changed", async ({
  page,
  request,
}) => {
  await openPage(page);
  const mcp = mcpClient(request, BASE);
  const before = await mcp.text("canvas_read", {});
  const revision = Number(/^revision (\d+)/.exec(before)?.[1] ?? -1);
  expect(revision).toBeGreaterThanOrEqual(0);

  // Drawing selects the new shape and syncs; the hint follows within a beat.
  await drawRectangle(page);
  await expect
    .poll(
      async () => {
        const status = JSON.parse(await mcp.text("canvas_status", {}));
        return status.hints?.selection?.length ?? 0;
      },
      { timeout: 5_000 },
    )
    .toBeGreaterThan(0);

  // The snapshot publish is debounced behind the hint; wait for the change
  // record to land before reading the delta.
  await expect
    .poll(async () => await mcp.text("canvas_read", { since: revision }), {
      timeout: 5_000,
    })
    .toContain("changed now:");
  const report = await mcp.text("canvas_read", { since: revision });
  expect(report).toContain("rect");
  const status = JSON.parse(await mcp.text("canvas_status", {}));
  expect(status.hints.camera.zoom).toBeGreaterThan(0);
  expect(status.hints.note).toContain("transient");
  expect(status.revision).toBeGreaterThan(revision);
});

test("discussion verbs work on the live canvas", async ({ page, request }) => {
  await openPage(page);
  const before = await page.evaluate(() => window.__rsEditor!.element_count());
  const mcp = mcpClient(request, BASE);
  const created = toolJson(
    await mcp.call("canvas_script", {
      beats: [
        { draw: { as: "a", kind: "sticky", text: "A" } },
        { draw: { as: "b", kind: "sticky", text: "B" } },
      ],
    }),
  );
  const a = created.created.a as string;
  const b = created.created.b as string;

  await mcp.call("canvas_annotate", { id: a, mark: "cross" });
  await mcp.call("canvas_emphasis", { ids: [b], mode: "dim" });
  await mcp.call("canvas_tidy", { ids: [a, b] });
  const cleared = toolJson(await mcp.call("canvas_clear", { ids: [a] }));
  expect(cleared.cleared).toBe(2);

  // Left: b + its label + the two cross strokes.
  await page.waitForFunction(
    (count) => window.__rsEditor!.element_count() === count,
    before + 4,
    { timeout: 3_000 },
  );
  const json = await page.evaluate(() => window.__rsEditor!.save_json());
  expect(json).toContain('"text": "B"');
  expect(json).toContain('"type": "freehand"');
});

test("an autosave and the server mirror merge on reconnect", async ({
  context,
  request,
}) => {
  const mcp = mcpClient(request, BASE);
  const base = elementCount(await mcp.text("canvas_read", {}));

  const first = await context.newPage();
  await openPage(first);
  await drawRectangle(first);
  // The human edit must have reached the server before the page goes away.
  await expect
    .poll(async () => elementCount(await mcp.text("canvas_read", {})), {
      timeout: 5_000,
    })
    .toBe(base + 1);
  await first.close();

  // The agent draws while the page is closed; the server keeps it.
  await mcp.call("canvas_draw", { kind: "sticky", text: "Cache" });

  // A fresh page loads its IndexedDB autosave first, then reconciles the
  // server mirror: both sides survive.
  const second = await context.newPage();
  await openPage(second);
  await second.waitForFunction(
    (count) => window.__rsEditor!.element_count() === count,
    base + 3,
    { timeout: 3_000 },
  );
  const json = await second.evaluate(() => window.__rsEditor!.save_json());
  expect(json).toContain("Cache");
  await second.close();
});

test("a server restart preserves the document", async ({ request }) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "rs-canvas-e2e-"));
  const docPath = path.join(dir, "document.json");
  const binary = path.resolve(process.cwd(), "..", "target", "debug", "canvas-server");

  const start = () =>
    spawn(binary, ["--web", ".", "--port", "4176", "--doc", docPath], {
      cwd: process.cwd(),
      stdio: "ignore",
    });
  const waitForServer = async (url: string) => {
    for (let attempt = 0; attempt < 100; attempt += 1) {
      try {
        if ((await fetch(url)).ok) return;
      } catch {
        // not up yet
      }
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    throw new Error(`server at ${url} did not start`);
  };
  const waitForExit = (child: ChildProcess) =>
    new Promise<void>((resolve) => child.once("exit", () => resolve()));

  let server = start();
  try {
    await waitForServer("http://localhost:4176");
    const first = mcpClient(request, "http://localhost:4176");
    const created = toolJson(
      await first.call("canvas_draw", { kind: "diamond", text: "Decision" }),
    );

    server.kill();
    await waitForExit(server);
    server = start();
    await waitForServer("http://localhost:4176");

    const second = mcpClient(request, "http://localhost:4176");
    const described = await second.text("canvas_read", {});
    expect(described).toContain(created.id as string);
    expect(described).toContain("Decision");
  } finally {
    server.kill();
    if (server.exitCode === null) await waitForExit(server);
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
