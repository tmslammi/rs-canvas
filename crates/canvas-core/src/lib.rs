//! canvas-core: the pure-Rust heart of rs-canvas.
//!
//! Owns the document model, interaction logic and rendering pipeline.
//! No browser dependencies; everything here is testable natively.

pub mod camera;
pub mod demo;
pub mod display;
pub mod geometry;

pub use camera::Camera;
pub use demo::demo_display_list;
pub use display::{Dash, DisplayList, DrawOp, FillStyle, PathOp, Renderer, StrokeStyle};
pub use geometry::{Affine, Point, Rect};
