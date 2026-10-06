//! The document model: a flat store of Elements keyed by ID.
//!
//! Every element carries merge metadata (`version`, `versionNonce`), a
//! fractional `index` for ordering and a soft-delete flag. Nothing here is
//! view state.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::geometry::{Point, Rect};
use crate::id::{new_element_id, new_version_nonce};
use crate::index;

pub type ElementId = String;

/// The current schema version written to files and autosaves.
pub const SCHEMA_VERSION: u32 = 1;

/// Shape family for shape elements.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShapeKind {
    Rectangle,
    Ellipse,
    Diamond,
    /// A sticky note: a filled, slightly rounded square with bound text.
    Sticky,
}

/// Fill treatment for shapes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FillPattern {
    Hachure,
    CrossHatch,
    Solid,
}

/// Stroke dash pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StrokeDash {
    Solid,
    Dashed,
    Dotted,
}

/// Connector routing style.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Routing {
    Straight,
    Elbow,
    Curved,
}

/// Horizontal alignment for text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextAlign {
    Left,
    Center,
    Right,
}

/// Visual style shared by every element type that draws.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Style {
    pub stroke_color: String,
    pub fill_color: String,
    pub fill_style: FillPattern,
    pub stroke_width: f64,
    /// 0 = architect, 1 = artist, 2 = cartoonist.
    pub roughness: u8,
    pub dash: StrokeDash,
    pub opacity: f64,
    pub font_size: f64,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            stroke_color: "#ece4d6".into(),
            fill_color: "transparent".into(),
            fill_style: FillPattern::Hachure,
            stroke_width: 2.0,
            roughness: 1,
            dash: StrokeDash::Solid,
            opacity: 1.0,
            font_size: 20.0,
        }
    }
}

/// A partial style update coming from the style panel.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StylePatch {
    pub stroke_color: Option<String>,
    pub fill_color: Option<String>,
    pub fill_style: Option<FillPattern>,
    pub stroke_width: Option<f64>,
    pub roughness: Option<u8>,
    pub dash: Option<StrokeDash>,
    pub opacity: Option<f64>,
    pub font_size: Option<f64>,
}

impl StylePatch {
    pub fn apply_to(&self, style: &mut Style) {
        if let Some(value) = &self.stroke_color {
            style.stroke_color = value.clone();
        }
        if let Some(value) = &self.fill_color {
            style.fill_color = value.clone();
        }
        if let Some(value) = self.fill_style {
            style.fill_style = value;
        }
        if let Some(value) = self.stroke_width.filter(|v| v.is_finite()) {
            style.stroke_width = value.clamp(0.5, 64.0);
        }
        if let Some(value) = self.roughness {
            style.roughness = value.min(2);
        }
        if let Some(value) = self.dash {
            style.dash = value;
        }
        if let Some(value) = self.opacity.filter(|v| v.is_finite()) {
            style.opacity = value.clamp(0.05, 1.0);
        }
        if let Some(value) = self.font_size.filter(|v| v.is_finite()) {
            style.font_size = value.clamp(8.0, 96.0);
        }
    }

    pub fn is_empty(&self) -> bool {
        self == &StylePatch::default()
    }
}

/// One end of a connector: a world point, optionally bound to an element.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Endpoint {
    pub x: f64,
    pub y: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<Binding>,
}

impl Endpoint {
    pub fn free(x: f64, y: f64) -> Self {
        Self {
            x,
            y,
            binding: None,
        }
    }

    pub fn point(&self) -> Point {
        Point::new(self.x, self.y)
    }
}

/// A connection to an element at a normalized focus point within its bounds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Binding {
    pub element_id: ElementId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<Point>,
}

/// Type-specific element data.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ElementKind {
    Shape {
        shape: ShapeKind,
        rect: Rect,
        style: Style,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bound_text_id: Option<ElementId>,
    },
    Freehand {
        points: Vec<Point>,
        #[serde(default)]
        pressures: Vec<f64>,
        style: Style,
    },
    Glyph {
        /// Library glyph identifier, e.g. `cloud_run`.
        glyph: String,
        rect: Rect,
        style: Style,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caption_id: Option<ElementId>,
    },
    Arrow {
        start: Endpoint,
        end: Endpoint,
        routing: Routing,
        start_arrowhead: bool,
        end_arrowhead: bool,
        style: Style,
        /// User-placed bends; the route passes through them in order. Empty
        /// means fully automated routing.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        waypoints: Vec<Point>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label_id: Option<ElementId>,
    },
    Text {
        rect: Rect,
        text: String,
        align: TextAlign,
        /// True when the width is user-fixed (drag-created); false = auto width.
        fixed_width: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        container_id: Option<ElementId>,
        style: Style,
    },
    /// An infinite source of blank sticky notes: drag notes out of it with
    /// the sticky tool; the pile itself never empties.
    StickyStack {
        rect: Rect,
        style: Style,
    },
    Frame {
        rect: Rect,
        name: String,
    },
}

