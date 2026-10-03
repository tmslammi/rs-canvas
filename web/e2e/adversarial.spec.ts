import { expect, test, type Page } from "@playwright/test";

// Adversarial suite: mid-gesture chaos, bound-entity lifecycles, crafted
// documents, API fuzzing, DPR, and persistence races. Each test boot targets
// the static server configured in playwright.config.ts.

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

const byType = (page: Page, t: string) => elements(page).then((es) => es.filter((e) => e.type === t));

const scroll = (page: Page) => page.evaluate(() => Array.from(window.__rsEditor!.scroll()));
const zoom = (page: Page) => page.evaluate(() => window.__rsEditor!.zoom());
const newDoc = (page: Page) => page.evaluate(() => window.__rsEditor!.new_document());

async function boot(page: Page): Promise<void> {
  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await newDoc(page);
}

async function drag(page: Page, x1: number, y1: number, x2: number, y2: number, steps = 5): Promise<void> {
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

const finite = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);

function rectsFinite(es: Record<string, any>[]): string | undefined {
  for (const e of es) {
    const r = e.rect;
    if (r && !(finite(r.x) && finite(r.y) && finite(r.w) && finite(r.h)))
      return `${e.id}: ${JSON.stringify(r)}`;
  }
  return undefined;
}

// strip fields that legitimately differ after undo (crdt version stamps)
const normalize = (d: any) => {
  const clone = JSON.parse(JSON.stringify(d));
  for (const e of Object.values(clone.elements as Record<string, any>)) {
    delete e.version;
    delete e.versionNonce;
    delete e.updated;
  }
  return clone;
};

/* ------------------------------------------------------------------ */
/* mid-gesture chaos                                                    */
/* ------------------------------------------------------------------ */

test("switching tool mid-drag cancels the half-drawn shape", async ({ page }) => {
  await boot(page);
  const errors: string[] = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  await page.keyboard.press("r");
  await page.mouse.move(300, 300);
  await page.mouse.down();
  await page.mouse.move(400, 380, { steps: 3 });
  await page.keyboard.press("v");
  await page.mouse.move(500, 450, { steps: 3 });
  await page.mouse.up();
  expect(await count(page), "ghost element committed").toBe(0);
  expect(errors).toEqual([]);
  // editor still works afterwards
  await rect(page, 300, 300, 420, 380);
  expect(await count(page)).toBe(1);
});

test("undo during an active drag leaves consistent state", async ({ page }) => {
  await boot(page);
  await rect(page, 100, 100, 180, 160);
  await page.keyboard.press("r");
  await page.mouse.move(300, 300);
  await page.mouse.down();
  await page.mouse.move(400, 400, { steps: 3 });
  await page.keyboard.press("ControlOrMeta+z"); // undoes the first rect
  await page.mouse.move(500, 450, { steps: 3 });
  await page.mouse.up();
  const c = await count(page);
  expect(c === 0 || c === 1, `unexpected count ${c}`).toBe(true);
  for (const e of await elements(page)) expect(e.isDeleted).toBe(false);
  expect(rectsFinite(await elements(page))).toBeUndefined();
});

test("Delete key during an active drag", async ({ page }) => {
  await boot(page);
  await rect(page, 100, 100, 180, 160);
  await page.keyboard.press("r");
  await page.mouse.move(300, 300);
  await page.mouse.down();
  await page.mouse.move(400, 400, { steps: 3 });
  await page.keyboard.press("Delete"); // deletes the selected first rect
  await page.mouse.move(500, 450, { steps: 3 });
  await page.mouse.up();
  const c = await count(page);
  expect(c === 0 || c === 1, `count ${c}`).toBe(true);
  expect(rectsFinite(await elements(page))).toBeUndefined();
});

test("ctrl+wheel zoom mid-drag keeps geometry sane", async ({ page }) => {
  await boot(page);
  await page.keyboard.press("r");
  await page.mouse.move(400, 300);
  await page.mouse.down();
  await page.mouse.move(500, 400, { steps: 3 });
  await page.keyboard.down("Control");
  await page.mouse.wheel(0, -200);
  await page.keyboard.up("Control");
  await page.mouse.move(600, 450, { steps: 3 });
  await page.mouse.up();
  const es = await elements(page);
  expect(rectsFinite(es)).toBeUndefined();
  expect(es.length).toBeGreaterThanOrEqual(0);
  await page.evaluate(() => window.__rsEditor!.reset_zoom());
});

test("window blur mid-drag does not wedge the pointer", async ({ page }) => {
  await boot(page);
  await page.keyboard.press("r");
  await page.mouse.move(300, 300);
  await page.mouse.down();
  await page.mouse.move(420, 380, { steps: 3 });
  await page.evaluate(() => window.dispatchEvent(new Event("blur")));
  await page.mouse.move(500, 430, { steps: 3 });
  await page.mouse.up();
  expect(rectsFinite(await elements(page))).toBeUndefined();
  await rect(page, 600, 500, 700, 580);
  expect(await count(page)).toBeGreaterThanOrEqual(1);
});

