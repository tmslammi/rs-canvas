import { expect, test } from "@playwright/test";

const count = (page: import("@playwright/test").Page): Promise<number> =>
  page.evaluate(() => window.__rsEditor!.element_count());

const elements = (page: import("@playwright/test").Page) =>
  page.evaluate(() => {
    const doc = JSON.parse(window.__rsEditor!.save_json());
    return Object.values(
      doc.elements as Record<string, Record<string, any>>,
    );
  });

test("sticky notes: place, type, colour and resize", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await page.keyboard.press("n");
  await page.mouse.click(400, 400);
  expect(await count(page)).toBe(1);

  let notes = (await elements(page)).filter((e) => e.type === "shape");
  expect(notes[0].shape).toBe("sticky");
  expect(notes[0].style.fillStyle).toBe("solid");
  expect(notes[0].style.fillColor).not.toBe("transparent");

  // Double-click types into the note; text is a bound element.
  await page.mouse.dblclick(400, 400);
  await expect(page.locator("#text-editor")).toBeVisible();
  await page.keyboard.type("idea");
  await page.keyboard.press("Escape");
  expect(await count(page)).toBe(2);
  const text = (await elements(page)).find((e) => e.type === "text");
  expect(text!.text).toBe("idea");

  // A fill swatch recolours the note.
  await page.keyboard.press("v");
  await page.mouse.click(400, 400);
  await page.getByTitle("Fill #a5d8ff").click();
  notes = (await elements(page)).filter((e) => e.type === "shape");
  expect(notes[0].style.fillColor).toBe("#a5d8ff");
});

test("sticky text edits in the note's ink, wraps, and leaves no ghost", async ({
  page,
}) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await page.keyboard.press("n");
  await page.mouse.click(400, 400);
  await page.mouse.dblclick(400, 400);
  const textarea = page.locator("#text-editor");
  await expect(textarea).toBeVisible();

  // The overlay must read like the note: dark ink on pastel, not the light
  // canvas default, and long lines must wrap instead of scrolling the text
  // out from under the left edge.
  const styling = await textarea.evaluate((element) => {
    const style = getComputedStyle(element);
    return { color: style.color, whiteSpace: style.whiteSpace };
  });
  expect(styling.color).toBe("rgb(59, 47, 28)");
  expect(styling.whiteSpace).toBe("pre-wrap");

  await page.keyboard.type("this note wraps onto several lines");
  const box = await textarea.evaluate((element) => ({
    scrollLeft: element.scrollLeft,
    scrollWidth: element.scrollWidth,
    clientWidth: element.clientWidth,
    scrollHeight: element.scrollHeight,
  }));
  expect(box.scrollLeft, "the overlay scrolls horizontally").toBe(0);
  expect(box.scrollWidth).toBeLessThanOrEqual(box.clientWidth + 1);
  expect(box.scrollHeight, "the overlay wraps onto several lines").toBeGreaterThan(
    60,
  );
  await page.keyboard.press("Escape");

  const textRect = await page.evaluate(() => {
    const doc = JSON.parse(window.__rsEditor!.save_json());
    const text = Object.values(doc.elements as Record<string, any>).find(
      (element) => element.type === "text",
    ) as { rect: { x: number; y: number; w: number; h: number } };
    return text.rect;
  });

  const countDark = () =>
    page.evaluate((rect) => {
      const canvas = document.getElementById("scene") as HTMLCanvasElement;
      const ctx = canvas.getContext("2d")!;
      const data = ctx.getImageData(rect.x, rect.y, rect.w, rect.h).data;
      let dark = 0;
      for (let i = 0; i < data.length; i += 4) {
        if (data[i] < 130 && data[i + 1] < 115 && data[i + 2] < 95) dark++;
      }
      return dark;
    }, textRect);
  expect(await countDark(), "committed sticky text is drawn").toBeGreaterThan(0);

  // Re-opening the editor hides the committed copy: no double vision and no
  // dirty-repaint seam across the note.
  await page.mouse.dblclick(400, 400);
  await expect(textarea).toBeVisible();
  expect(await countDark(), "the committed copy ghosts under the overlay").toBe(0);
  expect(await textarea.evaluate((element) => element.scrollLeft)).toBe(0);

  const offFill = await page.evaluate((rect) => {
    const canvas = document.getElementById("scene") as HTMLCanvasElement;
    const ctx = canvas.getContext("2d")!;
    const data = ctx.getImageData(rect.x, rect.y, rect.w, rect.h).data;
    let off = 0;
    for (let i = 0; i < data.length; i += 4) {
      const [r, g, b] = [data[i], data[i + 1], data[i + 2]];
      if (Math.abs(r - 255) > 6 || Math.abs(g - 236) > 6 || Math.abs(b - 153) > 6) {
        off++;
      }
    }
    return off;
  }, textRect);
  expect(offFill, "a repaint seam crosses the note").toBe(0);
});