/// A single object in a document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Element {
    pub id: ElementId,
    pub version: u64,
    pub version_nonce: u32,
    /// Fractional z/order key.
    pub index: String,
    pub is_deleted: bool,
    /// Unix milliseconds of the last local write.
    pub updated: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<ElementId>,
    #[serde(flatten)]
    pub kind: ElementKind,
}

impl Element {
    /// Build a fresh element. `id`, `index` and `now_ms` are supplied by the
    /// caller so core stays clock-free and tests stay deterministic.
    pub fn new(kind: ElementKind, index: String, now_ms: u64) -> Self {
        Self {
            id: new_element_id(),
            version: 1,
            version_nonce: new_version_nonce(),
            index,
            is_deleted: false,
            updated: now_ms,
            frame_id: None,
            kind,
        }
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    /// Record a local write: bump version, fresh nonce, touch timestamp.
    pub fn touch(&mut self, now_ms: u64) {
        self.version += 1;
        self.version_nonce = new_version_nonce();
        self.updated = now_ms;
    }

    /// Compare everything except merge metadata (version, nonce, timestamp).
    pub fn content_eq(&self, other: &Element) -> bool {
        self.is_deleted == other.is_deleted
            && self.frame_id == other.frame_id
            && self.index == other.index
            && self.kind == other.kind
    }

    /// The world-space bounding box this element occupies, ignoring styling.
    pub fn bounds(&self) -> Rect {
        match &self.kind {
            ElementKind::Shape { rect, .. }
            | ElementKind::Glyph { rect, .. }
            | ElementKind::Text { rect, .. }
            | ElementKind::StickyStack { rect, .. }
            | ElementKind::Frame { rect, .. } => *rect,
            ElementKind::Freehand { points, .. } => {
                let mut iter = points.iter();
                let Some(first) = iter.next() else {
                    return Rect::ZERO;
                };
                let mut min_x = first.x;
                let mut min_y = first.y;
                let mut max_x = first.x;
                let mut max_y = first.y;
                for p in iter {
                    min_x = min_x.min(p.x);
                    min_y = min_y.min(p.y);
                    max_x = max_x.max(p.x);
                    max_y = max_y.max(p.y);
                }
                Rect::new(min_x, min_y, max_x - min_x, max_y - min_y)
            }
            ElementKind::Arrow {
                start,
                end,
                waypoints,
                ..
            } => {
                let mut bounds = Rect::from_points(start.point(), end.point());
                for waypoint in waypoints {
                    bounds = bounds.union(&Rect::from_points(*waypoint, *waypoint));
                }
                bounds
            }
        }
    }

    pub fn is_frame(&self) -> bool {
        matches!(self.kind, ElementKind::Frame { .. })
    }

    pub fn is_sticky_stack(&self) -> bool {
        matches!(self.kind, ElementKind::StickyStack { .. })
    }

    /// Whether an arrow endpoint may bind to this element. Shapes, glyphs,
    /// frames and standalone text are targets; bound text belongs to its
    /// container and connectors/strokes/stacks can never be targets.
    pub fn is_bind_target(&self) -> bool {
        match &self.kind {
            ElementKind::Shape { .. } | ElementKind::Glyph { .. } | ElementKind::Frame { .. } => {
                true
            }
            ElementKind::Text { container_id, .. } => container_id.is_none(),
            _ => false,
        }
    }

    pub fn is_sticky(&self) -> bool {
        matches!(
            self.kind,
            ElementKind::Shape {
                shape: ShapeKind::Sticky,
                ..
            }
        )
    }

    pub fn is_text(&self) -> bool {
        matches!(self.kind, ElementKind::Text { .. })
    }
}

/// A complete drawing. Ordering is derived from each element's `index`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Document {
    pub schema_version: u32,
    pub elements: BTreeMap<ElementId, Element>,
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

impl Document {
    pub fn new() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            elements: BTreeMap::new(),
        }
    }

    /// Add an element on top of the current stack, returning its ID.
    pub fn add(&mut self, kind: ElementKind, now_ms: u64) -> ElementId {
        let index = match self.top_index() {
            Some(top) => index::key_between(Some(&top), None).expect("index space exhausted"),
            None => index::key_between(None, None).expect("first key"),
        };
        self.add_at(kind, index, now_ms)
    }

    /// Add an element at a specific order key (used by merge and tests).
    pub fn add_at(&mut self, kind: ElementKind, index: String, now_ms: u64) -> ElementId {
        let element = Element::new(kind, index, now_ms);
        let id = element.id.clone();
        self.elements.insert(id.clone(), element);
        id
    }

    /// Insert an existing element, preserving its identity and metadata.
    pub fn insert(&mut self, element: Element) {
        self.elements.insert(element.id.clone(), element);
    }

    pub fn element(&self, id: &str) -> Option<&Element> {
        self.elements.get(id)
    }

    pub fn element_mut(&mut self, id: &str) -> Option<&mut Element> {
        self.elements.get_mut(id)
    }

    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }

    pub fn len(&self) -> usize {
        self.elements.len()
    }

    /// Every element in z-order, tombstones included, as references.
    pub fn ordered_refs(&self) -> Vec<&Element> {
        let mut elements: Vec<&Element> = self.elements.values().collect();
        elements.sort_unstable_by(|a, b| a.index.cmp(&b.index).then_with(|| a.id.cmp(&b.id)));
        elements
    }

    /// Every element ID in z-order, tombstones included.
    pub fn ordered_ids(&self) -> Vec<ElementId> {
        self.ordered_refs()
            .into_iter()
            .map(|e| e.id.clone())
            .collect()
    }

    /// Live (non-tombstoned) elements in z-order.
    pub fn ordered_live(&self) -> Vec<&Element> {
        self.ordered_refs()
            .into_iter()
            .filter(|e| !e.is_deleted)
            .collect()
    }

    /// Number of live (non-tombstoned) elements.
    pub fn live_count(&self) -> usize {
        self.elements.values().filter(|e| !e.is_deleted).count()
    }

    /// Highest order key currently in the document, tombstones included.
    pub fn top_index(&self) -> Option<String> {
        self.elements
            .values()
            .map(|e| e.index.as_str())
            .max()
            .map(str::to_string)
    }

    /// The largest element bounds, or `None` for an empty document.
    pub fn bounds(&self) -> Option<Rect> {
        self.elements
            .values()
            .filter(|e| !e.is_deleted)
            .map(|e| e.bounds())
            .reduce(|acc, r| acc.union(&r))
    }

    /// Bounds for framing an element: a frame includes its live descendants,
    /// so focusing a container shows everything inside it.
    pub fn framing_bounds(&self, id: &str) -> Option<Rect> {
        let seed = self.element(id).filter(|e| !e.is_deleted)?;
        let mut bounds = seed.bounds();
        if !seed.is_frame() {
            return Some(bounds);
        }
        let mut frontier: Vec<&str> = vec![id];
        while let Some(parent) = frontier.pop() {
            for element in self.ordered_live() {
                if element.frame_id.as_deref() == Some(parent) {
                    bounds = bounds.union(&element.bounds());
                    if element.is_frame() {
                        frontier.push(element.id.as_str());
                    }
                }
            }
        }
        Some(bounds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(x: f64, y: f64) -> ElementKind {
        ElementKind::Shape {
            shape: ShapeKind::Rectangle,
            rect: Rect::new(x, y, 100.0, 60.0),
            style: Style::default(),
            bound_text_id: None,
        }
    }

    #[test]
    fn add_assigns_increasing_index() {
        let mut doc = Document::new();
        let a = doc.add(shape(0.0, 0.0), 1);
        let b = doc.add(shape(10.0, 10.0), 2);
        assert_eq!(doc.ordered_ids(), vec![a.clone(), b.clone()]);
        assert!(doc.element(&a).unwrap().index < doc.element(&b).unwrap().index);
    }

    #[test]
    fn new_elements_start_at_version_one() {
        let mut doc = Document::new();
        let id = doc.add(shape(0.0, 0.0), 7);
        let element = doc.element(&id).unwrap();
        assert_eq!(element.version, 1);
        assert!(!element.is_deleted);
        assert_eq!(element.updated, 7);
    }

    #[test]
    fn touch_bumps_version_and_changes_nonce() {
        let mut doc = Document::new();
        let id = doc.add(shape(0.0, 0.0), 1);
        let before = doc.element(&id).unwrap().clone();
        doc.element_mut(&id).unwrap().touch(9);
        let after = doc.element(&id).unwrap();
        assert_eq!(after.version, before.version + 1);
        assert_ne!(after.version_nonce, before.version_nonce);
        assert_eq!(after.updated, 9);
    }

    #[test]
    fn ordering_is_deterministic_under_equal_index() {
        let mut doc = Document::new();
        let a = doc.add_at(shape(0.0, 0.0), "a0".into(), 1);
        let b = doc.add_at(shape(1.0, 1.0), "a0".into(), 1);
        let mut expected = vec![a, b];
        expected.sort();
        assert_eq!(doc.ordered_ids(), expected);
    }

    #[test]
    fn tombstones_stay_out_of_live_iteration() {
        let mut doc = Document::new();
        let a = doc.add(shape(0.0, 0.0), 1);
        let b = doc.add(shape(1.0, 1.0), 1);
        doc.element_mut(&a).unwrap().is_deleted = true;
        assert_eq!(doc.ordered_ids().len(), 2);
        let live: Vec<&str> = doc.ordered_live().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(live, vec![b.as_str()]);
    }

    #[test]
    fn element_bounds_cover_all_kinds() {
        let mut doc = Document::new();
        let id = doc.add(shape(10.0, 20.0), 1);
        assert_eq!(
            doc.element(&id).unwrap().bounds(),
            Rect::new(10.0, 20.0, 100.0, 60.0)
        );

        let line = doc.add(
            ElementKind::Arrow {
                start: Endpoint::free(0.0, 0.0),
                end: Endpoint::free(50.0, 25.0),
                routing: Routing::Straight,
                start_arrowhead: false,
                end_arrowhead: true,
                style: Style::default(),
                waypoints: Vec::new(),
                label_id: None,
            },
            1,
        );
        assert_eq!(
            doc.element(&line).unwrap().bounds(),
            Rect::new(0.0, 0.0, 50.0, 25.0)
        );
    }

    #[test]
    fn document_bounds_union() {
        let mut doc = Document::new();
        doc.add(shape(0.0, 0.0), 1);
        doc.add(shape(200.0, 100.0), 1);
        assert_eq!(doc.bounds(), Some(Rect::new(0.0, 0.0, 300.0, 160.0)));
    }

    #[test]
    fn framing_bounds_of_a_frame_include_descendants() {
        let mut doc = Document::new();
        let inner = doc.add(
            ElementKind::Frame {
                rect: Rect::new(400.0, 400.0, 200.0, 200.0),
                name: "inner".into(),
            },
            1,
        );
        let frame = doc.add(
            ElementKind::Frame {
                rect: Rect::new(0.0, 0.0, 400.0, 300.0),
                name: "outer".into(),
            },
            1,
        );
        let child = doc.add(shape(50.0, 50.0), 1);
        let grandchild = doc.add(shape(420.0, 420.0), 1);
        let _outside = doc.add(shape(1_000.0, 1_000.0), 1);
        doc.element_mut(&child).unwrap().frame_id = Some(frame.clone());
        doc.element_mut(&inner).unwrap().frame_id = Some(frame.clone());
        doc.element_mut(&grandchild).unwrap().frame_id = Some(inner);

        assert_eq!(
            doc.framing_bounds(&frame),
            Some(Rect::new(0.0, 0.0, 600.0, 600.0))
        );
        assert_eq!(
            doc.framing_bounds(&child),
            Some(Rect::new(50.0, 50.0, 100.0, 60.0))
        );
        assert_eq!(doc.framing_bounds("missing"), None);
    }

    #[test]
    fn style_defaults_are_sane() {
        let style = Style::default();
        assert_eq!(style.stroke_color, "#ece4d6");
        assert_eq!(style.opacity, 1.0);
        assert!((0..=2).contains(&style.roughness));
    }
}
