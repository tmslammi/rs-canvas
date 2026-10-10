import { expect, test } from "@playwright/test";

const count = (page: import("@playwright/test").Page): Promise<number> =>
  page.evaluate(() => window.__rsEditor!.element_count());

async function drawRect(page: import("@playwright/test").Page): Promise<void> {
  await page.keyboard.press("r");
  await page.mouse.move(200, 200);
  await page.mouse.down();
  await page.mouse.move(400, 300, { steps: 5 });
  await page.mouse.up();
}

test("autosave survives a reload", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());
  await drawRect(page);
  expect(await count(page)).toBe(1);

  await page.waitForTimeout(1000);
  await page.reload();
  await page.waitForFunction(() => window.__rsReady === true);
  expect(await count(page)).toBe(1);
});

test("named workspaces have separate autosaves and unload checkpoints", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await drawRect(page);
  await page.waitForTimeout(1000);

  await page.goto("/?ws=separate");
  await page.waitForFunction(() => window.__rsReady === true);
  expect(await count(page), "named workspace does not inherit the default autosave").toBe(0);
  await drawRect(page);
  await page.keyboard.press("r");
  await page.mouse.move(500, 400);
  await page.mouse.down();
  await page.mouse.move(650, 500, { steps: 3 });
  await page.mouse.up();
  // Navigate before the debounce expires: the unload checkpoint must also
  // stay scoped to its workspace.
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  expect(await count(page), "named checkpoint does not replace the default drawing").toBe(1);
  await page.goto("/?ws=separate");
  await page.waitForFunction(() => window.__rsReady === true);
  expect(await count(page), "named workspace restores its own drawing").toBe(2);
});

test("save and open round-trip through JSON", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());
  await drawRect(page);

  const json = await page.evaluate(() => window.__rsEditor!.save_json());
  const parsed = JSON.parse(json) as { schemaVersion: number; elements: object };
  expect(parsed.schemaVersion).toBe(1);
  expect(Object.keys(parsed.elements).length).toBe(1);

  await page.evaluate(() => window.__rsEditor!.new_document());
  expect(await count(page)).toBe(0);

  await page.evaluate((saved) => window.__rsEditor!.load_json(saved), json);
  expect(await count(page)).toBe(1);
});

test("a broken document is rejected without losing the open one", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());
  await drawRect(page);
  expect(await count(page)).toBe(1);

  const message = await page.evaluate(() => {
    try {
      window.__rsEditor!.load_json("{ definitely not a document");
      return "";
    } catch (error) {
      return String(error);
    }
  });
  expect(message).not.toBe("");
  expect(await count(page), "the open document survives a failed load").toBe(1);
});

test("invalid imported layer indices are rejected without breaking later drawing", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await drawRect(page);
  const saved = await page.evaluate(() => window.__rsEditor!.save_json());
  for (const index of ["", "a", "é0", "a!"]) {
    const message = await page.evaluate(({ json, index }) => {
      const doc = JSON.parse(json);
      Object.values<any>(doc.elements)[0].index = index;
      try {
        window.__rsEditor!.load_json(JSON.stringify(doc));
        return "";
      } catch (error) {
        return String(error);
      }
    }, { json: saved, index });
    expect(message, `reject malformed layer index ${JSON.stringify(index)}`).not.toBe("");
    expect(await count(page)).toBe(1);
  }
  await drawRect(page);
  expect(await count(page)).toBe(2);
});
