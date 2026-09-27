//! canvas-wasm: the thin wasm-bindgen façade over canvas-core.
//!
//! Owns the two canvases and forwards input into the core Editor; every
//! input returns Effects that say what to repaint.

mod storage;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use canvas_core::{
    Affine, Dash, Document, DrawOp, Editor as CoreEditor, Effects, FillStyle, Modifiers, Point,
    Rect, Renderer, ShellAction, StrokeStyle, StylePatch, TextAlign, TextMeasurer, TextOp, Tool,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement};

const BACKGROUND: &str = "#1a1815";
const ACCENT: &str = "#d98e32";

fn now_ms() -> u64 {
    js_sys::Date::now() as u64
}

/// `measureText`-backed measurer with a small cache, on its own offscreen
/// context so measuring never disturbs the scene canvas state.
struct FontMeasurer {
    ctx: CanvasRenderingContext2d,
    cache: RefCell<HashMap<String, Vec<(u64, f64)>>>,
    font_size_bits: Cell<u64>,
}

impl FontMeasurer {
    fn new(ctx: CanvasRenderingContext2d) -> Self {
        Self {
            ctx,
            cache: RefCell::new(HashMap::new()),
            font_size_bits: Cell::new(f64::NAN.to_bits()),
        }
    }
}

impl TextMeasurer for FontMeasurer {
    fn measure_width(&self, text: &str, font_size: f64) -> f64 {
        let key = font_size.to_bits();
        if let Some(entries) = self.cache.borrow().get(text) {
            if let Some((_, width)) = entries.iter().find(|(bits, _)| *bits == key) {
                return *width;
            }
        }
        if self.font_size_bits.get() != key {
            self.ctx
                .set_font(&format!("{font_size}px ComicShanns, sans-serif"));
            self.font_size_bits.set(key);
        }
        let width = self
            .ctx
            .measure_text(text)
            .map(|metrics| metrics.width())
            .unwrap_or_else(|_| text.chars().count() as f64 * font_size * 0.55);
        let mut cache = self.cache.borrow_mut();
        if cache.len() > 4096 {
            cache.clear();
        }
        cache
            .entry(text.to_string())
            .or_default()
            .push((key, width));
        width
    }
}

/// A Canvas2D implementation of the core's renderer seam.
struct Canvas2dRenderer {
    ctx: CanvasRenderingContext2d,
    /// The last dash pattern pushed to the context, so repeated strokes with
    /// the same style skip the JS array construction and the state call.
    last_dash: Option<Dash>,
    /// The last font size pushed to the context for text.
    last_font_bits: u64,
}

impl Canvas2dRenderer {
    fn new(ctx: CanvasRenderingContext2d) -> Self {
        Self {
            ctx,
            last_dash: None,
            last_font_bits: f64::NAN.to_bits(),
        }
    }

    fn apply_stroke(&mut self, stroke: &StrokeStyle) {
        self.ctx.set_stroke_style_str(&stroke.color);
        self.ctx.set_line_width(stroke.width);
        self.ctx.set_global_alpha(stroke.opacity);
        if self.last_dash != Some(stroke.dash) {
            let pattern = js_sys::Array::new();
            for value in stroke.dash.pattern() {
                pattern.push(&JsValue::from_f64(*value));
            }
            let _ = self.ctx.set_line_dash(pattern.as_ref());
            self.last_dash = Some(stroke.dash);
        }
    }

