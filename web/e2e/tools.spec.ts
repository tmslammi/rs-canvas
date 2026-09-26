import { expect, test } from "@playwright/test";

const count = (page: import("@playwright/test").Page): Promise<number> =>
  page.evaluate(() => window.__rsEditor!.element_count());

test("freehand drawing captures a stroke and undo removes it", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await page.keyboard.press("p");
  await page.mouse.move(200, 300);
  await page.mouse.down();
  for (let i = 0; i <= 20; i++) {
    await page.mouse.move(200 + i * 10, 300 + Math.sin(i / 3) * 40);
  }
  await page.mouse.up();
  expect(await count(page)).toBe(1);

  await page.keyboard.press("ControlOrMeta+z");
  expect(await count(page)).toBe(0);
});

test("escape cancels an in-progress freehand stroke", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await page.keyboard.press("p");
  await page.mouse.move(200, 300);
  await page.mouse.down();
  await page.mouse.move(300, 340, { steps: 5 });
  await page.keyboard.press("Escape");
  await page.mouse.up();
  expect(await count(page)).toBe(0);
});

test("line tool draws a segment between drag endpoints", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await page.keyboard.press("l");
  await page.mouse.move(100, 100);
  await page.mouse.down();
  await page.mouse.move(400, 250, { steps: 6 });
  await page.mouse.up();
  expect(await count(page)).toBe(1);

  const bounds = await page.evaluate(() => {
    const b = window.__rsEditor!.selection_bounds();
    return b ? Array.from(b) : undefined;
  });
  expect(bounds![0]).toBeCloseTo(100, 0);
  expect(bounds![2]).toBeCloseTo(300, 0);
  expect(bounds![3]).toBeCloseTo(150, 0);
});

test("toolbar buttons switch tools", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);

  await page.getByTitle("Diamond — D").click();
  expect(await page.evaluate(() => window.__rsEditor!.active_tool())).toBe("diamond");
  await page.getByTitle("Draw — P").click();
  expect(await page.evaluate(() => window.__rsEditor!.active_tool())).toBe("freehand");
  await page.getByTitle("Select — V").click();
  expect(await page.evaluate(() => window.__rsEditor!.active_tool())).toBe("select");
});
