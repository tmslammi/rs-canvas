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

async function drawRect(page: import("@playwright/test").Page): Promise<void> {
  await page.keyboard.press("r");
  await page.mouse.move(200, 200);
  await page.mouse.down();
  await page.mouse.move(400, 300, { steps: 5 });
  await page.mouse.up();
}

async function placeText(page: import("@playwright/test").Page): Promise<void> {
  await page.keyboard.press("t");
  await page.mouse.click(300, 300);
  await expect(page.locator("#text-editor")).toBeVisible();
  await page.keyboard.type("hello");
  await page.keyboard.press("Escape");
  await expect(page.locator("#text-editor")).toBeHidden();
}

test("undo and redo work from the keyboard", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await drawRect(page);
  expect(await count(page)).toBe(1);

  await page.keyboard.press("ControlOrMeta+z");
  expect(await count(page), "undo removes the shape").toBe(0);

  await page.keyboard.press("ControlOrMeta+Shift+z");
  expect(await count(page), "redo brings it back").toBe(1);

  await page.keyboard.press("ControlOrMeta+z");
  expect(await count(page)).toBe(0);
});

test("undo and redo work from the toolbar", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await drawRect(page);
  expect(await count(page)).toBe(1);

  await page.getByTitle("Undo — ⌘Z").click();
  expect(await count(page)).toBe(0);

  await page.getByTitle("Redo — ⇧⌘Z").click();
  expect(await count(page)).toBe(1);
});

test("placed text can be selected and moved", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await placeText(page);
  expect(await count(page)).toBe(1);

  await page.keyboard.press("v");
  await page.mouse.click(800, 600);
  expect(await selectionBounds(page)).toBeUndefined();

  await page.mouse.click(320, 310);
  const selected = await selectionBounds(page);
  expect(selected, "clicking the placed text selects it").toBeDefined();
  expect(selected![0]).toBeCloseTo(300, -1);

  await page.mouse.move(320, 310);
  await page.mouse.down();
  await page.mouse.move(420, 360, { steps: 5 });
  await page.mouse.up();
  const moved = await selectionBounds(page);
  expect(moved![0]).toBeCloseTo(400, -1);
  expect(moved![1]).toBeCloseTo(selected![1] + 50, -1);
});

test("clicking placed text with the text tool edits it instead of stacking a new one", async ({
  page,
}) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await placeText(page);
  expect(await count(page)).toBe(1);

  // Re-arm the text tool; clicking the placed text should edit it.
  await page.keyboard.press("t");
  await page.mouse.click(320, 310);
  await expect(page.locator("#text-editor")).toBeVisible();
  const textarea = page.locator("#text-editor");
  expect(await textarea.inputValue()).toBe("hello");
  await page.keyboard.press("Escape");

  await page.keyboard.press("v");
  await page.mouse.click(320, 310);
  const selected = await selectionBounds(page);
  expect(selected, "the placed text is selectable afterwards").toBeDefined();
  expect(await count(page), "no extra text element was stacked").toBe(1);
});

test("restored text is selectable after a reload", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());
  await placeText(page);
  await page.waitForTimeout(1000);

  await page.reload();
  await page.waitForFunction(() => window.__rsReady === true);
  expect(await count(page)).toBe(1);

  await page.keyboard.press("v");
  await page.mouse.click(320, 310);
  const selected = await selectionBounds(page);
  expect(selected, "restored text is selectable").toBeDefined();
  expect(selected![0]).toBeCloseTo(300, -1);
});

test("undo and redo cover connectors", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await drawRect(page);
  await page.keyboard.press("r");
  await page.mouse.move(500, 200);
  await page.mouse.down();
  await page.mouse.move(700, 300, { steps: 5 });
  await page.mouse.up();

  await page.keyboard.press("a");
  await page.mouse.move(280, 250);
  await page.mouse.down();
  await page.mouse.move(520, 250, { steps: 5 });
  await page.mouse.up();
  expect(await count(page)).toBe(3);

  await page.keyboard.press("ControlOrMeta+z");
  expect(await count(page), "undo removes the connector").toBe(2);
  await page.keyboard.press("ControlOrMeta+Shift+z");
  expect(await count(page), "redo restores the connector").toBe(3);
});

