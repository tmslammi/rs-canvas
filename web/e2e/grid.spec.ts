import { expect, test, type Page } from "@playwright/test";

// The optional grid (G), snap-to-grid (⇧G) and their per-browser persistence.

async function boot(page: Page): Promise<void> {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
}

/// Non-transparent pixels on the grid canvas — the drawn grid lines.
async function gridPixels(page: Page): Promise<number> {
  return page.evaluate(() => {
    const canvas = document.getElementById("grid") as HTMLCanvasElement;
    const ctx = canvas.getContext("2d")!;
    const { data } = ctx.getImageData(0, 0, canvas.width, canvas.height);
    let lit = 0;
    for (let i = 3; i < data.length; i += 4) {
      if (data[i] > 0) lit++;
    }
    return lit;
  });
}

async function shapes(page: Page): Promise<Record<string, any>[]> {
  return page.evaluate(() => {
    const doc = JSON.parse(window.__rsEditor!.save_json());
    return Object.values(doc.elements as Record<string, any>).filter(
      (e: any) => e.type === "shape" && !e.isDeleted,
    );
  });
}

async function drag(
  page: Page,
  x1: number,
  y1: number,
  x2: number,
  y2: number,
): Promise<void> {
  await page.mouse.move(x1, y1);
  await page.mouse.down();
  await page.mouse.move(x2, y2, { steps: 5 });
  await page.mouse.up();
}

test("grid toggles on and off from the toolbar", async ({ page }) => {
  await boot(page);
  expect(await gridPixels(page), "grid starts hidden").toBe(0);

  await page.getByTitle("Grid — G").click();
  expect(await page.evaluate(() => window.__rsEditor!.grid_visible())).toBe(true);
  expect(await gridPixels(page), "grid lines are painted").toBeGreaterThan(0);

  await page.getByTitle("Grid — G").click();
  expect(await page.evaluate(() => window.__rsEditor!.grid_visible())).toBe(false);
  expect(await gridPixels(page), "grid is cleared again").toBe(0);
});

test("G toggles the grid, Shift+G toggles snapping", async ({ page }) => {
  await boot(page);
  await page.keyboard.press("g");
  expect(await page.evaluate(() => window.__rsEditor!.grid_visible())).toBe(true);
  await page.keyboard.press("Shift+G");
  expect(await page.evaluate(() => window.__rsEditor!.snap_enabled())).toBe(true);
  await page.keyboard.press("g");
  expect(await page.evaluate(() => window.__rsEditor!.grid_visible())).toBe(false);
  expect(await gridPixels(page)).toBe(0);
});

test("grid and snap preferences survive a reload", async ({ page }) => {
  await boot(page);
  await page.keyboard.press("g");
  await page.keyboard.press("Shift+G");
  await page.reload();
  await page.waitForFunction(() => window.__rsReady === true);
  expect(await page.evaluate(() => window.__rsEditor!.grid_visible())).toBe(true);
  expect(await page.evaluate(() => window.__rsEditor!.snap_enabled())).toBe(true);
  expect(await gridPixels(page)).toBeGreaterThan(0);
});

test("snap rounds new shapes to the grid; off leaves them alone", async ({ page }) => {
  await boot(page);
  await page.keyboard.press("Shift+G");

  await page.keyboard.press("r");
  await drag(page, 13, 27, 103, 217);
  let drawn = await shapes(page);
  expect(drawn.length).toBe(1);
  expect(drawn[0].rect.x).toBe(20);
  expect(drawn[0].rect.y).toBe(20);
  expect(drawn[0].rect.w).toBe(80);
  expect(drawn[0].rect.h).toBe(200);

  // Snapping off again: the same drag lands exactly where the pointer went.
  await page.keyboard.press("Shift+G");
  await page.keyboard.press("r");
  await drag(page, 313, 327, 403, 417);
  drawn = await shapes(page);
  expect(drawn.length).toBe(2);
  const loose = drawn.find((s) => s.rect.x !== 20)!;
  expect(loose.rect.x).toBe(313);
  expect(loose.rect.y).toBe(327);
});

test("snap moves shapes in grid steps", async ({ page }) => {
  await boot(page);
  await page.keyboard.press("Shift+G");
  await page.keyboard.press("r");
  await drag(page, 0, 0, 100, 60);

  await page.keyboard.press("v");
  await page.mouse.move(50, 30);
  await page.mouse.down();
  await page.mouse.move(63, 30, { steps: 4 });
  await page.mouse.up();

  const moved = (await shapes(page))[0];
  expect(moved.rect.x).toBe(20);
  expect(moved.rect.y).toBe(0);
});

test("grid size cycles and drives snapping", async ({ page }) => {
  await boot(page);
  expect(await page.evaluate(() => window.__rsEditor!.grid_size())).toBe(20);
  await page.getByTitle(/Grid size/).click();
  expect(await page.evaluate(() => window.__rsEditor!.grid_size())).toBe(40);
  await page.getByTitle(/Grid size/).click();
  expect(await page.evaluate(() => window.__rsEditor!.grid_size())).toBe(80);

  // Snap follows the chosen size: 13,27 -> 0,0 and 103,217 -> 80,240.
  await page.keyboard.press("Shift+G");
  await page.keyboard.press("r");
  await drag(page, 13, 27, 103, 217);
  const drawn = await shapes(page);
  expect(drawn[0].rect.x).toBe(0);
  expect(drawn[0].rect.y).toBe(0);
  expect(drawn[0].rect.w).toBe(80);
  expect(drawn[0].rect.h).toBe(240);
});

test("grid size survives a reload", async ({ page }) => {
  await boot(page);
  await page.getByTitle(/Grid size/).click();
  await page.reload();
  await page.waitForFunction(() => window.__rsReady === true);
  expect(await page.evaluate(() => window.__rsEditor!.grid_size())).toBe(40);
  expect(await page.getByTitle(/Grid size/).textContent()).toBe("40");
});

test("grid stays aligned while panning", async ({ page }) => {
  await boot(page);
  await page.keyboard.press("g");
  const before = await gridPixels(page);
  expect(before).toBeGreaterThan(0);
  await page.mouse.move(600, 400);
  await page.mouse.wheel(0, 240);
  await page.waitForTimeout(250);
  expect(await gridPixels(page), "still painted after a pan").toBeGreaterThan(0);
});
