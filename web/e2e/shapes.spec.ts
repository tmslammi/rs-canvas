import { expect, test } from "@playwright/test";

const count = (page: import("@playwright/test").Page): Promise<number> =>
  page.evaluate(() => window.__rsEditor!.element_count());

const selectionBounds = (
  page: import("@playwright/test").Page,
): Promise<number[] | undefined> =>
  page.evaluate(() => {
    const bounds = window.__rsEditor!.selection_bounds();
    return bounds ? Array.from(bounds) : undefined;
  });

test("draw, select, move, delete and undo from the UI", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await page.keyboard.press("r");
  await page.mouse.move(300, 200);
  await page.mouse.down();
  await page.mouse.move(500, 350, { steps: 8 });
  await page.mouse.up();
  expect(await count(page)).toBe(1);

  const drawn = await selectionBounds(page);
  expect(drawn).toBeDefined();
  expect(drawn![0]).toBeCloseTo(300, 0);
  expect(drawn![1]).toBeCloseTo(200, 0);
  expect(drawn![2]).toBeCloseTo(200, 0);

  const overlayInk = await page.evaluate(() => {
    const canvas = document.getElementById("overlay") as HTMLCanvasElement;
    const ctx = canvas.getContext("2d")!;
    const data = ctx.getImageData(0, 0, canvas.width, canvas.height).data;
    for (let i = 3; i < data.length; i += 4) {
      if (data[i] > 0) return true;
    }
    return false;
  });
  expect(overlayInk).toBe(true);

  await page.keyboard.press("v");
  await page.mouse.move(400, 275);
  await page.mouse.down();
  await page.mouse.move(500, 325, { steps: 5 });
  await page.mouse.up();
  const moved = await selectionBounds(page);
  expect(moved![0]).toBeCloseTo(400, 0);
  expect(moved![1]).toBeCloseTo(250, 0);

  await page.keyboard.press("Delete");
  expect(await count(page)).toBe(0);

  await page.keyboard.press("ControlOrMeta+z");
  expect(await count(page)).toBe(1);
  await page.mouse.click(450, 300);
  const restored = await selectionBounds(page);
  expect(restored![0]).toBeCloseTo(400, 0);

  await page.keyboard.press("ControlOrMeta+z");
  expect(await count(page)).toBe(1);
  await page.mouse.click(400, 275);
  const unmoved = await selectionBounds(page);
  expect(unmoved![0]).toBeCloseTo(300, 0);

  await page.keyboard.press("ControlOrMeta+z");
  expect(await count(page)).toBe(0);
});

test("marquee selects several shapes and shift-click adds", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await page.keyboard.press("r");
  for (const [x, y] of [
    [100, 100],
    [300, 100],
  ]) {
    await page.mouse.move(x, y);
    await page.mouse.down();
    await page.mouse.move(x + 80, y + 60, { steps: 3 });
    await page.mouse.up();
  }
  expect(await count(page)).toBe(2);

  await page.keyboard.press("v");
  await page.mouse.click(900, 600);

  await page.mouse.move(60, 60);
  await page.mouse.down();
  await page.mouse.move(420, 220, { steps: 5 });
  await page.mouse.up();

  const selected = await page.evaluate(() => {
    const bounds = window.__rsEditor!.selection_bounds();
    return bounds ? Array.from(bounds) : undefined;
  });
  expect(selected).toBeDefined();
  expect(selected![0]).toBeCloseTo(100, 0);
  expect(selected![2]).toBeCloseTo(280, 0);

  await page.keyboard.press("Delete");
  expect(await count(page)).toBe(0);
  await page.keyboard.press("ControlOrMeta+z");
  expect(await count(page)).toBe(2);
});
