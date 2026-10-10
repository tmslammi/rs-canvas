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
  expect(await page.evaluate(() => window.__rsEditor!.can_undo())).toBe(false);
});

test("pointer cancellation rolls back a move without adding an undo step", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.keyboard.press("r");
  await page.mouse.move(300, 300);
  await page.mouse.down();
  await page.mouse.move(450, 400, { steps: 3 });
  await page.mouse.up();
  const saved = await page.evaluate(() => window.__rsEditor!.save_json());
  await page.evaluate((json) => window.__rsEditor!.load_json(json), saved);
  await page.keyboard.press("v");
  await page.mouse.move(375, 350);
  await page.mouse.down();
  await page.mouse.move(550, 500, { steps: 3 });
  await page.evaluate(() => document.getElementById("scene")!.dispatchEvent(
    new PointerEvent("pointercancel", { pointerId: 1, bubbles: true }),
  ));
  await page.mouse.up();
  expect(await page.evaluate(() => window.__rsEditor!.save_json())).toBe(saved);
  expect(await page.evaluate(() => window.__rsEditor!.can_undo())).toBe(false);
});

test("pointer cancellation discards the incomplete shape", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.keyboard.press("r");
  await page.mouse.move(300, 300);
  await page.mouse.down();
  await page.mouse.move(450, 400, { steps: 3 });
  await page.evaluate(() => document.getElementById("scene")!.dispatchEvent(
    new PointerEvent("pointercancel", { pointerId: 1, bubbles: true }),
  ));
  await page.mouse.up();
  expect(await count(page)).toBe(0);
  expect(await page.evaluate(() => window.__rsEditor!.can_undo())).toBe(false);
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
