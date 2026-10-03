import { expect, test, type Page } from "@playwright/test";

// Torture suite: dense bound-arrow graphs, group moves, resize storms and
// nested-frame churn — composite scenarios the adversarial suite doesn't
// reach. Where semantics are ambiguous we log and assert invariants; where
// behavior is spec'd we assert it exactly.

const count = (page: Page): Promise<number> =>
  page.evaluate(() => window.__rsEditor!.element_count());

const bounds = (page: Page): Promise<number[] | undefined> =>
  page.evaluate(() => {
    const b = window.__rsEditor!.selection_bounds();
    return b ? Array.from(b) : undefined;
  });

const doc = (page: Page) => page.evaluate(() => JSON.parse(window.__rsEditor!.save_json()));

const elements = (page: Page) =>
  doc(page).then((d) => Object.values(d.elements) as Record<string, any>[]);

const liveElements = (page: Page) =>
  doc(page).then((d) => Object.values(d.elements as Record<string, any>).filter((e: any) => !e.isDeleted));

const byType = (page: Page, t: string) =>
  elements(page).then((es) => es.filter((e) => e.type === t && !e.isDeleted));

const liveByType = (page: Page, t: string) =>
  liveElements(page).then((es) => es.filter((e) => e.type === t));

const newDoc = (page: Page) => page.evaluate(() => window.__rsEditor!.new_document());

async function boot(page: Page): Promise<void> {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await newDoc(page);
}

async function drag(page: Page, x1: number, y1: number, x2: number, y2: number, steps = 6): Promise<void> {
  await page.mouse.move(x1, y1);
  await page.mouse.down();
  await page.mouse.move(x2, y2, { steps });
  await page.mouse.up();
}

async function shape(page: Page, key: string, x1: number, y1: number, x2: number, y2: number): Promise<void> {
  await page.keyboard.press(key);
  await drag(page, x1, y1, x2, y2);
}

async function rect(page: Page, x1: number, y1: number, x2: number, y2: number): Promise<void> {
  await shape(page, "r", x1, y1, x2, y2);
}

// World -> screen through the camera (needed once the fuzz pans/zooms).
const w2s = (page: Page, wx: number, wy: number) =>
  page.evaluate(([x, y]) => {
    const e = window.__rsEditor!;
    const s = e.scroll();
    const z = e.zoom();
    return [(x - s[0]) * z, (y - s[1]) * z];
  }, [wx, wy]);

async function clickWorld(page: Page, wx: number, wy: number): Promise<void> {
  const [sx, sy] = await w2s(page, wx, wy);
  await page.mouse.click(sx, sy);
}

async function dragWorld(page: Page, wx1: number, wy1: number, wx2: number, wy2: number, steps = 6): Promise<void> {
  const [sx1, sy1] = await w2s(page, wx1, wy1);
  const [sx2, sy2] = await w2s(page, wx2, wy2);
  await drag(page, sx1, sy1, sx2, sy2, steps);
}

const center = (el: any) => ({ x: el.rect.x + el.rect.w / 2, y: el.rect.y + el.rect.h / 2 });
const arrowMid = (el: any) => ({ x: (el.start.x + el.end.x) / 2, y: (el.start.y + el.end.y) / 2 });

/// Draw an arrow between the centers of two existing elements — both ends
/// land inside a connectable element, so the arrow binds at commit time.
async function bindArrow(page: Page, a: any, b: any): Promise<void> {
  const ca = center(a);
  const cb = center(b);
  const [sx1, sy1] = await w2s(page, ca.x, ca.y);
  const [sx2, sy2] = await w2s(page, cb.x, cb.y);
  await page.keyboard.press("a");
  await drag(page, sx1, sy1, sx2, sy2, 5);
}

/// Click along an arrow's stored geometry to select it. Elbow routes don't
/// pass through the endpoint midpoint, so probe a few points and confirm the
/// arrow — not a shape underneath — is what got selected.
async function selectArrow(page: Page, arrow: any): Promise<void> {
  const wantX = Math.min(arrow.start.x, arrow.end.x);
  const wantY = Math.min(arrow.start.y, arrow.end.y);
  for (const t of [0.5, 0.25, 0.75, 0.1, 0.9]) {
    const x = arrow.start.x + (arrow.end.x - arrow.start.x) * t;
    const y = arrow.start.y + (arrow.end.y - arrow.start.y) * t;
    await clickWorld(page, x, y);
    const b = await bounds(page);
    if (b && Math.abs(b[0] - wantX) < 3 && Math.abs(b[1] - wantY) < 3) return;
  }
  expect(false, "arrow not selectable").toBe(true);
}

/// With an arrow selected, drag one endpoint to a world point.
async function dragArrowEndpoint(page: Page, arrow: any, end: "start" | "end", wx: number, wy: number): Promise<void> {
  const p = arrow[end];
  await dragWorld(page, p.x, p.y, wx, wy, 6);
}

const HANDLES = ["nw", "n", "ne", "e", "se", "s", "sw", "w"] as const;
type HandleName = (typeof HANDLES)[number];

function handleWorld(b: number[], h: HandleName): { x: number; y: number } {
  const [x, y, w, hh] = b;
  const cx = x + w / 2;
  const cy = y + hh / 2;
  const map: Record<HandleName, [number, number]> = {
    nw: [x, y],
    n: [cx, y],
    ne: [x + w, y],
    e: [x + w, cy],
    se: [x + w, y + hh],
    s: [cx, y + hh],
    sw: [x, y + hh],
    w: [x, cy],
  };
  const [hx, hy] = map[h];
  return { x: hx, y: hy };
}

