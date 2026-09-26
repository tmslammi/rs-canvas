//! canvas-wasm: the thin wasm-bindgen façade over canvas-core.

use std::collections::BTreeSet;

use canvas_core::{
    demo_document, Affine, Camera, Document, DrawOp, ElementId, FillStyle, History,
    NavigationState, PathOp, Point, Renderer, Scene, StrokeStyle,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement};

const BACKGROUND: &str = "#f8f7f4";

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

    fn clear_rect(&mut self, rect: canvas_core::Rect, color: &str) {
        self.ctx.save();
        let _ = self.ctx.set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0);
        self.ctx.set_fill_style_str(color);
        self.ctx
            .fill_rect(rect.x, rect.y, rect.w.max(1.0), rect.h.max(1.0));
        self.ctx.restore();
    }

    fn draw_path(&mut self, op: &PathOp) {
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

    fn draw_line_batch(&mut self, segments: &[[canvas_core::Point; 2]], stroke: &StrokeStyle) {
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
    canvas: HtmlCanvasElement,
    renderer: Canvas2dRenderer,
    camera: Camera,
    dpr: f64,
    doc: Document,
    scene: Scene,
    nav: NavigationState,
    /// Undo/redo stacks; wired to the UI in the interaction ticket.
    #[allow(dead_code)]
    history: History,
}

#[wasm_bindgen]
impl Editor {
    #[wasm_bindgen(constructor)]
    pub fn new(canvas: HtmlCanvasElement) -> Result<Editor, JsValue> {
        console_error_panic_hook::set_once();
        let ctx = canvas
            .get_context("2d")?
            .ok_or_else(|| JsValue::from_str("canvas has no 2d context"))?
            .dyn_into::<CanvasRenderingContext2d>()?;
        let dpr = web_sys::window()
            .map(|w| w.device_pixel_ratio())
            .unwrap_or(1.0);
        let doc = demo_document();
        let mut scene = Scene::new();
        scene.rebuild_all(&doc);
        Ok(Editor {
            canvas,
            renderer: Canvas2dRenderer { ctx },
            camera: Camera::default(),
            dpr,
            doc,
            scene,
            nav: NavigationState::new(),
            history: History::new(),
        })
    }

    /// Match the backing store to the canvas's layout size.
    pub fn resize(&self) {
        let w = self.canvas.client_width() as f64;
        let h = self.canvas.client_height() as f64;
        self.canvas
            .set_width((w * self.dpr).round().max(1.0) as u32);
        self.canvas
            .set_height((h * self.dpr).round().max(1.0) as u32);
    }

    /// The camera's world scroll offset.
    pub fn scroll(&self) -> Vec<f64> {
        vec![self.camera.scroll.x, self.camera.scroll.y]
    }

    pub fn zoom(&self) -> f64 {
        self.camera.zoom
    }

    pub fn element_count(&self) -> usize {
        self.doc.ordered_live().len()
    }

    /// Draw everything visible for this frame.
    pub fn render(&mut self) {
        let viewport = self.camera.viewport(self.css_width(), self.css_height());
        self.renderer.begin_frame();
        self.renderer.clear(BACKGROUND);
        self.renderer
            .set_transform(self.camera.transform().then(Affine::scale(self.dpr)));
        for layer in self.scene.visible(viewport) {
            self.renderer.draw_ops(&layer.ops);
        }
        self.renderer.end_frame();
    }

    /// Rebuild the changed elements' layers and repaint only their dirty
    /// rectangles.
    pub fn apply_changes(&mut self, changed: Vec<String>) {
        let changed: BTreeSet<ElementId> = changed.into_iter().collect();
        let dirty = self.scene.update(&self.doc, &changed);
        self.paint_dirty(&dirty);
    }

    /// Screen point -> world point, through the current camera.
    pub fn screen_to_world(&self, x: f64, y: f64) -> Vec<f64> {
        let p = self.camera.screen_to_world(canvas_core::Point::new(x, y));
        vec![p.x, p.y]
    }

    /// Start a pointer interaction. Button: 0 left, 1 middle, 2 right.
    pub fn pointer_down(&mut self, x: f64, y: f64, button: u8) {
        self.nav.pointer_down(Point::new(x, y), button);
    }

    pub fn pointer_move(&mut self, x: f64, y: f64) {
        if self.nav.pointer_move(&mut self.camera, Point::new(x, y)) {
            self.render();
        }
    }

    pub fn pointer_up(&mut self, x: f64, y: f64, button: u8) {
        let _ = (x, y);
        self.nav.pointer_up(button);
    }

    /// Wheel/trackpad event. Ctrl means pinch-zoom.
    pub fn wheel(&mut self, x: f64, y: f64, dx: f64, dy: f64, ctrl: bool) {
        self.nav
            .wheel(&mut self.camera, Point::new(x, y), dx, dy, ctrl);
        self.render();
    }

    /// Pinch zoom from a shell-level touch gesture.
    pub fn zoom_at(&mut self, x: f64, y: f64, factor: f64) {
        self.camera.zoom_at(Point::new(x, y), factor);
        self.render();
    }

    /// Keyboard input. Returns true when the key was handled.
    pub fn key_down(&mut self, key: &str, shift: bool) -> bool {
        match key {
            " " => {
                self.nav.set_space(true);
                true
            }
            "1" if shift => {
                self.zoom_to_fit();
                true
            }
            "0" if shift => {
                self.reset_zoom();
                true
            }
            _ => false,
        }
    }

    pub fn key_up(&mut self, key: &str) {
        if key == " " {
            self.nav.set_space(false);
        }
    }

    /// Reset held state when the window loses focus.
    pub fn blur(&mut self) {
        self.nav.set_space(false);
        self.nav.cancel();
    }

    /// The CSS cursor the shell should apply.
    pub fn cursor(&self) -> String {
        if self.nav.is_panning() {
            "grabbing".into()
        } else if self.nav.space_held() {
            "grab".into()
        } else {
            "default".into()
        }
    }

    /// Fit the drawing into the viewport (Shift+1).
    pub fn zoom_to_fit(&mut self) {
        if let Some(bounds) = self.scene.bounds() {
            self.camera
                .zoom_to_fit(bounds, (self.css_width(), self.css_height()), 48.0);
        } else {
            self.reset_zoom();
            return;
        }
        self.render();
    }

    /// Snap back to 100% around the viewport centre (Shift+0).
    pub fn reset_zoom(&mut self) {
        let center = Point::new(self.css_width() / 2.0, self.css_height() / 2.0);
        self.camera.set_zoom_at(center, 1.0);
        self.render();
    }
}

impl Editor {
    fn css_width(&self) -> f64 {
        self.canvas.client_width() as f64
    }

    fn css_height(&self) -> f64 {
        self.canvas.client_height() as f64
    }

    fn paint_dirty(&mut self, dirty: &[canvas_core::Rect]) {
        let viewport = self.camera.viewport(self.css_width(), self.css_height());
        let visible_dirty: Vec<canvas_core::Rect> = dirty
            .iter()
            .filter(|r| r.intersects(&viewport))
            .copied()
            .collect();
        if visible_dirty.is_empty() {
            return;
        }
        self.renderer.begin_frame();
        for rect in &visible_dirty {
            let top_left = self
                .camera
                .world_to_screen(canvas_core::Point::new(rect.x, rect.y));
            let bottom_right = self
                .camera
                .world_to_screen(canvas_core::Point::new(rect.max_x(), rect.max_y()));
            let screen = canvas_core::Rect::from_points(top_left, bottom_right).expand(1.0);
            let device = canvas_core::Rect::new(
                screen.x * self.dpr,
                screen.y * self.dpr,
                screen.w * self.dpr,
                screen.h * self.dpr,
            );
            self.renderer.clear_rect(device, BACKGROUND);
        }
        self.renderer
            .set_transform(self.camera.transform().then(Affine::scale(self.dpr)));
        for layer in self.scene.layers.iter() {
            let intersects_dirty = visible_dirty
                .iter()
                .any(|rect| layer.bounds.intersects(rect));
            if intersects_dirty && layer.bounds.intersects(&viewport) {
                self.renderer.draw_ops(&layer.ops);
            }
        }
        self.renderer.end_frame();
    }
}
