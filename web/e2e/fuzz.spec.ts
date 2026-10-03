import { expect, test, type Page } from "@playwright/test";

// Seeded random fuzzing. Every N ops we assert document invariants; on failure
// we dump the seed + recent op log so the crash sequence is replayable.

const SEEDS = [7, 1337, 424242, 90210];
const OPS_PER_SEED = 250;

function mulberry32(a: number) {
  return () => {
    a |= 0;
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

type Ctx = {
  page: Page;
  rng: () => number;
  log: string[];
  push: (s: string) => void;
};

const W = 1280;
const H = 800;

const rx = (ctx: Ctx, lo = 100, hi = W - 100) => Math.round(lo + ctx.rng() * (hi - lo));
const ry = (ctx: Ctx, lo = 120, hi = H - 100) => Math.round(lo + ctx.rng() * (hi - lo));
const pick = <T,>(ctx: Ctx, xs: T[]): T => xs[Math.floor(ctx.rng() * xs.length)];

async function checkInvariants(ctx: Ctx, tag: string): Promise<string[]> {
  const { page } = ctx;
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
          out.push(`non-finite rect on ${el.id}: ${JSON.stringify(r)}`);
        if (r && (r.w < 0 || r.h < 0)) out.push(`negative size on ${el.id}: ${JSON.stringify(r)}`);
        if (el.type === "arrow") {
          for (const end of ["start", "end"]) {
            const b = el[end]?.binding?.elementId;
            if (b !== undefined && b !== null && !ids.has(b))
              out.push(`arrow ${el.id} ${end} binds to missing ${b}`);
          }
        }
        for (const field of ["container_id", "bound_text_id", "caption_id"]) {
          const ref = el[field];
          if (ref && !ids.has(ref)) out.push(`${el.id} ${field} -> missing ${ref}`);
        }
      }
      const s = Array.from(e.scroll());
      const z = e.zoom();
      if (!Number.isFinite(s[0]) || !Number.isFinite(s[1])) out.push(`non-finite scroll ${s}`);
      if (!Number.isFinite(z) || z <= 0) out.push(`bad zoom ${z}`);
      // serialization must round-trip losslessly right now
      const again = JSON.parse(e.save_json());
      if (JSON.stringify(d) !== JSON.stringify(again)) out.push("save_json not deterministic");
    } catch (err) {
      out.push(`invariant check threw: ${String(err).slice(0, 200)}`);
    }
    return out;
  });
  return issues.map((i) => `${tag}: ${i}`);
}

