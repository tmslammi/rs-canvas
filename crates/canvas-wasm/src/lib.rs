//! canvas-wasm: the thin wasm-bindgen façade over canvas-core.
//!
//! Owns the two canvases and forwards input into the core Editor; every
//! input returns Effects that say what to repaint.

use canvas_core::{
    Affine, Document, DrawOp, Editor as CoreEditor, Effects, FillStyle, Modifiers, Point, Rect,
    Renderer, StrokeStyle, Tool,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement};

const BACKGROUND: &str = "#f8f7f4";
const ACCENT: &str = "#1971c2";

fn now_ms() -> u64 {
    js_sys::Date::now() as u64
}

/// A Canvas2D implementation of the core's renderer seam.
struct Canvas2dRenderer {
    ctx: CanvasRenderingContext2d,
}

impl Canvas2dRenderer {
    fn apply_stroke(&self, stroke: &StrokeStyle) {
        self.ctx.set_stroke_style_str(&stroke.color);
        self.ctx.set_line_width(stroke.width);
        self.ctx.set_global_alpha(stroke.opacity);
        let pattern = js_sys::Array::new();
        for value in stroke.dash.pattern() {
            pattern.push(&JsValue::from_f64(*value));
        }
        let _ = self.ctx.set_line_dash(pattern.as_ref());
    }

    fn draw_ops(&mut self, ops: &[DrawOp]) {
        for op in ops {
            match op {
                DrawOp::Path(path) => self.draw_path(path),
                DrawOp::LineBatch { segments, stroke } => self.draw_line_batch(segments, stroke),
            }
        }
    }
}

impl Renderer for Canvas2dRenderer {
    fn begin_frame(&mut self) {
        self.ctx.save();
    }

    fn end_frame(&mut self) {
        self.ctx.restore();
    }

    fn set_transform(&mut self, transform: Affine) {
        let _ = self.ctx.set_transform(
            transform.a,
            transform.b,
            transform.c,
            transform.d,
            transform.e,
            transform.f,
        );
    }

    fn clear(&mut self, color: &str) {
        self.ctx.save();
        let _ = self.ctx.set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0);
        self.ctx.set_fill_style_str(color);
        if let Some(canvas) = self.ctx.canvas() {
            self.ctx
                .fill_rect(0.0, 0.0, canvas.width() as f64, canvas.height() as f64);
        }
        self.ctx.restore();
    }

    fn clear_rect(&mut self, rect: Rect, color: &str) {
        self.ctx.save();
        let _ = self.ctx.set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0);
        self.ctx.set_fill_style_str(color);
        self.ctx
            .fill_rect(rect.x, rect.y, rect.w.max(1.0), rect.h.max(1.0));
        self.ctx.restore();
    }

    fn draw_path(&mut self, op: &canvas_core::PathOp) {
        if op.points.len() < 2 {
            return;
        }
        self.ctx.begin_path();
        self.ctx.move_to(op.points[0].x, op.points[0].y);
        for p in &op.points[1..] {
            self.ctx.line_to(p.x, p.y);
        }
        if op.closed {
            self.ctx.close_path();
        }
        if let Some(fill) = &op.fill {
            match fill {
                FillStyle::Solid { color, opacity } => {
                    self.ctx.set_fill_style_str(color);
                    self.ctx.set_global_alpha(*opacity);
                }
            }
            self.ctx.fill();
            self.ctx.set_global_alpha(1.0);
        }
        if let Some(stroke) = &op.stroke {
            self.apply_stroke(stroke);
            self.ctx.stroke();
            self.ctx.set_global_alpha(1.0);
        }
    }

    fn draw_line_batch(&mut self, segments: &[[Point; 2]], stroke: &StrokeStyle) {
        if segments.is_empty() {
            return;
        }
        self.ctx.begin_path();
        for [a, b] in segments {
            self.ctx.move_to(a.x, a.y);
            self.ctx.line_to(b.x, b.y);
        }
        self.apply_stroke(stroke);
        self.ctx.stroke();
        self.ctx.set_global_alpha(1.0);
    }
}

