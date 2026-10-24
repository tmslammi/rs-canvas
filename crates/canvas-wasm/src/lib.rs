//! canvas-wasm: the thin wasm-bindgen façade over canvas-core.
//!
//! Owns the two canvases and forwards input into the core Editor; every
//! input returns Effects that say what to repaint.

mod storage;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};

use canvas_core::{
    Affine, ArrowHandle, Camera, CameraTween, Dash, Document, DrawOp, Editor as CoreEditor,
    Effects, FillStyle, Modifiers, Point, Rect, Renderer, ShellAction, StrokeStyle, StylePatch,
    TextAlign, TextMeasurer, TextOp, Tool,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement};

const BACKGROUND: &str = "#1a1815";
const ACCENT: &str = "#d98e32";
const GRID_LINE: &str = "rgba(236, 228, 214, 0.07)";
/// Line spacing at which the grid doubles rather than getting denser, in
/// device px (at 6 device px a 1px line is still readable).
const GRID_MIN_SPACING_PX: f64 = 6.0;
/// Agent camera moves glide for this long; user input cancels them early.
const CAMERA_TWEEN_MS: u64 = 380;
/// How long a `point` highlight pulses for.
const PULSE_MS: u64 = 1100;
/// Focus never zooms past this unless the server grants a higher cap: small
/// elements should not cause a dramatic jump.
const DEFAULT_FOCUS_MAX_ZOOM: f64 = 2.0;
/// A focus target already inside the viewport by this margin is left alone.
const FOCUS_MARGIN_PX: f64 = 32.0;
/// Below this zoom the renderer switches to the batched overview (LOD) path:
/// one filled rect per element, grouped by color into a single fill each,
/// text skipped. At 0.25 a 140-unit shape covers 35 screen px and hachure
/// gaps are sub-pixel, so rough detail is imperceptible — while full ops
/// would submit ~5 Canvas2D calls per element (500k calls at 100k elements).
/// Interaction and hit-testing still use full geometry; zooming back in
/// restores exact hand-drawn rendering with no invalidation lag (same frame).
const LOD_ZOOM: f64 = 0.25;

fn default_zoom_cap() -> f64 {
    DEFAULT_FOCUS_MAX_ZOOM
}

/// Clamp a camera's zoom without moving the world point at its centre.
fn cap_zoom(camera: Camera, viewport: (f64, f64), cap: f64) -> Camera {
    if camera.zoom <= cap {
        return camera;
    }
    Camera::from_center(camera.center(viewport), cap, viewport)
}

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

    /// The cached dash/font only describe the context state while it is
    /// untouched: any `restore()` resets them to the canvas defaults, so the
    /// cache must be dropped or the next stroke/text silently reuses a stale
    /// assumption (text came back at the default 10px size).
    fn invalidate_state_cache(&mut self) {
        self.last_dash = None;
        self.last_font_bits = f64::NAN.to_bits();
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
        self.invalidate_state_cache();
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
        self.invalidate_state_cache();
    }

    fn clear_rect(&mut self, rect: Rect, color: &str) {
        self.ctx.save();
        let _ = self.ctx.set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0);
        self.ctx.set_fill_style_str(color);
        self.ctx
            .fill_rect(rect.x, rect.y, rect.w.max(1.0), rect.h.max(1.0));
        self.ctx.restore();
        self.invalidate_state_cache();
    }

    fn clip_rects(&mut self, rects: &[Rect]) {
        if rects.is_empty() {
            return;
        }
        let _ = self.ctx.set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0);
        self.ctx.begin_path();
        for rect in rects {
            self.ctx
                .rect(rect.x, rect.y, rect.w.max(1.0), rect.h.max(1.0));
        }
        self.ctx.clip();
        // The caller sets the scene transform next and restores its saved
        // state in `end_frame`, which also drops this clip.
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

/// A fading highlight ring the agent asked for; view state only.
#[derive(Clone, Copy)]
struct Pulse {
    bounds: Rect,
    started_ms: u64,
    duration_ms: u64,
}

/// One step of an agent presentation timeline.
#[derive(Clone, Debug, serde::Deserialize)]
struct PresentStep {
    #[serde(default)]
    reveal: Vec<canvas_core::ElementId>,
    #[serde(default)]
    focus: Option<canvas_core::ElementId>,
    #[serde(default)]
    point: Option<canvas_core::ElementId>,
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    hold_ms: u64,
}

#[derive(Clone, Debug, serde::Deserialize)]
struct PresentScript {
    id: u64,
    steps: Vec<PresentStep>,
    #[serde(default = "default_zoom_cap")]
    zoom_cap: f64,
}

/// Playback of an agent explanation: the document is already final; this is
/// the reveal frontier, caption and camera cue over it. View state only.
struct Presentation {
    id: u64,
    steps: Vec<PresentStep>,
    index: usize,
    step_started_ms: u64,
    /// Elements not yet revealed.
    hidden: BTreeSet<canvas_core::ElementId>,
    caption: Option<String>,
    /// The server's zoom cap for focus beats.
    zoom_cap: f64,
}

#[derive(Clone, Debug, serde::Serialize)]
struct PresentOutcome {
    id: u64,
    cancelled: bool,
}