async function op(ctx: Ctx): Promise<void> {
  const { page, rng } = ctx;
  const roll = rng();
  const toolKeys = ["v", "r", "d", "o", "p", "l", "a", "t", "f", "e", "h", "n", "k"];
  if (roll < 0.30) {
    // drag with current tool
    const [x1, y1, x2, y2] = [rx(ctx), ry(ctx), rx(ctx), ry(ctx)];
    ctx.push(`drag ${x1},${y1} -> ${x2},${y2}`);
    await page.mouse.move(x1, y1);
    await page.mouse.down();
    await page.mouse.move(x2, y2, { steps: 1 + Math.floor(rng() * 6) });
    await page.mouse.up();
  } else if (roll < 0.42) {
    const k = pick(ctx, toolKeys);
    ctx.push(`tool ${k}`);
    await page.keyboard.press(k);
  } else if (roll < 0.52) {
    const [x, y] = [rx(ctx), ry(ctx)];
    ctx.push(`click ${x},${y}`);
    await page.mouse.click(x, y);
  } else if (roll < 0.58) {
    const [x, y] = [rx(ctx), ry(ctx)];
    ctx.push(`dblclick ${x},${y}`);
    await page.mouse.dblclick(x, y);
    // if a text editor opened, maybe type something then escape
    if (await page.locator("#text-editor").isVisible().catch(() => false)) {
      const text = pick(ctx, ["hello", "a b\nc", "", "x".repeat(50)]);
      ctx.push(`type "${text.slice(0, 20)}"`);
      if (text) await page.keyboard.insertText(text);
      await page.keyboard.press("Escape");
    }
  } else if (roll < 0.68) {
    const k = pick(ctx, [
      "ControlOrMeta+z",
      "ControlOrMeta+Shift+z",
      "ControlOrMeta+a",
      "ControlOrMeta+d",
      "Delete",
      "Backspace",
      "Escape",
      "ArrowLeft",
      "ArrowRight",
      "ArrowUp",
      "ArrowDown",
      "Shift+1",
      "Shift+0",
    ]);
    ctx.push(`key ${k}`);
    await page.keyboard.press(k);
  } else if (roll < 0.74) {
    const dy = Math.round((rng() - 0.5) * 600);
    ctx.push(`wheel ${dy}${rng() < 0.3 ? " ctrl" : ""}`);
    await page.mouse.move(rx(ctx), ry(ctx));
    if (rng() < 0.3) {
      await page.keyboard.down("Control");
      await page.mouse.wheel(0, dy);
      await page.keyboard.up("Control");
    } else {
      await page.mouse.wheel(0, dy);
    }
  } else if (roll < 0.80) {
    // shift-click modifier select
    const [x, y] = [rx(ctx), ry(ctx)];
    ctx.push(`shift-click ${x},${y}`);
    await page.keyboard.down("Shift");
    await page.mouse.click(x, y);
    await page.keyboard.up("Shift");
  } else if (roll < 0.86) {
    // occasionally hostile API calls — the editor must shrug them off
    const call = pick(ctx, [
      `window.__rsEditor.update_style('{"opacity":${(rng() * 4 - 2).toFixed(2)}}')`,
      `window.__rsEditor.update_style('${JSON.stringify({ strokeWidth: Math.round(rng() * 20 - 5) })}')`,
      `window.__rsEditor.update_style('garbage')`,
      `window.__rsEditor.insert_glyph('${pick(ctx, ["compute_engine", "cloud_sql", "nope", "queue"])}')`,
      `window.__rsEditor.zoom_at(${rx(ctx)}, ${ry(ctx)}, ${(rng() * 4).toFixed(2)})`,
      `window.__rsEditor.undo()`,
      `window.__rsEditor.redo()`,
      `window.__rsEditor.wheel(0,0,${Math.round(rng() * 800 - 400)},${Math.round(rng() * 800 - 400)},${rng() < 0.5})`,
      `window.__rsEditor.commit_text('no-such-id','x')`,
      `window.__rsEditor.set_tool('${pick(ctx, ["select", "rectangle", "eraser", "bogus-tool", "arrow"])}')`,
    ]);
    ctx.push(`eval ${call}`);
    await page.evaluate((c) => {
      try {
        window.eval(c);
      } catch {}
    }, call);
  } else if (roll < 0.92) {
    // mid-drag key events: Escape, tool switch, undo
    const [x1, y1, x2, y2] = [rx(ctx), ry(ctx), rx(ctx), ry(ctx)];
    const k = pick(ctx, ["Escape", "r", "v", "ControlOrMeta+z", "Delete", " "]);
    ctx.push(`drag ${x1},${y1} +key ${k} -> ${x2},${y2}`);
    await page.mouse.move(x1, y1);
    await page.mouse.down();
    await page.mouse.move((x1 + x2) / 2, (y1 + y2) / 2, { steps: 2 });
    if (k === " ") {
      await page.keyboard.down(" ");
      await page.mouse.move(x2, y2, { steps: 2 });
      await page.keyboard.up(" ");
    } else {
      await page.keyboard.press(k);
      await page.mouse.move(x2, y2, { steps: 2 });
    }
    await page.mouse.up();
  } else {
    // save/load round-trip mid-stream — brutal for bound refs
    ctx.push("save->load round-trip");
    await page.evaluate(() => {
      try {
        window.__rsEditor!.load_json(window.__rsEditor!.save_json());
      } catch {}
    });
  }
}

for (const seed of SEEDS) {
  test(`fuzz seed ${seed}: ${OPS_PER_SEED} random ops`, async ({ page }) => {
    test.setTimeout(240_000);
    const pageErrors: string[] = [];
    const consoleErrors: string[] = [];
    page.on("pageerror", (e) => pageErrors.push(String(e)));
    page.on("console", (m) => {
      if (m.type() === "error" && !/favicon|404/.test(m.text())) consoleErrors.push(m.text());
    });
    await page.goto("/");
    await page.waitForFunction(() => window.__rsReady === true);
    await page.evaluate(() => window.__rsEditor!.new_document());

    const ctx: Ctx = {
      page,
      rng: mulberry32(seed),
      log: [],
      push(s) {
        this.log.push(s);
        if (this.log.length > 60) this.log.shift();
      },
    };

    const failures: string[] = [];
    for (let i = 0; i < OPS_PER_SEED; i++) {
      try {
        await op(ctx);
      } catch (e) {
        failures.push(`op ${i} threw: ${String(e).slice(0, 160)}`);
      }
      if (i % 10 === 9) {
        const issues = await checkInvariants(ctx, `op ${i}`);
        failures.push(...issues);
      }
      // a text editor stuck open would swallow later keys — close it
      if (await page.locator("#text-editor").isVisible().catch(() => false)) {
        await page.keyboard.press("Escape");
      }
      if (failures.length > 6) break;
    }
    const tail = await checkInvariants(ctx, "final");
    failures.push(...tail);
    failures.push(...pageErrors.map((e) => `pageerror: ${e}`));
    failures.push(...consoleErrors.map((e) => `console: ${e.slice(0, 200)}`));

    if (failures.length) {
      console.log(`\n=== seed ${seed} FAILURES ===`);
      for (const f of failures.slice(0, 12)) console.log("  " + f);
      console.log("=== recent ops ===");
      for (const l of ctx.log) console.log("  " + l);
    }
    expect(failures, `seed ${seed}: ${failures.length} invariant violations`).toEqual([]);
  });
}