/// The editor handle the shell talks to.
#[wasm_bindgen]
pub struct Editor {
    scene_canvas: HtmlCanvasElement,
    overlay_canvas: HtmlCanvasElement,
    renderer: Canvas2dRenderer,
    overlay: CanvasRenderingContext2d,
    dpr: f64,
    core: CoreEditor,
}

#[wasm_bindgen]
impl Editor {
    #[wasm_bindgen(constructor)]
    pub fn new(
        scene_canvas: HtmlCanvasElement,
        overlay_canvas: HtmlCanvasElement,
    ) -> Result<Editor, JsValue> {
        console_error_panic_hook::set_once();
        let ctx = scene_canvas
            .get_context("2d")?
            .ok_or_else(|| JsValue::from_str("scene canvas has no 2d context"))?
            .dyn_into::<CanvasRenderingContext2d>()?;
        let overlay = overlay_canvas
            .get_context("2d")?
            .ok_or_else(|| JsValue::from_str("overlay canvas has no 2d context"))?
            .dyn_into::<CanvasRenderingContext2d>()?;
        let dpr = web_sys::window()
            .map(|w| w.device_pixel_ratio())
            .unwrap_or(1.0);
        let core = CoreEditor::new(canvas_core::demo_document());
        Ok(Editor {
            scene_canvas,
            overlay_canvas,
            renderer: Canvas2dRenderer { ctx },
            overlay,
            dpr,
            core,
        })
    }

    /// Match both backing stores to their layout sizes.
    pub fn resize(&self) {
        for canvas in [&self.scene_canvas, &self.overlay_canvas] {
            let w = canvas.client_width() as f64;
            let h = canvas.client_height() as f64;
            canvas.set_width((w * self.dpr).round().max(1.0) as u32);
            canvas.set_height((h * self.dpr).round().max(1.0) as u32);
        }
    }

    /// Replace the drawing with an empty one.
    pub fn new_document(&mut self) {
        self.core = CoreEditor::new(Document::new());
        self.render();
    }

    pub fn scroll(&self) -> Vec<f64> {
        vec![self.core.camera.scroll.x, self.core.camera.scroll.y]
    }

    pub fn zoom(&self) -> f64 {
        self.core.camera.zoom
    }

    pub fn element_count(&self) -> usize {
        self.core.doc.ordered_live().len()
    }

    pub fn active_tool(&self) -> String {
        self.core.tool.name().into()
    }

    pub fn can_undo(&self) -> bool {
        self.core.history.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.core.history.can_redo()
    }

    /// Selection bounds in world space, or undefined.
    pub fn selection_bounds(&self) -> Option<Vec<f64>> {
        self.core
            .selection_bounds()
            .map(|r| vec![r.x, r.y, r.w, r.h])
    }

    pub fn set_tool(&mut self, name: &str) {
        if let Some(tool) = Tool::from_name(name) {
            self.core.tool = tool;
            self.draw_overlay();
        }
    }

    pub fn undo(&mut self) {
        let effects = self.core.undo();
        self.apply(effects);
    }

    pub fn redo(&mut self) {
        let effects = self.core.redo();
        self.apply(effects);
    }

    /// Draw everything visible for this frame.
    pub fn render(&mut self) {
        let viewport = self.core.camera.viewport(
            self.scene_canvas.client_width() as f64,
            self.scene_canvas.client_height() as f64,
        );
        self.renderer.begin_frame();
        self.renderer.clear(BACKGROUND);
        self.renderer
            .set_transform(self.core.camera.transform().then(Affine::scale(self.dpr)));
        for layer in self.core.scene.visible(viewport) {
            self.renderer.draw_ops(&layer.ops);
        }
        self.renderer.end_frame();
        self.draw_overlay();
    }

    // -- input -------------------------------------------------------------

    pub fn pointer_down(&mut self, x: f64, y: f64, button: u8, mods: JsValue) {
        let mods = parse_modifiers(&mods);
        let effects = self
            .core
            .pointer_down(Point::new(x, y), button, mods, now_ms());
        self.apply(effects);
    }

    /// Forward pointer pressure for freehand capture.
    pub fn set_pressure(&mut self, pressure: f64) {
        self.core.set_pressure(pressure);
    }

    pub fn pointer_move(&mut self, x: f64, y: f64, mods: JsValue) {
        let mods = parse_modifiers(&mods);
        let effects = self.core.pointer_move(Point::new(x, y), mods, now_ms());
        self.apply(effects);
    }

