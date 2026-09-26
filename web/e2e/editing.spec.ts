import { expect, test } from "@playwright/test";

const selectionBounds = (
  page: import("@playwright/test").Page,
): Promise<number[] | undefined> =>
  page.evaluate(() => {
    const bounds = window.__rsEditor!.selection_bounds();
    return bounds ? Array.from(bounds) : undefined;
  });

const count = (page: import("@playwright/test").Page): Promise<number> =>
  page.evaluate(() => window.__rsEditor!.element_count());

test("resize by dragging the SE handle, then undo", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await page.keyboard.press("r");
  await page.mouse.move(100, 100);
  await page.mouse.down();
  await page.mouse.move(300, 200, { steps: 5 });
  await page.mouse.up();

  await page.keyboard.press("v");
  await page.mouse.move(300, 200);
  await page.mouse.down();
  await page.mouse.move(400, 250, { steps: 5 });
  await page.mouse.up();

  const resized = await selectionBounds(page);
  expect(resized![2]).toBeCloseTo(300, 0);
  expect(resized![3]).toBeCloseTo(150, 0);

  await page.keyboard.press("ControlOrMeta+z");
  await page.mouse.click(200, 150);
  const restored = await selectionBounds(page);
  expect(restored![2]).toBeCloseTo(200, 0);
  expect(restored![3]).toBeCloseTo(100, 0);
});

test("duplicate, nudge and eraser work from the keyboard", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await page.keyboard.press("r");
  await page.mouse.move(100, 100);
  await page.mouse.down();
  await page.mouse.move(250, 200, { steps: 4 });
  await page.mouse.up();
  expect(await count(page)).toBe(1);

  await page.keyboard.press("ControlOrMeta+d");
  expect(await count(page)).toBe(2);

  const before = await selectionBounds(page);
  await page.keyboard.press("ArrowRight");
  const after = await selectionBounds(page);
  expect(after![0] - before![0]).toBeCloseTo(1, 0);

  await page.keyboard.press("e");
  await page.mouse.move(50, 50);
  await page.mouse.down();
  await page.mouse.move(400, 300, { steps: 6 });
  await page.mouse.up();
  expect(await count(page)).toBe(0);

  await page.keyboard.press("ControlOrMeta+z");
  expect(await count(page)).toBe(2);
});
