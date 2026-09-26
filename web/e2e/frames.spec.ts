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

test("frame owns dropped shapes, moves them, and cascades deletes", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await page.keyboard.press("f");
  await page.mouse.move(200, 200);
  await page.mouse.down();
  await page.mouse.move(600, 500, { steps: 6 });
  await page.mouse.up();
  expect(await count(page)).toBe(1);

  // Draw two shapes inside the frame.
  for (const y of [300, 400]) {
    await page.keyboard.press("r");
    await page.mouse.move(300, y - 30);
    await page.mouse.down();
    await page.mouse.move(450, y, { steps: 4 });
    await page.mouse.up();
  }
  expect(await count(page)).toBe(3);

  // Grab the frame on its border away from handles and move it.
  await page.keyboard.press("v");
  await page.mouse.move(200, 350);
  await page.mouse.down();
  await page.mouse.move(260, 400, { steps: 6 });
  await page.mouse.up();

  // Deselect, then select one of the children: it followed the frame.
  await page.mouse.click(900, 700);
  await page.mouse.click(420, 335);
  const child = await selectionBounds(page);
  expect(child).toBeDefined();
  expect(child![0]).toBeCloseTo(360, -1);
  expect(child![1]).toBeCloseTo(320, -1);

  // Deleting the frame (click its border) deletes children; one undo restores all.
  await page.mouse.click(400, 250);
  await page.keyboard.press("Delete");
  expect(await count(page)).toBe(0);
  await page.keyboard.press("ControlOrMeta+z");
  expect(await count(page)).toBe(3);
});

test("renaming a frame through the text overlay", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await page.keyboard.press("f");
  await page.mouse.move(200, 200);
  await page.mouse.down();
  await page.mouse.move(500, 400, { steps: 5 });
  await page.mouse.up();

  await page.keyboard.press("v");
  await page.mouse.dblclick(250, 192);
  const textEditor = page.locator("#text-editor");
  await expect(textEditor).toBeVisible();
  await page.keyboard.press("ControlOrMeta+a");
  await page.keyboard.type("us-central1");
  await page.keyboard.press("Escape");
  await expect(textEditor).toBeHidden();

  const painted = await page.evaluate(() => {
    const canvas = document.getElementById("scene") as HTMLCanvasElement;
    const ctx = canvas.getContext("2d")!;
    const data = ctx.getImageData(195, 175, 150, 25).data;
    for (let i = 0; i < data.length; i += 4) {
      if (data[i] < 200 && data[i + 1] < 200) return true;
    }
    return false;
  });
  expect(painted).toBe(true);
});
