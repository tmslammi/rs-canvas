import { expect, test } from "@playwright/test";
import { mcpClient, toolJson } from "./mcp-client";

// The agent bridge: the same MCP endpoint opencode uses, driven over HTTP here.
// Every test exercises server -> WebSocket -> wasm, so the whole view path is
// covered without an LLM in the loop.
//
// This spec runs against its own server on 4174 and in serial: view commands
// broadcast to every connected page, so parallel pages would stomp each other.

test.describe.configure({ mode: "serial" });

const BASE = "http://localhost:4174";

const camera = (page: import("@playwright/test").Page) =>
  page.evaluate(() => ({
    scroll: Array.from(window.__rsEditor!.scroll()),
    zoom: window.__rsEditor!.zoom(),
  }));

/** An element id plus its world-space centre, picked from the seeded demo. */
const pickTarget = (page: import("@playwright/test").Page) =>
  page.evaluate(() => {
    const doc = JSON.parse(window.__rsEditor!.save_json());
    for (const [id, element] of Object.entries<Record<string, unknown>>(
      doc.elements,
    )) {
      const rect = element.rect as
        | { x: number; y: number; w: number; h: number }
        | undefined;
      const type = element.type as string | undefined;
      if (
        rect &&
        !element.isDeleted &&
        (type === "shape" || type === "glyph") &&
        rect.w > 60 &&
        rect.w < 600
      ) {
        return { id, cx: rect.x + rect.w / 2, cy: rect.y + rect.h / 2 };
      }
    }
    throw new Error("no framing target in the demo document");
  });

async function seed(page: import("@playwright/test").Page) {
  await page.goto(`${BASE}/`);
  await page.waitForFunction(() => window.__rsReady === true);
  await page.waitForFunction(() => window.__rsAgentConnected === true);
  await page.getByTitle("Seed demo").click();
}

test("canvas_zoom glides the camera in", async ({ page, request }) => {
  await seed(page);
  const before = await camera(page);
  const mcp = mcpClient(request, BASE);
  await mcp.call("canvas_zoom", { mode: "in" });

  await page.waitForFunction(
    (zoom) => window.__rsEditor!.zoom() > zoom + 0.05,
    before.zoom,
    { timeout: 3_000 },
  );
  expect((await camera(page)).zoom).toBeGreaterThan(before.zoom);
});

test("canvas_focus gently frames an off-screen element", async ({
  page,
  request,
}) => {
  await page.goto(`${BASE}/`);
  await page.waitForFunction(() => window.__rsReady === true);
  await page.waitForFunction(() => window.__rsAgentConnected === true);
  const mcp = mcpClient(request, BASE);
  const created = toolJson(
    await mcp.call("canvas_draw", {
      kind: "sticky",
      text: "Far",
      x: 3000,
      y: 3000,
    }),
  );
  const id = created.id as string;
  await mcp.call("canvas_focus", { id });

  await page.waitForFunction(
    () => {
      const doc = JSON.parse(window.__rsEditor!.save_json());
      const label = Object.values<Record<string, any>>(doc.elements).find(
        (e) => e.text === "Far" && !e.isDeleted,
      );
      const rect = label?.container_id
        ? doc.elements[label.container_id]?.rect
        : undefined;
      if (!rect) return false;
      const cx = rect.x + rect.w / 2;
      const cy = rect.y + rect.h / 2;
      const editor = window.__rsEditor!;
      const [sx, sy] = Array.from(editor.scroll());
      const zoom = editor.zoom();
      const canvas = document.getElementById("scene") as HTMLCanvasElement;
      const wx = sx + canvas.clientWidth / 2 / zoom;
      const wy = sy + canvas.clientHeight / 2 / zoom;
      return Math.hypot(wx - cx, wy - cy) < 2 / zoom;
    },
    undefined,
    { timeout: 3_000 },
  );

  const zoom = await page.evaluate(() => window.__rsEditor!.zoom());
  expect(zoom).toBeGreaterThan(1);
  expect(zoom).toBeLessThanOrEqual(2.01);
});