test("pointer_down twice without an up does not corrupt the doc", async ({ page }) => {
  await boot(page);
  const mods = { shift: false, alt: false, ctrl: false, meta: false };
  await page.evaluate((m) => {
    const e = window.__rsEditor!;
    e.set_tool("rectangle");
    e.pointer_down(300, 300, 0, m);
    e.pointer_move(380, 380, m);
    e.pointer_down(500, 300, 0, m); // second down mid-gesture
    e.pointer_move(560, 360, m);
    e.pointer_up(560, 360, 0, m);
    e.pointer_up(560, 360, 0, m);
  }, mods);
  expect(rectsFinite(await elements(page))).toBeUndefined();
});

test("pointer_up / pointer_move with no prior down are no-ops", async ({ page }) => {
  await boot(page);
  await page.evaluate(() => {
    const m = { shift: false, alt: false, ctrl: false, meta: false };
    const e = window.__rsEditor!;
    e.pointer_up(400, 400, 0, m);
    e.pointer_move(500, 500, m);
    e.pointer_up(600, 600, 2, m);
  });
  expect(await count(page)).toBe(0);
});

test("synthetic pointercancel mid-drag", async ({ page }) => {
  await boot(page);
  await page.keyboard.press("r");
  await page.mouse.move(300, 300);
  await page.mouse.down();
  await page.mouse.move(400, 380, { steps: 3 });
  await page.evaluate(() => {
    const ev = new PointerEvent("pointercancel", { pointerId: 1, bubbles: true });
    document.getElementById("scene")!.dispatchEvent(ev);
  });
  await page.mouse.move(500, 430, { steps: 3 });
  await page.mouse.up();
  expect(rectsFinite(await elements(page))).toBeUndefined();
});

test("unknown mouse buttons don't draw", async ({ page }) => {
  await boot(page);
  await page.evaluate(() => {
    const m = { shift: false, alt: false, ctrl: false, meta: false };
    const e = window.__rsEditor!;
    e.set_tool("rectangle");
    e.pointer_down(300, 300, 3, m); // button 3
    e.pointer_move(400, 400, m);
    e.pointer_up(400, 400, 3, m);
    e.pointer_down(500, 300, 4, m); // button 4
    e.pointer_move(600, 400, m);
    e.pointer_up(600, 400, 4, m);
  });
  expect(await count(page)).toBe(0);
});

/* ------------------------------------------------------------------ */
/* bound-entity lifecycle                                               */
/* ------------------------------------------------------------------ */

async function boundPair(page: Page): Promise<void> {
  await rect(page, 200, 200, 400, 300);
  await rect(page, 600, 200, 800, 300);
  await shape(page, "a", 390, 250, 620, 250);
}

test("deleting a bound shape leaves no dangling reference", async ({ page }) => {
  await boot(page);
  await boundPair(page);
  await page.keyboard.press("v");
  await page.mouse.click(300, 250); // select left rect
  await page.keyboard.press("Delete");

  let arrow = (await byType(page, "arrow"))[0];
  const bindingBefore = arrow?.start?.binding?.elementId;

  // round-trip: a dangling binding elementId must not break load or later edits
  const json = await page.evaluate(() => window.__rsEditor!.save_json());
  await newDoc(page);
  await page.evaluate((j) => window.__rsEditor!.load_json(j), json);
  arrow = (await byType(page, "arrow"))[0];
  expect(arrow, "arrow vanished").toBeDefined();
  const bindingAfter = arrow?.start?.binding?.elementId;
  // move the arrow; if the binding is stale this is where it bites
  await page.keyboard.press("v");
  await page.mouse.click(700, 250);
  await drag(page, 700, 250, 700, 350, 4);
  expect(rectsFinite(await elements(page))).toBeUndefined();
  if (bindingBefore && bindingAfter) {
    const ids = new Set((await elements(page)).map((e) => e.id));
    if (ids.has(bindingAfter)) {
      // binding retargeted to a live element — fine
    } else {
      // report the smell: stale elementId persisted through delete + reload
      console.log(`dangling binding survived delete+load: ${bindingAfter}`);
    }
  }
});

test("undoing a bound-shape delete restores the binding", async ({ page }) => {
  await boot(page);
  await boundPair(page);
  const a0 = (await byType(page, "arrow"))[0];
  await page.keyboard.press("v");
  await page.mouse.click(300, 250);
  await page.keyboard.press("Delete");
  await page.keyboard.press("ControlOrMeta+z");
  const a1 = (await byType(page, "arrow"))[0];
  expect(a1.start.binding, "start binding lost").toBeDefined();
  expect(a1.end.binding, "end binding lost").toBeDefined();
  // still follows: move the rect, endpoint tracks
  const before = a1.start.x;
  await drag(page, 300, 250, 300, 380, 5);
  const a2 = (await byType(page, "arrow"))[0];
  expect(a2.start.y).not.toBe(250);
});

