import { expect, test, type Page } from "@playwright/test";
import { mcpClient, toolJson } from "./mcp-client";
import { testBase } from "./server-address";

// Binary wire: snapshots and patches travel as WebSocket binary frames between
// the page and the server; only control frames stay JSON. Runs against its own
// server so element counts are exact.
test.describe.configure({ mode: "serial" });

const BASE = testBase(5);

type FrameRecord = {
  kind: string;
  size: number;
  magic: string;
  head: string;
  at: number;
};

type WireProbe = {
  sent: FrameRecord[];
  received: FrameRecord[];
  docParses: string[];
  docStringifies: string[];
  reset(): void;
};

declare global {
  interface Window {
    __wireProbe?: WireProbe;
  }
}

// Installed before the app boots. It records every WebSocket frame and flags
// any JSON parse/stringify that sees a document-shaped payload, so the hot
// path can prove that no document crosses as JSON.
const PROBE = String.raw`(() => {
  const probe = (window.__wireProbe = {
    sent: [], received: [], docParses: [], docStringifies: [],
    reset() {
      this.sent.length = 0;
      this.received.length = 0;
      this.docParses.length = 0;
      this.docStringifies.length = 0;
    },
  });
  const classify = (data) => {
    if (typeof data === "string") {
      return { kind: "text", size: data.length, magic: "", head: data.slice(0, 4096) };
    }
    let bytes = null;
    if (data instanceof ArrayBuffer) bytes = new Uint8Array(data);
    else if (ArrayBuffer.isView(data)) bytes = new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    if (bytes) {
      return {
        kind: data instanceof ArrayBuffer ? "arraybuffer" : "typedarray",
        size: bytes.byteLength,
        magic: String.fromCharCode(...bytes.subarray(0, 8)),
        head: "",
      };
    }
    return { kind: typeof data, size: 0, magic: "", head: "" };
  };
  const record = (list, data) => {
    const frame = classify(data);
    frame.at = performance.now();
    list.push(frame);
  };
  const send = WebSocket.prototype.send;
  WebSocket.prototype.send = function (data) {
    record(probe.sent, data);
    return send.call(this, data);
  };
  const addEventListener = WebSocket.prototype.addEventListener;
  WebSocket.prototype.addEventListener = function (type, listener, options) {
    if (type === "message" && typeof listener === "function") {
      const wrapped = function (event) {
        record(probe.received, event.data);
        return listener.call(this, event);
      };
      return addEventListener.call(this, type, wrapped, options);
    }
    return addEventListener.call(this, type, listener, options);
  };
  const parse = JSON.parse;
  JSON.parse = function (text, reviver) {
    if (typeof text === "string" &&
        (text.includes('"type":"snapshot"') || text.includes('"type":"patch"') || text.includes('"schemaVersion"'))) {
      probe.docParses.push(text.slice(0, 160));
    }
    return parse.call(this, text, reviver);
  };
  const stringify = JSON.stringify;
  JSON.stringify = function (value, replacer, space) {
    const out = stringify.call(this, value, replacer, space);
    if (typeof out === "string" &&
        (out.includes('"type":"snapshot"') || out.includes('"type":"patch"'))) {
      probe.docStringifies.push(out.slice(0, 160));
    }
    return out;
  };
})();`;

const isDocText = (frame: FrameRecord): boolean =>
  frame.kind === "text" &&
  (frame.head.startsWith('{"type":"snapshot"') ||
    frame.head.startsWith('{"type":"patch"'));

const isWireBinary = (frame: FrameRecord): boolean =>
  frame.magic === "RSCWIRE\n";

const normalize = (json: string): string => {
  const doc = JSON.parse(json) as {
    elements: Record<string, Record<string, unknown>>;
  };
  for (const element of Object.values(doc.elements)) {
    delete element.version;
    delete element.versionNonce;
    delete element.updated;
  }
  return JSON.stringify(doc);
};

const snapshot = (page: Page): Promise<string> =>
  page
    .evaluate(() => window.__rsEditor!.save_json())
    .then((json) => normalize(json));

