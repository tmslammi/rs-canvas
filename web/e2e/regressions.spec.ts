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
  expect(moved![1]).toBeCloseTo(350, -1);
});

test("clicking placed text with the text tool edits it instead of stacking a new one", async ({
  page,
}) => {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());

  await placeText(page);
  expect(await count(page)).toBe(1);

  // Text tool is still active; clicking the placed text should edit it.
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

test("text committed by clicking away is one undo step", async ({ page }) => {
  await page.goto("/");
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