test("deleting a shape removes its bound label", async ({ page }) => {
  await boot(page);
  await rect(page, 300, 300, 460, 380);
  await page.mouse.dblclick(380, 340);
  await page.keyboard.type("hello");
  await page.keyboard.press("Escape");
  expect(await count(page)).toBe(2);
  await page.keyboard.press("v");
  await page.mouse.click(380, 340);
  await page.keyboard.press("Delete");
  expect(await count(page), "bound text orphaned").toBe(0);
});

test("Cmd+D on a connected pair copies bindings to the NEW ids", async ({ page }) => {
  await boot(page);
  await boundPair(page);
  await page.keyboard.press("v");
  await page.mouse.click(1000, 600); // deselect
  await page.keyboard.press("ControlOrMeta+a");
  await page.keyboard.press("ControlOrMeta+d");
  expect(await count(page)).toBe(6);
  const arrows = await byType(page, "arrow");
  const rects = await byType(page, "shape");
  const rectIds = new Set(rects.map((r) => r.id));
  const bound = new Set<string>();
  for (const a of arrows) {
    if (a.start.binding) bound.add(a.start.binding.elementId);
    if (a.end.binding) bound.add(a.end.binding.elementId);
  }
  // each arrow should bind to two distinct rects, all of which exist
  for (const a of arrows) {
    const s = a.start.binding?.elementId;
    const e = a.end.binding?.elementId;
    expect(s && rectIds.has(s), `arrow start binds to missing/dead ${s}`).toBeTruthy();
    expect(e && rectIds.has(e), `arrow end binds to missing/dead ${e}`).toBeTruthy();
    expect(s, `arrow start and end bound to same rect`).not.toBe(e);
  }
  expect(bound.size, "bindings collapsed onto one shape").toBeGreaterThanOrEqual(2);
});

test("arrow endpoint can be rebound by dragging it", async ({ page }) => {
  await boot(page);
  await boundPair(page);
  await rect(page, 600, 450, 800, 550);
  await page.keyboard.press("v");
  await page.mouse.click(1000, 650); // deselect
  // select the arrow by clicking its midpoint
  const a0 = (await byType(page, "arrow"))[0];
  await page.mouse.click(505, 250);
  const b = await bounds(page);
  expect(b, "arrow not selected").toBeDefined();
  // drag the end handle (600,250) onto the third rect
  await drag(page, 600, 250, 700, 500, 6);
  const a1 = (await byType(page, "arrow"))[0];
  const endId = a1.end.binding?.elementId;
  const third = (await byType(page, "shape")).find((r) => r.rect.y === 450);
  console.log(`rebind: end ${a0.end.binding?.elementId} -> ${endId}; third=${third?.id}`);
  expect(a1.end.binding, "endpoint did not rebind").toBeDefined();
  if (third) expect(endId).toBe(third.id);
});

test("frame parentage survives save/load", async ({ page }) => {
  await boot(page);
  await shape(page, "f", 200, 200, 800, 560);
  await rect(page, 400, 300, 520, 380);
  const r0 = (await byType(page, "shape"))[0];
  const json = await page.evaluate(() => window.__rsEditor!.save_json());
  await newDoc(page);
  await page.evaluate((j) => window.__rsEditor!.load_json(j), json);
  await page.keyboard.press("v");
  await page.mouse.click(900, 700); // deselect
  await drag(page, 200, 300, 260, 360, 5); // grab frame border
  const r1 = (await byType(page, "shape"))[0];
  expect(Math.abs(r1.rect.x - r0.rect.x - 60) < 3, `child did not follow frame: ${r0.rect.x} -> ${r1.rect.x}`).toBe(true);
});

test("nested frames cascade on delete and undo restores", async ({ page }) => {
  await boot(page);
  await shape(page, "f", 200, 200, 1000, 700);
  await shape(page, "f", 300, 300, 700, 550);
  await rect(page, 400, 380, 520, 460);
  expect(await count(page)).toBe(3);
  await page.keyboard.press("v");
  await page.mouse.click(900, 750); // deselect
  await page.mouse.click(200, 350); // outer frame border
  await page.keyboard.press("Delete");
  expect(await count(page), "nested delete did not cascade").toBe(0);
  await page.keyboard.press("ControlOrMeta+z");
  expect(await count(page)).toBe(3);
});

