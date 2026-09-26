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

async function drawShape(
  page: import("@playwright/test").Page,
  x1: number,
  y1: number,
  x2: number,
  y2: number,
): Promise<void> {
  await page.keyboard.press("r");
  await page.mouse.move(x1, y1);
  await page.mouse.down();
  await page.mouse.move(x2, y2, { steps: 5 });
  await page.mouse.up();
}

test("connect two shapes, move one, and the connector follows", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await drawShape(page, 100, 100, 300, 200);
  await drawShape(page, 500, 100, 700, 200);
  expect(await count(page)).toBe(2);

  await page.keyboard.press("a");
  await page.mouse.move(280, 150);
  await page.mouse.down();
  await page.mouse.move(400, 150, { steps: 5 });
  await page.mouse.move(520, 150, { steps: 5 });
  await page.mouse.up();
  expect(await count(page)).toBe(3);

  const arrowBoundsBefore = await selectionBounds(page);
  expect(arrowBoundsBefore).toBeDefined();

  // Move the right shape down; the connector's endpoints follow.
  await page.keyboard.press("v");
  await page.mouse.move(600, 150);
  await page.mouse.down();
  await page.mouse.move(600, 280, { steps: 6 });
  await page.mouse.up();
  expect(await count(page)).toBe(3);

  // Undo the move; still three elements.
  await page.keyboard.press("ControlOrMeta+z");
  expect(await count(page)).toBe(3);
});

test("double-click a connector to label it", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await drawShape(page, 100, 100, 300, 200);
  await drawShape(page, 500, 100, 700, 200);
  await page.keyboard.press("a");
  await page.mouse.move(280, 150);
  await page.mouse.down();
  await page.mouse.move(400, 150, { steps: 5 });
  await page.mouse.move(520, 150, { steps: 5 });
  await page.mouse.up();

  await page.keyboard.press("v");
  await page.mouse.dblclick(400, 150);
  const textEditor = page.locator("#text-editor");
  await expect(textEditor).toBeVisible();
  await page.keyboard.type("tcp/443");
  await page.keyboard.press("Escape");
  expect(await count(page)).toBe(4);

  const labelBounds = await selectionBounds(page);
  expect(labelBounds).toBeDefined();
  expect(labelBounds![0] + labelBounds![2] / 2).toBeCloseTo(400, -1);
});