test("dragging a resize handle past the opposite corner flips the shape", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await page.keyboard.press("r");
  await page.mouse.move(200, 200);
  await page.mouse.down();
  await page.mouse.move(400, 300, { steps: 5 });
  await page.mouse.up();

  // NE handle at (400, 200); drag it past the SW corner (200, 300).
  await page.mouse.move(400, 200);
  await page.mouse.down();
  await page.mouse.move(150, 350, { steps: 6 });
  await page.mouse.up();

  const flipped = await selectionBounds(page);
  expect(flipped).toBeDefined();
  expect(flipped![0]).toBeCloseTo(150, -1);
  expect(flipped![1]).toBeCloseTo(300, -1);
  expect(flipped![2]).toBeCloseTo(50, -1);
  expect(flipped![3]).toBeCloseTo(50, -1);

  await page.keyboard.press("ControlOrMeta+z");
  await page.keyboard.press("v");
  await page.mouse.click(300, 250);
  const restored = await selectionBounds(page);
  expect(restored![0]).toBeCloseTo(200, -1);
  expect(restored![2]).toBeCloseTo(200, -1);
});

test("after drawing, the shape can be resized without re-picking a tool", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await page.keyboard.press("r");
  await page.mouse.move(200, 200);
  await page.mouse.down();
  await page.mouse.move(400, 300, { steps: 5 });
  await page.mouse.up();

  expect(
    await page.evaluate(() => window.__rsEditor!.active_tool()),
    "the draw tool hands back to select",
  ).toBe("select");

  // Grab the SE handle straight away: no tool switch.
  await page.mouse.move(400, 300);
  await page.mouse.down();
  await page.mouse.move(500, 350, { steps: 5 });
  await page.mouse.up();

  const resized = await selectionBounds(page);
  expect(resized![2]).toBeCloseTo(300, -1);
  expect(resized![3]).toBeCloseTo(150, -1);
});

test("a locked tool draws repeatedly; unlocking returns to select", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  const rectangle = page.getByTitle("Rectangle — R");
  await rectangle.click();
  await rectangle.dblclick();
  expect(await page.evaluate(() => window.__rsEditor!.is_tool_locked())).toBe(true);

  for (const x of [100, 400]) {
    await page.mouse.move(x, 150);
    await page.mouse.down();
    await page.mouse.move(x + 120, 230, { steps: 4 });
    await page.mouse.up();
  }
  expect(await count(page)).toBe(2);
  expect(await page.evaluate(() => window.__rsEditor!.active_tool())).toBe("rectangle");

  await rectangle.dblclick();
  expect(await page.evaluate(() => window.__rsEditor!.is_tool_locked())).toBe(false);
  await page.mouse.move(700, 150);
  await page.mouse.down();
  await page.mouse.move(820, 230, { steps: 4 });
  await page.mouse.up();
  expect(await count(page)).toBe(3);
  expect(await page.evaluate(() => window.__rsEditor!.active_tool())).toBe("select");
});

test("an inserted library glyph can be selected and moved", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());
  await page.evaluate(() => window.__rsEditor!.insert_glyph("cloud_storage"));
  expect(await count(page)).toBe(2);

  await page.keyboard.press("v");
  await page.mouse.click(950, 650);
  expect(await selectionBounds(page)).toBeUndefined();

  // The glyph is 96x96 at the viewport centre (1280x800 -> 592..688).
  await page.mouse.click(640, 400);
  const selected = await selectionBounds(page);
  expect(selected, "clicking the glyph selects it").toBeDefined();
  expect(selected![0]).toBeCloseTo(592, -1);
  expect(selected![1]).toBeCloseTo(352, -1);
  expect(selected![2]).toBeCloseTo(96, -1);

  await page.mouse.move(640, 400);
  await page.mouse.down();
  await page.mouse.move(740, 450, { steps: 5 });
  await page.mouse.up();
  const moved = await selectionBounds(page);
  expect(moved![0]).toBeCloseTo(692, -1);
  expect(moved![1]).toBeCloseTo(402, -1);

  // The caption belongs to the component: clicking it selects the glyph
  // (which owns the caption), not the caption alone.
  await page.keyboard.press("ControlOrMeta+z");
  await page.mouse.click(950, 650);
  expect(await selectionBounds(page)).toBeUndefined();
  await page.mouse.click(640, 465);
  const viaCaption = await selectionBounds(page);
  expect(viaCaption, "clicking the caption selects the component").toBeDefined();
  expect(viaCaption![2]).toBeCloseTo(96, -1);

  // Dragging the caption moves the whole component.
  await page.mouse.move(640, 465);
  await page.mouse.down();
  await page.mouse.move(540, 515, { steps: 5 });
  await page.mouse.up();
  const dragged = await selectionBounds(page);
  expect(dragged![0]).toBeCloseTo(492, -1);
  expect(dragged![1]).toBeCloseTo(402, -1);
});