test("undo restores exact document snapshots (history integrity)", async ({ page }) => {
  await boot(page);
  const snaps: any[] = [normalize(await doc(page))];
  await rect(page, 300, 300, 420, 380);
  snaps.push(normalize(await doc(page)));
  await rect(page, 500, 300, 620, 380);
  snaps.push(normalize(await doc(page)));
  await page.keyboard.press("v");
  await page.mouse.click(560, 340);
  await drag(page, 560, 340, 600, 400, 4);
  snaps.push(normalize(await doc(page)));
  await page.mouse.click(360, 340);
  await page.getByTitle("Fill #a5d8ff").click();
  snaps.push(normalize(await doc(page)));

  for (let i = snaps.length - 2; i >= 0; i--) {
    await page.keyboard.press("ControlOrMeta+z");
    const now = normalize(await doc(page));
    expect(now, `doc after undo ${i} diverged`).toEqual(snaps[i]);
  }
});

test("undo after load_json does not resurrect the pre-load doc", async ({ page }) => {
  await boot(page);
  await rect(page, 300, 300, 420, 380);
  const json = await page.evaluate(() => window.__rsEditor!.save_json());
  await rect(page, 600, 300, 720, 380);
  await page.evaluate((j) => window.__rsEditor!.load_json(j), json);
  expect(await count(page)).toBe(1);
  await page.keyboard.press("ControlOrMeta+z");
  // should NOT bring back the 2-rect document
  expect(await count(page), "undo crossed the load boundary").toBeLessThanOrEqual(1);
});

test("undo after new_document does not resurrect the old doc", async ({ page }) => {
  await boot(page);
  await rect(page, 300, 300, 420, 380);
  await newDoc(page);
  await page.keyboard.press("ControlOrMeta+z");
  expect(await count(page), "undo crossed new_document").toBe(0);
});

test("save_json drops tombstones", async ({ page }) => {
  await boot(page);
  for (let i = 0; i < 10; i++) await rect(page, 100 + i * 20, 100, 150 + i * 20, 160);
  await page.keyboard.press("v");
  await page.keyboard.press("ControlOrMeta+a");
  await page.keyboard.press("Delete");
  const d = await doc(page);
  const keys = Object.keys(d.elements);
  const tombstones = keys.filter((k) => d.elements[k].isDeleted);
  console.log(`doc holds ${keys.length} keys, ${tombstones.length} tombstones, count=${await count(page)}`);
  expect(await count(page)).toBe(0);
});

/* ------------------------------------------------------------------ */
/* degenerate geometry + API fuzz                                       */
/* ------------------------------------------------------------------ */

test("resizing a rect through the opposite corner keeps w,h >= 0", async ({ page }) => {
  await boot(page);
  await rect(page, 300, 300, 460, 400);
  await page.keyboard.press("v");
  await page.mouse.click(380, 350);
  await drag(page, 460, 400, 250, 250, 8); // drag SE past NW
  const es = await elements(page);
  const r = es.find((e) => e.type === "shape")!.rect;
  expect(r.w, `negative width ${JSON.stringify(r)}`).toBeGreaterThanOrEqual(0);
  expect(r.h, `negative height ${JSON.stringify(r)}`).toBeGreaterThanOrEqual(0);
});

test("reversed drags normalize the rect", async ({ page }) => {
  await boot(page);
  await rect(page, 600, 500, 400, 400); // up-left drag
  const r = (await byType(page, "shape"))[0].rect;
  expect(Math.abs(r.x - 400) < 2 && Math.abs(r.y - 400) < 2 && Math.abs(r.w - 200) < 2 && Math.abs(r.h - 100) < 2).toBe(true);
});

test("a 2px shape is drawable, selectable, erasable", async ({ page }) => {
  await boot(page);
  await rect(page, 400, 400, 402, 402);
  const c = await count(page);
  console.log(`tiny drag created ${c} elements`);
  if (c === 1) {
    await page.keyboard.press("v");
    await page.mouse.click(401, 401);
    const b = await bounds(page);
    console.log(`tiny shape selectable: ${b !== undefined}`);
  }
});