async function openPage(page: Page, query: string): Promise<void> {
  await page.addInitScript(PROBE);
  await page.goto(`${BASE}/${query}`);
  await page.waitForFunction(
    () => window.__rsReady === true && window.__rsAgentConnected === true,
  );
}

async function waitForHello(page: Page): Promise<string> {
  await expect
    .poll(() =>
      page.evaluate(
        () =>
          window.__wireProbe!.received.find((frame) =>
            frame.head.includes('"type":"hello"'),
          )?.head ?? "",
      ),
    )
    .toContain('"type":"hello"');
  return page.evaluate(
    () =>
      window.__wireProbe!.received.find((frame) =>
        frame.head.includes('"type":"hello"'),
      )!.head,
  );
}

const probe = (page: Page) =>
  page.evaluate(() => ({
    sent: window.__wireProbe!.sent,
    received: window.__wireProbe!.received,
    docParses: window.__wireProbe!.docParses.length,
    docStringifies: window.__wireProbe!.docStringifies.length,
  }));

const resetProbe = (page: Page) =>
  page.evaluate(() => window.__wireProbe!.reset());

async function drawRectangle(page: Page): Promise<void> {
  await page.keyboard.press("r");
  await page.mouse.move(420, 320);
  await page.mouse.down();
  await page.mouse.move(580, 400, { steps: 5 });
  await page.mouse.up();
}

test("two pages and an agent converge over binary frames", async ({
  page,
  context,
  request,
}) => {
  const workspace = "wire-sync";
  const first = page;
  await openPage(first, `?ws=${workspace}`);
  const hello = await waitForHello(first);
  expect(hello).toContain('"sync":true');

  const second = await context.newPage();
  await openPage(second, `?ws=${workspace}`);
  await waitForHello(second);
  await expect
    .poll(() =>
      first.evaluate(() =>
        window.__wireProbe!.sent.some((frame) => frame.magic === "RSCWIRE\n"),
      ),
    )
    .toBe(true);

  await resetProbe(first);
  await resetProbe(second);

  await drawRectangle(first);
  await expect
    .poll(() => second.evaluate(() => window.__rsEditor!.element_count()))
    .toBe(1);

  const mcp = mcpClient(request, BASE);
  const before = await first.evaluate(() => window.__rsEditor!.element_count());
  await mcp.call("canvas_draw", { kind: "sticky", text: "Binary" });
  await expect
    .poll(() => first.evaluate(() => window.__rsEditor!.element_count()))
    .toBe(before + 2);
  await expect
    .poll(() => second.evaluate(() => window.__rsEditor!.element_count()))
    .toBe(before + 2);

  await drawRectangle(second);
  await expect
    .poll(() => first.evaluate(() => window.__rsEditor!.element_count()))
    .toBe(before + 3);
  await expect
    .poll(async () => (await snapshot(first)) === (await snapshot(second)))
    .toBe(true);

  for (const page of [first, second]) {
    const captured = await probe(page);
    expect(captured.docParses, "document frames parsed as JSON").toBe(0);
    expect(captured.docStringifies, "document frames stringified as JSON").toBe(0);
    expect(captured.sent.filter(isDocText), "text document frame sent").toEqual([]);
    expect(
      captured.sent.filter(isWireBinary).length,
      "binary document frame sent",
    ).toBeGreaterThan(0);
    expect(
      captured.received.filter(isDocText),
      "text document frame received",
    ).toEqual([]);
    expect(
      captured.received.filter(
        (frame) => frame.kind === "arraybuffer" && isWireBinary(frame),
      ).length,
      "binary document frame received",
    ).toBeGreaterThan(0);
  }

  await second.close();
});

