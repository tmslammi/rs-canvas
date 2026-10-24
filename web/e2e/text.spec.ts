import { expect, test } from "@playwright/test";

const count = (page: import("@playwright/test").Page): Promise<number> =>
  page.evaluate(() => window.__rsEditor!.element_count());

const textEditor = (page: import("@playwright/test").Page) =>
  page.locator("#text-editor");

test("type point text and commit with Escape", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await page.keyboard.press("t");
  await page.mouse.click(300, 300);
  await expect(textEditor(page)).toBeVisible();
  await page.keyboard.type("Hello world");
  await page.keyboard.press("Escape");

  await expect(textEditor(page)).toBeHidden();
  expect(await count(page)).toBe(1);

  const hasTextInk = await page.evaluate(() => {
    const canvas = document.getElementById("scene") as HTMLCanvasElement;
    const ctx = canvas.getContext("2d")!;
    const data = ctx.getImageData(250, 280, 300, 60).data;
    for (let i = 0; i < data.length; i += 4) {
      if (data[i] > 180 && data[i + 1] > 180 && data[i + 2] > 170) return true;
    }
    return false;
  });
  expect(hasTextInk).toBe(true);

  await page.keyboard.press("ControlOrMeta+z");
  expect(await count(page)).toBe(0);
});

test("large text keeps its size after the stroke is repainted", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await page.getByTitle("Font size 28").click();
  await page.keyboard.press("t");
  await page.mouse.click(300, 300);
  await page.keyboard.type("asd");
  await page.keyboard.press("Escape");

  // Moving the text repaints its layer in a later frame. The renderer used
  // to skip re-applying the font after the context state was restored, so
  // the glyphs came back at the canvas default size.
  const before = await page.evaluate(() => {
    const bounds = window.__rsEditor!.selection_bounds();
    return bounds ? Array.from(bounds) : [300, 300, 46, 36];
  });
  await page.mouse.move(before[0] + before[2] / 2, before[1] + before[3] / 2);
  await page.mouse.down();
  await page.mouse.move(before[0] + before[2] / 2 + 10, before[1] + before[3] / 2 + 10, {
    steps: 3,
  });
  await page.mouse.up();

  const inkHeight = await page.evaluate(() => {
    const canvas = document.getElementById("scene") as HTMLCanvasElement;
    const ctx = canvas.getContext("2d")!;
    const data = ctx.getImageData(290, 285, 120, 80).data;
    let minY = Infinity;
    let maxY = -Infinity;
    for (let y = 0; y < 80; y++) {
      for (let x = 0; x < 120; x++) {
        const i = (y * 120 + x) * 4;
        if (data[i] > 100 && data[i + 1] > 100) {
          minY = Math.min(minY, y);
          maxY = Math.max(maxY, y);
        }
      }
    }
    return maxY - minY;
  });
  expect(inkHeight, "28px text stays 28px after a repaint").toBeGreaterThan(15);
});

test("double-click a shape to add bound text that moves with it", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await page.keyboard.press("r");
  await page.mouse.move(200, 200);
  await page.mouse.down();
  await page.mouse.move(400, 300, { steps: 5 });
  await page.mouse.up();

  await page.keyboard.press("v");
  await page.mouse.dblclick(300, 250);
  await expect(textEditor(page)).toBeVisible();
  await page.keyboard.type("node");
  await page.keyboard.press("Escape");
  expect(await count(page)).toBe(2);

  // Select and move the shape by dragging its edge, then select the text.
  await page.mouse.move(210, 210);
  await page.mouse.down();
  await page.mouse.move(270, 210, { steps: 5 });
  await page.mouse.up();

  await page.mouse.click(330, 237);
  const selectionAfter = await page.evaluate(() => {
    const bounds = window.__rsEditor!.selection_bounds();
    return bounds ? Array.from(bounds) : undefined;
  });
  expect(selectionAfter).toBeDefined();
  // Clicking the bound text selects its shape, which moved +60.
  expect(selectionAfter![0]).toBeCloseTo(260, -1);
});
