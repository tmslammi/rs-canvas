import { expect, test } from "@playwright/test";

async function drawFrame(page: import("@playwright/test").Page) {
  await page.keyboard.press("f");
  await page.mouse.move(200, 200);
  await page.mouse.down();
  await page.mouse.move(600, 500, { steps: 5 });
  await page.mouse.up();
}

async function encloseFrame(page: import("@playwright/test").Page, reverse = false) {
  await page.mouse.move(reverse ? 650 : 150, reverse ? 550 : 150);
  await page.mouse.down();
  await page.mouse.move(reverse ? 150 : 650, reverse ? 150 : 550, { steps: 5 });
  await page.mouse.up();
}

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

for (const reverse of [false, true]) {
  test(`a mouse marquee enclosing an empty frame selects it (${reverse ? "right to left" : "left to right"})`, async ({ page }) => {
    await page.goto("/");
    await page.waitForFunction(() => window.__rsReady === true);
    await drawFrame(page);
    await page.keyboard.press("v");
    await page.mouse.click(900, 650);
    expect(await selectionBounds(page)).toBeUndefined();
    await encloseFrame(page, reverse);
    const selected = await selectionBounds(page);
    expect(selected, "enclosing the entire frame selects its container").toBeDefined();
    expect(selected).toEqual([200, 200, 400, 300]);

    await page.mouse.move(200, 400);
    await page.mouse.down();
    await page.mouse.move(260, 450, { steps: 5 });
    await page.mouse.up();
    expect(await selectionBounds(page)).toEqual([260, 250, 400, 300]);
    await page.keyboard.press("ControlOrMeta+z");
    const frame = await page.evaluate(() => Object.values<any>(JSON.parse(window.__rsEditor!.save_json()).elements).find((element) => element.type === "frame"));
    expect(frame.rect).toEqual({ x: 200, y: 200, w: 400, h: 300 });
    await page.keyboard.press("ControlOrMeta+z");
    expect(await page.evaluate(() => window.__rsEditor!.element_count()), "marquee selection did not add an undo step").toBe(0);
  });
}

test("an enclosing marquee moves a frame and its children exactly once", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await drawFrame(page);
  await page.keyboard.press("r");
  await page.mouse.move(300, 300);
  await page.mouse.down();
  await page.mouse.move(450, 350, { steps: 4 });
  await page.mouse.up();
  await page.keyboard.press("v");
  await page.mouse.click(900, 650);
  await encloseFrame(page);
  expect(await selectionBounds(page), "container is included, not just its child").toEqual([200, 200, 400, 300]);
  await page.mouse.move(200, 400);
  await page.mouse.down();
  await page.mouse.move(260, 450, { steps: 5 });
  await page.mouse.up();
  const elements = await page.evaluate(() => Object.values<any>(JSON.parse(window.__rsEditor!.save_json()).elements));
  expect(elements.find((element) => element.type === "frame").rect).toEqual({ x: 260, y: 250, w: 400, h: 300 });
  expect(elements.find((element) => element.type === "shape").rect).toEqual({ x: 360, y: 350, w: 150, h: 50 });
});

test("shift marquee adds an enclosed frame to the existing selection", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await drawFrame(page);
  await page.keyboard.press("r");
  await page.mouse.move(750, 250);
  await page.mouse.down();
  await page.mouse.move(900, 330, { steps: 4 });
  await page.mouse.up();
  await page.keyboard.press("v");
  await page.mouse.click(900, 650);
  await page.mouse.click(825, 290);
  expect(await selectionBounds(page)).toEqual([750, 250, 150, 80]);
  await page.keyboard.down("Shift");
  await encloseFrame(page);
  await page.keyboard.up("Shift");
  expect(await selectionBounds(page)).toEqual([200, 200, 700, 300]);
});

test("a marquee partially overlapping a frame selects its contents without the container", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await drawFrame(page);
  await page.keyboard.press("r");
  await page.mouse.move(300, 300);
  await page.mouse.down();
  await page.mouse.move(450, 350, { steps: 4 });
  await page.mouse.up();
  await page.keyboard.press("v");
  await page.mouse.click(900, 650);
  await page.mouse.move(150, 150);
  await page.mouse.down();
  await page.mouse.move(500, 450, { steps: 5 });
  await page.mouse.up();
  expect(await selectionBounds(page)).toEqual([300, 300, 150, 50]);
});
