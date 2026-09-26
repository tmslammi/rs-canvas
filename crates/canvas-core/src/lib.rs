//! canvas-core: the pure-Rust heart of rs-canvas.
//!
//! Owns the document model, interaction logic and rendering pipeline.
//! No browser dependencies; everything here is testable natively.

pub mod camera;
pub mod delta;
pub mod demo;
pub mod display;
pub mod editor;
pub mod geometry;
pub mod history;
pub mod hit;
pub mod id;
pub mod index;
pub mod merge;
pub mod model;
pub mod nav;
pub mod rough;
pub mod scene;
pub mod serialization;
pub mod text;
pub mod tool;

pub use camera::Camera;
pub use delta::{Delta, EditError};
pub use demo::demo_document;
pub use display::{Dash, DisplayList, DrawOp, FillStyle, PathOp, Renderer, StrokeStyle, TextOp};
pub use editor::{
    element_style, element_style_mut, translate_element, Editor, Effects, Handle, Modifiers,
    ShellAction,
};
pub use geometry::{Affine, Point, Rect};
pub use history::{History, Transaction};
pub use hit::{elements_in_rect, hit_test};
pub use merge::reconcile;
pub use model::{
    Binding, Document, Element, ElementId, ElementKind, Endpoint, FillPattern, Routing, ShapeKind,
    StrokeDash, Style, StylePatch, TextAlign, SCHEMA_VERSION,
};
pub use nav::NavigationState;
pub use scene::{Scene, SceneLayer};
pub use text::{layout_text, ApproxMeasurer, TextLayout, TextMeasurer};
pub use tool::Tool;
