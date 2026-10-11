import { spawn, type ChildProcess } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { expect, test, type Page } from "@playwright/test";
import { mcpClient } from "./mcp-client";

// Workspace management gets its own persisted server. Its focus changes and
// destructive operations must not affect the existing agent specs' documents.
test.describe.configure({ mode: "default" });
const BASE = "http://localhost:4177";
let server: ChildProcess;
let directory: string;

test.beforeAll(async () => {
  const occupied = await fetch(BASE).then(() => true, () => false);
  if (occupied) throw new Error("Workspace test port 4177 is already in use");
  directory = fs.mkdtempSync(path.join(os.tmpdir(), "rs-canvas-workspaces-"));
  server = spawn(path.resolve("../target/debug/canvas-server"), [
    "--web", ".", "--port", "4177", "--doc", path.join(directory, "current.json"),
  ], { cwd: process.cwd(), stdio: "ignore" });
  for (let attempt = 0; attempt < 100; attempt++) {
    if (server.exitCode !== null) throw new Error("Workspace test server exited before startup");
    try {
      if ((await fetch(BASE)).ok) return;
    } catch { /* startup */ }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error("Workspace test server did not start");
});

test.afterAll(async () => {
  if (server && server.exitCode === null) {
    const exited = new Promise<void>((resolve) => server.once("exit", () => resolve()));
    server.kill();
    await exited;
  }
  if (directory) fs.rmSync(directory, { recursive: true, force: true });
});

async function ready(page: Page) {
  await page.waitForFunction(() => window.__rsReady === true);
  await expect(page.locator("#workspace-button")).toBeVisible();
}

async function openPicker(page: Page) {
  await page.locator("#workspace-button").click();
  await expect(page.getByRole("dialog", { name: "Workspaces", exact: true })).toBeVisible();
}

async function createWorkspace(page: Page, name: string) {
  await openPicker(page);
  await page.getByLabel("New workspace name", { exact: true }).fill(name);
  await page.getByRole("button", { name: "Create workspace", exact: true }).click();
  await expect(page).toHaveURL(new RegExp(`[?&]ws=${name}(?:&|#|$)`));
  await ready(page);
}

async function switchWorkspace(page: Page, name: string) {
  await openPicker(page);
  await page.getByRole("button", { name: `Switch to workspace ${name}`, exact: true }).click();
  await expect.poll(() => new URL(page.url()).searchParams.get("ws")).toBe(name === "default" ? null : name);
  await ready(page);
}

async function drawRectangle(page: Page) {
  await page.keyboard.press("r");
  await page.mouse.move(300, 200);
  await page.mouse.down();
  await page.mouse.move(450, 300, { steps: 3 });
  await page.mouse.up();
}

const count = (page: Page) => page.evaluate(() => window.__rsEditor!.element_count());
const documentJson = (page: Page) => page.evaluate(() => window.__rsEditor!.save_json());

test("create, switch and reopen preserve separate documents and other URL state", async ({ page }) => {
  await page.goto(`${BASE}/?theme=test#keep`);
  await ready(page);
  await page.getByTitle("New drawing", { exact: true }).click();
  await drawRectangle(page);
  await createWorkspace(page, "ui-create");
  expect(new URL(page.url()).searchParams.get("theme")).toBe("test");
  expect(new URL(page.url()).hash).toBe("#keep");
  await expect(page.locator("#workspace-button")).toContainText("ui-create");
  await expect(page).toHaveTitle(/ui-create/);
  expect(await count(page)).toBe(0);
  await drawRectangle(page);
  await page.keyboard.press("t");
  await page.mouse.click(500, 400);
  await page.locator("#text-editor").fill("pending text survives switching");
  await switchWorkspace(page, "default");
  expect(await count(page)).toBe(1);
  expect(new URL(page.url()).searchParams.get("theme")).toBe("test");
  expect(new URL(page.url()).hash).toBe("#keep");
  await switchWorkspace(page, "ui-create");
  await expect.poll(() => documentJson(page)).toContain("pending text survives switching");
  expect(await count(page)).toBe(2);
  await page.reload();
  await ready(page);
  expect(await count(page)).toBe(2);
});

test("validation and picker keyboard controls do not edit the canvas", async ({ page }) => {
  await page.goto(`${BASE}/?ws=ui-validation`);
  await ready(page);
  await openPicker(page);
  const active = await page.evaluate(() => window.__rsEditor!.active_tool());
  for (const name of ["Bad Name", "-bad", "../other", "é", "a".repeat(41), ""]) {
    await page.getByLabel("New workspace name", { exact: true }).fill(name);
    await page.getByRole("button", { name: "Create workspace", exact: true }).click();
    await expect(page.locator("#workspace-error")).not.toBeEmpty();
    expect(new URL(page.url()).searchParams.get("ws")).toBe("ui-validation");
  }
  await page.getByLabel("New workspace name", { exact: true }).fill("rectangle");
  await page.keyboard.press("r");
  expect(await page.evaluate(() => window.__rsEditor!.active_tool())).toBe(active);
  await page.keyboard.press("Escape");
  await expect(page.getByRole("dialog", { name: "Workspaces", exact: true })).toBeHidden();
  await expect(page.locator("#workspace-button")).toBeFocused();
  expect(await count(page)).toBe(0);
});

test("switching cancels an unfinished drawing before checkpointing", async ({ page }) => {
  await page.goto(`${BASE}/?ws=ui-gesture`);
  await ready(page);
  await page.keyboard.press("r");
  await page.evaluate(() => {
    const scene = document.getElementById("scene")!;
    for (const [type, x, y] of [["pointerdown", 300, 200], ["pointermove", 450, 300]] as const) {
      scene.dispatchEvent(new PointerEvent(type, {
        bubbles: true, pointerId: 17, pointerType: "mouse", button: 0, buttons: 1,
        clientX: x, clientY: y,
      }));
    }
  });
  await createWorkspace(page, "ui-after-gesture");
  await switchWorkspace(page, "ui-gesture");
  expect(await count(page)).toBe(0);
  expect(await page.evaluate(() => window.__rsEditor!.can_undo())).toBe(false);
});

test("legacy browser autosaves remain listed and switchable when the API is unavailable", async ({ page }) => {
  await page.goto(`${BASE}/?ws=ui-legacy`);
  await ready(page);
  await drawRectangle(page);
  await switchWorkspace(page, "default");
  await page.evaluate(() => localStorage.removeItem("rs-canvas-workspaces"));
  await page.route("**/api/workspaces", (route) => route.abort());
  await page.reload();
  await ready(page);
  await openPicker(page);
  await expect(page.getByRole("button", { name: "Switch to workspace ui-legacy", exact: true })).toBeVisible();
  await expect(page.locator(".workspace-status")).toContainText(/unavailable|browser/i);
  await page.getByRole("button", { name: "Switch to workspace ui-legacy", exact: true }).click();
  await ready(page);
  await expect.poll(() => count(page)).toBe(1);
});

test("a checkpoint-only workspace is discoverable despite damaged catalogue and unavailable API", async ({ page }) => {
  await page.goto(`${BASE}/?ws=ui-checkpoint-source`);
  await ready(page);
  await drawRectangle(page);
  const saved = await documentJson(page);
  await switchWorkspace(page, "default");
  await page.evaluate((json) => {
    localStorage.setItem("rs-canvas-unload-checkpoint:workspace:ui-checkpoint-only", json);
    localStorage.setItem("rs-canvas-workspaces", "{broken catalogue");
  }, saved);
  await page.route("**/api/workspaces", (route) => route.abort());
  await page.reload();
  await ready(page);
  await openPicker(page);
  await expect(page.getByRole("button", { name: "Switch to workspace ui-checkpoint-only", exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Switch to workspace ui-checkpoint-only", exact: true }).click();
  await expect.poll(() => new URL(page.url()).searchParams.get("ws")).toBe("ui-checkpoint-only");
  await ready(page);
  expect(await count(page)).toBe(1);
});

test("an invalid direct workspace URL visibly falls back to default and preserves other URL state", async ({ page }) => {
  await page.goto(`${BASE}/?ws=Bad%20Name&theme=keep#anchor`);
  await ready(page);
  expect(new URL(page.url()).searchParams.get("ws")).toBeNull();
  expect(new URL(page.url()).searchParams.get("theme")).toBe("keep");
  expect(new URL(page.url()).hash).toBe("#anchor");
  await expect(page.locator("#workspace-button")).toContainText("default");
  await expect(page.locator("#toast")).toContainText(/Invalid workspace name.*default/);
  await expect(page.locator("#toast")).toBeVisible();
});

test("picker lists server workspaces safely and remains usable on a narrow screen", async ({ page }) => {
  await page.route("**/api/workspaces", (route) => route.fulfill({
    json: { workspaces: [{ name: "ui-server-only", persisted: true }, { name: "<img src=x onerror=alert(1)>" }] },
  }));
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(`${BASE}/?ws=ui-mobile`);
  await ready(page);
  const box = await page.locator("#workspace-button").boundingBox();
  expect(box!.x).toBeGreaterThanOrEqual(0);
  expect(box!.x + box!.width).toBeLessThanOrEqual(390);
  await openPicker(page);
  await expect(page.getByRole("button", { name: "Switch to workspace ui-server-only", exact: true })).toBeVisible();
  expect(await page.locator("#workspace-picker img").count()).toBe(0);
  const picker = await page.locator("#workspace-picker").boundingBox();
  expect(picker!.x).toBeGreaterThanOrEqual(0);
  expect(picker!.x + picker!.width).toBeLessThanOrEqual(390);
  expect(picker!.y).toBeGreaterThanOrEqual(0);
  await page.getByRole("button", { name: "Close workspaces", exact: true }).click();
});

test("tab focus selects the agent workspace without another drawing gesture", async ({ context, request }) => {
  const first = await context.newPage();
  const second = await context.newPage();
  await first.goto(`${BASE}/?ws=ui-focus-first`);
  await ready(first);
  await first.waitForFunction(() => window.__rsAgentConnected === true);
  await second.goto(`${BASE}/?ws=ui-focus-second`);
  await ready(second);
  await second.waitForFunction(() => window.__rsAgentConnected === true);
  const mcp = mcpClient(request, BASE);
  await expect.poll(async () => JSON.parse(await mcp.text("canvas_status", {})).workspace).toBe("ui-focus-second");
  await first.bringToFront();
  await first.evaluate(() => window.dispatchEvent(new Event("focus")));
  await expect.poll(async () => JSON.parse(await mcp.text("canvas_status", {})).workspace).toBe("ui-focus-first");
});

test("duplicate copies the current drawing and rename preserves it under the new name", async ({ page }) => {
  await page.goto(`${BASE}/?ws=ui-management&theme=keep#anchor`);
  await ready(page);
  await drawRectangle(page);
  await openPicker(page);
  await page.getByLabel("New workspace name", { exact: true }).fill("ui-duplicate");
  await page.getByRole("button", { name: "Duplicate workspace", exact: true }).click();
  await expect.poll(() => new URL(page.url()).searchParams.get("ws")).toBe("ui-duplicate");
  await ready(page);
  expect(await count(page)).toBe(1);
  await openPicker(page);
  await page.getByLabel("Rename workspace name", { exact: true }).fill("ui-renamed");
  await page.getByRole("button", { name: "Rename workspace", exact: true }).click();
  await expect.poll(() => new URL(page.url()).searchParams.get("ws")).toBe("ui-renamed");
  await ready(page);
  expect(await count(page)).toBe(1);
  expect(new URL(page.url()).searchParams.get("theme")).toBe("keep");
  expect(new URL(page.url()).hash).toBe("#anchor");
  await expect(page).toHaveTitle(/ui-renamed/);
  await page.reload();
  await ready(page);
  expect(await count(page)).toBe(1);
  await openPicker(page);
  await expect(page.getByRole("button", { name: "Switch to workspace ui-duplicate", exact: true })).toHaveCount(0);
  await page.getByRole("button", { name: "Switch to workspace ui-management", exact: true }).click();
  await ready(page);
  expect(await count(page)).toBe(1);
});

test("rename collisions and default protection cannot replace another workspace", async ({ page }) => {
  await page.goto(`${BASE}/?ws=ui-collision-source`);
  await ready(page);
  await drawRectangle(page);
  await createWorkspace(page, "ui-collision-target");
  await switchWorkspace(page, "ui-collision-source");
  await openPicker(page);
  await page.getByLabel("Rename workspace name", { exact: true }).fill("ui-collision-target");
  await page.getByRole("button", { name: "Rename workspace", exact: true }).click();
  await expect(page.locator("#workspace-error")).not.toBeEmpty();
  expect(new URL(page.url()).searchParams.get("ws")).toBe("ui-collision-source");
  expect(await count(page)).toBe(1);
  await page.getByRole("button", { name: "Switch to workspace default", exact: true }).click();
  await ready(page);
  await openPicker(page);
  await expect(page.getByRole("button", { name: "Rename workspace", exact: true })).toBeDisabled();
  await expect(page.getByRole("button", { name: "Delete workspace", exact: true })).toBeDisabled();
});

test("deletion requires explicit confirmation and removes the saved drawing", async ({ page }) => {
  await page.goto(`${BASE}/?ws=ui-delete&theme=keep#anchor`);
  await ready(page);
  await drawRectangle(page);
  await openPicker(page);
  await page.getByRole("button", { name: "Delete workspace", exact: true }).click();
  await expect(page.getByText(/Delete workspace ui-delete and its saved drawing\?/)).toBeVisible();
  await page.getByLabel("Confirm workspace name", { exact: true }).fill("different");
  await page.getByRole("button", { name: "Delete permanently", exact: true }).click();
  await expect(page.locator("#workspace-error")).toContainText(/confirm|Type/i);
  expect(new URL(page.url()).searchParams.get("ws")).toBe("ui-delete");
  await page.getByRole("button", { name: "Cancel", exact: true }).click();
  expect(await count(page)).toBe(1);
  await page.getByRole("button", { name: "Delete workspace", exact: true }).click();
  await page.getByLabel("Confirm workspace name", { exact: true }).fill("ui-delete");
  await page.getByRole("button", { name: "Delete permanently", exact: true }).click();
  await expect.poll(() => new URL(page.url()).searchParams.get("ws")).toBeNull();
  await ready(page);
  expect(new URL(page.url()).searchParams.get("theme")).toBe("keep");
  expect(new URL(page.url()).hash).toBe("#anchor");
  await openPicker(page);
  await expect(page.getByRole("button", { name: "Switch to workspace ui-delete", exact: true })).toHaveCount(0);
  await page.goto(`${BASE}/?ws=ui-delete`);
  await ready(page);
  expect(await count(page)).toBe(0);
});

test("another open tab prevents rename and deletion without changing either drawing", async ({ context }) => {
  const first = await context.newPage();
  await first.goto(`${BASE}/?ws=ui-shared`);
  await ready(first);
  await drawRectangle(first);
  const second = await context.newPage();
  await second.goto(`${BASE}/?ws=ui-shared`);
  await ready(second);
  await second.waitForFunction(() => window.__rsAgentConnected === true);
  await expect.poll(() => count(second)).toBe(1);
  await openPicker(first);
  await first.getByLabel("Rename workspace name", { exact: true }).fill("ui-shared-new");
  await first.getByRole("button", { name: "Rename workspace", exact: true }).click();
  await expect(first.locator("#workspace-error")).toContainText(/tab|page|open/i);
  expect(new URL(first.url()).searchParams.get("ws")).toBe("ui-shared");
  await first.getByRole("button", { name: "Delete workspace", exact: true }).click();
  await first.getByLabel("Confirm workspace name", { exact: true }).fill("ui-shared");
  await first.getByRole("button", { name: "Delete permanently", exact: true }).click();
  await expect(first.locator("#workspace-error")).toContainText(/tab|page|open/i);
  expect(await count(first)).toBe(1);
  expect(await count(second)).toBe(1);
});

test("offline management remains explicit and preserves local documents", async ({ page }) => {
  await page.route("**/api/workspaces**", (route) => route.abort());
  await page.goto(`${BASE}/?ws=ui-offline`);
  await ready(page);
  await drawRectangle(page);
  await openPicker(page);
  await expect(page.locator(".workspace-status")).toContainText(/unavailable/i);
  await expect(page.getByRole("button", { name: "Rename workspace", exact: true })).toBeDisabled();
  await expect(page.getByRole("button", { name: "Delete workspace", exact: true })).toBeDisabled();
  await page.getByLabel("New workspace name", { exact: true }).fill("ui-offline-new");
  await page.getByRole("button", { name: "Create workspace", exact: true }).click();
  await expect.poll(() => new URL(page.url()).searchParams.get("ws")).toBe("ui-offline-new");
  await ready(page);
  expect(await count(page)).toBe(0);
  await switchWorkspace(page, "ui-offline");
  expect(await count(page)).toBe(1);
});