test("focus leaves an already-visible target alone", async ({
  page,
  request,
}) => {
  await page.goto(`${BASE}/`);
  await page.waitForFunction(() => window.__rsReady === true);
  await page.waitForFunction(() => window.__rsAgentConnected === true);
  const mcp = mcpClient(request, BASE);
  const created = toolJson(
    await mcp.call("canvas_draw", {
      kind: "sticky",
      text: "Near",
      x: 100,
      y: 100,
    }),
  );
  const before = await camera(page);
  await mcp.call("canvas_focus", { id: created.id as string });
  await page.waitForTimeout(600);
  expect(await camera(page)).toEqual(before);
});

test("the agent panel can switch camera moves off", async ({
  page,
  request,
}) => {
  await page.goto(`${BASE}/`);
  await page.waitForFunction(() => window.__rsReady === true);
  await page.waitForFunction(() => window.__rsAgentConnected === true);
  const mcp = mcpClient(request, BASE);
  const created = toolJson(
    await mcp.call("canvas_draw", {
      kind: "sticky",
      text: "Far",
      x: 3000,
      y: 3000,
    }),
  );

  await page.getByTitle("Agent settings").click();
  const panel = page.getByRole("dialog", { name: "Agent settings" });
  await panel.getByRole("button", { name: "Off" }).click();
  await expect
    .poll(
      async () =>
        JSON.parse(await mcp.text("canvas_status", {})).settings.camera,
      { timeout: 3_000 },
    )
    .toBe("off");

  const before = await camera(page);
  const result = toolJson(
    await mcp.call("canvas_focus", { id: created.id as string }),
  );
  expect(result.moved).toBe(false);
  await page.waitForTimeout(500);
  expect(await camera(page)).toEqual(before);

  await panel.getByRole("button", { name: "Gentle" }).click();
  await expect
    .poll(
      async () =>
        JSON.parse(await mcp.text("canvas_status", {})).settings.camera,
      { timeout: 3_000 },
    )
    .toBe("gentle");
});

test("canvas_point highlights without moving the camera", async ({
  page,
  request,
}) => {
  await seed(page);
  const target = await pickTarget(page);
  const before = await camera(page);
  const mcp = mcpClient(request, BASE);
  await mcp.call("canvas_point", { id: target.id });

  await page.waitForFunction(
    () => {
      const canvas = document.getElementById("overlay") as HTMLCanvasElement;
      const ctx = canvas.getContext("2d")!;
      const data = ctx.getImageData(0, 0, canvas.width, canvas.height).data;
      for (let i = 3; i < data.length; i += 4) {
        if (data[i] > 8) return true;
      }
      return false;
    },
    undefined,
    { timeout: 1_000 },
  );

  const after = await camera(page);
  expect(after).toEqual(before);
});

test("user wheel input cancels the agent's glide", async ({ page, request }) => {
  await seed(page);
  const mcp = mcpClient(request, BASE);
  await mcp.call("canvas_zoom", { mode: "fit" });
  await page.waitForTimeout(500);

  await mcp.call("canvas_zoom", { mode: "in" });
  await page.waitForTimeout(60);
  await page.mouse.move(640, 400);
  await page.mouse.wheel(0, 1);
  const cancelled = await camera(page);

  await page.waitForTimeout(600);
  expect(await camera(page)).toEqual(cancelled);
});

const presentationState = (page: import("@playwright/test").Page) =>
  page.evaluate(() => {
    const raw = window.__rsEditor!.presentation_state();
    return raw ? JSON.parse(raw) : null;
  });