/// Drag a resize handle of the current selection by a screen delta.
async function resizeByHandle(page: Page, h: HandleName, dx: number, dy: number): Promise<void> {
  const b = await bounds(page);
  expect(b, "nothing selected to resize").toBeDefined();
  const p = handleWorld(b!, h);
  const [sx, sy] = await w2s(page, p.x, p.y);
  await drag(page, sx, sy, sx + dx, sy + dy, 5);
}

const finite = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);

function rectsFinite(es: Record<string, any>[]): string | undefined {
  for (const e of es) {
    const r = e.rect;
    if (r && !(finite(r.x) && finite(r.y) && finite(r.w) && finite(r.h)))
      return `${e.id}: ${JSON.stringify(r)}`;
    if (e.type === "arrow") {
      if (!finite(e.start?.x) || !finite(e.start?.y) || !finite(e.end?.x) || !finite(e.end?.y))
        return `${e.id}: bad endpoints`;
    }
  }
  return undefined;
}

const normalize = (d: any) => {
  const clone = JSON.parse(JSON.stringify(d));
  for (const e of Object.values(clone.elements as Record<string, any>)) {
    delete e.version;
    delete e.versionNonce;
    delete e.updated;
  }
  return clone;
};

/// The invariant battery shared with the fuzz suite, plus label_id and
/// frame_id referential checks.
async function invariants(page: Page, tag: string): Promise<string[]> {
  const issues = await page.evaluate(() => {
    const out: string[] = [];
    const e = window.__rsEditor!;
    try {
      const d = JSON.parse(e.save_json());
      const live = Object.values(d.elements as Record<string, any>).filter((x) => !x.isDeleted);
      if (e.element_count() !== live.length)
        out.push(`count ${e.element_count()} != live elements ${live.length}`);
      const ids = new Set(Object.keys(d.elements));
      for (const el of live as any[]) {
        const r = el.rect;
        if (r && !(Number.isFinite(r.x) && Number.isFinite(r.y) && Number.isFinite(r.w) && Number.isFinite(r.h)))
          out.push(`non-finite rect on ${el.id}`);
        if (r && (r.w < 0 || r.h < 0)) out.push(`negative size on ${el.id}`);
        if (el.type === "arrow") {
          for (const end of ["start", "end"]) {
            const b = el[end]?.binding?.elementId;
            if (b !== undefined && b !== null && !ids.has(b))
              out.push(`arrow ${el.id} ${end} binds to missing ${b}`);
          }
        }
        for (const field of ["container_id", "bound_text_id", "caption_id", "label_id", "frameId"]) {
          const ref = el[field];
          if (ref && !ids.has(ref)) out.push(`${el.id} ${field} -> missing ${ref}`);
        }
      }
      const s = Array.from(e.scroll());
      const z = e.zoom();
      if (!Number.isFinite(s[0]) || !Number.isFinite(s[1])) out.push(`non-finite scroll ${s}`);
      if (!Number.isFinite(z) || z <= 0) out.push(`bad zoom ${z}`);
      const again = JSON.parse(e.save_json());
      if (JSON.stringify(d) !== JSON.stringify(again)) out.push("save_json not deterministic");
    } catch (err) {
      out.push(`invariant check threw: ${String(err).slice(0, 200)}`);
    }
    return out;
  });
  return issues.map((i) => `${tag}: ${i}`);
}

/* ------------------------------------------------------------------ */
/* a dense arrow mesh                                                  */
/* ------------------------------------------------------------------ */

/// Six rects in two columns + seven arrows bound across/between them.
async function meshBoard(page: Page): Promise<{ rects: any[]; arrows: any[] }> {
  await rect(page, 150, 160, 290, 240); // A
  await rect(page, 150, 320, 290, 400); // B
  await rect(page, 150, 480, 290, 560); // C
  await rect(page, 650, 160, 790, 240); // D
  await rect(page, 650, 320, 790, 400); // E
  await rect(page, 650, 480, 790, 560); // F
  const rects = await liveByType(page, "shape");
  const [a, b, c, d, e, f] = rects;
  await bindArrow(page, a, d);
  await bindArrow(page, a, e);
  await bindArrow(page, b, e);
  await bindArrow(page, c, f);
  await bindArrow(page, a, b);
  await bindArrow(page, d, e);
  await bindArrow(page, b, f);
  return { rects, arrows: await liveByType(page, "arrow") };
}

