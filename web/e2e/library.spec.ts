import { expect, test } from "@playwright/test";

const count = (page: import("@playwright/test").Page): Promise<number> =>
  page.evaluate(() => window.__rsEditor!.element_count());

test("search, insert a glyph with caption, and undo", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());
  await page.getByTitle("Shapes library").click();

  const search = page.locator(".library-search");
  await search.fill("cloud run");
  await page.getByTitle("Insert Cloud Run").click();
  expect(await count(page)).toBe(2);

  const glyphInk = await page.evaluate(() => {
    const canvas = document.getElementById("scene") as HTMLCanvasElement;
    const ctx = canvas.getContext("2d")!;
    const data = ctx.getImageData(560, 340, 160, 130).data;
    let nonBackground = 0;
    for (let i = 0; i < data.length; i += 4) {
      const background = data[i] > 244 && data[i + 1] > 243 && data[i + 2] > 240;
      if (!background) nonBackground++;
    }
    return nonBackground;
  });
  expect(glyphInk).toBeGreaterThan(50);

  await search.fill("queue");
  await page.getByTitle("Insert Queue").click();
  expect(await count(page)).toBe(4);

  await page.keyboard.press("ControlOrMeta+z");
  expect(await count(page)).toBe(2);
  await page.keyboard.press("ControlOrMeta+z");
  expect(await count(page)).toBe(0);
});

test("the catalog covers the curated services and generic shapes", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.getByTitle("Shapes library").click();

  const labels = await page.locator("#library-panel .library-label").allTextContents();
  expect(labels).toContain("Compute");
  expect(labels).toContain("Storage & Data");
  expect(labels).toContain("Networking");
  expect(labels).toContain("Generic");

  await page.locator(".library-search").fill("compute engine");
  await expect(page.getByTitle("Insert Compute Engine")).toBeVisible();
  await expect(page.getByTitle("Insert Cloud Run")).toBeHidden();
});
