import { expect, test, type Page } from "@playwright/test";

// Connector routing quality: arrows step around elements in the way, and a
// selected arrow's path can be bent by dragging it.

async function boot(page: Page): Promise<void> {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.evaluate(() => window.__rsEditor!.new_document());
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
  await page.mouse.move(x2, y2, { steps: 6 });
  await page.mouse.up();
}

async function rect(
  page: Page,
  x1: number,
  y1: number,
  x2: number,
  y2: number,
): Promise<void> {
  await page.keyboard.press("r");
  await drag(page, x1, y1, x2, y2);
}

async function arrowAt(
  page: Page,
  from: { x: number; y: number },
  to: { x: number; y: number },
): Promise<void> {
  await page.keyboard.press("a");
  await drag(page, from.x, from.y, to.x, to.y);
}

const elements = (page: Page) =>
  page.evaluate(() => {
    const doc = JSON.parse(window.__rsEditor!.save_json());
    return Object.values(doc.elements as Record<string, any>).filter(
      (e: any) => !e.isDeleted,
    );
  });

const arrows = async (page: Page) =>
  (await elements(page)).filter((e) => e.type === "arrow");

const route = async (page: Page, id: string): Promise<{ x: number; y: number }[]> => {
  const flat = await page.evaluate(
    (arrowId) => window.__rsEditor!.route_for(arrowId),
    id,
  );
  const points: { x: number; y: number }[] = [];
  for (let i = 0; i + 1 < flat.length; i += 2) {
    points.push({ x: flat[i], y: flat[i + 1] });
  }
  return points;
};

function crossesBlock(
  points: { x: number; y: number }[],
  block: { x: number; y: number; w: number; h: number },
): boolean {
  for (let i = 0; i + 1 < points.length; i++) {
    if (segmentHitsRect(points[i], points[i + 1], block)) return true;
  }
  return false;
}

function segmentHitsRect(
  a: { x: number; y: number },
  b: { x: number; y: number },
  r: { x: number; y: number; w: number; h: number },
): boolean {
  // Sampling is fine: routes are axis-aligned and the block has area.
  const steps = 200;
  for (let i = 0; i <= steps; i++) {
    const t = i / steps;
    const x = a.x + (b.x - a.x) * t;
    const y = a.y + (b.y - a.y) * t;
    if (x > r.x && x < r.x + r.w && y > r.y && y < r.y + r.h) return true;
  }
  return false;
}

test("arrows route around an element in the way", async ({ page }) => {
  await boot(page);
  await rect(page, 100, 200, 200, 300); // A
  await rect(page, 340, 180, 460, 380); // the blocker
  await rect(page, 600, 200, 700, 300); // B
  await arrowAt(page, { x: 150, y: 250 }, { x: 650, y: 250 });

  const [arrow] = await arrows(page);
  expect(arrow, "arrow created").toBeDefined();
  const points = await route(page, arrow.id);
  expect(points.length).toBeGreaterThanOrEqual(2);
  expect({ x: points[0].x, y: points[0].y }).toEqual({ x: 200, y: 250 });
  expect({
    x: points[points.length - 1].x,
    y: points[points.length - 1].y,
  }).toEqual({ x: 600, y: 250 });

  const blocker = { x: 340, y: 180, w: 120, h: 200 };
  expect(crossesBlock(points, blocker), `route crosses the blocker: ${JSON.stringify(points)}`).toBe(false);
  expect(
    points.some((p) => p.y < 180 || p.y > 380),
    `route detours: ${JSON.stringify(points)}`,
  ).toBe(true);
});

test("a clear path is not detoured", async ({ page }) => {
  await boot(page);
  await rect(page, 100, 200, 200, 300);
  await rect(page, 600, 200, 700, 300);
  await arrowAt(page, { x: 150, y: 250 }, { x: 650, y: 250 });
  const [arrow] = await arrows(page);
  const points = await route(page, arrow.id);
  expect(points.every((p) => Math.abs(p.y - 250) < 0.001)).toBe(true);
});

test("dragging an arrow segment bends the path; double-click removes the bend", async ({
  page,
}) => {
  await boot(page);
  await rect(page, 100, 200, 200, 300);
  await rect(page, 600, 200, 700, 300);
  await arrowAt(page, { x: 150, y: 250 }, { x: 650, y: 250 });
  const [arrow] = await arrows(page);
  expect(arrow, "arrow created").toBeDefined();

  // The drop selected the arrow; grab the insert handle at the route
  // midpoint and drag it down (a separate click first would register as a
  // double-click).
  await page.keyboard.press("v");
  await drag(page, 400, 250, 400, 360);
  let saved = (await arrows(page))[0];
  expect(saved.waypoints).toEqual([
    { x: 232, y: 360 },
    { x: 568, y: 360 },
  ]);
  let points = await route(page, saved.id);
  for (const waypoint of saved.waypoints) {
    expect(
      points.some((p) => p.x === waypoint.x && p.y === waypoint.y),
      `route passes through ${JSON.stringify(waypoint)}`,
    ).toBe(true);
  }

  // The bend survives a save/load round trip.
  const json = await page.evaluate(() => window.__rsEditor!.save_json());
  await page.evaluate(() => window.__rsEditor!.new_document());
  await page.evaluate((value) => window.__rsEditor!.load_json(value), json);
  saved = (await arrows(page))[0];
  expect(saved.waypoints).toEqual([
    { x: 232, y: 360 },
    { x: 568, y: 360 },
  ]);

  // Double-click a bend to remove it; the other stays.
  await page.mouse.dblclick(232, 360);
  saved = (await arrows(page))[0];
  expect(saved.waypoints).toEqual([{ x: 568, y: 360 }]);
});
