import { expect, test } from "@playwright/test";

const selectionBounds = (
  page: import("@playwright/test").Page,
): Promise<number[] | undefined> =>
  page.evaluate(() => {
    const bounds = window.__rsEditor!.selection_bounds();
    return bounds ? Array.from(bounds) : undefined;
  });

test("selecting things inside a frame picks them, not the frame", async ({
  page,
}) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  // A frame with two texts and a shape inside.
  await page.keyboard.press("f");
  await page.mouse.move(100, 100);
  await page.mouse.down();
  await page.mouse.move(600, 500, { steps: 5 });
  await page.mouse.up();

  for (const [x, y] of [
    [350, 250],
    [350, 350],
  ]) {
    await page.keyboard.press("t");
    await page.mouse.click(x, y);
    await page.keyboard.type("asd");
    await page.keyboard.press("Escape");
  }

  // Clicking a text selects the text (the frame is only grabbable by its
  // border or name).
  await page.keyboard.press("v");
  await page.mouse.click(800, 700);
  await page.mouse.click(360, 260);
  const clicked = await selectionBounds(page);
  expect(clicked, "a text is selected").toBeDefined();
  expect(clicked![2], "the text, not the frame").toBeLessThan(100);

  // A marquee dragged inside the frame selects its contents only.
  await page.mouse.move(200, 150);
  await page.mouse.down();
  await page.mouse.move(500, 450, { steps: 5 });
  await page.mouse.up();
  const marquee = await selectionBounds(page);
  expect(marquee, "the texts are selected").toBeDefined();
  expect(marquee![2], "not the frame's 500px width").toBeLessThan(100);
});
