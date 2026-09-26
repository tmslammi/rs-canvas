import { expect, test } from "@playwright/test";

const style = (page: import("@playwright/test").Page): Promise<Record<string, unknown>> =>
  page.evaluate(() => JSON.parse(window.__rsEditor!.current_style()));

test("style panel applies stroke colour to the selection and undo restores", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await page.keyboard.press("r");
  await page.mouse.move(200, 200);
  await page.mouse.down();
  await page.mouse.move(400, 300, { steps: 5 });
  await page.mouse.up();

  expect((await style(page)).strokeColor).toBe("#ece4d6");

  await page.getByTitle("Stroke #ff8787").click();
  expect((await style(page)).strokeColor).toBe("#ff8787");

  await page.keyboard.press("ControlOrMeta+z");
  await page.keyboard.press("v");
  await page.mouse.click(300, 250);
  expect((await style(page)).strokeColor).toBe("#ece4d6");
});

test("fill style and roughness buttons update the selection", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await page.keyboard.press("r");
  await page.mouse.move(200, 200);
  await page.mouse.down();
  await page.mouse.move(400, 300, { steps: 5 });
  await page.mouse.up();

  await page.getByTitle("Fill style Solid").click();
  await page.getByTitle("Fill #ffec99").click();
  await page.getByTitle("Roughness 2").click();
  const applied = await style(page);
  expect(applied.fillStyle).toBe("solid");
  expect(applied.roughness).toBe(2);

  const painted = await page.evaluate(() => {
    const canvas = document.getElementById("scene") as HTMLCanvasElement;
    const ctx = canvas.getContext("2d")!;
    const data = ctx.getImageData(280, 240, 40, 20).data;
    let nonBackground = 0;
    for (let i = 0; i < data.length; i += 4) {
      const isBackground =
        data[i] < 40 && data[i + 1] < 40 && data[i + 2] < 40;
      if (!isBackground) nonBackground++;
    }
    return nonBackground;
  });
  expect(painted).toBeGreaterThan(0);
});

test("style defaults apply to the next drawn shape", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await page.getByTitle("Stroke #69db7c").click();
  await page.keyboard.press("r");
  await page.mouse.move(200, 200);
  await page.mouse.down();
  await page.mouse.move(300, 260, { steps: 4 });
  await page.mouse.up();

  expect((await style(page)).strokeColor).toBe("#69db7c");
});