test("text committed by clicking away is one undo step", async ({ page }) => {  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await page.keyboard.press("t");
  await page.mouse.click(300, 300);
  await page.keyboard.type("hello");
  // Clicking away commits the text (and the still-active text tool opens a
  // fresh empty editor at the click point).
  await page.mouse.click(700, 500);
  await page.keyboard.press("Escape");
  expect(await count(page)).toBe(1);

  await page.keyboard.press("ControlOrMeta+z");
  expect(await count(page), "one undo removes the committed text").toBe(0);
  await page.keyboard.press("ControlOrMeta+Shift+z");
  expect(await count(page)).toBe(1);
});

test("redo is cleared by a new edit after undo", async ({ page }) => {  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await drawRect(page);
  await page.keyboard.press("ControlOrMeta+z");
  expect(await count(page)).toBe(0);
  expect(await page.evaluate(() => window.__rsEditor!.can_redo())).toBe(true);

  await page.keyboard.press("r");
  await page.mouse.move(500, 400);
  await page.mouse.down();
  await page.mouse.move(600, 460, { steps: 4 });
  await page.mouse.up();
  expect(await page.evaluate(() => window.__rsEditor!.can_redo())).toBe(false);

  await page.keyboard.press("ControlOrMeta+z");
  expect(await count(page), "undo removes the new edit, not the old one").toBe(0);
  await page.keyboard.press("ControlOrMeta+z");
  expect(await count(page)).toBe(0);
});

test("after a freehand stroke, the brush stays armed and the stroke is selectable", async ({
  page,
}) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await page.keyboard.press("p");
  await page.mouse.move(300, 400);
  await page.mouse.down();
  await page.mouse.move(500, 400, { steps: 6 });
  await page.mouse.up();
  expect(await count(page)).toBe(1);
  expect(
    await page.evaluate(() => window.__rsEditor!.active_tool()),
    "the pencil stays armed for the next stroke",
  ).toBe("freehand");

  // The next drag is another stroke, not a selection.
  await page.mouse.move(300, 500);
  await page.mouse.down();
  await page.mouse.move(500, 500, { steps: 6 });
  await page.mouse.up();
  expect(await count(page)).toBe(2);

  await page.keyboard.press("v");
  await page.mouse.click(800, 600);
  expect(await selectionBounds(page)).toBeUndefined();
  await page.mouse.click(400, 400);
  const selected = await selectionBounds(page);
  expect(selected, "the stroke is selectable once Select is armed").toBeDefined();
});

test("dragging a bound connector keeps it attached to its shapes", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await drawRect(page);
  await page.keyboard.press("r");
  await page.mouse.move(500, 200);
  await page.mouse.down();
  await page.mouse.move(700, 300, { steps: 5 });
  await page.mouse.up();

  await page.keyboard.press("a");
  await page.mouse.move(360, 250);
  await page.mouse.down();
  await page.mouse.move(540, 250, { steps: 5 });
  await page.mouse.up();
  expect(await count(page)).toBe(3);

  // Drag the connector's shaft by its middle.
  await page.mouse.move(450, 250);
  await page.mouse.down();
  await page.mouse.move(460, 310, { steps: 4 });
  await page.mouse.up();

  // Move the right shape down by 100px.
  await page.mouse.move(600, 250);
  await page.mouse.down();
  await page.mouse.move(600, 350, { steps: 5 });
  await page.mouse.up();

  // Re-select the connector where it still passes: the endpoint must have
  // followed the shape, so the selection is now 100px tall.
  await page.mouse.click(450, 250);
  const bounds = await selectionBounds(page);
  expect(bounds, "the connector is selectable where it always was").toBeDefined();
  expect(bounds![1]).toBeCloseTo(250, -1);
  expect(bounds![3], "the connector stretched down with the shape").toBeGreaterThan(50);
});

