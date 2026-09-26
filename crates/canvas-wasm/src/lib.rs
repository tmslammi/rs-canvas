//! canvas-wasm: the thin wasm-bindgen façade over canvas-core.

use canvas_core::{
    demo_display_list, Affine, Camera, DisplayList, DrawOp, FillStyle, PathOp, Point, Renderer,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement};

/// A Canvas2D implementation of the core's renderer seam.
struct Canvas2dRenderer {
    ctx: CanvasRenderingContext2d,
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
                FillStyle::Solid { color } => self.ctx.set_fill_style_str(color),
            }
            self.ctx.fill();
        }
        if let Some(stroke) = &op.stroke {
            self.ctx.set_stroke_style_str(&stroke.color);
            self.ctx.set_line_width(stroke.width);
            self.ctx.set_global_alpha(stroke.opacity);
            self.ctx.stroke();
            self.ctx.set_global_alpha(1.0);
        }
    }
}

/// The editor handle the shell talks to.
#[wasm_bindgen]
pub struct Editor {
    canvas: HtmlCanvasElement,
    renderer: Canvas2dRenderer,
    camera: Camera,
    list: DisplayList,
    dpr: f64,
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
        Ok(Editor {
            canvas,
            renderer: Canvas2dRenderer { ctx },
            camera: Camera::default(),
            list: demo_display_list(),
            dpr,
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

    /// Draw the current display list for this frame.
    pub fn render(&mut self) {
        self.renderer.begin_frame();
        self.renderer.clear("#f8f7f4");
        self.renderer
            .set_transform(self.camera.transform().then(Affine::scale(self.dpr)));
        for op in &self.list.ops {
            match op {
                DrawOp::Path(path) => self.renderer.draw_path(path),
            }
        }
        self.renderer.end_frame();
    }

    /// Screen point -> world point, through the current camera.
    pub fn screen_to_world(&self, x: f64, y: f64) -> Vec<f64> {
        let p = self.camera.screen_to_world(Point::new(x, y));
        vec![p.x, p.y]
    }
}
