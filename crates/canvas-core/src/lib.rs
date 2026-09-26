//! canvas-core: the pure-Rust heart of rs-canvas.
//!
//! Owns the document model, interaction logic and rendering pipeline.
//! No browser dependencies; everything here is testable natively.

pub mod camera;
pub mod demo;
pub mod display;
pub mod geometry;
pub mod id;
pub mod index;
pub mod model;
pub mod serialization;

pub use camera::Camera;
pub use demo::demo_display_list;
pub use display::{Dash, DisplayList, DrawOp, FillStyle, PathOp, Renderer, StrokeStyle};
pub use geometry::{Affine, Point, Rect};
pub use model::{
    Binding, Document, Element, ElementId, ElementKind, Endpoint, FillPattern, Routing, ShapeKind,
    StrokeDash, Style, TextAlign, SCHEMA_VERSION,
};