// A partial repaint clears a dirty rectangle and redraws every layer whose
// bounds intersect it. The redraw must be clipped to the cleared region:
// redrawing a whole lower layer paints over higher layers that lie outside
// the dirty rect, which made the higher elements vanish until the next full
// render (reported as elements disappearing while drawing).
const shapeStyle = (fillColor: string) => ({
  strokeColor: "#ece4d6",
  fillColor,
  fillStyle: "solid",
  strokeWidth: 2,
  roughness: 0,
  dash: "solid",
  opacity: 1,
  fontSize: 20,
});

const element = (
  id: string,
  index: string,
  rect: { x: number; y: number; w: number; h: number },
  fillColor: string,
) => ({
  id,
  version: 1,
  versionNonce: 1,
  index,
  isDeleted: false,
  updated: 1,
  type: "shape",
  shape: "rectangle",
  rect,
  style: shapeStyle(fillColor),
});

const pixelAt = (
  page: import("@playwright/test").Page,
  x: number,
  y: number,
): Promise<number[]> =>
  page.evaluate(
    ([sx, sy]) => {
      const canvas = document.getElementById("scene") as HTMLCanvasElement;
      const data = canvas
        .getContext("2d")!
        .getImageData(Math.round(sx!), Math.round(sy!), 1, 1).data;
      return [data[0], data[1], data[2], data[3]];
    },
    [x, y],
  );

const isGreen = (pixel: number[]) => pixel[1] > 120 && pixel[0] < 120 && pixel[2] < 120;
const isBlue = (pixel: number[]) => pixel[2] > 120 && pixel[0] < 120;

const stackedDocument = () => ({
  schemaVersion: 1,
  elements: {
    // The blue rectangle sits under the green one and is far bigger, so it
    // intersects both the freehand dirty band and the pan strips.
    back: element("back", "a0", { x: 0, y: 0, w: 1500, h: 700 }, "#1f5fbf"),
    front: element("front", "a1", { x: 100, y: 100, w: 200, h: 120 }, "#3cb44b"),
  },
});

test("a partial repaint keeps a higher element on top of a redrawn lower one", async ({
  page,
}) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());
  await page.evaluate(
    (doc) => window.__rsEditor!.load_json(JSON.stringify(doc)),
    stackedDocument(),
  );

  const frontCentre = await pixelAt(page, 200, 160);
  expect(isGreen(frontCentre), "the green rectangle renders on top").toBe(true);

  // Draw a freehand stroke across the blue rectangle, well below the green
  // one: the stroke's dirty band intersects the blue layer only.
  await page.keyboard.press("p");
  await page.mouse.move(100, 560);
  await page.mouse.down();
  await page.mouse.move(1000, 560, { steps: 8 });

  expect(
    isGreen(await pixelAt(page, 200, 160)),
    "green stays on top while the stroke repaints the blue rectangle",
  ).toBe(true);
  await page.mouse.up();
});

test("a pan strip repaint keeps a higher element on top of a redrawn lower one", async ({
  page,
}) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());
  await page.evaluate(
    (doc) => window.__rsEditor!.load_json(JSON.stringify(doc)),
    stackedDocument(),
  );
  await page.evaluate(() => window.__rsEditor!.render());

  // Pan 30px left; the exposed right strip intersects only the blue layer.
  await page.evaluate(() => window.__rsEditor!.wheel(640, 400, 30, 0, false));

  expect(
    isGreen(await pixelAt(page, 170, 160)),
    "green stays on top during a strip-repaint pan",
  ).toBe(true);
  expect(isBlue(await pixelAt(page, 1260, 400)), "the exposed strip shows blue").toBe(true);
});