    pub fn pointer_up(&mut self, x: f64, y: f64, button: u8, mods: JsValue) {
        let mods = parse_modifiers(&mods);
        let effects = self.core.pointer_up(Point::new(x, y), mods, now_ms());
        let _ = button;
        self.apply(effects);
    }

    pub fn wheel(&mut self, x: f64, y: f64, dx: f64, dy: f64, ctrl: bool) {
        self.core
            .nav
            .wheel(&mut self.core.camera, Point::new(x, y), dx, dy, ctrl);
        self.render();
    }

    pub fn zoom_at(&mut self, x: f64, y: f64, factor: f64) {
        self.core.camera.zoom_at(Point::new(x, y), factor);
        self.render();
    }

    /// Returns true when the shell should preventDefault.
    pub fn key_down(&mut self, key: &str, shift: bool, alt: bool, ctrl: bool, meta: bool) -> bool {
        let mods = Modifiers {
            shift,
            alt,
            ctrl,
            meta,
        };
        if shift && !mods.command() && key == "1" {
            self.zoom_to_fit();
            return true;
        }
        if shift && !mods.command() && key == "0" {
            self.reset_zoom();
            return true;
        }
        let effects = self.core.key_down(key, mods, now_ms());
        self.apply(effects);
        matches!(
            key,
            " " | "Escape"
                | "Delete"
                | "Backspace"
                | "ArrowUp"
                | "ArrowDown"
                | "ArrowLeft"
                | "ArrowRight"
        ) || mods.command()
            || Tool::from_shortcut(key).is_some()
    }

    pub fn key_up(&mut self, key: &str) {
        let effects = self.core.key_up(key);
        self.apply(effects);
    }

    pub fn blur(&mut self) {
        self.core.nav.set_space(false);
        self.core.nav.cancel();
        self.draw_overlay();
    }

    pub fn cursor(&self) -> String {
        if self.core.nav.is_panning() {
            return "grabbing".into();
        }
        if self.core.nav.space_held() || self.core.tool == Tool::Hand {
            return "grab".into();
        }
        match self.core.tool {
            Tool::Select => "default".into(),
            Tool::Text => "text".into(),
            _ => "crosshair".into(),
        }
    }

    pub fn zoom_to_fit(&mut self) {
        if let Some(bounds) = self.core.scene.bounds() {
            self.core.camera.zoom_to_fit(
                bounds,
                (
                    self.scene_canvas.client_width() as f64,
                    self.scene_canvas.client_height() as f64,
                ),
                48.0,
            );
        } else {
            self.core.camera = canvas_core::Camera::default();
        }
        self.render();
    }

    pub fn reset_zoom(&mut self) {
        let center = Point::new(
            self.scene_canvas.client_width() as f64 / 2.0,
            self.scene_canvas.client_height() as f64 / 2.0,
        );
        self.core.camera.set_zoom_at(center, 1.0);
        self.render();
    }

    /// Screen point -> world point, through the current camera.
    pub fn screen_to_world(&self, x: f64, y: f64) -> Vec<f64> {
        let p = self.core.camera.screen_to_world(Point::new(x, y));
        vec![p.x, p.y]
    }
}

impl Editor {
    fn apply(&mut self, effects: Effects) {
        if effects.repaint_all {
            self.render();
            return;
        }
        if !effects.changed.is_empty() {
            let dirty = self.core.scene.update(&self.core.doc, &effects.changed);
            self.paint_dirty(&dirty);
        }
        if effects.overlay_changed || !effects.changed.is_empty() {
            self.draw_overlay();
        }
    }

