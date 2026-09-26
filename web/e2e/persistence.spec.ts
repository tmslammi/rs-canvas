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

test("Save button downloads a JSON file", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());
  await drawRect(page);

  const downloadPromise = page.waitForEvent("download");
  await page.getByTitle("Save (⌘S)").click();
  const download = await downloadPromise;
  expect(download.suggestedFilename()).toContain("rs-canvas");
  expect(download.suggestedFilename()).toContain(".json");
});

test("Open a JSON file from disk, with a toast for invalid files", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());
  await drawRect(page);
  expect(await count(page)).toBe(1);

  await page.setInputFiles("#file-input", {
    name: "drawing.json",
    mimeType: "application/json",
    buffer: Buffer.from(JSON.stringify({ schemaVersion: 1, elements: {} })),
  });
  expect(await count(page)).toBe(0);

  await page.setInputFiles("#file-input", {
    name: "broken.json",
    mimeType: "application/json",
    buffer: Buffer.from("{ definitely not a document"),
  });
  await expect(page.locator("#toast")).toHaveClass(/error/);
});
