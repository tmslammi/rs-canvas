//! The display list the renderer consumes, and the renderer seam.
//!
//! Core turns a Document into a Scene of per-element layers, each holding
//! drawing instructions in world coordinates. Backends only implement the
//! `Renderer` trait.

use crate::geometry::{Affine, Point, Rect};

/// Dash pattern for strokes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Dash {
    #[default]
    Solid,
    Dashed,
    Dotted,
}

impl Dash {
    /// Dash pattern in world units (`[]` for solid).
    pub fn pattern(self) -> &'static [f64] {
        match self {
            Dash::Solid => &[],
            Dash::Dashed => &[8.0, 8.0],
            Dash::Dotted => &[1.5, 4.0],
        }
    }
}

/// How a path's outline is stroked.
#[derive(Clone, Debug, PartialEq)]
pub struct StrokeStyle {
    pub color: String,
    pub width: f64,
    pub dash: Dash,
    pub opacity: f64,
}

impl StrokeStyle {
    pub fn solid(color: impl Into<String>, width: f64) -> Self {
        Self {
            color: color.into(),
            width,
            dash: Dash::Solid,
            opacity: 1.0,
        }
    }
}

/// How a path's interior is filled.
#[derive(Clone, Debug, PartialEq)]
pub enum FillStyle {
    Solid { color: String, opacity: f64 },
}

/// A path of straight segments in world coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct PathOp {
    pub points: Vec<Point>,
    pub closed: bool,
    pub stroke: Option<StrokeStyle>,
    pub fill: Option<FillStyle>,
}

/// A run of laid-out text in world coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct TextOp {
    /// Anchor x: left edge, centre or right edge depending on `align`.
    pub x: f64,
    /// Baseline y.
    pub y: f64,
    pub text: String,
    pub font_size: f64,
    pub color: String,
    pub opacity: f64,
    pub align: crate::model::TextAlign,
}

/// One drawing instruction.
#[derive(Clone, Debug, PartialEq)]
pub enum DrawOp {
    Path(PathOp),
    /// Many disjoint two-point segments sharing one stroke: one canvas path,
    /// one stroke call. Used for hachure fills.
    LineBatch {
        segments: Vec<[Point; 2]>,
        stroke: StrokeStyle,
    },
    Text(TextOp),
}

/// A frame's worth of drawing instructions in world coordinates.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DisplayList {
    pub ops: Vec<DrawOp>,
}

impl DisplayList {
    pub fn push_path(&mut self, op: PathOp) {
        self.ops.push(DrawOp::Path(op));
    }

    pub fn push(&mut self, op: DrawOp) {
        self.ops.push(op);
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

/// The seam every rendering backend implements.
pub trait Renderer {
    fn begin_frame(&mut self);
    fn end_frame(&mut self);
    /// Set the world-to-device transform for subsequent drawing.
    fn set_transform(&mut self, transform: Affine);
    /// Fill the whole surface, in device space, ignoring the current transform.
    fn clear(&mut self, color: &str);
    /// Fill one device-space rectangle, ignoring the current transform.
    fn clear_rect(&mut self, rect: Rect, color: &str);
    /// Intersect the clip region with the union of these device-space
    /// rectangles, ignoring the current transform. Partial repaints draw
    /// whole layers clipped to the repainted region so a layer can never
    /// paint over untouched pixels (which would let a lower layer cover the
    /// higher layers around it).
    fn clip_rects(&mut self, rects: &[Rect]);
    /// Fill a batch of rectangles in the current transform space with one
    /// fill call. This is the LOD overview primitive: one batch per palette
    /// color instead of one path and fill per element. Backends paint
    /// sub-pixel rectangles at a minimum hairline (Canvas2D uses 0.5 units)
    /// so dense overviews read solid instead of sparkling.
    fn fill_rects(&mut self, rects: &[Rect], color: &str);
    fn draw_path(&mut self, op: &PathOp);
    fn draw_line_batch(&mut self, segments: &[[Point; 2]], stroke: &StrokeStyle);
    fn draw_text(&mut self, op: &TextOp);
}

/// One call recorded by [`RecordingRenderer`], in the order it happened.
#[derive(Clone, Debug, PartialEq)]
pub enum RecordedCall {
    BeginFrame,
    EndFrame,
    SetTransform(Affine),
    Clear {
        color: String,
    },
    ClearRect {
        rect: Rect,
        color: String,
    },
    ClipRects(Vec<Rect>),
    FillRects {
        rects: Vec<Rect>,
        color: String,
    },
    DrawPath(PathOp),
    DrawLineBatch {
        segments: Vec<[Point; 2]>,
        stroke: StrokeStyle,
    },
    DrawText(TextOp),
}

/// A browser-free [`Renderer`] that records every call for native tests.
///
/// Frames are not nested: `BeginFrame`/`EndFrame` appear in `calls` exactly
/// where they were issued, so a test can assert both frame boundaries and
/// draw order. The recorder clones the ops it receives; it is a test
/// instrument, not a paint path.
#[derive(Clone, Debug, Default)]
pub struct RecordingRenderer {
    calls: Vec<RecordedCall>,
}

impl RecordingRenderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every recorded call, in order.
    pub fn calls(&self) -> &[RecordedCall] {
        &self.calls
    }

    /// Drop all recorded calls so the renderer can record another scenario.
    pub fn clear(&mut self) {
        self.calls.clear();
    }

    pub fn len(&self) -> usize {
        self.calls.len()
    }

    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }
}

impl Renderer for RecordingRenderer {
    fn begin_frame(&mut self) {
        self.calls.push(RecordedCall::BeginFrame);
    }

    fn end_frame(&mut self) {
        self.calls.push(RecordedCall::EndFrame);
    }

    fn set_transform(&mut self, transform: Affine) {
        self.calls.push(RecordedCall::SetTransform(transform));
    }

    fn clear(&mut self, color: &str) {
        self.calls.push(RecordedCall::Clear {
            color: color.to_string(),
        });
    }

    fn clear_rect(&mut self, rect: Rect, color: &str) {
        self.calls.push(RecordedCall::ClearRect {
            rect,
            color: color.to_string(),
        });
    }

    fn clip_rects(&mut self, rects: &[Rect]) {
        self.calls.push(RecordedCall::ClipRects(rects.to_vec()));
    }

    fn fill_rects(&mut self, rects: &[Rect], color: &str) {
        self.calls.push(RecordedCall::FillRects {
            rects: rects.to_vec(),
            color: color.to_string(),
        });
    }

    fn draw_path(&mut self, op: &PathOp) {
        self.calls.push(RecordedCall::DrawPath(op.clone()));
    }

    fn draw_line_batch(&mut self, segments: &[[Point; 2]], stroke: &StrokeStyle) {
        self.calls.push(RecordedCall::DrawLineBatch {
            segments: segments.to_vec(),
            stroke: stroke.clone(),
        });
    }

    fn draw_text(&mut self, op: &TextOp) {
        self.calls.push(RecordedCall::DrawText(op.clone()));
    }
}