test("a captioned script plays beat by beat and finishes", async ({
  page,
  request,
}) => {
  await page.goto(`${BASE}/`);
  await page.waitForFunction(() => window.__rsReady === true);
  await page.waitForFunction(() => window.__rsAgentConnected === true);
  const mcp = mcpClient(request, BASE);
  const call = mcp.call("canvas_script", {
    beats: [
      {
        draw: { as: "one", kind: "sticky", text: "One", x: 3000, y: 3000 },
        caption: "First idea",
        hold: 1_200,
      },
      {
        draw: { as: "two", kind: "sticky", text: "Two", near: "one" },
        focus: "two",
        caption: "Second idea",
        hold: 1_200,
      },
    ],
  });

  // Mid-play: the first beat is on screen, the second still hidden.
  await page.waitForFunction(
    () => {
      const raw = window.__rsEditor!.presentation_state();
      return !!raw && JSON.parse(raw).hidden === 2;
    },
    undefined,
    { timeout: 3_000 },
  );
  const first = await presentationState(page);
  expect(first?.caption).toBe("First idea");
  expect(first?.playing).toBe(true);

  // The caption bar paints on the overlay.
  await page.waitForFunction(
    () => {
      const canvas = document.getElementById("overlay") as HTMLCanvasElement;
      const ctx = canvas.getContext("2d")!;
      const data = ctx.getImageData(0, 0, canvas.width, canvas.height).data;
      for (let i = 3; i < data.length; i += 4) {
        if (data[i] > 8) return true;
      }
      return false;
    },
    undefined,
    { timeout: 1_000 },
  );

  // The second beat reveals while playback continues.
  await page.waitForFunction(
    () => {
      const raw = window.__rsEditor!.presentation_state();
      return !!raw && JSON.parse(raw).hidden === 0;
    },
    undefined,
    { timeout: 3_000 },
  );
  const second = await presentationState(page);
  expect(second?.caption).toBe("Second idea");

  const result = toolJson(await call);
  expect(result.played).toBe(true);
  expect(result.cancelled).toBe(false);
  expect(await presentationState(page)).toBeNull();
  expect(await page.evaluate(() => window.__rsEditor!.element_count())).toBe(4);
  // The focus beat glided the camera onto the second sticky, gently.
  const zoom = await page.evaluate(() => window.__rsEditor!.zoom());
  expect(zoom).toBeGreaterThan(1);
  expect(zoom).toBeLessThanOrEqual(2.01);
});

test("user input cancels playback and the tool reports it", async ({
  page,
  request,
}) => {
  await page.goto(`${BASE}/`);
  await page.waitForFunction(() => window.__rsReady === true);
  await page.waitForFunction(() => window.__rsAgentConnected === true);
  const mcp = mcpClient(request, BASE);
  const call = mcp.call("canvas_script", {
    beats: [
      {
        draw: { as: "a", kind: "sticky", text: "A" },
        caption: "Long beat",
        hold: 3_000,
      },
      {
        draw: { as: "b", kind: "sticky", text: "B", near: "a" },
        caption: "Longer beat",
        hold: 3_000,
      },
    ],
  });

  await page.waitForFunction(
    () => window.__rsEditor!.presentation_state() !== "",
    undefined,
    { timeout: 3_000 },
  );
  await page.mouse.click(400, 300);

  const result = toolJson(await call);
  expect(result.cancelled).toBe(true);
  expect(await presentationState(page)).toBeNull();
  // The snap to final leaves every element visible.
  expect(await page.evaluate(() => window.__rsEditor!.element_count())).toBe(4);
});

test("reduced motion skips playback entirely", async ({ page, request }) => {
  await page.emulateMedia({ reducedMotion: "reduce" });
  await page.goto(`${BASE}/`);
  await page.waitForFunction(() => window.__rsReady === true);
  await page.waitForFunction(() => window.__rsAgentConnected === true);
  const mcp = mcpClient(request, BASE);

  const started = Date.now();
  const result = toolJson(
    await mcp.call("canvas_script", {
      beats: [
        {
          draw: { as: "a", kind: "sticky", text: "A" },
          caption: "Should not pace",
          hold: 3_000,
        },
      ],
    }),
  );
  expect(Date.now() - started).toBeLessThan(1_500);
  expect(result.played).toBe(true);
  expect(result.cancelled).toBe(false);
  expect(await presentationState(page)).toBeNull();
  expect(await page.evaluate(() => window.__rsEditor!.element_count())).toBe(2);
});