test("arrow mesh: group-drag moves every node and every bound endpoint", async ({ page }) => {
  await boot(page);
  const { rects, arrows } = await meshBoard(page);
  expect(arrows.length, "arrows did not bind").toBe(7);
  const before = new Map(rects.map((r) => [r.id, { x: r.rect.x, y: r.rect.y }]));
  const arrowBefore = new Map(arrows.map((a) => [a.id, a]));

  // Select everything and drag a gap point inside the union bounds.
  await page.keyboard.press("v");
  await page.keyboard.press("ControlOrMeta+a");
  await drag(page, 470, 400, 570, 460, 6);

  const rectsAfter = await liveByType(page, "shape");
  for (const r of rectsAfter) {
    const b0 = before.get(r.id)!;
    expect(Math.abs(r.rect.x - b0.x - 100), `${r.id} x`).toBeLessThan(3);
    expect(Math.abs(r.rect.y - b0.y - 60), `${r.id} y`).toBeLessThan(3);
  }
  // Bound endpoints ride their own shape's delta; free endpoints ride the drag.
  const arrowsAfter = await liveByType(page, "arrow");
  for (const a of arrowsAfter) {
    const a0 = arrowBefore.get(a.id)!;
    expect(Math.abs(a.start.x - a0.start.x - 100), `${a.id} start.x`).toBeLessThan(3);
    expect(Math.abs(a.end.x - a0.end.x - 100), `${a.id} end.x`).toBeLessThan(3);
    expect(a.start.binding).toBeDefined();
    expect(a.end.binding).toBeDefined();
  }
  // Undo restores the whole mesh as one step.
  await page.keyboard.press("ControlOrMeta+z");
  const restored = await liveByType(page, "shape");
  for (const r of restored) {
    const b0 = before.get(r.id)!;
    expect(Math.abs(r.rect.x - b0.x)).toBeLessThan(0.5);
  }
  expect(await invariants(page, "group-drag"), "invariants").toEqual([]);
});

test("rebind chain: arrow endpoints can hop between three shapes", async ({ page }) => {
  await boot(page);
  await rect(page, 200, 200, 340, 280); // A
  await rect(page, 700, 200, 840, 280); // B
  await rect(page, 450, 480, 590, 560); // C
  const [a, b, c] = await liveByType(page, "shape");
  await bindArrow(page, a, b);
  let arrow = (await liveByType(page, "arrow"))[0];
  expect(arrow.start.binding.elementId).toBe(a.id);
  expect(arrow.end.binding.elementId).toBe(b.id);

  await page.keyboard.press("v");
  await page.mouse.click(1000, 700);
  await selectArrow(page, arrow);

  // Each hop lands on a shape the other end is NOT on — a self-loop parks
  // both stored endpoints at the same anchor, which makes the grab ambiguous.
  const hop = async (end: "start" | "end", target: any) => {
    arrow = (await liveByType(page, "arrow"))[0];
    await dragArrowEndpoint(page, arrow, end, center(target).x, center(target).y);
    arrow = (await liveByType(page, "arrow"))[0];
    expect(arrow[end].binding?.elementId, `${end} did not rebind`).toBe(target.id);
  };

  await hop("end", c);   // A -> C
  await hop("start", b); // B -> C
  await hop("end", a);   // B -> A
  await hop("start", c); // C -> A
  expect(await invariants(page, "rebind"), "invariants").toEqual([]);
});

test("resize storm: all 8 handles on a labeled, arrow-bound shape", async ({ page }) => {
  await boot(page);
  await rect(page, 300, 300, 460, 400);
  await rect(page, 800, 300, 960, 400);
  const shapes0 = await liveByType(page, "shape");
  const r0 = shapes0.find((s) => s.rect.x === 300)!; // the labeled one
  const r1 = shapes0.find((s) => s.rect.x === 800)!;
  // label the first shape
  await page.mouse.dblclick(380, 350);
  await page.keyboard.type("hub");
  await page.keyboard.press("Escape");
  await bindArrow(page, r0, r1);
  await bindArrow(page, r1, r0);

  const deltas: Array<[number, number]> = [
    [30, 20],
    [-45, -30],
    [60, 10],
    [-20, 50],
    [40, -60],
    [-70, 40],
    [25, 25],
    [-30, -45],
  ];
  for (let i = 0; i < HANDLES.length; i++) {
    await page.keyboard.press("v");
    await page.mouse.click(900, 650); // deselect
    const labeled = (await liveByType(page, "shape")).find((s) => s.id === r0.id)!;
    await clickWorld(page, center(labeled).x, center(labeled).y);
    await resizeByHandle(page, HANDLES[i], deltas[i][0], deltas[i][1]);
    const bad = rectsFinite(await elements(page));
    expect(bad, `resize ${HANDLES[i]}`).toBeUndefined();
    // the label must still live inside the shape, arrows still bound
    const texts = await liveByType(page, "text");
    expect(texts.length, "label vanished").toBe(1);
    const shape2 = (await liveByType(page, "shape")).find((s) => s.id === r0.id)!;
    const t = texts[0];
    expect(t.rect.x >= shape2.rect.x - 2 && t.rect.x + t.rect.w <= shape2.rect.x + shape2.rect.w + 2,
      `label escaped shape after ${HANDLES[i]}: ${JSON.stringify(t.rect)} vs ${JSON.stringify(shape2.rect)}`).toBe(true);
    const arrows = await liveByType(page, "arrow");
    for (const a of arrows) {
      const ids = new Set([r0.id, r1.id]);
      if (a.start.binding) expect(ids.has(a.start.binding.elementId), "start binding dead").toBe(true);
      if (a.end.binding) expect(ids.has(a.end.binding.elementId), "end binding dead").toBe(true);
    }
  }
  expect(await invariants(page, "resize-storm"), "invariants").toEqual([]);
});

test("flip resize past the opposite edge keeps bindings", async ({ page }) => {
  await boot(page);
  await rect(page, 300, 300, 460, 400);
  await rect(page, 800, 300, 960, 400);
  const [r0, r1] = await liveByType(page, "shape");
  await bindArrow(page, r0, r1);
  await page.keyboard.press("v");
  await page.mouse.click(1000, 650);
  await clickWorld(page, 380, 350);
  // drag the E handle left of the W edge: the shape flips.
  await resizeByHandle(page, "e", -220, 0);
  const flipped = (await liveByType(page, "shape"))[0];
  expect(flipped.rect.w).toBeGreaterThan(0);
  const arrow = (await liveByType(page, "arrow"))[0];
  expect(arrow.start.binding?.elementId, "binding lost on flip").toBe(r0.id);
  expect(await invariants(page, "flip"), "invariants").toEqual([]);
});