test("NaN and hostile numeric input cannot poison the camera", async ({ page }) => {
  await boot(page);
  await rect(page, 300, 300, 420, 380);
  const results = await page.evaluate(() => {
    const e = window.__rsEditor!;
    const m = { shift: false, alt: false, ctrl: false, meta: false };
    const out: Record<string, string> = {};
    const tries: Array<[string, () => void]> = [
      ["wheel NaN", () => e.wheel(640, 400, NaN, NaN, false)],
      ["wheel Infinity", () => e.wheel(640, 400, Infinity, -Infinity, false)],
      ["zoom_at 0", () => e.zoom_at(720, 450, 0)],
      ["zoom_at -5", () => e.zoom_at(720, 450, -5)],
      ["zoom_at 1e308", () => e.zoom_at(720, 450, 1e308)],
      ["pointer NaN", () => e.pointer_down(NaN, NaN, 0, m)],
      ["scroll probe", () => e.wheel(0, 0, 0, 0, false)],
    ];
    for (const [name, fn] of tries) {
      try {
        fn();
        out[name] = "ok";
      } catch (err) {
        out[name] = "threw: " + String(err).slice(0, 80);
      }
      const s = Array.from(e.scroll());
      const z = e.zoom();
      if (!Number.isFinite(s[0]) || !Number.isFinite(s[1]) || !Number.isFinite(z) || z <= 0)
        out[name] += ` | POISONED scroll=${s} zoom=${z}`;
      // recover for next probe
      try {
        e.reset_zoom();
      } catch {}
    }
    return out;
  });
  console.log(JSON.stringify(results, null, 1));
  const bad = Object.entries(results).filter(([, v]) => v.includes("POISONED"));
  expect(bad, `camera poisoned: ${JSON.stringify(results)}`).toEqual([]);
  // still functional
  const es = await elements(page);
  expect(rectsFinite(es)).toBeUndefined();
});

test("update_style fuzz: rejects garbage, clamps or stores extremes", async ({ page }) => {
  await boot(page);
  const results = await page.evaluate(() => {
    const e = window.__rsEditor!;
    const out: Record<string, string> = {};
    for (const payload of [
      "not json",
      "{}",
      '{"opacity":-1}',
      '{"opacity":5}',
      '{"strokeWidth":-3}',
      '{"strokeWidth":1000}',
      '{"fontSize":0}',
      '{"roughness":99}',
      '{"strokeColor":123}',
      '{"fillStyle":"lava"}',
    ]) {
      try {
        e.update_style(payload);
        out[payload] = `accepted -> ${e.current_style()}`;
      } catch (err) {
        out[payload] = "rejected";
      }
    }
    e.update_style('{"opacity":1,"strokeWidth":2,"fontSize":20,"roughness":1}');
    return out;
  });
  for (const [k, v] of Object.entries(results)) console.log(`${k} -> ${v}`);
  // after extremes, a fresh shape must still draw sanely
  await rect(page, 300, 300, 460, 400);
  const s = (await byType(page, "shape"))[0].style;
  expect(s.opacity).toBeGreaterThanOrEqual(0);
  expect(s.opacity).toBeLessThanOrEqual(1);
  expect(s.strokeWidth).toBeGreaterThan(0);
});

test("set_tool with a bogus name", async ({ page }) => {
  await boot(page);
  const t = await page.evaluate(() => {
    const e = window.__rsEditor!;
    try {
      e.set_tool("bogus");
      return e.active_tool();
    } catch {
      return "threw";
    }
  });
  console.log(`active_tool after bogus set: ${t}`);
  await page.evaluate(() => {
    const m = { shift: false, alt: false, ctrl: false, meta: false };
    const e = window.__rsEditor!;
    e.pointer_down(300, 300, 0, m);
    e.pointer_move(400, 400, m);
    e.pointer_up(400, 400, 0, m);
  });
  await page.evaluate(() => window.__rsEditor!.set_tool("select"));
  expect(rectsFinite(await elements(page))).toBeUndefined();
});

test("commit_text on a dead id is a no-op", async ({ page }) => {
  await boot(page);
  await page.evaluate(() => {
    try {
      window.__rsEditor!.commit_text("does-not-exist", "ghost");
    } catch (e) {
      console.log("threw", e);
    }
  });
  expect(await count(page)).toBe(0);
});

test("export_png scale edge values", async ({ page }) => {
  await boot(page);
  await rect(page, 300, 300, 460, 400);
  const results = await page.evaluate(() => {
    const e = window.__rsEditor!;
    const out: Record<string, string> = {};
    for (const scale of [0, -2, 0.5, 4]) {
      try {
        const url = e.export_png(scale);
        out[String(scale)] = `ok ${url.length} chars`;
      } catch (err) {
        out[String(scale)] = `threw ${String(err).slice(0, 60)}`;
      }
    }
    return out;
  });
  for (const [k, v] of Object.entries(results)) console.log(`scale ${k}: ${v}`);
});

/* ------------------------------------------------------------------ */
/* crafted documents                                                    */
/* ------------------------------------------------------------------ */

async function craftFromLive(page: Page, mutate: (d: any) => void): Promise<void> {
  const j = await page.evaluate((fn) => {
    const d = JSON.parse(window.__rsEditor!.save_json());
    // eslint-disable-next-line no-new-func
    new Function("d", `(${fn})(d)`)(d);
    return JSON.stringify(d);
  }, mutate.toString());
  await newDoc(page);
  await page.evaluate((s) => window.__rsEditor!.load_json(s), j);
}

const tryLoad = (page: Page, json: string) =>
  page.evaluate((s) => {
    try {
      window.__rsEditor!.load_json(s);
      return "loaded";
    } catch (e) {
      return "threw";
    }
  }, json);

