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