test("marquee + Cmd+D duplicates a wired subgraph with fresh bindings", async ({ page }) => {
  await boot(page);
  await rect(page, 200, 200, 340, 280); // A
  await rect(page, 520, 200, 660, 280); // B
  await rect(page, 360, 430, 500, 510); // C
  const [a, b, c] = await liveByType(page, "shape");
  await bindArrow(page, a, b);
  await bindArrow(page, b, c);
  const oldArrowIds = new Set((await liveByType(page, "arrow")).map((x) => x.id));
  const oldRectIds = new Set([a.id, b.id, c.id]);

  await page.keyboard.press("v");
  await drag(page, 150, 150, 700, 560, 8); // marquee over the whole graph
  await page.keyboard.press("ControlOrMeta+d");
  expect(await count(page), "duplicate count").toBe(10);

  const arrows = await liveByType(page, "arrow");
  expect(arrows.length).toBe(4);
  const rects = await liveByType(page, "shape");
  const newRectIds = new Set(rects.map((r) => r.id).filter((id) => !oldRectIds.has(id)));
  for (const ar of arrows) {
    const s = ar.start.binding?.elementId;
    const e = ar.end.binding?.elementId;
    expect(s && e, "duplicated arrow lost a binding").toBeTruthy();
    if (oldArrowIds.has(ar.id)) {
      expect(oldRectIds.has(s!) && oldRectIds.has(e!), "original arrow rebound").toBe(true);
    } else {
      expect(newRectIds.has(s!) && newRectIds.has(e!),
        `duplicate arrow bound to old id ${s}->${e}`).toBe(true);
    }
  }
  expect(await invariants(page, "dup-remap"), "invariants").toEqual([]);
});

test("deleting a hub node leaves clean arrows after save/load", async ({ page }) => {
  await boot(page);
  await rect(page, 480, 300, 640, 400); // hub
  const spokeRects = [
    [120, 140],
    [760, 140],
    [120, 520],
    [760, 520],
    [480, 620],
  ];
  for (const [x, y] of spokeRects) await rect(page, x, y, x + 120, y + 80);
  const hub = (await liveByType(page, "shape")).find((r) => r.rect.x === 480 && r.rect.y === 300)!;
  const spokes = (await liveByType(page, "shape")).filter((r) => r.id !== hub.id);
  for (const s of spokes) await bindArrow(page, hub, s);

  await page.keyboard.press("v");
  await page.mouse.click(1000, 100);
  // Click inside the hub until the hub itself is selected — don't click once
  // and hope: probe interior points and verify the selection bounds match.
  let hubSelected = false;
  for (const [px, py] of [
    [560, 350],
    [560, 330],
    [600, 320],
    [520, 380],
    [560, 390],
  ]) {
    await clickWorld(page, px, py);
    const b = await bounds(page);
    if (b && Math.abs(b[0] - 480) < 2 && Math.abs(b[1] - 300) < 2 && Math.abs(b[2] - 160) < 2) {
      hubSelected = true;
      break;
    }
  }
  expect(hubSelected, "hub could not be selected").toBe(true);
  await page.keyboard.press("Delete");
  expect(await liveByType(page, "shape").then((x) => x.length)).toBe(5);
  expect(await liveByType(page, "arrow").then((x) => x.length)).toBe(5);

  const json = await page.evaluate(() => window.__rsEditor!.save_json());
  await newDoc(page);
  await page.evaluate((j) => window.__rsEditor!.load_json(j), json);
  const arrows = await liveByType(page, "arrow");
  for (const a of arrows) {
    const s = a.start.binding?.elementId;
    const e = a.end.binding?.elementId;
    expect(s, `dangling start binding survived load`).not.toBe(hub.id);
    expect(e, `dangling end binding survived load`).not.toBe(hub.id);
    if (s) expect(spokes.some((x) => x.id === s), "binding retargeted to non-spoke").toBe(true);
    if (e) expect(spokes.some((x) => x.id === e), "binding retargeted to non-spoke").toBe(true);
  }
  // remaining arrows still follow their spoke on move — endpoints re-anchor
  // dynamically, so assert direction of travel rather than an exact delta
  const b0 = (await liveByType(page, "arrow"))[0];
  const targetId = b0.end.binding?.elementId ?? b0.start.binding?.elementId;
  if (targetId) {
    const t = (await liveByType(page, "shape")).find((x) => x.id === targetId)!;
    const y0 = b0.end.binding?.elementId === targetId ? b0.end.y : b0.start.y;
    await dragWorld(page, center(t).x, center(t).y, center(t).x, center(t).y + 80, 5);
    const b1 = (await liveByType(page, "arrow")).find((x) => x.id === b0.id)!;
    const stillBound =
      b1.start.binding?.elementId === targetId || b1.end.binding?.elementId === targetId;
    expect(stillBound, "arrow lost its spoke binding").toBe(true);
    const y1 = b1.end.binding?.elementId === targetId ? b1.end.y : b1.start.y;
    expect(y1 - y0, "bound endpoint did not follow spoke").toBeGreaterThan(10);
  }
  expect(await invariants(page, "hub-delete"), "invariants").toEqual([]);
});