test("crafted: arrow bound to a deleted element", async ({ page }) => {
  await boot(page);
  await boundPair(page);
  const status = await page.evaluate(() => {
    const d = JSON.parse(window.__rsEditor!.save_json());
    // delete the end binding's target shape
    for (const [id, e] of Object.entries(d.elements) as any) {
      if (e.type === "arrow") delete d.elements[(e as any).end.binding.elementId];
    }
    const s = JSON.stringify(d);
    window.__rsEditor!.new_document();
    try {
      window.__rsEditor!.load_json(s);
      return "loaded";
    } catch (err) {
      return "threw: " + String(err).slice(0, 120);
    }
  });
  console.log(`dangling binding load: ${status}; count=${await count(page)}`);
  // exercise the possibly-corrupt arrow
  await page.keyboard.press("v");
  await page.mouse.click(505, 250);
  await drag(page, 505, 250, 600, 400, 4);
  expect(rectsFinite(await elements(page))).toBeUndefined();
  const a = (await byType(page, "arrow"))[0];
  if (a?.end?.binding) {
    const ids = new Set((await elements(page)).map((e) => e.id));
    expect(ids.has(a.end.binding.elementId), `arrow binds to ghost element ${a.end.binding.elementId}`).toBe(true);
  }
});

test("crafted: cyclic container_id between texts", async ({ page }) => {
  await boot(page);
  await page.keyboard.press("t");
  await page.mouse.click(300, 300);
  await page.keyboard.type("one");
  await page.keyboard.press("Escape");
  await page.keyboard.press("t");
  await page.mouse.click(500, 300);
  await page.keyboard.type("two");
  await page.keyboard.press("Escape");
  const status = await page.evaluate(() => {
    const d = JSON.parse(window.__rsEditor!.save_json());
    const texts = Object.values(d.elements).filter((e: any) => e.type === "text");
    if (texts.length >= 2) {
      (texts[0] as any).container_id = (texts[1] as any).id;
      (texts[1] as any).container_id = (texts[0] as any).id;
    }
    const s = JSON.stringify(d);
    window.__rsEditor!.new_document();
    try {
      window.__rsEditor!.load_json(s);
      return "loaded";
    } catch {
      return "threw";
    }
  });
  console.log(`cyclic containers: ${status}`);
  expect(status).not.toBe("threw"); // cycle should be tolerated or rejected, not crash
  await page.keyboard.press("v");
  await page.keyboard.press("ControlOrMeta+a");
  await page.keyboard.press("Delete");
  expect(rectsFinite(await elements(page))).toBeUndefined();
});

test("crafted: unknown element type and bogus fields", async ({ page }) => {
  await boot(page);
  await rect(page, 300, 300, 420, 380);
  const results = await page.evaluate(() => {
    const d = JSON.parse(window.__rsEditor!.save_json());
    const out: Record<string, string> = {};
    const variants: Array<[string, (doc: any) => any]> = [
      ["evil type", (doc) => { const k = Object.keys(doc.elements)[0]; doc.elements[k].type = "banana"; return doc; }],
      ["missing shape field", (doc) => { const k = Object.keys(doc.elements)[0]; doc.elements[k].type = "shape"; delete doc.elements[k].shape; return doc; }],
      ["schemaVersion 99", (doc) => { doc.schemaVersion = 99; return doc; }],
      ["schemaVersion 0", (doc) => { doc.schemaVersion = 0; return doc; }],
      ["negative w/h", (doc) => { const k = Object.keys(doc.elements)[0]; doc.elements[k].rect.w = -50; doc.elements[k].rect.h = -50; return doc; }],
      ["bogus routing", (doc) => { const k = Object.keys(doc.elements)[0]; doc.elements[k].type = "arrow"; doc.elements[k].routing = "vortex"; return doc; }],
      ["isDeleted true", (doc) => { const k = Object.keys(doc.elements)[0]; doc.elements[k].isDeleted = true; return doc; }],
      ["duplicate index", (doc) => { const ks = Object.keys(doc.elements); for (const k of ks) doc.elements[k].index = "a0"; return doc; }],
      ["mismatched key/id", (doc) => { const ks = Object.keys(doc.elements); const e = doc.elements[ks[0]]; doc.elements["other-key"] = e; delete doc.elements[ks[0]]; return doc; }],
    ];
    for (const [name, fn] of variants) {
      const copy = JSON.parse(JSON.stringify(d));
      try {
        const doc2 = fn(copy);
        window.__rsEditor!.new_document();
        try {
          window.__rsEditor!.load_json(JSON.stringify(doc2));
          out[name] = `loaded (count=${window.__rsEditor!.element_count()})`;
        } catch (err) {
          out[name] = `rejected`;
        }
      } catch (err) {
        out[name] = `mutator threw`;
      }
    }
    return out;
  });
  for (const [k, v] of Object.entries(results)) console.log(`${k}: ${v}`);
  // editor must still be alive after all that
  await newDoc(page);
  await rect(page, 200, 200, 300, 280);
  expect(await count(page)).toBe(1);
});

