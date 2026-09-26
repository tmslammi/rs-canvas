import { expect, test } from "@playwright/test";

async function drawRect(page: import("@playwright/test").Page): Promise<void> {
  await page.keyboard.press("r");
  await page.mouse.move(200, 200);
  await page.mouse.down();
  await page.mouse.move(400, 300, { steps: 5 });
  await page.mouse.up();
}

test("SVG export carries geometry, styles and text", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());
  await drawRect(page);

  await page.keyboard.press("t");
  await page.mouse.click(500, 400);
  await page.keyboard.type("hello");
  await page.keyboard.press("Escape");

  const svg = await page.evaluate(() => window.__rsEditor!.export_svg());
  expect(svg.startsWith("<svg")).toBe(true);
  expect(svg).toContain("viewBox");
  expect(svg).toContain("<path");
  expect(svg).toContain("fill=\"#ece4d6\"");
  expect(svg).toContain("font-family=\"ComicShanns");
  expect(svg).toContain(">hello</text>");
});

test("PNG export downloads a 2x image", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());
  await drawRect(page);

  const downloadPromise = page.waitForEvent("download");
  await page.getByTitle("Export PNG").click();
  const download = await downloadPromise;
  expect(download.suggestedFilename()).toMatch(/rs-canvas-.*\.png/);
});

test("SVG export downloads a file", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());
  await drawRect(page);

  const downloadPromise = page.waitForEvent("download");
  await page.getByTitle("Export SVG").click();
  const download = await downloadPromise;
  expect(download.suggestedFilename()).toMatch(/rs-canvas-.*\.svg/);
});

test("Copy PNG puts an image on the clipboard", async ({ page, context }) => {
  await context.grantPermissions(["clipboard-read", "clipboard-write"]);
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());
  await drawRect(page);

  await page.getByTitle("Copy PNG").click();
  await expect(page.locator("#toast")).toContainText("Copied");

  const hasImage = await page.evaluate(async () => {
    const items = await navigator.clipboard.read();
    return items.some((item) => item.types.includes("image/png"));
  });
  expect(hasImage).toBe(true);
});