test("frame members and a frame-bound arrow move with the frame", async ({ page }) => {
  await boot(page);
  await shape(page, "f", 150, 150, 800, 560); // outer frame
  await rect(page, 250, 250, 390, 330); // inside
  await rect(page, 500, 250, 640, 330); // inside
  const [frame] = await liveByType(page, "frame");
  const [r0, r1] = await liveByType(page, "shape");
  await bindArrow(page, r0, r1); // inside the frame
  // an arrow that ends inside the frame but on no shape -> binds to the frame
  const [sx1, sy1] = await w2s(page, center(r0).x, center(r0).y);
  await page.keyboard.press("a");
  await page.mouse.move(sx1, sy1);
  await page.mouse.down();
  const [sx2, sy2] = await w2s(page, 700, 560); // on the bottom border -> binds the frame
  await page.mouse.move(sx2, sy2, { steps: 5 });
  await page.mouse.up();

  const frameArrow = (await liveByType(page, "arrow")).find(
    (a) => a.end.binding?.elementId === frame.id,
  );
  expect(frameArrow, "arrow did not bind to the frame border").toBeDefined();
  const arrows0 = await liveByType(page, "arrow");
  const ep0 = new Map(arrows0.map((a) => [a.id, { s: { ...a.start }, e: { ...a.end } }]));

  // move the frame by dragging its bottom border
  await page.keyboard.press("v");
  await page.mouse.click(1000, 650);
  await drag(page, 470, 560, 470, 640, 6); // bottom border, +80y

  const rects2 = await liveByType(page, "shape");
  for (const r of rects2) {
    const orig = [r0, r1].find((o) => o.id === r.id)!;
    expect(Math.abs(r.rect.y - orig.rect.y - 80), `${r.id} did not follow frame`).toBeLessThan(3);
  }
  const arrows2 = await liveByType(page, "arrow");
  for (const a of arrows2) {
    const p = ep0.get(a.id)!;
    // Bound endpoints re-anchor dynamically on the moved element — assert
    // they travelled with the frame, not the exact +80 anchor position.
    expect(a.start.y - p.s.y, `${a.id} start did not follow`).toBeGreaterThan(30);
    if (a.end.binding?.elementId === frame.id) {
      expect(a.end.y - p.e.y, "frame-bound endpoint did not follow").toBeGreaterThan(30);
    }
  }
  expect(await invariants(page, "frame-arrow"), "invariants").toEqual([]);
});

test("nested frame leaves and rejoins its parent on drag", async ({ page }) => {
  await boot(page);
  await shape(page, "f", 150, 150, 800, 560); // outer
  await shape(page, "f", 250, 250, 480, 430); // inner (strictly inside)
  await rect(page, 300, 300, 380, 360); // inside inner
  const frames0 = await liveByType(page, "frame");
  const outer = frames0.find((f) => f.rect.x === 150)!;
  const inner = frames0.find((f) => f.rect.x === 250)!;
  const shape0 = (await liveByType(page, "shape"))[0];
  expect(inner.frameId, "inner frame not adopted").toBe(outer.id);
  expect(shape0.frameId, "shape not adopted by inner").toBe(inner.id);

  // drag the inner frame fully outside the outer -> parentage clears
  await page.keyboard.press("v");
  await page.mouse.click(1000, 650);
  await drag(page, 250, 340, 1000, 340, 8); // inner frame left border -> far right
  let frames = await liveByType(page, "frame");
  let innerNow = frames.find((f) => f.id === inner.id)!;
  expect(innerNow.frameId, "inner still owned after leaving").toBeUndefined();
  // the shape is still inner's child — moved along with it
  const shape1 = (await liveByType(page, "shape"))[0];
  expect(shape1.frameId).toBe(inner.id);
  expect(shape1.rect.x).toBeGreaterThanOrEqual(innerNow.rect.x);

  // drag it back inside -> re-adopted
  await drag(page, innerNow.rect.x - 0, innerNow.rect.y + 40, 250, 340, 8); // left border back
  frames = await liveByType(page, "frame");
  innerNow = frames.find((f) => f.id === inner.id)!;
  expect(innerNow.frameId, "inner not re-adopted").toBe(outer.id);
  expect(await invariants(page, "reparent"), "invariants").toEqual([]);
});

test("arrow labels ride the connector and die with it", async ({ page }) => {
  await boot(page);
  await rect(page, 200, 250, 340, 330);
  await rect(page, 700, 250, 840, 330);
  const [a, b] = await liveByType(page, "shape");
  await bindArrow(page, a, b);
  const arrow = (await liveByType(page, "arrow"))[0];
  const mid = arrowMid(arrow);
  const [smx, smy] = await w2s(page, mid.x, mid.y);
  await page.mouse.dblclick(smx, smy);
  await page.keyboard.type("sync");
  await page.keyboard.press("Escape");
  const texts = await liveByType(page, "text");
  expect(texts.length, "no arrow label").toBe(1);
  expect(texts[0].container_id).toBe(arrow.id);
  const labelY0 = texts[0].rect.y;

  // move shape A down: the label should track the new midpoint
  await page.keyboard.press("v");
  await page.mouse.click(1000, 650);
  await dragWorld(page, center(a).x, center(a).y, center(a).x, center(a).y + 90, 6);
  const texts2 = await liveByType(page, "text");
  expect(Math.abs(texts2[0].rect.y - labelY0), "label did not follow arrow").toBeGreaterThan(10);

  // delete the arrow: its bound label must go too
  const arrow2 = (await liveByType(page, "arrow"))[0];
  await selectArrow(page, arrow2);
  await page.keyboard.press("Delete");
  expect(await liveByType(page, "arrow").then((x) => x.length)).toBe(0);
  expect(await liveByType(page, "text").then((x) => x.length), "label orphaned").toBe(0);
  expect(await invariants(page, "arrow-label"), "invariants").toEqual([]);
});

