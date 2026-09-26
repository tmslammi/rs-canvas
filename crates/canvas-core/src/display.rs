//! The display list the renderer consumes, and the renderer seam.

use crate::geometry::{Affine, Point};

/// Dash pattern for strokes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Dash {
    #[default]
    Solid,
    Dashed,
    Dotted,
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
    Solid { color: String },
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
    fn draw_path(&mut self, op: &PathOp);
}