/// The editor handle the shell talks to.
#[wasm_bindgen]
pub struct Editor {
    scene_canvas: HtmlCanvasElement,
    overlay_canvas: HtmlCanvasElement,
    grid_canvas: HtmlCanvasElement,
    renderer: Canvas2dRenderer,
    overlay: CanvasRenderingContext2d,
    /// The grid sits between the scene and the overlay so elements paint on
    /// top of it.
    grid: CanvasRenderingContext2d,
    /// Offscreen context for text measurement (never painted).
    measure_ctx: CanvasRenderingContext2d,
    dpr: f64,
    core: CoreEditor,
    workspace: String,
    on_action: Option<js_sys::Function>,
    /// Where the scene canvas pixels currently sit in world space.
    last_paint: Option<ViewSnapshot>,
    /// Agent camera tween in flight, if any (view state only).
    tween: Option<CameraTween>,
    /// Agent highlight pulse in flight, if any (view state only).
    pulse: Option<Pulse>,
    /// A server snapshot that arrived mid-gesture, applied on pointer-up.
    pending_remote: Option<String>,
    /// Server patches that arrived mid-gesture, applied on pointer-up.
    pending_patches: Vec<String>,
    /// Agent explanation playback in flight, if any (view state only).
    presentation: Option<Presentation>,
    /// Finished/cancelled playbacks the shell must acknowledge.
    present_outcomes: Vec<PresentOutcome>,
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
        grid_canvas: HtmlCanvasElement,
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
        let grid = grid_canvas
            .get_context("2d")?
            .ok_or_else(|| JsValue::from_str("grid canvas has no 2d context"))?
            .dyn_into::<CanvasRenderingContext2d>()?;
        let dpr = web_sys::window()
            .map(|w| w.device_pixel_ratio())
            .unwrap_or(1.0);
        let core = CoreEditor::new(Document::new());
        let measure_ctx = offscreen_ctx()?;
        let mut editor = Editor {
            scene_canvas,
            overlay_canvas,
            grid_canvas,
            renderer: Canvas2dRenderer::new(ctx),
            overlay,
            grid,
            measure_ctx: measure_ctx.clone(),
            dpr,
            core,
            workspace: "default".into(),
            on_action: None,
            last_paint: None,
            tween: None,
            pulse: None,
            pending_remote: None,
            pending_patches: Vec::new(),
            presentation: None,
            present_outcomes: Vec::new(),
        };
        editor
            .core
            .set_measurer(Box::new(FontMeasurer::new(measure_ctx)));
        Ok(editor)
    }

    /// Set the autosave namespace before loading or editing this workspace.
    pub fn set_workspace(&mut self, workspace: &str) {
        self.workspace = workspace.to_string();
    }

    /// Match every backing store to its layout size.
    pub fn resize(&mut self) {
        for canvas in [&self.scene_canvas, &self.overlay_canvas, &self.grid_canvas] {
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
        // View preferences survive replacing the document.
        let (grid_visible, snap_to_grid, grid_size) = (
            self.core.grid_visible,
            self.core.snap_to_grid,
            self.core.grid_size,
        );
        let zoom_sensitivity = self.core.nav.zoom_sensitivity();
        self.core = CoreEditor::new(Document::new());
        self.core.grid_visible = grid_visible;
        self.core.snap_to_grid = snap_to_grid;
        self.core.grid_size = grid_size;
        self.core.nav.set_zoom_sensitivity(zoom_sensitivity);
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

    /// Load the dense connected board: `nodes` shapes wired to their
    /// neighbours by `arrows_per_node` (0..=2) bound connectors each.
    pub fn load_connected_fixture(&mut self, nodes: usize, arrows_per_node: usize) {
        let doc = canvas_core::benchmark::connected_fixture(nodes, arrows_per_node);
        let effects = self.core.replace_document(doc);
        self.apply(effects);
    }

    /// Benchmark driver for the all-elements-moving workload: uniform rigid
    /// drift of the whole document (simulation + scene prep, no draw). The
    /// caller times this separately from `render()` to report app-side CPU
    /// as simulation vs draw submission. History is untouched: this is
    /// motion, not an undoable edit.
    pub fn benchmark_tick(&mut self, dx: f64, dy: f64) {
        if !dx.is_finite() || !dy.is_finite() {
            return;
        }
        self.core.benchmark_tick(dx, dy);
        // The pixel-shift cache no longer describes the canvas contents:
        // element motion (unlike camera motion) changes what each pixel
        // shows, so the next pan must full-render.
        self.last_paint = None;
    }

    /// Serialize the document for saving to a file.
    pub fn save_json(&self) -> String {
        canvas_core::serialization::to_json_pretty(&self.core.doc).unwrap_or_default()
    }

    /// The live document as compact JSON for the agent bridge.
    pub fn snapshot_json(&self) -> String {
        canvas_core::serialization::to_json(&self.core.doc).unwrap_or_default()
    }

    /// The live document wrapped as a `{"type":"snapshot","doc":{...}}` frame,
    /// so the shell sends it without a parse/stringify round trip.
    pub fn snapshot_frame_json(&self) -> String {
        #[derive(serde::Serialize)]
        struct Snapshot<'a> {
            #[serde(rename = "type")]
            kind: &'static str,
            doc: &'a Document,
        }
        serde_json::to_string(&Snapshot {
            kind: "snapshot",
            doc: &self.core.doc,
        })
        .unwrap_or_default()
    }

    /// Merge a server snapshot into this page's document using the merge-ready
    /// rules, keeping local edits. Inbound merges never touch history, so the
    /// user's undo stack stays theirs. Returns whether anything changed.
    ///
    /// A snapshot that lands mid-gesture is deferred to pointer-up: replacing
    /// the document under an in-flight drag would corrupt the gesture.
    pub fn reconcile_json(&mut self, json: &str) -> Result<bool, JsValue> {
        if self.core.is_interacting() {
            self.pending_remote = Some(json.to_string());
            return Ok(false);
        }
        self.reconcile_now(json)
    }

    /// Drain the deltas applied since the last call as one wire patch frame
    /// (`{"type":"patch","put":[...]}`), or an empty string when nothing
    /// changed. The shell sends it to the bridge; when the bridge is down the
    /// frame is dropped and the full snapshot sent on reconnect catches the
    /// server up.
    pub fn take_outbound_frame(&mut self) -> String {
        let deltas = self.core.history.drain_outbox();
        if deltas.is_empty() {
            return String::new();
        }
        let patch = canvas_core::Patch::from_deltas(&deltas, now_ms());
        serde_json::json!({ "type": "patch", "put": patch.put }).to_string()
    }

    /// Merge a server patch — a JSON array of full elements, tombstones
    /// included — into the document with the merge rules. Returns whether
    /// anything changed. Mid-gesture patches are deferred to pointer-up.
    pub fn merge_patch_json(&mut self, json: &str) -> Result<bool, JsValue> {
        if self.core.is_interacting() {
            self.pending_patches.push(json.to_string());
            return Ok(false);
        }
        self.merge_patch_now(json)
    }

    /// Serialize the document and hand it to storage (the shell's debounced
    /// autosave calls this; serializing inside input handlers is what makes
    /// large documents stutter).
    pub fn flush_save(&self) {
        let json = canvas_core::serialization::to_json(&self.core.doc).unwrap_or_default();
        let workspace = self.workspace.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let _ = storage::save_document(&json, &workspace).await;
        });
    }

    /// Export the drawing as a standalone SVG.
    pub fn export_svg(&self) -> String {
        canvas_core::svg::to_svg(&self.core.doc, self.core.measurer())
    }

    /// Export the drawing as a PNG data URL at the requested scale.
    pub fn export_png(&mut self, scale: f64) -> Result<String, JsValue> {
        if !scale.is_finite() || scale <= 0.0 {
            return Err(JsValue::from_str(
                "export scale must be a positive, finite number",
            ));
        }
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
        self.core.scene.flush_pending_ops();
        if let Some(all) = self.core.scene.cached_bounds() {
            self.core.ensure_area(all);
        }
        self.core.scene.flush_all_preview_stale();
        for layer in &self.core.scene.layers {
            renderer.draw_ops(&layer.ops);
        }
        canvas.to_data_url_with_type("image/png")
    }

    /// The current viewport as a PNG data URL: exactly what the user is
    /// looking at, camera included. Overlay decorations (selection, captions)
    /// are excluded — this is the drawing, not the UI. The long edge is
    /// capped so the image stays a sane size for the model.
    pub fn export_viewport_png(&mut self, scale: f64) -> Result<String, JsValue> {
        if !scale.is_finite() || scale <= 0.0 {
            return Err(JsValue::from_str(
                "viewport scale must be a positive, finite number",
            ));
        }
        let (css_w, css_h) = (self.css_width().max(1.0), self.css_height().max(1.0));
        let scale = scale.clamp(0.1, 4.0).min(1600.0 / css_w.max(css_h));
        let width = (css_w * scale).ceil().max(1.0);
        let height = (css_h * scale).ceil().max(1.0);

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

        let hidden = self.presentation.as_ref().map(|p| &p.hidden);
        let hidden_active = hidden.is_some_and(|s| !s.is_empty());
        let mut renderer = Canvas2dRenderer::new(ctx);
        renderer.clear(BACKGROUND);
        renderer.set_transform(self.core.camera.transform().then(Affine::scale(scale)));
        let viewport = self.core.camera.viewport(css_w, css_h);
        self.core.scene.flush_pending_ops();
        self.core.ensure_area(viewport);
        self.core.scene.flush_preview_stale(viewport);
        for layer in self.core.scene.visible(viewport) {
            if hidden_active && hidden.unwrap().contains(&layer.element_id) {
                continue;
            }
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
        if !x.is_finite() || !y.is_finite() {
            return;
        }
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
    pub fn update_style(&mut self, patch_json: &str) -> Result<(), JsValue> {
        let patch: StylePatch = serde_json::from_str(patch_json)
            .map_err(|error| JsValue::from_str(&format!("invalid style patch: {error}")))?;
        let effects = self.core.update_style(&patch, now_ms());
        self.apply(effects);
        Ok(())
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
            let effects = self.core.set_tool(tool);
            self.apply(effects);
        }
    }

    /// Select a tool and keep it armed for repeated use.
    pub fn set_tool_locked(&mut self, name: &str) {
        if let Some(tool) = Tool::from_name(name) {
            let effects = self.core.set_tool_locked(tool);
            self.apply(effects);
        }
    }

    pub fn toggle_tool_lock(&mut self) {
        self.core.tool_locked = !self.core.tool_locked;
    }

    pub fn is_tool_locked(&self) -> bool {
        self.core.tool_locked
    }

    /// Show or hide the optional grid.
    pub fn set_grid_visible(&mut self, visible: bool) {
        self.core.grid_visible = visible;
        self.draw_grid();
    }

    pub fn grid_visible(&self) -> bool {
        self.core.grid_visible
    }

    /// Set the grid spacing (world units). Out-of-range values are clamped;
    /// non-finite values are ignored.
    pub fn set_grid_size(&mut self, size: f64) {
        if size.is_finite() {
            self.core.grid_size = size.clamp(4.0, 400.0);
            self.draw_grid();
        }
    }

    pub fn grid_size(&self) -> f64 {
        self.core.grid_size
    }

    /// Multiplier on wheel-zoom strength (1 is the tuned default).
    pub fn set_zoom_sensitivity(&mut self, sensitivity: f64) {
        self.core.nav.set_zoom_sensitivity(sensitivity);
    }

    pub fn zoom_sensitivity(&self) -> f64 {
        self.core.nav.zoom_sensitivity()
    }

    /// Enable or disable snapping edits to the grid.
    pub fn set_snap_enabled(&mut self, enabled: bool) {
        self.core.snap_to_grid = enabled;
    }

    pub fn snap_enabled(&self) -> bool {
        self.core.snap_to_grid
    }

    /// Load the seed demo: a small cloud architecture to explore.
    pub fn seed_demo(&mut self) {
        let doc = canvas_core::demo_document();
        let effects = self.core.replace_document(doc);
        self.apply(effects);
    }

    pub fn undo(&mut self) {
        let effects = self.core.undo(now_ms());
        self.apply(effects);
    }

    pub fn redo(&mut self) {
        let effects = self.core.redo(now_ms());
        self.apply(effects);
    }

    /// Draw everything visible for this frame.
    pub fn render(&mut self) {
        if self.core.camera.zoom < LOD_ZOOM {
            self.render_lod();
            return;
        }
        // Bulk drift defers cached-point shifts (bounds move eagerly); the
        // first full-detail frame after a drift pays one flush. LOD frames
        // never touch ops, so drift loops at overview zoom skip it entirely.
        self.core.scene.flush_pending_ops();
        let (w, h) = self.core.viewport_size();
        let viewport = self.core.camera.viewport(w, h);
        self.core.ensure_area(viewport);
        self.core.scene.flush_preview_stale(viewport);
        // During playback, unrevealed elements stay off the canvas. The
        // document is final the whole time; this is view state only.
        // Borrowed, never cloned: the old per-frame BTreeSet clone cost
        // O(hidden) allocations on every frame even when idle.
        let hidden = self.presentation.as_ref().map(|p| &p.hidden);
        let transform = self.core.camera.transform().then(Affine::scale(self.dpr));
        {
            let scene = &self.core.scene;
            let renderer = &mut self.renderer;
            renderer.begin_frame();
            renderer.clear(BACKGROUND);
            renderer.set_transform(transform);
            // Contains-all fast path: the cached union makes the check O(1);
            // overviews skip 100k per-layer intersection tests.
            let all_visible = scene
                .cached_bounds()
                .is_some_and(|b| viewport_contains(&viewport, &b));
            if all_visible {
                match hidden {
                    Some(set) if !set.is_empty() => {
                        for layer in scene.all_layers() {
                            if set.contains(&layer.element_id) {
                                continue;
                            }
                            renderer.draw_ops(&layer.ops);
                        }
                    }
                    _ => {
                        for layer in scene.all_layers() {
                            renderer.draw_ops(&layer.ops);
                        }
                    }
                }
            } else {
                match hidden {
                    Some(set) if !set.is_empty() => {
                        for layer in scene.visible(viewport) {
                            if set.contains(&layer.element_id) {
                                continue;
                            }
                            renderer.draw_ops(&layer.ops);
                        }
                    }
                    _ => {
                        for layer in scene.visible(viewport) {
                            renderer.draw_ops(&layer.ops);
                        }
                    }
                }
            }
            renderer.end_frame();
        }
        self.draw_overlay();
        self.remember_view();
    }

    /// Zoomed-out overview: batched rects grouped by color, text skipped.
    /// Same layout and palette as full rendering at a fraction of the
    /// submission cost; zooming back in flushes the deferred drift once and
    /// restores exact ink (full ops stay cached, bounds stay exact throughout).
    fn render_lod(&mut self) {
        let (w, h) = self.core.viewport_size();
        let viewport = self.core.camera.viewport(w, h);
        let hidden = self.presentation.as_ref().map(|p| &p.hidden);
        let hidden_active = hidden.is_some_and(|s| !s.is_empty());
        let transform = self.core.camera.transform().then(Affine::scale(self.dpr));
        let renderer = &mut self.renderer;
        renderer.begin_frame();
        renderer.clear(BACKGROUND);
        renderer.set_transform(transform);
        // Collect the palette indices present in this view (a handful for
        // diagram boards) so each color submits exactly one fill. Integer
        // compares only — no per-frame string work. Two passes over layers,
        // zero per-rect allocation.
        let all_visible = self
            .core
            .scene
            .cached_bounds()
            .is_some_and(|b| viewport_contains(&viewport, &b));
        // Pass 1: distinct palette indices, in first-seen (z) order.
        let mut lod_present: Vec<u32> = Vec::new();
        if all_visible {
            for layer in self.core.scene.all_layers() {
                let Some(idx) = layer.lod else {
                    continue;
                };
                if hidden_active && hidden.unwrap().contains(&layer.element_id) {
                    continue;
                }
                if !lod_present.contains(&idx) {
                    lod_present.push(idx);
                    if lod_present.len() >= 64 {
                        break;
                    }
                }
            }
        } else {
            for layer in self.core.scene.visible(viewport) {
                let Some(idx) = layer.lod else {
                    continue;
                };
                if hidden_active && hidden.unwrap().contains(&layer.element_id) {
                    continue;
                }
                if !lod_present.contains(&idx) {
                    lod_present.push(idx);
                    if lod_present.len() >= 64 {
                        break;
                    }
                }
            }
        }
        // Pass 2: one path + fill per palette entry. `rect` calls stay in
        // world space under the scene transform; sub-pixel boxes still paint
        // a hairline so dense overviews read solid instead of sparkling.
        // Palette strings are cloned once per color per frame (a handful of
        // small allocs); rects stream with no allocation.
        let palette: Vec<String> = self.core.scene.palette().to_vec();
        for idx in &lod_present {
            let Some(color) = palette.get(*idx as usize) else {
                continue;
            };
            renderer.ctx.begin_path();
            if all_visible {
                for layer in self.core.scene.all_layers() {
                    if layer.lod != Some(*idx) {
                        continue;
                    }
                    if hidden_active && hidden.unwrap().contains(&layer.element_id) {
                        continue;
                    }
                    let b = layer.bounds;
                    renderer.ctx.rect(b.x, b.y, b.w.max(0.5), b.h.max(0.5));
                }
            } else {
                for layer in self.core.scene.visible(viewport) {
                    if layer.lod != Some(*idx) {
                        continue;
                    }
                    if hidden_active && hidden.unwrap().contains(&layer.element_id) {
                        continue;
                    }
                    let b = layer.bounds;
                    renderer.ctx.rect(b.x, b.y, b.w.max(0.5), b.h.max(0.5));
                }
            }
            renderer.ctx.set_fill_style_str(color);
            renderer.ctx.set_global_alpha(1.0);
            renderer.ctx.fill();
        }
        renderer.end_frame();
        self.draw_overlay();
        self.remember_view();
    }

    // -- input -------------------------------------------------------------

    pub fn pointer_down(&mut self, x: f64, y: f64, button: u8, mods: JsValue) {
        if !x.is_finite() || !y.is_finite() {
            return;
        }
        self.cancel_camera_transition();
        self.cancel_presentation();
        let mods = parse_modifiers(&mods);
        let effects = self
            .core
            .pointer_down(Point::new(x, y), button, mods, now_ms());
        self.apply(effects);
    }

    /// Forward pointer pressure for freehand capture.
    pub fn set_pressure(&mut self, pressure: f64) {
        if !pressure.is_finite() {
            return;
        }
        self.core.set_pressure(pressure);
    }

    pub fn pointer_move(&mut self, x: f64, y: f64, mods: JsValue) {
        if !x.is_finite() || !y.is_finite() {
            return;
        }
        let mods = parse_modifiers(&mods);
        let effects = self.core.pointer_move(Point::new(x, y), mods, now_ms());
        self.apply(effects);
    }

    pub fn pointer_up(&mut self, x: f64, y: f64, button: u8, mods: JsValue) {
        if !x.is_finite() || !y.is_finite() {
            return;
        }
        let mods = parse_modifiers(&mods);
        let effects = self.core.pointer_up(Point::new(x, y), mods, now_ms());
        let _ = button;
        self.apply(effects);
        // End of a pan gesture: replace the shifted approximation with a
        // crisp render.
        self.settle_view();
        // A remote snapshot deferred during the gesture can land now.
        if let Some(pending) = self.pending_remote.take() {
            let _ = self.reconcile_now(&pending);
        }
        for patch in std::mem::take(&mut self.pending_patches) {
            let _ = self.merge_patch_now(&patch);
        }
    }

    /// Abort an interrupted pointer gesture instead of committing its last
    /// preview. Used by browser pointer cancellation and touch pinch takeover.
    pub fn pointer_cancel(&mut self) {
        self.core.nav.cancel();
        if self.core.is_interacting() {
            let effects = self.core.cancel();
            self.apply(effects);
        }
        self.settle_view();
        // The cancelled gesture no longer blocks incoming document edits.
        if let Some(pending) = self.pending_remote.take() {
            let _ = self.reconcile_now(&pending);
        }
        for patch in std::mem::take(&mut self.pending_patches) {
            let _ = self.merge_patch_now(&patch);
        }
    }

    pub fn wheel(&mut self, x: f64, y: f64, dx: f64, dy: f64, ctrl: bool) {
        if !x.is_finite() || !y.is_finite() || !dx.is_finite() || !dy.is_finite() {
            return;
        }
        self.cancel_camera_transition();
        self.cancel_presentation();
        self.core
            .nav
            .wheel(&mut self.core.camera, Point::new(x, y), dx, dy, ctrl);
        if !self.try_paint_pan_current() {
            self.render();
        }
    }

    pub fn zoom_at(&mut self, x: f64, y: f64, factor: f64) {
        if !x.is_finite() || !y.is_finite() || !factor.is_finite() {
            return;
        }
        self.cancel_camera_transition();
        self.cancel_presentation();
        self.core.camera.zoom_at(Point::new(x, y), factor);
        self.render();
    }

    /// Returns true when the shell should preventDefault.
    pub fn key_down(&mut self, key: &str, shift: bool, alt: bool, ctrl: bool, meta: bool) -> bool {
        if !matches!(key, "Shift" | "Control" | "Alt" | "Meta" | "CapsLock") {
            self.cancel_presentation();
        }
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
        self.core.clear_hover();
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
        if self.core.tool == Tool::Select && !self.core.hover_anchor_points().is_empty() {
            return "crosshair".into();
        }
        if self.core.tool == Tool::Select && self.core.stack_pull_hover() {
            return "grab".into();
        }
        match self.core.tool {
            Tool::Select => "default".into(),
            Tool::Text => "text".into(),
            _ => "crosshair".into(),
        }
    }

    pub fn zoom_to_fit(&mut self) {
        self.cancel_camera_transition();
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
        self.cancel_camera_transition();
        let center = Point::new(
            self.scene_canvas.client_width() as f64 / 2.0,
            self.scene_canvas.client_height() as f64 / 2.0,
        );
        self.core.camera.set_zoom_at(center, 1.0);
        self.render();
    }

    /// Apply a view command from the agent bridge (JSON:
    /// `{action: "focus"|"zoom"|"point", id?, pad?, mode?, animate?}` —
    /// `animate: false` jumps instead of gliding). Returns true when an
    /// animation is running and the shell should pump `advance_animation`.
    ///
    /// View commands never touch the document: no deltas, no history, no
    /// persistence.
    pub fn apply_view_command(&mut self, json: &str) -> bool {
        let Ok(command) = serde_json::from_str::<serde_json::Value>(json) else {
            return false;
        };
        let Some(action) = command.get("action").and_then(|value| value.as_str()) else {
            return false;
        };
        let animate = command
            .get("animate")
            .and_then(|value| value.as_bool())
            .unwrap_or(true);
        let zoom_cap = command
            .get("zoom_cap")
            .and_then(|value| value.as_f64())
            .unwrap_or(DEFAULT_FOCUS_MAX_ZOOM);
        match action {
            "focus" => {
                let id = command.get("id").and_then(|value| value.as_str());
                let pad = command
                    .get("pad")
                    .and_then(|value| value.as_f64())
                    .unwrap_or(64.0);
                match self.focus_plan(id, pad, zoom_cap) {
                    Some(target) => self.start_camera_transition(target, animate),
                    None => false,
                }
            }
            "zoom" => {
                let mode = command
                    .get("mode")
                    .and_then(|value| value.as_str())
                    .unwrap_or("in");
                let viewport = (self.css_width(), self.css_height());
                let target = match mode {
                    "in" => {
                        let mut camera = self.core.camera;
                        camera.zoom_at(Point::new(viewport.0 / 2.0, viewport.1 / 2.0), 1.3);
                        Some(cap_zoom(camera, viewport, zoom_cap))
                    }
                    "out" => {
                        let mut camera = self.core.camera;
                        camera.zoom_at(Point::new(viewport.0 / 2.0, viewport.1 / 2.0), 1.0 / 1.3);
                        Some(camera)
                    }
                    "fit" => self
                        .core
                        .focus_camera(None, 48.0)
                        .map(|camera| cap_zoom(camera, viewport, zoom_cap)),
                    "reset" => Some(Camera::from_center(
                        self.core.camera.center(viewport),
                        1.0,
                        viewport,
                    )),
                    _ => None,
                };
                target
                    .map(|camera| self.start_camera_transition(camera, animate))
                    .unwrap_or(false)
            }
            "point" => {
                let Some(id) = command.get("id").and_then(|value| value.as_str()) else {
                    return false;
                };
                let Some(bounds) = self.core.doc.framing_bounds(id) else {
                    return false;
                };
                self.pulse = Some(Pulse {
                    bounds,
                    started_ms: now_ms(),
                    duration_ms: PULSE_MS,
                });
                self.draw_overlay();
                true
            }
            _ => false,
        }
    }

    /// Advance any in-flight agent animation to `now_ms`; returns whether
    /// another frame is needed. The shell's requestAnimationFrame loop drives
    /// this, so the core stays clock-free.
    pub fn advance_animation(&mut self, now_ms: f64) -> bool {
        if !now_ms.is_finite() || now_ms < 0.0 {
            return false;
        }
        let now = now_ms as u64;
        let mut active = false;
        if let Some(tween) = self.tween {
            let viewport = (self.css_width(), self.css_height());
            let (camera, done) = tween.camera_at(now, viewport);
            self.core.camera = camera;
            if done {
                self.tween = None;
            } else {
                active = true;
            }
            self.render();
        }
        if let Some(pulse) = self.pulse {
            if now >= pulse.started_ms + pulse.duration_ms {
                self.pulse = None;
                self.draw_overlay();
            } else {
                active = true;
                if self.tween.is_none() {
                    self.draw_overlay();
                }
            }
        }
        // Presentation pacing: hold the step, then reveal the next one (or
        // finish). The pump stays alive for the whole timeline; pure holds
        // cost a rAF tick, not a repaint.
        let advance_step = match &self.presentation {
            Some(presentation) => {
                now >= presentation.step_started_ms + presentation.steps[presentation.index].hold_ms
            }
            None => false,
        };
        if advance_step {
            let last = self
                .presentation
                .as_ref()
                .is_some_and(|presentation| presentation.index + 1 >= presentation.steps.len());
            if last {
                self.finish_presentation(false);
            } else {
                if let Some(presentation) = self.presentation.as_mut() {
                    presentation.index += 1;
                }
                self.begin_presentation_step();
            }
        }
        if self.presentation.is_some() {
            active = true;
        }
        active
    }

    /// Start an agent presentation timeline (JSON: `{id, steps:[{reveal,
    /// focus, point, caption, hold_ms}]}`). The document is already final;
    /// this stages the reveal, caption and camera cues over it. Returns true
    /// when the shell should pump `advance_animation`.
    pub fn start_presentation(&mut self, json: &str) -> bool {
        let Ok(script) = serde_json::from_str::<PresentScript>(json) else {
            return false;
        };
        if script.steps.is_empty() {
            return false;
        }
        self.cancel_presentation();
        let mut hidden: BTreeSet<canvas_core::ElementId> = BTreeSet::new();
        for step in &script.steps {
            hidden.extend(step.reveal.iter().cloned());
        }
        self.presentation = Some(Presentation {
            id: script.id,
            steps: script.steps,
            index: 0,
            step_started_ms: now_ms(),
            hidden,
            caption: None,
            zoom_cap: script.zoom_cap,
        });
        self.begin_presentation_step();
        true
    }

    /// Finished/cancelled playbacks the shell must acknowledge, as JSON like
    /// `[{"id":3,"cancelled":false}]`; empty string when there are none.
    pub fn presentation_outcomes(&mut self) -> String {
        if self.present_outcomes.is_empty() {
            return String::new();
        }
        let json = serde_json::to_string(&self.present_outcomes).unwrap_or_default();
        self.present_outcomes.clear();
        json
    }

    /// Introspection for the shell and tests: empty when idle, else JSON with
    /// `playing`, `hidden` and `caption`. View state only.
    pub fn presentation_state(&self) -> String {
        match &self.presentation {
            None => String::new(),
            Some(presentation) => serde_json::json!({
                "playing": true,
                "hidden": presentation.hidden.len(),
                "caption": presentation.caption,
            })
            .to_string(),
        }
    }

    /// The page's transient view state for the agent: camera, viewport and
    /// selection. Never persisted, never merged; the server treats it as a
    /// hint (see ADR 0006).
    pub fn view_state_json(&self) -> String {
        let (w, h) = (self.css_width(), self.css_height());
        let selection: Vec<&str> = self.core.selection.iter().map(String::as_str).collect();
        serde_json::json!({
            "camera": {
                "x": self.core.camera.scroll.x,
                "y": self.core.camera.scroll.y,
                "zoom": self.core.camera.zoom,
            },
            "viewport": { "w": w, "h": h },
            "selection": selection,
        })
        .to_string()
    }

    /// Screen point -> world point, through the current camera.
    pub fn screen_to_world(&self, x: f64, y: f64) -> Vec<f64> {
        let p = self.core.camera.screen_to_world(Point::new(x, y));
        vec![p.x, p.y]
    }

    /// The routed polyline currently drawn for a connector, as flat x/y
    /// world coordinates. Empty for anything that is not a connector.
    pub fn route_for(&self, id: &str) -> Vec<f64> {
        let Some(element) = self.core.doc.element(id) else {
            return Vec::new();
        };
        let canvas_core::model::ElementKind::Arrow {
            start,
            end,
            routing,
            waypoints,
            ..
        } = &element.kind
        else {
            return Vec::new();
        };
        canvas_core::connector::route_avoiding(
            start,
            end,
            *routing,
            waypoints,
            &self.core.doc,
            &self.core.obstacles,
        )
        .into_iter()
        .flat_map(|p| [p.x, p.y])
        .collect()
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

    /// Begin an eased camera move to `target`, or jump when motion is
    /// unwanted. Returns whether the shell should pump frames.
    fn start_camera_transition(&mut self, target: Camera, animate: bool) -> bool {
        if !animate {
            self.tween = None;
            self.core.camera = target;
            self.render();
            return false;
        }
        let viewport = (self.css_width(), self.css_height());
        self.tween = Some(CameraTween::between(
            &self.core.camera,
            &target,
            viewport,
            now_ms(),
            CAMERA_TWEEN_MS,
        ));
        true
    }

    /// Any direct user camera input wins over the agent's animation.
    fn cancel_camera_transition(&mut self) {
        self.tween = None;
    }

    /// The camera move a focus request implies: `None` when the target is
    /// already comfortably visible — the camera belongs to the user — and the
    /// fit zoom is capped so small elements don't cause a dramatic jump.
    fn focus_plan(&self, id: Option<&str>, pad: f64, zoom_cap: f64) -> Option<Camera> {
        let bounds = match id {
            Some(id) => self.core.doc.framing_bounds(id)?,
            None => self.core.doc.bounds()?,
        };
        let viewport = (self.css_width(), self.css_height());
        if viewport.0 <= 0.0 || viewport.1 <= 0.0 {
            return None;
        }
        let visible = self.core.camera.viewport(viewport.0, viewport.1);
        let margin = FOCUS_MARGIN_PX / self.core.camera.zoom;
        let comfortably_visible = bounds.x >= visible.x + margin
            && bounds.y >= visible.y + margin
            && bounds.max_x() <= visible.max_x() - margin
            && bounds.max_y() <= visible.max_y() - margin;
        if comfortably_visible {
            return None;
        }
        let mut camera = self.core.camera;
        camera.zoom_to_fit(bounds, viewport, pad);
        Some(Camera::from_center(
            bounds.center(),
            camera.zoom.min(zoom_cap),
            viewport,
        ))
    }

    /// Apply the current presentation step: reveal its elements, move the
    /// camera, pulse the point target and show the caption.
    fn begin_presentation_step(&mut self) {
        let Some(presentation) = self.presentation.as_mut() else {
            return;
        };
        if presentation.index >= presentation.steps.len() {
            return;
        }
        let step = presentation.steps[presentation.index].clone();
        let zoom_cap = presentation.zoom_cap;
        for id in &step.reveal {
            presentation.hidden.remove(id);
        }
        presentation.caption = step.caption.clone();
        presentation.step_started_ms = now_ms();
        if let Some(id) = step.focus.as_deref() {
            if let Some(target) = self.focus_plan(Some(id), 64.0, zoom_cap) {
                self.start_camera_transition(target, true);
            }
        }
        if let Some(id) = step.point.as_deref() {
            if let Some(bounds) = self.core.doc.framing_bounds(id) {
                self.pulse = Some(Pulse {
                    bounds,
                    started_ms: now_ms(),
                    duration_ms: PULSE_MS,
                });
            }
        }
        self.last_paint = None;
        self.render();
    }

    /// End playback: reveal everything (the document was always final), drop
    /// its decorations, and record the outcome for the shell to report.
    fn finish_presentation(&mut self, cancelled: bool) {
        let Some(presentation) = self.presentation.take() else {
            return;
        };
        self.present_outcomes.push(PresentOutcome {
            id: presentation.id,
            cancelled,
        });
        self.pulse = None;
        self.tween = None;
        self.last_paint = None;
        self.render();
    }

    /// Any direct user input wins over the agent's explanation.
    fn cancel_presentation(&mut self) {
        if self.presentation.is_some() {
            self.finish_presentation(true);
        }
    }

    /// Merge a remote snapshot now: reconcile, rebuild the scene, repaint.
    fn reconcile_now(&mut self, json: &str) -> Result<bool, JsValue> {
        let remote = canvas_core::serialization::from_json(json)
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        let merged = canvas_core::reconcile(&self.core.doc, &remote);
        if merged == self.core.doc {
            return Ok(false);
        }
        self.core.doc = merged;
        self.core
            .selection
            .retain(|id| self.core.doc.element(id).is_some_and(|e| !e.is_deleted));
        self.last_paint = None;
        self.core.refresh_scene();
        self.render();
        Ok(true)
    }

    /// Merge a remote patch now: per-element LWW, then rebuild only the
    /// changed layers and repaint them. This is the steady-state path — the
    /// cost is proportional to what changed, not to the document.
    fn merge_patch_now(&mut self, json: &str) -> Result<bool, JsValue> {
        let elements: Vec<canvas_core::Element> =
            serde_json::from_str(json).map_err(|error| JsValue::from_str(&error.to_string()))?;
        let patch = canvas_core::Patch { put: elements };
        let changed: BTreeSet<canvas_core::ElementId> =
            patch.apply_to(&mut self.core.doc).into_iter().collect();
        if changed.is_empty() {
            return Ok(false);
        }
        self.core
            .selection
            .retain(|id| self.core.doc.element(id).is_some_and(|e| !e.is_deleted));
        self.last_paint = None;
        let dirty = self.core.rebuild_changed(&changed);
        self.paint_dirty(&dirty);
        self.draw_overlay();
        Ok(true)
    }

    /// Shift the existing scene pixels by a pure pan and repaint only the
    /// exposed strips. Returns false when a full render is required (zoom
    /// changed, canvas resized, no valid snapshot).
    ///
    /// Cost is proportional to the freshly exposed edge pixels, not to the
    /// number of visible elements.
    fn try_paint_pan_current(&mut self) -> bool {
        // At overview zoom the LOD full render is cheaper than pixel-shifting
        // plus per-strip full-detail repaints (which would also mismatch the
        // LOD main image). Fall back to `render()` there.
        if self.core.camera.zoom < LOD_ZOOM {
            return false;
        }
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
        self.renderer.invalidate_state_cache();

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
        // Same rule as `paint_dirty`: layers paint in full, so confine them
        // to the freshly exposed strips.
        self.renderer.clip_rects(&strips);
        self.renderer
            .set_transform(camera.transform().then(Affine::scale(self.dpr)));
        // Non-LOD only (LOD pans fall back to render before reaching here):
        // deferred drift points must be flushed so strips paint new positions.
        self.core.scene.flush_pending_ops();
        for strip in world.iter().copied() {
            self.core.ensure_area(strip);
            self.core.scene.flush_preview_stale(strip);
        }
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
        } else if effects.preview {
            // Selection-drag preview: caches already shifted in place, so
            // repaint the drained dirty regions without rebuilding layers or
            // touching indexes. The release pass settles everything exactly.
            let dirty = self.core.preview_dirty();
            if !dirty.is_empty() {
                self.paint_dirty(&dirty);
            }
            if effects.overlay_changed {
                self.draw_overlay();
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
        // At overview zoom a single LOD pass is cheaper and keeps the whole
        // frame on the same detail level (no full-ink island in an LOD sea).
        if self.core.camera.zoom < LOD_ZOOM {
            self.render();
            return;
        }
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
        let mut cleared: Vec<Rect> = Vec::with_capacity(merged.len());
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
            cleared.push(device);
        }
        // Clip the redraws to the cleared region: a layer that intersects a
        // dirty rect repaints in full, so without the clip its parts outside
        // the cleared rect would paint over untouched pixels — most visibly
        // letting a lower layer cover the higher layers around it.
        self.renderer.clip_rects(&cleared);
        self.renderer
            .set_transform(self.core.camera.transform().then(Affine::scale(self.dpr)));
        // Non-LOD only (LOD edits full-render before reaching here): flush
        // deferred drift so repainted layers draw at new positions.
        self.core.scene.flush_pending_ops();
        self.core.ensure_area(union);
        self.core.scene.flush_preview_stale(union);
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

    /// Paint the optional grid on its own canvas, under the elements. The
    /// spacing doubles as the view zooms out so lines never crowd together.
    fn draw_grid(&mut self) {
        let (w, h) = (
            self.grid_canvas.width() as f64,
            self.grid_canvas.height() as f64,
        );
        self.grid.save();
        let _ = self.grid.set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0);
        self.grid.clear_rect(0.0, 0.0, w, h);
        if self.core.grid_visible && w > 0.0 && h > 0.0 {
            let camera = self.core.camera;
            let mut step = self.core.grid_size;
            while step * camera.zoom * self.dpr < GRID_MIN_SPACING_PX {
                step *= 2.0;
            }
            let top_left = camera.screen_to_world(Point::ZERO);
            let bottom_right = camera.screen_to_world(Point::new(w / self.dpr, h / self.dpr));
            self.grid.set_stroke_style_str(GRID_LINE);
            self.grid.set_line_width(1.0);
            self.grid.begin_path();
            let mut x = (top_left.x / step).floor() * step;
            while x <= bottom_right.x {
                let sx = (camera.world_to_screen(Point::new(x, 0.0)).x * self.dpr).round();
                self.grid.move_to(sx, 0.0);
                self.grid.line_to(sx, h);
                x += step;
            }
            let mut y = (top_left.y / step).floor() * step;
            while y <= bottom_right.y {
                let sy = (camera.world_to_screen(Point::new(0.0, y)).y * self.dpr).round();
                self.grid.move_to(0.0, sy);
                self.grid.line_to(w, sy);
                y += step;
            }
            self.grid.stroke();
        }
        self.grid.restore();
    }

    fn draw_overlay(&mut self) {
        self.draw_grid();
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

        // Bend handles on a selected connector: filled = existing waypoint,
        // hollow = drag to add one.
        for handle in self.core.arrow_path_handles() {
            let screen = self.core.camera.world_to_screen(handle.point());
            let (x, y) = (screen.x * self.dpr, screen.y * self.dpr);
            let existing = matches!(handle, ArrowHandle::Waypoint { .. });
            let radius = if existing { 4.0 } else { 3.5 } * self.dpr;
            self.overlay.begin_path();
            let _ = self.overlay.arc(x, y, radius, 0.0, std::f64::consts::TAU);
            if existing {
                self.overlay.set_fill_style_str(ACCENT);
                self.overlay.fill();
            } else {
                self.overlay.set_fill_style_str(BACKGROUND);
                self.overlay.fill();
            }
            self.overlay.set_stroke_style_str(ACCENT);
            self.overlay.set_line_width(self.dpr * 1.5);
            self.overlay.stroke();
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

        // The agent's highlight: an expanding, fading ring around one element.
        if let Some(pulse) = self.pulse {
            let elapsed = now_ms().saturating_sub(pulse.started_ms) as f64;
            let t = (elapsed / pulse.duration_ms as f64).clamp(0.0, 1.0);
            let grow = 6.0 + 26.0 * t;
            let alpha = (1.0 - t) * (1.0 - t);
            let a = camera.world_to_screen(Point::new(pulse.bounds.x, pulse.bounds.y));
            let b = camera.world_to_screen(Point::new(pulse.bounds.max_x(), pulse.bounds.max_y()));
            let r = Rect::new(
                a.x - grow,
                a.y - grow,
                (b.x - a.x) + 2.0 * grow,
                (b.y - a.y) + 2.0 * grow,
            );
            self.overlay
                .set_stroke_style_str(&format!("rgba(217, 142, 50, {alpha:.3})"));
            self.overlay.set_line_width(2.5 * self.dpr);
            self.overlay.stroke_rect(
                r.x * self.dpr,
                r.y * self.dpr,
                r.w * self.dpr,
                r.h * self.dpr,
            );
        }

        // The agent's caption: a bar at the bottom of the viewport.
        if let Some(caption) = self
            .presentation
            .as_ref()
            .and_then(|presentation| presentation.caption.clone())
        {
            let font_size = 22.0 * self.dpr;
            self.overlay
                .set_font(&format!("{font_size}px ComicShanns, sans-serif"));
            self.overlay.set_text_align("center");
            self.overlay.set_text_baseline("middle");
            let text_width = self
                .overlay
                .measure_text(&caption)
                .map(|metrics| metrics.width())
                .unwrap_or(0.0);
            let bar_w = (text_width + 56.0 * self.dpr).min(w - 24.0);
            let bar_h = 46.0 * self.dpr;
            let x = (w - bar_w) / 2.0;
            let y = h - bar_h - 32.0 * self.dpr;
            self.overlay.set_fill_style_str("rgba(26, 24, 21, 0.92)");
            self.overlay.fill_rect(x, y, bar_w, bar_h);
            self.overlay.set_stroke_style_str(ACCENT);
            self.overlay.set_line_width(self.dpr);
            self.overlay.stroke_rect(x, y, bar_w, bar_h);
            self.overlay.set_fill_style_str("#ece4d6");
            let _ = self.overlay.fill_text(&caption, w / 2.0, y + bar_h / 2.0);
        }
        self.overlay.restore();
    }
}

/// Load the workspace's autosaved document, if any (the shell calls this at
/// boot).
#[wasm_bindgen]
pub async fn load_saved_json(workspace: Option<String>) -> Option<String> {
    storage::load_document(workspace.as_deref().unwrap_or("default"))
        .await
        .ok()
        .flatten()
}

/// Names of workspace autosaves in this browser.
#[wasm_bindgen]
pub async fn list_saved_workspaces() -> String {
    serde_json::to_string(&storage::list_workspaces().await.unwrap_or_default())
        .unwrap_or_else(|_| "[]".into())
}

/// Await a save before leaving or managing a workspace.
#[wasm_bindgen]
pub async fn save_workspace_json(workspace: String, json: String) -> Result<(), JsValue> {
    canvas_core::serialization::from_json(&json)
        .map_err(|error| JsValue::from_str(&error.to_string()))?;
    storage::save_document(&json, &workspace)
        .await
        .map_err(|error| JsValue::from_str(&error))
}

#[wasm_bindgen]
pub async fn delete_saved_workspace(workspace: String) -> Result<(), JsValue> {
    storage::delete_workspace(&workspace)
        .await
        .map_err(|error| JsValue::from_str(&error))
}

/// The shape-library catalog (JSON) for the panel.
#[wasm_bindgen]
pub fn glyph_catalog() -> String {
    serde_json::to_string(&canvas_core::glyph_catalog()).unwrap_or_else(|_| "[]".into())
}

/// Whether `outer` fully contains `inner`. Used for the renderer's
/// contains-all fast path against the scene's cached union.
fn viewport_contains(outer: &Rect, inner: &Rect) -> bool {
    outer.x <= inner.x
        && outer.y <= inner.y
        && outer.max_x() >= inner.max_x()
        && outer.max_y() >= inner.max_y()
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