test("resize lands in world units at zoom 2.5", async ({ page }) => {
  await boot(page);
  await rect(page, 300, 300, 440, 380);
  await page.evaluate(() => window.__rsEditor!.zoom_at(500, 300, 2.5));
  await page.keyboard.press("v");
  await clickWorld(page, 370, 340);
  const b0 = await bounds(page);
  // drag SE handle 50 screen px = 20 world px at zoom 2.5
  await resizeByHandle(page, "se", 50, 50);
  const after = (await liveByType(page, "shape"))[0];
  expect(Math.abs(after.rect.w - b0![2] - 20), `w ${after.rect.w} != ${b0![2]}+20`).toBeLessThan(2);
  expect(Math.abs(after.rect.h - b0![3] - 20), `h ${after.rect.h} != ${b0![3]}+20`).toBeLessThan(2);
  await page.evaluate(() => window.__rsEditor!.reset_zoom());
  expect(await invariants(page, "zoom-resize"), "invariants").toEqual([]);
});

test("undo/redo walks a 20-element board to empty and back", async ({ page }) => {
  await boot(page);
  const { rects, arrows } = await meshBoard(page);
  await shape(page, "n", 900, 400, 1040, 500); // sticky
  const total = await count(page);
  expect(total).toBe(14);
  for (let i = 0; i < 20; i++) await page.keyboard.press("ControlOrMeta+z");
  expect(await count(page), "undo did not reach empty").toBe(0);
  for (let i = 0; i < 20; i++) await page.keyboard.press("ControlOrMeta+Shift+z");
  expect(await count(page), "redo did not restore").toBe(total);
  const ids = new Set((await liveElements(page)).map((e) => e.id));
  const before = new Set([...rects, ...arrows].map((e) => e.id));
  for (const id of before) expect(ids.has(id), `redo lost ${id}`).toBe(true);
  expect(await invariants(page, "undo-redo"), "invariants").toEqual([]);
});

test("save/load mid-scenario restores the mesh exactly", async ({ page }) => {
  await boot(page);
  await meshBoard(page);
  // Raw JSON for the reload — normalize() is comparison-only (it strips the
  // version fields load_json validates).
  const raw = await page.evaluate(() => window.__rsEditor!.save_json());
  const snapshot = normalize(JSON.parse(raw));
  // mutate, then restore from the snapshot
  await page.keyboard.press("v");
  await page.mouse.click(1000, 700);
  const r0 = (await liveByType(page, "shape"))[0];
  await dragWorld(page, center(r0).x, center(r0).y, center(r0).x + 60, center(r0).y + 40, 4);
  const victim = (await liveByType(page, "shape"))[2];
  await clickWorld(page, center(victim).x, center(victim).y);
  await page.keyboard.press("Delete");
  await page.evaluate((j) => window.__rsEditor!.load_json(j), raw);
  const restored = normalize(await doc(page));
  expect(restored, "load diverged from snapshot").toEqual(snapshot);
  // and bindings still function after the restore
  const a0 = (await liveByType(page, "shape"))[0];
  // A route between two other nodes can cross this rect's interior, so probe
  // interior points until the drag actually moves a0 (not an arrow on top).
  const probes = [0.5, 0.25, 0.75].flatMap((fx) =>
    [0.5, 0.25, 0.75].map((fy) => ({ x: a0.rect.x + a0.rect.w * fx, y: a0.rect.y + a0.rect.h * fy })),
  );
  let moved = false;
  for (const p of probes) {
    const before = (await liveByType(page, "shape")).find((s) => s.id === a0.id)!.rect;
    await dragWorld(page, p.x, p.y, p.x, p.y + 50, 4);
    const after = (await liveByType(page, "shape")).find((s) => s.id === a0.id)!.rect;
    if (Math.abs(after.y - (before.y + 50)) < 1) {
      moved = true;
      break;
    }
  }
  expect(moved, "could not drag the bound shape").toBe(true);
  const a0arrows = (await liveByType(page, "arrow")).filter(
    (ar) => ar.start.binding?.elementId === a0.id || ar.end.binding?.elementId === a0.id,
  );
  expect(a0arrows.length, "no arrows bound after restore").toBeGreaterThan(0);
  const changed = a0arrows.some((ar) => {
    const orig = Object.values(snapshot.elements as Record<string, any>).find((x) => x.id === ar.id) as any;
    return ar.start.y !== orig.start.y || ar.end.y !== orig.end.y;
  });
  expect(changed, "bound arrow did not track its moved node").toBe(true);
  expect(await invariants(page, "save-load"), "invariants").toEqual([]);
});