test("sticky stack: pull notes from the middle, move it by the border", async ({
  page,
}) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await page.keyboard.press("k");
  await page.mouse.click(300, 300);
  expect(await count(page)).toBe(1);
  const stack = (await elements(page)).find((e) => e.type === "sticky_stack");
  expect(stack).toBeDefined();
  const stackRect = stack!.rect;

  // A click on the pile pulls nothing.
  await page.keyboard.press("n");
  await page.mouse.click(300, 300);
  expect(await count(page)).toBe(1);

  // N-drag off the pile creates a note at the drop point.
  await page.keyboard.press("n");
  await page.mouse.move(300, 300);
  await page.mouse.down();
  await page.mouse.move(600, 450, { steps: 5 });
  await page.mouse.up();
  expect(await count(page)).toBe(2);
  const note = (await elements(page)).find((e) => e.type === "shape");
  expect(note!.shape).toBe("sticky");
  expect(note!.rect.x).toBeCloseTo(600 - stackRect.w / 2, 0);
  expect(note!.rect.y).toBeCloseTo(450 - stackRect.h / 2, 0);

  // With Select, dragging the middle pulls another note.
  await page.keyboard.press("v");
  await page.mouse.move(300, 300);
  await page.mouse.down();
  await page.mouse.move(700, 300, { steps: 5 });
  await page.mouse.up();
  expect(await count(page)).toBe(3);
  const afterPull = await elements(page);
  expect(
    afterPull.find((e) => e.type === "sticky_stack")!.rect.x,
    "the pile stayed put",
  ).toBeCloseTo(stackRect.x, 0);
  const noteXs = afterPull
    .filter((e) => e.type === "shape")
    .map((e) => e.rect.x)
    .sort();

  // Dragging the border ring moves the pile; notes stay put.
  await page.mouse.move(235, 300);
  await page.mouse.down();
  await page.mouse.move(285, 360, { steps: 5 });
  await page.mouse.up();
  const after = await elements(page);
  const movedStack = after.find((e) => e.type === "sticky_stack");
  expect(movedStack!.rect.x).toBeCloseTo(stackRect.x + 50, 0);
  expect(movedStack!.rect.y).toBeCloseTo(stackRect.y + 60, 0);
  const noteXsAfter = after
    .filter((e) => e.type === "shape")
    .map((e) => e.rect.x)
    .sort();
  expect(noteXsAfter, "the notes stayed put").toEqual(noteXs);
});

test("dragging from a connection node draws a bound arrow", async ({ page }) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await page.keyboard.press("r");
  await page.mouse.move(200, 200);
  await page.mouse.down();
  await page.mouse.move(400, 300, { steps: 5 });
  await page.mouse.up();
  await page.keyboard.press("r");
  await page.mouse.move(600, 200);
  await page.mouse.down();
  await page.mouse.move(800, 300, { steps: 5 });
  await page.mouse.up();
  await page.keyboard.press("v");
  await page.mouse.click(1000, 600);

  // Hover the first shape, then drag from its east connection node (the
  // node floats 12px outside the edge) to the second shape.
  await page.mouse.move(300, 250);
  await page.mouse.move(412, 250);
  await page.mouse.down();
  await page.mouse.move(588, 250, { steps: 5 });
  await page.mouse.up();

  expect(await count(page)).toBe(3);
  const arrow = (await elements(page)).find((e) => e.type === "arrow");
  expect(arrow!.start.binding).toBeDefined();
  expect(arrow!.end.binding).toBeDefined();
  expect(
    await page.evaluate(() => window.__rsEditor!.active_tool()),
    "the new arrow is selected and Select stays armed",
  ).toBe("select");

  // Moving the source shape drags the bound start along.
  const startY = arrow!.start.y;
  await page.mouse.move(300, 250);
  await page.mouse.down();
  await page.mouse.move(300, 350, { steps: 5 });
  await page.mouse.up();
  const moved = (await elements(page)).find((e) => e.type === "arrow");
  expect(moved!.start.y).toBeCloseTo(startY + 100, 0);
});
