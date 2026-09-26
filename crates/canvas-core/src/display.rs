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
    fn draw_path(&mut self, op: &PathOp);
    fn draw_line_batch(&mut self, segments: &[[Point; 2]], stroke: &StrokeStyle);
}
