import { spawn, type ChildProcess } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import net from "node:net";
import { expect, test } from "@playwright/test";
import { mcpClient, toolJson } from "./mcp-client";

// A private persisted server keeps this discussion separate from the user's work.
test("explore, revise, back out and revisit an engineering discussion", async ({ page, request }, testInfo) => {
  test.setTimeout(60_000);
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "rs-canvas-conversation-"));
  const listener = net.createServer();
  await new Promise<void>((resolve) => listener.listen(0, "127.0.0.1", resolve));
  const port = (listener.address() as net.AddressInfo).port;
  await new Promise<void>((resolve) => listener.close(() => resolve()));
  const base = `http://127.0.0.1:${port}`;
  const binary = path.resolve(process.cwd(), "..", "target", "debug", "canvas-server");
  const start = () => spawn(binary, ["--web", ".", "--port", `${port}`, "--doc", path.join(dir, "current.json")], { cwd: process.cwd(), stdio: "ignore" });
  const ready = async () => {
    await expect.poll(async () => {
      try { return (await request.get(base)).ok(); } catch { return false; }
    }, { timeout: 10_000 }).toBe(true);
  };
  const stop = async (child: ChildProcess) => {
    if (child.exitCode !== null || child.signalCode !== null) return;
    const exited = new Promise<void>((resolve) => child.once("exit", () => resolve()));
    child.kill("SIGTERM");
    await exited;
  };
  let server = start();
  try {
    await ready();
    await page.goto(`${base}/?ws=jobs`);
    await page.waitForFunction(() => window.__rsReady && window.__rsAgentConnected);
    const mcp = mcpClient(request, base);
    const initial = toolJson(await mcp.call("canvas_status", {}));
    const created = toolJson(await mcp.call("canvas_script", {
      context: initial.context, revision: initial.revision,
      beats: [
        { draw: { as: "producer", kind: "rectangle", text: "Producer", x: 40, y: 60 } },
        { draw: { as: "queue", kind: "rectangle", text: "Queue", x: 340, y: 60 } },
        { draw: { as: "worker", kind: "rectangle", text: "Worker", x: 640, y: 60 } },
        { connect: { from: "producer", to: "queue", label: "enqueue" } },
        { connect: { from: "queue", to: "worker", label: "deliver" } },
        { draw: { as: "workers", kind: "frame", name: "Workers", around: ["worker"] } },
      ],
    }));
    const handles = created.created as Record<string, string>;
    const snapshot = () => page.evaluate(() => JSON.parse(window.__rsEditor!.snapshot_json()));
    await expect.poll(async () => Object.keys((await snapshot()).elements).length).toBe(created.elements);
    const detail = await mcp.text("canvas_read", { ids: [handles.worker], detail: true });
    const workerId = JSON.parse(detail.split("\n").find((line) => line.startsWith("{"))!).id;
    const status = toolJson(await mcp.call("canvas_status", {}));
    const experiment = toolJson(await mcp.call("canvas_script", {
      context: status.context, revision: status.revision,
      beats: [{ update: { id: handles.worker, text: "Ack before processing" } }],
    }));
    await expect.poll(async () => JSON.stringify(await snapshot())).toContain("Ack before processing");
    const edit = experiment.edit as Record<string, unknown>;
    await mcp.call("canvas_revert", { transaction: edit.transaction, context: edit.context });
    await expect.poll(async () => JSON.stringify(await snapshot())).not.toContain("Ack before processing");
    const revised = toolJson(await mcp.call("canvas_script", {
      context: edit.context,
      beats: [{ update: { id: handles.worker, text: "Ack after processing", style: { stroke_color: "#69db7c" } } }],
    }));
    await expect.poll(async () => JSON.stringify(await snapshot())).toContain("Ack after processing");
    expect((await snapshot()).elements[workerId]).toBeTruthy();
    const frameRead = await mcp.text("canvas_read", { frame: handles.workers });
    expect(frameRead).toContain("Ack after processing");
    expect(frameRead).not.toContain("Producer");
    const notesStatus = toolJson(await mcp.call("canvas_status", {}));
    const discussion = notesStatus.discussion as { revision: number };
    await mcp.call("canvas_remember", {
      context: notesStatus.context, revision: discussion.revision,
      notes: {
        constraints: ["At-least-once delivery"],
        decisions: ["Ack after processing so a worker crash cannot silently lose a job."],
        questions: ["How should workers deduplicate repeated jobs?"],
      },
    });
    await page.getByRole("button", { name: "◎", exact: true }).click();
    const panel = page.getByRole("dialog", { name: "Agent settings" });
    await expect(panel).toContainText("At-least-once delivery");
    await expect(panel).toContainText("worker crash");
    await expect(panel).toContainText("deduplicate repeated jobs");
    await expect(panel).toContainText("Saved with this workspace");
    await mcp.call("canvas_zoom", { mode: "fit" });
    await mcp.call("canvas_zoom", { mode: "out" });
    await page.evaluate(() => document.fonts.ready);
    await page.waitForTimeout(400); // Let the deliberate camera glide finish for visual QA.
    await page.screenshot({ path: testInfo.outputPath("engineering-discussion.png") });
    await stop(server);
    server = start();
    await ready();
    await page.reload();
    await page.waitForFunction(() => window.__rsReady && window.__rsAgentConnected);
    await page.getByRole("button", { name: "◎", exact: true }).click();
    await expect(panel).toContainText("Ack after processing so a worker crash");
    const resumed = mcpClient(request, base);
    const resumedStatus = toolJson(await resumed.call("canvas_status", {}));
    expect(resumedStatus.context).not.toBe((revised.edit as Record<string, unknown>).context);
    expect(await resumed.text("canvas_read", { query: "Ack after" })).toContain("Ack after processing");
  } finally {
    await page.close();
    await stop(server);
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