    fn paint_dirty(&mut self, dirty: &[Rect]) {
        let viewport = self.core.camera.viewport(
            self.scene_canvas.client_width() as f64,
            self.scene_canvas.client_height() as f64,
        );
        let visible_dirty: Vec<Rect> = dirty
            .iter()
            .filter(|r| r.intersects(&viewport))
            .copied()
            .collect();
        if visible_dirty.is_empty() {
            return;
        }
        self.renderer.begin_frame();
        for rect in &visible_dirty {
            let top_left = self.core.camera.world_to_screen(Point::new(rect.x, rect.y));
            let bottom_right = self
                .core
                .camera
                .world_to_screen(Point::new(rect.max_x(), rect.max_y()));
            let screen = Rect::from_points(top_left, bottom_right).expand(1.0);
            let device = Rect::new(
                screen.x * self.dpr,
                screen.y * self.dpr,
                screen.w * self.dpr,
                screen.h * self.dpr,
            );
            self.renderer.clear_rect(device, BACKGROUND);
        }
        self.renderer
            .set_transform(self.core.camera.transform().then(Affine::scale(self.dpr)));
        for layer in self.core.scene.layers.iter() {
            let intersects_dirty = visible_dirty
                .iter()
                .any(|rect| layer.bounds.intersects(rect));
            if intersects_dirty && layer.bounds.intersects(&viewport) {
                self.renderer.draw_ops(&layer.ops);
            }
        }
        self.renderer.end_frame();
    }

    fn draw_overlay(&mut self) {
        let (w, h) = (
            self.overlay_canvas.width() as f64,
            self.overlay_canvas.height() as f64,
        );
        self.overlay.save();
        let _ = self.overlay.set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0);
        self.overlay.clear_rect(0.0, 0.0, w, h);

        let camera = self.core.camera;
        if let Some(rect) = self.core.marquee_rect() {
            let a = camera.world_to_screen(Point::new(rect.x, rect.y));
            let b = camera.world_to_screen(Point::new(rect.max_x(), rect.max_y()));
            let r = Rect::from_points(a, b);
            self.overlay.set_fill_style_str("rgba(25, 113, 194, 0.08)");
            self.overlay.set_stroke_style_str(ACCENT);
            self.overlay.set_line_width(self.dpr);
            self.overlay.fill_rect(
                r.x * self.dpr,
                r.y * self.dpr,
                r.w * self.dpr,
                r.h * self.dpr,
            );
            self.overlay.stroke_rect(
                r.x * self.dpr,
                r.y * self.dpr,
                r.w * self.dpr,
                r.h * self.dpr,
            );
        }

        if let Some(bounds) = self.core.selection_bounds() {
            let a = camera.world_to_screen(Point::new(bounds.x, bounds.y));
            let b = camera.world_to_screen(Point::new(bounds.max_x(), bounds.max_y()));
            let r = Rect::from_points(a, b).expand(1.0);
            let device = |r: Rect| {
                Rect::new(
                    r.x * self.dpr,
                    r.y * self.dpr,
                    r.w * self.dpr,
                    r.h * self.dpr,
                )
            };
            let r = device(r);
            self.overlay.set_stroke_style_str(ACCENT);
            self.overlay.set_line_width(self.dpr);
            let dash = js_sys::Array::new();
            dash.push(&JsValue::from_f64(5.0 * self.dpr));
            dash.push(&JsValue::from_f64(5.0 * self.dpr));
            let _ = self.overlay.set_line_dash(dash.as_ref());
            self.overlay.stroke_rect(r.x, r.y, r.w, r.h);
            let _ = self.overlay.set_line_dash(&js_sys::Array::new());

            let handle = 4.0 * self.dpr;
            let points = [
                (r.x, r.y),
                (r.x + r.w / 2.0, r.y),
                (r.x + r.w, r.y),
                (r.x + r.w, r.y + r.h / 2.0),
                (r.x + r.w, r.y + r.h),
                (r.x + r.w / 2.0, r.y + r.h),
                (r.x, r.y + r.h),
                (r.x, r.y + r.h / 2.0),
            ];
            self.overlay.set_fill_style_str("#ffffff");
            for (x, y) in points {
                self.overlay
                    .fill_rect(x - handle, y - handle, handle * 2.0, handle * 2.0);
                self.overlay
                    .stroke_rect(x - handle, y - handle, handle * 2.0, handle * 2.0);
            }
        }
        self.overlay.restore();
    }
}

fn parse_modifiers(value: &JsValue) -> Modifiers {
    Modifiers {
        shift: js_sys::Reflect::get(value, &"shift".into())
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        alt: js_sys::Reflect::get(value, &"alt".into())
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        ctrl: js_sys::Reflect::get(value, &"ctrl".into())
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        meta: js_sys::Reflect::get(value, &"meta".into())
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}
