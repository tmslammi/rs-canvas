import { expect, test } from "@playwright/test";

const scroll = (page: import("@playwright/test").Page): Promise<number[]> =>
  page.evaluate(() => Array.from(window.__rsEditor!.scroll()));

const zoom = (page: import("@playwright/test").Page): Promise<number> =>
  page.evaluate(() => window.__rsEditor!.zoom());

test("space-drag pans the camera", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);

  const before = await scroll(page);
  await page.keyboard.down(" ");
  await page.mouse.move(640, 400);
  await page.mouse.down();
  await page.mouse.move(760, 460, { steps: 5 });
  await page.mouse.up();
  await page.keyboard.up(" ");

  const after = await scroll(page);
  expect(after[0] - before[0]).toBeCloseTo(-120, 0);
  expect(after[1] - before[1]).toBeCloseTo(-60, 0);
});

test("trackpad scroll pans, ctrl+wheel zooms around the cursor", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);

  const beforePan = await scroll(page);
  await page.mouse.move(640, 400);
  await page.mouse.wheel(0, 120);
  const afterPan = await scroll(page);
  expect(afterPan[1] - beforePan[1]).toBeCloseTo(120, 0);
  expect(await zoom(page)).toBe(1);

  const worldBefore = await page.evaluate(() =>
    Array.from(window.__rsEditor!.screen_to_world(500, 350)),
  );
  await page.mouse.move(500, 350);
  await page.keyboard.down("Control");
  await page.mouse.wheel(0, -240);
  await page.keyboard.up("Control");

  expect(await zoom(page)).toBeGreaterThan(1);
  const worldAfter = await page.evaluate(() =>
    Array.from(window.__rsEditor!.screen_to_world(500, 350)),
  );
  expect(Math.abs(worldBefore[0] - worldAfter[0])).toBeLessThan(1.5);
  expect(Math.abs(worldBefore[1] - worldAfter[1])).toBeLessThan(1.5);
});

test("zoom speed setting scales ctrl+wheel zoom and persists", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());
  expect(await page.evaluate(() => window.__rsEditor!.zoom_sensitivity())).toBe(1);

  const wheelZoom = async () => {
    await page.evaluate(() => window.__rsEditor!.reset_zoom());
    await page.mouse.move(640, 400);
    await page.keyboard.down("Control");
    await page.mouse.wheel(0, -240);
    await page.keyboard.up("Control");
    return zoom(page);
  };
  const normal = await wheelZoom();
  expect(normal).toBeGreaterThan(1);

  await page.getByTitle(/Zoom speed/).click();
  expect(await page.evaluate(() => window.__rsEditor!.zoom_sensitivity())).toBe(2);
  const fast = await wheelZoom();
  expect(fast, "2× zoom speed zooms further per wheel tick").toBeGreaterThan(normal);

  await page.reload();
  await page.waitForFunction(() => window.__rsReady === true);
  expect(await page.evaluate(() => window.__rsEditor!.zoom_sensitivity())).toBe(2);
});

test("shift+1 fits the drawing, shift+0 resets to 100%", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.getByTitle("Seed demo").click();

  await page.keyboard.press("Shift+1");
  const fitZoom = await zoom(page);
  expect(fitZoom).not.toBe(1);

  await page.keyboard.press("Shift+0");
  expect(await zoom(page)).toBeCloseTo(1, 5);
});