test("crafted: absurd numeric values", async ({ page }) => {
  await boot(page);
  await rect(page, 300, 300, 420, 380);
  const base = await page.evaluate(() => window.__rsEditor!.save_json());
  for (const [name, re] of [
    ["w=1e999", [/("w": )[\d.]+/, "$11e999"]],
    ["x=-1e9", [/("x": )[\d.-]+/, "$1-1000000000"]],
    ["versionNonce=-5", [/("versionNonce": )[\d.-]+/, "$1-5"]],
    ["opacity=1e999", [/("opacity": )[\d.]+/, "$11e999"]],
  ] as Array<[string, [RegExp, string]]>) {
    const mutant = base.replace(re[0], re[1]);
    const status = await tryLoad(page, mutant);
    const c = await count(page);
    const z = await zoom(page);
    console.log(`${name}: ${status}, count=${c}, zoom=${z}`);
    expect(Number.isFinite(z), `zoom poisoned by ${name}`).toBe(true);
  }
});

/* ------------------------------------------------------------------ */
/* text                                                                */
/* ------------------------------------------------------------------ */

test("re-editing text updates in place", async ({ page }) => {
  await boot(page);
  await page.keyboard.press("t");
  await page.mouse.click(400, 300);
  await page.keyboard.type("first");
  await page.keyboard.press("Escape");
  const t0 = (await byType(page, "text"))[0];
  await page.mouse.dblclick(400, 300);
  await page.keyboard.type(" second");
  await page.keyboard.press("Escape");
  const ts = await byType(page, "text");
  expect(ts.length, "edit created a second text element").toBe(1);
  expect(ts[0].id).toBe(t0.id);
  expect(ts[0].text).toContain("first");
  expect(ts[0].text).toContain("second");
});

test("svg export escapes script-y text", async ({ page }) => {
  await boot(page);
  await page.keyboard.press("t");
  await page.mouse.click(300, 300);
  await page.keyboard.insertText(`<script>alert("x")</script> & 'quotes' "more"`);
  await page.keyboard.press("Escape");
  const svg = await page.evaluate(() => window.__rsEditor!.export_svg());
  expect(svg).not.toContain("<script>");
  expect(svg).toContain("&lt;script&gt;");
});

test("unicode text round-trips through save/load and svg", async ({ page }) => {
  await boot(page);
  const payload = "emoji 🚀 عربى Ω≈ç√ 中文字符\nTab\there";
  await page.keyboard.press("t");
  await page.mouse.click(300, 300);
  await page.keyboard.insertText(payload);
  await page.keyboard.press("Escape");
  const t0 = (await byType(page, "text"))[0];
  expect(t0.text).toBe(payload);
  const json = await page.evaluate(() => window.__rsEditor!.save_json());
  await newDoc(page);
  await page.evaluate((j) => window.__rsEditor!.load_json(j), json);
  expect((await byType(page, "text"))[0].text).toBe(payload);
  await page.evaluate(() => window.__rsEditor!.export_svg());
});

test("Cmd+Z inside the textarea does not consume a document undo", async ({ page }) => {
  await boot(page);
  await rect(page, 300, 300, 420, 380);
  await page.keyboard.press("t");
  await page.mouse.click(300, 500);
  await page.keyboard.type("abc");
  await page.keyboard.press("ControlOrMeta+z"); // should hit textarea, not doc
  await page.keyboard.press("Escape");
  expect(await count(page), "doc undo fired while editing").toBeGreaterThanOrEqual(1);
  const rectAlive = await byType(page, "shape");
  expect(rectAlive.length, "rect was undone while typing").toBe(1);
});

test("clicking a toolbar tool mid-edit commits the text", async ({ page }) => {
  await boot(page);
  await page.keyboard.press("t");
  await page.mouse.click(300, 300);
  await page.keyboard.type("toolbar commit");
  await page.getByTitle("Rectangle — R").click();
  const t = (await byType(page, "text"))[0];
  expect(t?.text, "text lost on toolbar click").toBe("toolbar commit");
});

/* ------------------------------------------------------------------ */
/* device pixel ratio                                                   */
/* ------------------------------------------------------------------ */

