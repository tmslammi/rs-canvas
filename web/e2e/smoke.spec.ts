import { expect, test } from "@playwright/test";

test("boots to an empty canvas, seeds the demo, and paints it", async ({ page }) => {
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));

  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);

  expect(
    await page.evaluate(() => window.__rsEditor!.element_count()),
    "a fresh boot is empty",
  ).toBe(0);

  await page.getByTitle("Seed demo").click();
  const count = await page.evaluate(() => window.__rsEditor!.element_count());
  expect(count).toBeGreaterThan(15);

  const hasInk = await page.evaluate(() => {
    const canvas = document.getElementById("scene") as HTMLCanvasElement;
    const ctx = canvas.getContext("2d");
    if (!ctx || canvas.width === 0 || canvas.height === 0) return false;
    const data = ctx.getImageData(0, 0, canvas.width, canvas.height).data;
    for (let i = 0; i < data.length; i += 4) {
      const distance =
        Math.abs(data[i] - 26) + Math.abs(data[i + 1] - 24) + Math.abs(data[i + 2] - 21);
      if (distance > 90) return true;
    }
    return false;
  });

  expect(hasInk).toBe(true);
  expect(errors).toEqual([]);
});