test("multi-selection resize scales the group", async ({ page }) => {
  await boot(page);
  await rect(page, 200, 200, 300, 280);
  await rect(page, 400, 200, 500, 280);
  await page.keyboard.press("v");
  await page.keyboard.press("ControlOrMeta+a");
  const b0 = await bounds(page);
  const origW = new Map((await liveByType(page, "shape")).map((r) => [r.id, r.rect.w]));
  await resizeByHandle(page, "se", 100, 60);
  const rects = await liveByType(page, "shape");
  // the union grew; each member resized proportionally (or at least sanely)
  const b1 = rects.reduce(
    (u, r) => [
      Math.min(u[0], r.rect.x),
      Math.min(u[1], r.rect.y),
      Math.max(u[2], r.rect.x + r.rect.w),
      Math.max(u[3], r.rect.y + r.rect.h),
    ],
    [Infinity, Infinity, -Infinity, -Infinity],
  );
  expect(rectsFinite(rects)).toBeUndefined();
  expect(b1[2], "group did not grow").toBeGreaterThan(b0![0] + b0![2] + 40);
  for (const r of rects) {
    expect(r.rect.w, `${r.id} did not scale`).toBeGreaterThan(origW.get(r.id)!);
  }
  expect(await invariants(page, "group-resize"), "invariants").toEqual([]);
});

test("sticky pulled inside a frame follows the frame", async ({ page }) => {
  await boot(page);
  await shape(page, "f", 150, 150, 800, 560);
  await shape(page, "k", 250, 250, 330, 330); // stack inside frame
  const stack = (await liveByType(page, "sticky_stack"))[0];
  expect(stack.frameId, "stack not adopted by frame").toBeDefined();
  // pull a note out of the stack, still inside the frame — the pull zone is
  // the stack's inset interior, so drag from its centre
  await page.keyboard.press("v");
  await page.mouse.click(1000, 650);
  await drag(page, 250, 250, 500, 450, 6);
  const stickies = (await liveByType(page, "shape")).filter((s) => s.shape === "sticky");
  expect(stickies.length, "no note pulled").toBe(1);
  expect(stickies[0].frameId, "pulled note not adopted").toBeDefined();
  // move the frame; both stack and note follow
  const stackX0 = stack.rect.x;
  const noteX0 = stickies[0].rect.x;
  await drag(page, 150, 350, 250, 350, 6); // frame left border +100x
  const stack2 = (await liveByType(page, "sticky_stack"))[0];
  const note2 = (await liveByType(page, "shape"))[0];
  expect(Math.abs(stack2.rect.x - stackX0 - 100)).toBeLessThan(3);
  expect(Math.abs(note2.rect.x - noteX0 - 100)).toBeLessThan(3);
  expect(await invariants(page, "sticky-frame"), "invariants").toEqual([]);
});

/* ------------------------------------------------------------------ */
/* element-aware fuzz: ops aim at real elements, not empty space       */
/* ------------------------------------------------------------------ */