    fn draw_ops(&mut self, ops: &[DrawOp]) {
        for op in ops {
            match op {
                DrawOp::Path(path) => self.draw_path(path),
                DrawOp::LineBatch { segments, stroke } => self.draw_line_batch(segments, stroke),
                DrawOp::Text(text) => self.draw_text(text),
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

    fn draw_text(&mut self, op: &TextOp) {
        let bits = op.font_size.to_bits();
        if self.last_font_bits != bits {
            self.ctx
                .set_font(&format!("{}px ComicShanns, sans-serif", op.font_size));
            self.last_font_bits = bits;
        }
        self.ctx.set_fill_style_str(&op.color);
        self.ctx.set_global_alpha(op.opacity);
        let align = match op.align {
            TextAlign::Left => "left",
            TextAlign::Center => "center",
            TextAlign::Right => "right",
        };
        self.ctx.set_text_align(align);
        let _ = self.ctx.fill_text(&op.text, op.x, op.y);
        self.ctx.set_global_alpha(1.0);
    }
}

/// The camera the scene canvas currently shows, so a pan can shift the
/// existing pixels instead of redrawing the whole document.
#[derive(Clone, Copy)]
struct ViewSnapshot {
    scroll: Point,
    zoom: f64,
    width: u32,
    height: u32,
}

/// The editor handle the shell talks to.
#[wasm_bindgen]
pub struct Editor {
    scene_canvas: HtmlCanvasElement,
    overlay_canvas: HtmlCanvasElement,
    renderer: Canvas2dRenderer,
    overlay: CanvasRenderingContext2d,
    /// Offscreen context for text measurement (never painted).
    measure_ctx: CanvasRenderingContext2d,
    dpr: f64,
    core: CoreEditor,
    on_action: Option<js_sys::Function>,
    /// Where the scene canvas pixels currently sit in world space.
    last_paint: Option<ViewSnapshot>,
}

fn offscreen_ctx() -> Result<CanvasRenderingContext2d, JsValue> {
    let document = web_sys::window()
        .and_then(|window| window.document())
        .ok_or_else(|| JsValue::from_str("no document"))?;
    let canvas: HtmlCanvasElement = document
        .create_element("canvas")
        .map_err(|_| JsValue::from_str("cannot create measure canvas"))?
        .dyn_into()
        .map_err(|_| JsValue::from_str("cannot create measure canvas"))?;
    let ctx = canvas
        .get_context("2d")?
        .ok_or_else(|| JsValue::from_str("measure canvas has no 2d context"))?
        .dyn_into::<CanvasRenderingContext2d>()
        .map_err(|_| JsValue::from_str("measure canvas has no 2d context"))?;
    Ok(ctx)
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
        let core = CoreEditor::new(Document::new());
        let measure_ctx = offscreen_ctx()?;
        let mut editor = Editor {
            scene_canvas,
            overlay_canvas,
            renderer: Canvas2dRenderer::new(ctx),
            overlay,
            measure_ctx: measure_ctx.clone(),
            dpr,
            core,
            on_action: None,
            last_paint: None,
        };
        editor
            .core
            .set_measurer(Box::new(FontMeasurer::new(measure_ctx)));
        Ok(editor)
    }

    /// Match both backing stores to their layout sizes.
    pub fn resize(&mut self) {
        for canvas in [&self.scene_canvas, &self.overlay_canvas] {
            let w = canvas.client_width() as f64;
            let h = canvas.client_height() as f64;
            canvas.set_width((w * self.dpr).round().max(1.0) as u32);
            canvas.set_height((h * self.dpr).round().max(1.0) as u32);
        }
        self.core.set_viewport(self.css_width(), self.css_height());
        self.last_paint = None;
    }

    /// Insert a shape-library glyph at the viewport centre.
    pub fn insert_glyph(&mut self, glyph_id: &str) {
        let effects = self.core.insert_glyph(glyph_id, now_ms());
        self.apply(effects);
    }

    /// Replace the drawing with an empty one.
    pub fn new_document(&mut self) {
        self.core = CoreEditor::new(Document::new());
        self.core
            .set_measurer(Box::new(FontMeasurer::new(self.measure_ctx.clone())));
        self.core.set_viewport(self.css_width(), self.css_height());
        self.render();
        self.notify_document_changed();
    }

    /// The shell installs a callback for shell actions (text editor, …).
    pub fn set_action_handler(&mut self, handler: js_sys::Function) {
        self.on_action = Some(handler);
    }

    /// Load the deterministic large fixture (scale verification).
    pub fn load_benchmark_fixture(&mut self, count: usize) {
        let doc = canvas_core::benchmark_fixture(count);
        let effects = self.core.replace_document(doc);
        self.apply(effects);
    }

    /// Serialize the document for saving to a file.
    pub fn save_json(&self) -> String {
        canvas_core::serialization::to_json_pretty(&self.core.doc).unwrap_or_default()
    }

    /// Serialize the document and hand it to storage (the shell's debounced
    /// autosave calls this; serializing inside input handlers is what makes
    /// large documents stutter).
    pub fn flush_save(&self) {
        let json = canvas_core::serialization::to_json(&self.core.doc).unwrap_or_default();
        wasm_bindgen_futures::spawn_local(async move {
            let _ = storage::save_document(&json).await;
        });
    }

    /// Export the drawing as a standalone SVG.
    pub fn export_svg(&self) -> String {
        canvas_core::svg::to_svg(&self.core.doc, self.core.measurer())
    }

    /// Export the drawing as a PNG data URL at the requested scale.
    pub fn export_png(&self, scale: f64) -> Result<String, JsValue> {
        let bounds = self
            .core
            .scene
            .bounds()
            .unwrap_or(Rect::new(0.0, 0.0, 100.0, 100.0))
            .expand(16.0);
        let scale = scale.clamp(0.25, 4.0);
        let width = (bounds.w * scale).ceil().max(1.0);
        let height = (bounds.h * scale).ceil().max(1.0);

        let document = web_sys::window()
            .and_then(|window| window.document())
            .ok_or_else(|| JsValue::from_str("no document"))?;
        let canvas: HtmlCanvasElement = document
            .create_element("canvas")
            .map_err(|_| JsValue::from_str("cannot create canvas"))?
            .dyn_into()
            .map_err(|_| JsValue::from_str("cannot create canvas"))?;
        canvas.set_width(width as u32);
        canvas.set_height(height as u32);
        let ctx = canvas
            .get_context("2d")?
            .ok_or_else(|| JsValue::from_str("no 2d context"))?
            .dyn_into::<CanvasRenderingContext2d>()?;

        let mut renderer = Canvas2dRenderer::new(ctx);
        renderer.clear(BACKGROUND);
        renderer.set_transform(Affine::translate(-bounds.x, -bounds.y).then(Affine::scale(scale)));
        for layer in &self.core.scene.layers {
            renderer.draw_ops(&layer.ops);
        }
        canvas.to_data_url_with_type("image/png")
    }

    /// Replace the document from a JSON file; errors are user-facing messages.
    pub fn load_json(&mut self, json: &str) -> Result<(), JsValue> {
        let doc = canvas_core::serialization::from_json(json)
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        let effects = self.core.replace_document(doc);
        self.apply(effects);
        Ok(())
    }

    pub fn double_click(&mut self, x: f64, y: f64) {
        let effects = self.core.double_click(Point::new(x, y), now_ms());
        self.apply(effects);
    }

    /// Commit the open text edit with the textarea's contents.
    pub fn commit_text(&mut self, id: &str, text: &str) {
        let effects = self.core.commit_text(id, text, now_ms());
        self.apply(effects);
    }

    pub fn cancel_text(&mut self) {
        let effects = self.core.cancel_text();
        self.apply(effects);
    }

    /// Apply a style patch (JSON) to the selection and/or defaults.
    pub fn update_style(&mut self, patch_json: &str) {
        if let Ok(patch) = serde_json::from_str::<StylePatch>(patch_json) {
            let effects = self.core.update_style(&patch, now_ms());
            self.apply(effects);
        }
    }

    /// The style the panel should show (selection style, else defaults).
    pub fn current_style(&self) -> String {
        serde_json::to_string(&self.core.current_style()).unwrap_or_default()
    }

    pub fn scroll(&self) -> Vec<f64> {
        vec![self.core.camera.scroll.x, self.core.camera.scroll.y]
    }

    pub fn zoom(&self) -> f64 {
        self.core.camera.zoom
    }

    pub fn element_count(&self) -> usize {
        self.core.doc.live_count()
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
            self.core.set_tool(tool);
            self.draw_overlay();
        }
    }

    /// Select a tool and keep it armed for repeated use.
    pub fn set_tool_locked(&mut self, name: &str) {
        if let Some(tool) = Tool::from_name(name) {
            self.core.set_tool_locked(tool);
            self.draw_overlay();
        }
    }

    pub fn toggle_tool_lock(&mut self) {
        self.core.tool_locked = !self.core.tool_locked;
    }

    pub fn is_tool_locked(&self) -> bool {
        self.core.tool_locked
    }

    /// Load the seed demo: a small cloud architecture to explore.
    pub fn seed_demo(&mut self) {
        let doc = canvas_core::demo_document();
        let effects = self.core.replace_document(doc);
        self.apply(effects);
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
        let (w, h) = self.core.viewport_size();
        let viewport = self.core.camera.viewport(w, h);
        self.renderer.begin_frame();
        self.renderer.clear(BACKGROUND);
        self.renderer
            .set_transform(self.core.camera.transform().then(Affine::scale(self.dpr)));
        for layer in self.core.scene.visible(viewport) {
            self.renderer.draw_ops(&layer.ops);
        }
        self.renderer.end_frame();
        self.draw_overlay();
        self.remember_view();
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
        // End of a pan gesture: replace the shifted approximation with a
        // crisp render.
        self.settle_view();
    }

    pub fn wheel(&mut self, x: f64, y: f64, dx: f64, dy: f64, ctrl: bool) {
        self.core
            .nav
            .wheel(&mut self.core.camera, Point::new(x, y), dx, dy, ctrl);
        if !self.try_paint_pan_current() {
            self.render();
        }
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
        // Claim only the command chords the editor actually handles; anything
        // else (Cmd+R, Cmd+W, …) must reach the browser.
        let command_chord = mods.command()
            && matches!(
                key,
                "a" | "A"
                    | "z"
                    | "Z"
                    | "d"
                    | "D"
                    | "c"
                    | "C"
                    | "x"
                    | "X"
                    | "v"
                    | "V"
                    | "]"
                    | "}"
                    | "["
                    | "{"
            );
        matches!(
            key,
            " " | "Escape"
                | "Delete"
                | "Backspace"
                | "ArrowUp"
                | "ArrowDown"
                | "ArrowLeft"
                | "ArrowRight"
        ) || command_chord
            || (!mods.command() && !mods.alt && Tool::from_shortcut(key).is_some())
    }

    pub fn key_up(&mut self, key: &str) {
        let effects = self.core.key_up(key);
        self.apply(effects);
    }

    pub fn blur(&mut self) {
        self.core.nav.set_space(false);
        self.core.nav.cancel();
        self.settle_view();
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
    fn css_width(&self) -> f64 {
        self.scene_canvas.client_width() as f64
    }

    fn css_height(&self) -> f64 {
        self.scene_canvas.client_height() as f64
    }

    fn remember_view(&mut self) {
        self.last_paint = Some(ViewSnapshot {
            scroll: self.core.camera.scroll,
            zoom: self.core.camera.zoom,
            width: self.scene_canvas.width(),
            height: self.scene_canvas.height(),
        });
    }

    /// Snap back to a crisp full render if the canvas is showing a panned or
    /// zoomed approximation.
    fn settle_view(&mut self) {
        if let Some(from) = self.last_paint {
            if from.scroll != self.core.camera.scroll || from.zoom != self.core.camera.zoom {
                self.render();
            }
        }
    }

    /// Shift the existing scene pixels by a pure pan and repaint only the
    /// exposed strips. Returns false when a full render is required (zoom
    /// changed, canvas resized, no valid snapshot).
    ///
    /// Cost is proportional to the freshly exposed edge pixels, not to the
    /// number of visible elements.
    fn try_paint_pan_current(&mut self) -> bool {
        let Some(from) = self.last_paint else {
            return false;
        };
        let camera = self.core.camera;
        if camera.zoom != from.zoom {
            return false;
        }
        let w = self.scene_canvas.width();
        let h = self.scene_canvas.height();
        if w == 0 || h == 0 || w != from.width || h != from.height {
            return false;
        }
        let dx = ((from.scroll.x - camera.scroll.x) * camera.zoom * self.dpr).round() as i64;
        let dy = ((from.scroll.y - camera.scroll.y) * camera.zoom * self.dpr).round() as i64;
        if dx == 0 && dy == 0 {
            return true;
        }
        if dx.abs() >= w as i64 || dy.abs() >= h as i64 {
            return false;
        }

        // Shift existing pixels (Canvas2D snapshots the source when the
        // destination canvas is also the source).
        self.renderer.ctx.save();
        let _ = self
            .renderer
            .ctx
            .set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0);
        let _ = self.renderer.ctx.draw_image_with_html_canvas_element(
            &self.scene_canvas,
            dx as f64,
            dy as f64,
        );
        self.renderer.ctx.restore();

        // Exposed strips in device space, padded to swallow rounding seams.
        let pad = 2.0;
        let mut strips: Vec<Rect> = Vec::with_capacity(2);
        if dx > 0 {
            strips.push(Rect::new(0.0, 0.0, dx as f64 + pad, h as f64));
        } else if dx < 0 {
            strips.push(Rect::new(
                (w as i64 + dx) as f64 - pad,
                0.0,
                -dx as f64 + pad,
                h as f64,
            ));
        }
        if dy > 0 {
            strips.push(Rect::new(0.0, 0.0, w as f64, dy as f64 + pad));
        } else if dy < 0 {
            strips.push(Rect::new(
                0.0,
                (h as i64 + dy) as f64 - pad,
                w as f64,
                -dy as f64 + pad,
            ));
        }

        let world: Vec<Rect> = strips
            .iter()
            .map(|strip| {
                let a = camera.screen_to_world(Point::new(strip.x / self.dpr, strip.y / self.dpr));
                let b = camera.screen_to_world(Point::new(
                    strip.max_x() / self.dpr,
                    strip.max_y() / self.dpr,
                ));
                Rect::from_points(a, b)
            })
            .collect();

        self.renderer.begin_frame();
        for strip in &strips {
            self.renderer.clear_rect(*strip, BACKGROUND);
        }
        self.renderer
            .set_transform(camera.transform().then(Affine::scale(self.dpr)));
        for layer in self.core.scene.layers.iter() {
            if world.iter().any(|rect| layer.bounds.intersects(rect)) {
                self.renderer.draw_ops(&layer.ops);
            }
        }
        self.renderer.end_frame();
        self.draw_overlay();
        // Record where the shifted pixels now sit, so the next pan composes
        // exactly; `settle_view` later snaps to the true camera.
        self.last_paint = Some(ViewSnapshot {
            scroll: Point::new(
                from.scroll.x - dx as f64 / (camera.zoom * self.dpr),
                from.scroll.y - dy as f64 / (camera.zoom * self.dpr),
            ),
            zoom: camera.zoom,
            width: w,
            height: h,
        });
        true
    }

    fn apply(&mut self, effects: Effects) {
        let durable = !self.core.is_interacting()
            && (effects.document_changed || !effects.changed.is_empty());
        if effects.repaint_all {
            let can_pan = !effects.document_changed && effects.changed.is_empty();
            if !(can_pan && self.try_paint_pan_current()) {
                self.render();
            }
        } else {
            if !effects.changed.is_empty() {
                let dirty = self.core.rebuild_changed(&effects.changed);
                self.paint_dirty(&dirty);
            }
            if effects.overlay_changed || !effects.changed.is_empty() {
                self.draw_overlay();
            }
        }
        self.dispatch_actions(&effects.actions);
        if durable {
            self.notify_document_changed();
        }
    }

    /// Ask the shell to debounce, then call `flush_save`.
    ///
    /// Serializing here would put a full-document `serde_json` pass inside the
    /// pointer/keystroke handler; on a large document that is the single
    /// biggest source of dropped frames.
    fn notify_document_changed(&self) {
        if self.on_action.is_some() {
            let action = canvas_core::ShellAction::DocumentChanged;
            self.dispatch_actions(&[action]);
        }
    }

    fn dispatch_actions(&self, actions: &[ShellAction]) {
        let Some(handler) = &self.on_action else {
            return;
        };
        for action in actions {
            if let Ok(json) = serde_json::to_string(action) {
                let _ = handler.call1(&JsValue::NULL, &JsValue::from_str(&json));
            }
        }
    }

    fn paint_dirty(&mut self, dirty: &[Rect]) {
        if let Some(from) = self.last_paint {
            if from.scroll != self.core.camera.scroll || from.zoom != self.core.camera.zoom {
                // The canvas no longer corresponds to the snapshot; the next
                // pan must fall back to a full render.
                self.last_paint = None;
            }
        }
        let (w, h) = self.core.viewport_size();
        let viewport = self.core.camera.viewport(w, h);
        let visible: Vec<Rect> = dirty
            .iter()
            .filter(|r| r.intersects(&viewport))
            .copied()
            .collect();
        if visible.is_empty() {
            return;
        }
        let merged = merge_rects(&visible);
        // One bounding box culls the layer list; the merged rects then decide
        // exactly which layers to repaint.
        let mut union = merged[0];
        for rect in &merged[1..] {
            union = union.union(rect);
        }
        self.renderer.begin_frame();
        for rect in &merged {
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
            if !layer.bounds.intersects(&union) {
                continue;
            }
            if merged.iter().any(|rect| layer.bounds.intersects(rect)) {
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
            self.overlay.set_fill_style_str("rgba(217, 142, 50, 0.12)");
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

        let anchors = self.core.hover_anchor_points();
        if !anchors.is_empty() {
            self.overlay.set_fill_style_str("#ffffff");
            self.overlay.set_stroke_style_str(ACCENT);
            self.overlay.set_line_width(self.dpr * 1.5);
            for &point in anchors {
                let screen = self.core.camera.world_to_screen(point);
                let (x, y) = (screen.x * self.dpr, screen.y * self.dpr);
                let radius = 4.0 * self.dpr;
                self.overlay.begin_path();
                let _ = self.overlay.arc(x, y, radius, 0.0, std::f64::consts::TAU);
                self.overlay.fill();
                self.overlay.stroke();
            }
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

/// Load the autosaved document, if any (the shell calls this at boot).
#[wasm_bindgen]
pub async fn load_saved_json() -> Option<String> {
    storage::load_document().await.ok().flatten()
}

/// The shape-library catalog (JSON) for the panel.
#[wasm_bindgen]
pub fn glyph_catalog() -> String {
    serde_json::to_string(&canvas_core::glyph_catalog()).unwrap_or_else(|_| "[]".into())
}

/// Merge overlapping dirty rectangles so a repaint clears and redraws each
/// region once instead of once per contributing element.
fn merge_rects(rects: &[Rect]) -> Vec<Rect> {
    let mut merged: Vec<Rect> = Vec::with_capacity(rects.len());
    for &rect in rects {
        let mut current = rect;
        'merge: loop {
            let mut i = 0;
            while i < merged.len() {
                if merged[i].intersects(&current) {
                    current = merged[i].union(&current);
                    merged.swap_remove(i);
                    continue 'merge;
                }
                i += 1;
            }
            break;
        }
        merged.push(current);
    }
    merged
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