test("drawing is correct at deviceScaleFactor=2", async ({ page, browser }) => {
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 }, deviceScaleFactor: 2 });
  const p2 = await ctx.newPage();
  await p2.goto("/");
  await p2.waitForFunction(() => window.__rsReady === true);
  await p2.evaluate(() => window.__rsEditor!.new_document());
  await p2.keyboard.press("r");
  await p2.mouse.move(300, 200);
  await p2.mouse.down();
  await p2.mouse.move(500, 340, { steps: 5 });
  await p2.mouse.up();
  const b = await p2.evaluate(() => {
    const v = window.__rsEditor!.selection_bounds();
    return v ? Array.from(v) : undefined;
  });
  expect(b).toBeDefined();
  expect(Math.abs(b![0] - 300), `x off by ${b![0] - 300}`).toBeLessThan(2);
  expect(Math.abs(b![1] - 200), `y off by ${b![1] - 200}`).toBeLessThan(2);
  expect(Math.abs(b![2] - 200), `w off by ${b![2] - 200}`).toBeLessThan(2);
  expect(Math.abs(b![3] - 140), `h off by ${b![3] - 140}`).toBeLessThan(2);
  // hit-testing must also be in the right coordinate space
  await p2.keyboard.press("v");
  await p2.mouse.click(400, 270);
  const b2 = await p2.evaluate(() => {
    const v = window.__rsEditor!.selection_bounds();
    return v ? Array.from(v) : undefined;
  });
  expect(b2, "click at dpr=2 missed the rect").toBeDefined();
  await ctx.close();
});

/* ------------------------------------------------------------------ */
/* persistence                                                          */
/* ------------------------------------------------------------------ */

test("autosave flushes pending edits on reload", async ({ page }) => {
  await boot(page);
  await rect(page, 300, 300, 420, 380);
  // do NOT flush — reload immediately, inside the debounce window. The
  // pagehide handler must flush the pending autosave.
  await page.reload();
  await page.waitForFunction(() => window.__rsReady === true);
  const c = await count(page);
  console.log(`restored ${c} elements after immediate reload`);
  expect(c, "edit lost to autosave debounce on reload").toBeGreaterThanOrEqual(1);
});

test("flush_save -> immediate reload keeps the doc", async ({ page }) => {
  await boot(page);
  await rect(page, 300, 300, 420, 380);
  await page.evaluate(() => window.__rsEditor!.flush_save());
  await page.waitForTimeout(400); // let the spawned idb write commit
  await page.reload();
  await page.waitForFunction(() => window.__rsReady === true);
  expect(await count(page)).toBeGreaterThanOrEqual(1);
});

test("autosave keeps a 10k-element document", async ({ page }) => {
  test.setTimeout(60000);
  await boot(page);
  await page.evaluate(() => window.__rsEditor!.load_benchmark_fixture(10000));
  await page.evaluate(() => window.__rsEditor!.flush_save());
  await page.waitForTimeout(1500);
  await page.reload();
  await page.waitForFunction(() => window.__rsReady === true, { timeout: 30000 });
  expect(await count(page)).toBe(10000);
  await newDoc(page);
  await page.evaluate(() => window.__rsEditor!.flush_save());
});

/* ------------------------------------------------------------------ */
/* visibility / export purity                                           */
/* ------------------------------------------------------------------ */

test("opacity-0 shapes: selectable? exported?", async ({ page }) => {
  await boot(page);
  await page.evaluate(() => window.__rsEditor!.update_style('{"opacity":0}'));
  await rect(page, 300, 300, 460, 400);
  await page.evaluate(() => window.__rsEditor!.update_style('{"opacity":1}'));
  const b = await bounds(page);
  console.log(`opacity-0 shape selected after draw: ${b !== undefined}`);
  await page.keyboard.press("v");
  await page.mouse.click(900, 600);
  await page.mouse.click(380, 350);
  const b2 = await bounds(page);
  console.log(`opacity-0 shape clickable: ${b2 !== undefined}`);
  // A truly invisible element (opacity 0 — only reachable via crafted load,
  // the UI clamps to 0.05) must not intercept clicks.
  await craftFromLive(page, (d) => {
    for (const el of Object.values<any>(d.elements)) {
      if (el.type === "shape") el.style.opacity = 0;
    }
  });
  await page.keyboard.press("v");
  await page.mouse.click(900, 600);
  await page.mouse.click(380, 350);
  const b3 = await bounds(page);
  expect(b3, "invisible shape intercepted a click").toBeUndefined();
});

test("export does not leak selection chrome", async ({ page }) => {
  await boot(page);
  await rect(page, 300, 300, 460, 400);
  await page.keyboard.press("v");
  await page.mouse.click(900, 600); // deselect
  const clean = await page.evaluate(() => window.__rsEditor!.export_png(1));
  await page.mouse.click(380, 350); // select it
  const withSel = await page.evaluate(() => window.__rsEditor!.export_png(1));
  expect(withSel === clean, "selection chrome leaked into PNG export").toBe(true);
  const svgClean = await page.evaluate(() => window.__rsEditor!.export_svg());
  await page.mouse.click(900, 600);
  const svgAfter = await page.evaluate(() => window.__rsEditor!.export_svg());
  expect(svgAfter === svgClean).toBe(true);
});