test("a 10k snapshot and patch stay binary end to end", async ({
  page,
  browser,
  request,
}) => {
  test.setTimeout(180_000);
  const workspace = "wire-scale";
  await openPage(page, `?ws=${workspace}`);
  await waitForHello(page);

  // Save 10k locally, then reload: boot restores the autosave and the hello
  // handshake uploads it as one binary snapshot.
  await page.evaluate(() => {
    window.__rsEditor!.load_benchmark_fixture(10000);
    window.__rsEditor!.flush_save();
  });
  await page.waitForTimeout(1_500);
  const reloadStart = Date.now();
  await page.reload();
  await page.waitForFunction(
    () => window.__rsReady === true && window.__rsAgentConnected === true,
  );
  await expect
    .poll(() =>
      page.evaluate(
        () =>
          window.__wireProbe!.sent.filter(
            (frame) => frame.magic === "RSCWIRE\n" && frame.size > 100_000,
          ).length,
      ),
    )
    .toBeGreaterThan(0);
  const reloadMs = Date.now() - reloadStart;

  const upload = await page.evaluate(() => {
    const frame = window.__wireProbe!.sent.find(
      (candidate) => candidate.magic === "RSCWIRE\n" && candidate.size > 100_000,
    )!;
    return { size: frame.size, at: frame.at, count: window.__rsEditor!.element_count() };
  });
  expect(upload.count).toBe(10000);

  // The server absorbed the 10k upload.
  const mcp = mcpClient(request, BASE);
  await expect
    .poll(
      async () =>
        (
          toolJson(await mcp.call("canvas_status", {})).document as {
            elements: number;
          }
        ).elements,
      { timeout: 30_000 },
    )
    .toBe(10000);

  // A fresh page (empty storage) downloads the mirror as one binary snapshot.
  const freshContext = await browser.newContext();
  await freshContext.addInitScript(PROBE);
  const fresh = await freshContext.newPage();
  const downloadStart = Date.now();
  await fresh.goto(`${BASE}/?ws=${workspace}`);
  await fresh.waitForFunction(
    () => window.__rsReady === true && window.__rsAgentConnected === true,
  );
  await expect
    .poll(
      () => fresh.evaluate(() => window.__rsEditor!.element_count()),
      { timeout: 30_000 },
    )
    .toBe(10000);
  const downloadMs = Date.now() - downloadStart;
  const download = await fresh.evaluate(() => {
    const frame = window.__wireProbe!.received.find(
      (candidate) => candidate.magic === "RSCWIRE\n" && candidate.size > 100_000,
    );
    return frame ? { size: frame.size, at: frame.at, kind: frame.kind } : null;
  });
  expect(download, "binary 10k snapshot received").not.toBeNull();
  expect(download!.kind).toBe("arraybuffer");

  // A patch on a 10k board is still one small binary frame each way.
  await resetProbe(page);
  await resetProbe(fresh);
  const patchStart = Date.now();
  const before = await page.evaluate(() => window.__rsEditor!.element_count());
  await mcp.call("canvas_draw", { kind: "sticky", text: "Patched" });
  await expect
    .poll(() => page.evaluate(() => window.__rsEditor!.element_count()))
    .toBe(before + 2);
  await expect
    .poll(() => fresh.evaluate(() => window.__rsEditor!.element_count()))
    .toBe(before + 2);
  const patchMs = Date.now() - patchStart;

  for (const [name, target] of [
    ["uploader", page],
    ["downloader", fresh],
  ] as const) {
    const captured = await probe(target);
    expect(captured.docParses, `${name}: document JSON parse`).toBe(0);
    expect(captured.docStringifies, `${name}: document JSON stringify`).toBe(0);
    expect(
      captured.sent.filter(isDocText),
      `${name}: text document frame sent`,
    ).toEqual([]);
    expect(
      captured.received.filter(isDocText),
      `${name}: text document frame received`,
    ).toEqual([]);
    expect(
      captured.received.filter(
        (frame) => frame.kind === "arraybuffer" && isWireBinary(frame),
      ).length,
      `${name}: binary document frame received`,
    ).toBeGreaterThan(0);
  }
  const patchSizes = await probe(fresh);
  const patchFrame = patchSizes.received.find(
    (frame) => frame.kind === "arraybuffer" && isWireBinary(frame),
  )!;
  console.log(
    `[binary-wire] 10k upload ${upload.size}B sent ${upload.at.toFixed(0)}ms after boot (reload ${reloadMs}ms); ` +
      `download ${download!.size}B, visible in ${downloadMs}ms (frame ${download!.at.toFixed(0)}ms after boot); ` +
      `agent patch ${patchFrame.size}B to two pages in ${patchMs}ms`,
  );

  await freshContext.close();
});