function mulberry32(a: number) {
  return () => {
    a |= 0;
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

type FuzzCtx = { page: Page; rng: () => number; log: string[]; push: (s: string) => void };

const pickEl = <T,>(ctx: FuzzCtx, xs: T[]): T => xs[Math.floor(ctx.rng() * xs.length)];

async function snapshot(page: Page) {
  return page.evaluate(() => {
    const d = JSON.parse(window.__rsEditor!.save_json());
    const s = Array.from(window.__rsEditor!.scroll());
    const z = window.__rsEditor!.zoom();
    return {
      live: Object.values(d.elements as Record<string, any>).filter((x: any) => !x.isDeleted),
      scroll: s,
      zoom: z,
    };
  });
}

/// One targeted operation against the live document.
async function targetedOp(ctx: FuzzCtx): Promise<void> {
  const { page, rng } = ctx;
  const snap = await snapshot(page);
  const shapes = snap.live.filter((e: any) => ["shape", "glyph", "sticky_stack", "frame"].includes(e.type));
  const arrows = snap.live.filter((e: any) => e.type === "arrow");
  const connectable = snap.live.filter((e: any) => ["shape", "glyph", "text", "frame"].includes(e.type));
  const w2sl = (wx: number, wy: number): [number, number] => [
    (wx - snap.scroll[0]) * snap.zoom,
    (wy - snap.scroll[1]) * snap.zoom,
  ];
  const roll = rng();

  if (roll < 0.22 && connectable.length >= 2) {
    // bound arrow between two random elements
    const a = pickEl(ctx, connectable);
    const b = pickEl(ctx, connectable);
    const ca = center(a);
    const cb = center(b);
    const [x1, y1] = w2sl(ca.x, ca.y);
    const [x2, y2] = w2sl(cb.x, cb.y);
    ctx.push(`arrow ${a.id} -> ${b.id}`);
    await page.keyboard.press("a");
    await drag(page, x1, y1, x2, y2, 3);
  } else if (roll < 0.38 && snap.live.length > 0) {
    // move a random element by dragging its centre (arrows move via shaft)
    const el = pickEl(ctx, snap.live);
    const p = el.type === "arrow" ? arrowMid(el) : center(el);
    const [x1, y1] = w2sl(p.x, p.y);
    const [x2, y2] = [x1 + (rng() - 0.5) * 300, y1 + (rng() - 0.5) * 200];
    ctx.push(`move ${el.id} (${el.type})`);
    await page.keyboard.press("v");
    await drag(page, x1, y1, x2, y2, 4);
  } else if (roll < 0.52 && shapes.length > 0) {
    // resize a random element via a random handle
    const el = pickEl(ctx, shapes);
    const [x1, y1] = w2sl(center(el).x, center(el).y);
    ctx.push(`resize ${el.id}`);
    await page.keyboard.press("v");
    await page.mouse.click(x1, y1);
    const b = await bounds(page);
    if (!b) return;
    const h = pickEl(ctx, [...HANDLES]);
    const hw = handleWorld(b, h);
    const [hx, hy] = w2sl(hw.x, hw.y);
    const [x2, y2] = [hx + (rng() - 0.4) * 200, hy + (rng() - 0.4) * 150];
    await drag(page, hx, hy, x2, y2, 4);
  } else if (roll < 0.62 && arrows.length > 0 && connectable.length > 0) {
    // rebind a random arrow endpoint onto a random element
    const ar = pickEl(ctx, arrows);
    const target = pickEl(ctx, connectable);
    const end = rng() < 0.5 ? "start" : "end";
    ctx.push(`rebind ${ar.id} ${end} -> ${target.id}`);
    const m = arrowMid(ar);
    const [mx, my] = w2sl(m.x, m.y);
    await page.keyboard.press("v");
    await page.mouse.click(mx, my);
    if (!(await bounds(page))) return;
    const p = ar[end];
    const [px, py] = w2sl(p.x, p.y);
    const tc = center(target);
    const [tx, ty] = w2sl(tc.x, tc.y);
    await drag(page, px, py, tx, ty, 4);
  } else if (roll < 0.70 && snap.live.length > 1) {
    // group-drag the whole document
    ctx.push("group-drag all");
    await page.keyboard.press("v");
    await page.keyboard.press("ControlOrMeta+a");
    const b = await bounds(page);
    if (!b) return;
    const gx = b[0] + b[2] / 2;
    const gy = b[1] + b[3] / 2;
    const [x1, y1] = w2sl(gx, gy);
    await drag(page, x1, y1, x1 + (rng() - 0.5) * 200, y1 + (rng() - 0.5) * 150, 4);
  } else if (roll < 0.80 && snap.live.length > 0) {
    // delete a random element (click its centre, then Delete)
    const el = pickEl(ctx, snap.live);
    const p = el.type === "arrow" ? arrowMid(el) : center(el);
    const [x, y] = w2sl(p.x, p.y);
    ctx.push(`delete ${el.id} (${el.type})`);
    await page.keyboard.press("v");
    await page.mouse.click(x, y);
    await page.keyboard.press("Delete");
  } else if (roll < 0.88) {
    const k = pickEl(ctx, ["ControlOrMeta+z", "ControlOrMeta+Shift+z", "ControlOrMeta+d", "Escape"]);
    ctx.push(`key ${k}`);
    await page.keyboard.press(k);
  } else if (roll < 0.93) {
    // draw something with a random tool
    const tool = pickEl(ctx, ["r", "d", "o", "l", "n", "f"]);
    const [x1, y1, x2, y2] = [100 + rng() * 900, 120 + rng() * 560, 100 + rng() * 900, 120 + rng() * 560];
    ctx.push(`draw ${tool}`);
    await shape(page, tool, x1, y1, x2, y2);
  } else if (roll < 0.97) {
    ctx.push("save->load");
    await page.evaluate(() => {
      try {
        window.__rsEditor!.load_json(window.__rsEditor!.save_json());
      } catch {}
    });
  } else {
    // pan/zoom so element-aware ops exercise the camera transform too
    const dy = Math.round((rng() - 0.5) * 400);
    ctx.push(`wheel ${dy}`);
    await page.mouse.move(600 + rng() * 100, 400);
    await page.mouse.wheel(0, dy);
  }
}

for (const seed of [555, 777777]) {
  test(`torture fuzz seed ${seed}: 150 targeted ops`, async ({ page }) => {
    test.setTimeout(300_000);
    const pageErrors: string[] = [];
    const consoleErrors: string[] = [];
    page.on("pageerror", (e) => pageErrors.push(String(e)));
    page.on("console", (m) => {
      if (m.type() === "error" && !/favicon|404/.test(m.text())) consoleErrors.push(m.text());
    });
    await boot(page);
    const ctx: FuzzCtx = {
      page,
      rng: mulberry32(seed),
      log: [],
      push(s) {
        this.log.push(s);
        if (this.log.length > 60) this.log.shift();
      },
    };
    // seed a small graph so targeted ops have victims early
    await rect(page, 300, 250, 440, 330);
    await rect(page, 700, 250, 840, 330);
    await rect(page, 500, 480, 640, 560);

    const failures: string[] = [];
    for (let i = 0; i < 150; i++) {
      try {
        await targetedOp(ctx);
      } catch (e) {
        failures.push(`op ${i} threw: ${String(e).slice(0, 160)}`);
      }
      if (await page.locator("#text-editor").isVisible().catch(() => false)) {
        await page.keyboard.press("Escape");
      }
      if (i % 10 === 9) failures.push(...(await invariants(page, `op ${i}`)));
      if (failures.length > 6) break;
    }
    failures.push(...(await invariants(page, "final")));
    failures.push(...pageErrors.map((e) => `pageerror: ${e}`));
    failures.push(...consoleErrors.map((e) => `console: ${e.slice(0, 200)}`));

    if (failures.length) {
      console.log(`\n=== torture seed ${seed} FAILURES ===`);
      for (const f of failures.slice(0, 12)) console.log("  " + f);
      console.log("=== recent ops ===");
      for (const l of ctx.log) console.log("  " + l);
    }
    expect(failures, `seed ${seed}: ${failures.length} violations`).toEqual([]);
  });
}
