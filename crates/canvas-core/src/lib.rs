//! canvas-core: the pure-Rust heart of rs-canvas.
//!
//! Owns the document model, interaction logic and rendering pipeline.
//! No browser dependencies; everything here is testable natively.

pub mod benchmark;
mod binary;
pub mod bulk;
pub mod camera;
pub mod commands;
pub mod connector;
pub mod delta;
pub mod demo;
pub mod describe;
pub mod display;
pub mod editor;
pub mod envelope;
/// Compatibility alias for the pre-commands module name.
#[doc(hidden)]
pub mod edit {
    pub use crate::commands::*;
}
pub mod geometry;
pub mod glyphs;
pub mod history;
pub mod hit;
pub mod id;
pub mod index;
pub mod layout;
pub mod merge;
pub mod model;
pub mod nav;
pub mod patch;
pub mod relations;
pub mod rough;
pub mod scene;
pub mod serialization;
pub mod spatial;
pub mod svg;
pub mod sync;
pub mod text;
pub mod tool;
pub mod wire;

pub use benchmark::benchmark_fixture;
pub use bulk::{BulkStats, BulkStyle, GridSpec, KeyChain, LinkMode, ShapeCode, ShapeSpec};
pub use camera::{Camera, CameraTween};
pub use connector::{anchor_points, nearest_anchor, resolve_endpoint, route, AnchorSide};
pub use delta::{Delta, EditError, Tombstone};
pub use demo::demo_document;
pub use describe::{
    describe, describe_element, describe_element_with, describe_with, IdMap, UuidMap,
};
pub use display::{Dash, DisplayList, DrawOp, FillStyle, PathOp, Renderer, StrokeStyle, TextOp};
pub use editor::{
    element_style, element_style_mut, translate_element, ArrowHandle, Editor, Effects, Handle,
    Modifiers, ShellAction, GRID_SIZE,
};
pub use geometry::{Affine, Point, Rect};
pub use glyphs::{catalog as glyph_catalog, lookup as glyph_lookup, GlyphCategory, GlyphInfo};
pub use history::{freshen_element, inverse_for_replay, History, Transaction};
pub use hit::{
    elements_in_rect, elements_in_rect_indexed, hit_test, hit_test_indexed, hit_test_raw_indexed,
};
pub use merge::{merge_elements, reconcile, ElementChange, MergeOutcome, ReconcileOutcome};
pub use model::{
    Binding, Document, Element, ElementId, ElementKind, Endpoint, FillPattern, Routing, ShapeKind,
    StrokeDash, Style, StylePatch, TextAlign, SCHEMA_VERSION,
};
pub use nav::NavigationState;
pub use patch::{merge_element, Patch};
pub use scene::{Scene, SceneLayer};
pub use spatial::HitIndex;
pub use spatial::ObstacleIndex;
pub use sync::{PageFrame, PurgeLedger};
pub use text::{layout_text, ApproxMeasurer, TextLayout, TextMeasurer};
pub use tool::Tool;
