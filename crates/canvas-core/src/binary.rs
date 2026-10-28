//! Version 1 binary payload: a compact, explicit schema for the document
//! model.
//!
//! The JSON representation is shaped for people and import/export (field
//! names, internally tagged enums, `flatten`), which makes it a poor fit for
//! positional binary codecs. This module keeps a frozen, deliberately plain
//! mirror of the model for persistence and the wire:
//!
//! * structs become positional fields in declaration order;
//! * enums become a variant index plus positional fields;
//! * `Option` becomes a presence tag, never an omitted field.
//!
//! Every persisted type has its own v1 mirror, including nested types such as
//! [`Style`], [`Rect`], [`Point`] and the style enums. The conversions are
//! exhaustive and construct model types without `..`, so adding or removing a
//! model field, enum variant or element kind (or changing a field type) fails
//! to compile here instead of silently changing what a version 1 payload
//! means. Reordering the live model cannot change the payload because the
//! conversions match by name. Field and variant order in this module is the
//! schema: append only in a new version. The borrowed types encode a live
//! document without cloning; the owned types decode straight into the model
//! without an intermediate JSON tree.
//!
//! `SCHEMA_VERSION` 1 is the first binary schema. When the model changes, keep
//! these types as the frozen v1 reader and add a new payload version rather
//! than editing them.

use std::collections::BTreeMap;

use serde::ser::SerializeSeq;
use serde::{Deserialize, Serialize, Serializer};

use crate::geometry::{Point, Rect};
use crate::model::{
    Binding, Document, Element, ElementKind, Endpoint, FillPattern, Routing, ShapeKind, StrokeDash,
    Style, TextAlign, SCHEMA_VERSION,
};

/// Encode the version 1 payload. The envelope carries the schema version, so
/// the payload does not repeat it.
pub(crate) fn encode(doc: &Document) -> Result<Vec<u8>, postcard::Error> {
    postcard::to_allocvec(&DocumentBorrowed::from(doc))
}

/// Decode a version 1 payload into the model.
pub(crate) fn decode(payload: &[u8]) -> Result<Document, postcard::Error> {
    let owned: DocumentOwned = postcard::from_bytes(payload)?;
    Ok(owned.into())
}

/// Encode a wire patch: a bare sequence of full elements in the same frozen v1
/// element layout. Unlike the document payload there is no map key, because a
/// patch carries only the elements that changed.
pub(crate) fn encode_elements(elements: &[Element]) -> Result<Vec<u8>, postcard::Error> {
    postcard::to_allocvec(&ElementSliceBorrowed(elements))
}

/// Decode a wire patch payload into full elements.
pub(crate) fn decode_elements(payload: &[u8]) -> Result<Vec<Element>, postcard::Error> {
    let owned: Vec<ElementOwned> = postcard::from_bytes(payload)?;
    Ok(owned.into_iter().map(Element::from).collect())
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct PointV1 {
    x: f64,
    y: f64,
}

impl From<Point> for PointV1 {
    fn from(point: Point) -> Self {
        Self {
            x: point.x,
            y: point.y,
        }
    }
}

impl From<PointV1> for Point {
    fn from(point: PointV1) -> Self {
        Self {
            x: point.x,
            y: point.y,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct RectV1 {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl From<Rect> for RectV1 {
    fn from(rect: Rect) -> Self {
        Self {
            x: rect.x,
            y: rect.y,
            w: rect.w,
            h: rect.h,
        }
    }
}

impl From<RectV1> for Rect {
    fn from(rect: RectV1) -> Self {
        Self {
            x: rect.x,
            y: rect.y,
            w: rect.w,
            h: rect.h,
        }
    }
}

/// A frozen enum mirror with exhaustive conversions in both directions.
///
/// The variant list is the version 1 order. Adding, removing or renaming a
/// model variant fails to compile until this list is revisited; reordering the
/// model cannot change the payload because conversions match by name.
macro_rules! frozen_enum {
    ($mirror:ident, $model:ident { $($variant:ident),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
        enum $mirror {
            $($variant),+
        }

        impl From<$model> for $mirror {
            fn from(value: $model) -> Self {
                match value {
                    $($model::$variant => $mirror::$variant),+
                }
            }
        }

        impl From<$mirror> for $model {
            fn from(value: $mirror) -> Self {
                match value {
                    $($mirror::$variant => $model::$variant),+
                }
            }
        }
    };
}

frozen_enum!(
    ShapeKindV1,
    ShapeKind {
        Rectangle,
        Ellipse,
        Diamond,
        Sticky,
    }
);
frozen_enum!(
    FillPatternV1,
    FillPattern {
        Hachure,
        CrossHatch,
        Solid,
    }
);
frozen_enum!(
    StrokeDashV1,
    StrokeDash {
        Solid,
        Dashed,
        Dotted,
    }
);
frozen_enum!(
    RoutingV1,
    Routing {
        Straight,
        Elbow,
        Curved,
    }
);
frozen_enum!(
    TextAlignV1,
    TextAlign {
        Left,
        Center,
        Right
    }
);

/// A borrowed `[Point]` that encodes the frozen v1 point layout without
/// allocating a parallel vector.
struct PointSliceBorrowed<'a>(&'a [Point]);

impl Serialize for PointSliceBorrowed<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for point in self.0 {
            sequence.serialize_element(&PointV1::from(*point))?;
        }
        sequence.end()
    }
}

/// A borrowed element list for wire patches: the same v1 element layout as a
/// document, as a bare sequence instead of an id-keyed map.
struct ElementSliceBorrowed<'a>(&'a [Element]);

impl Serialize for ElementSliceBorrowed<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for element in self.0 {
            sequence.serialize_element(&ElementBorrowed::from(element))?;
        }
        sequence.end()
    }
}

#[derive(Serialize)]
struct DocumentBorrowed<'a> {
    elements: BTreeMap<&'a str, ElementBorrowed<'a>>,
}

#[derive(Serialize)]
struct ElementBorrowed<'a> {
    id: &'a str,
    version: u64,
    version_nonce: u32,
    index: &'a str,
    is_deleted: bool,
    updated: u64,
    frame_id: Option<&'a str>,
    kind: KindBorrowed<'a>,
}

#[derive(Serialize)]
struct EndpointBorrowed<'a> {
    x: f64,
    y: f64,
    binding: Option<BindingBorrowed<'a>>,
}

#[derive(Serialize)]
struct BindingBorrowed<'a> {
    element_id: &'a str,
    focus: Option<PointV1>,
}

/// Field order is the version 1 style schema.
#[derive(Serialize)]
struct StyleBorrowed<'a> {
    stroke_color: &'a str,
    fill_color: &'a str,
    fill_style: FillPatternV1,
    stroke_width: f64,
    roughness: u8,
    dash: StrokeDashV1,
    opacity: f64,
    font_size: f64,
}

/// Variant and field order is the version 1 element schema.
#[derive(Serialize)]
enum KindBorrowed<'a> {
    Shape {
        shape: ShapeKindV1,
        rect: RectV1,
        style: StyleBorrowed<'a>,
        bound_text_id: Option<&'a str>,
    },
    Freehand {
        points: PointSliceBorrowed<'a>,
        pressures: &'a [f64],
        style: StyleBorrowed<'a>,
    },
    Glyph {
        glyph: &'a str,
        rect: RectV1,
        style: StyleBorrowed<'a>,
        caption_id: Option<&'a str>,
    },
    Arrow {
        start: EndpointBorrowed<'a>,
        end: EndpointBorrowed<'a>,
        routing: RoutingV1,
        start_arrowhead: bool,
        end_arrowhead: bool,
        style: StyleBorrowed<'a>,
        waypoints: PointSliceBorrowed<'a>,
        label_id: Option<&'a str>,
    },
    Text {
        rect: RectV1,
        text: &'a str,
        align: TextAlignV1,
        fixed_width: bool,
        container_id: Option<&'a str>,
        style: StyleBorrowed<'a>,
    },
    StickyStack {
        rect: RectV1,
        style: StyleBorrowed<'a>,
    },
    Frame {
        rect: RectV1,
        name: &'a str,
    },
}

impl<'a> From<&'a Document> for DocumentBorrowed<'a> {
    fn from(doc: &'a Document) -> Self {
        Self {
            elements: doc
                .elements
                .iter()
                .map(|(id, element)| (id.as_str(), ElementBorrowed::from(element)))
                .collect(),
        }
    }
}

impl<'a> From<&'a Element> for ElementBorrowed<'a> {
    fn from(element: &'a Element) -> Self {
        Self {
            id: &element.id,
            version: element.version,
            version_nonce: element.version_nonce,
            index: &element.index,
            is_deleted: element.is_deleted,
            updated: element.updated,
            frame_id: element.frame_id.as_deref(),
            kind: KindBorrowed::from(&element.kind),
        }
    }
}

impl<'a> From<&'a Endpoint> for EndpointBorrowed<'a> {
    fn from(endpoint: &'a Endpoint) -> Self {
        Self {
            x: endpoint.x,
            y: endpoint.y,
            binding: endpoint.binding.as_ref().map(BindingBorrowed::from),
        }
    }
}

impl<'a> From<&'a Binding> for BindingBorrowed<'a> {
    fn from(binding: &'a Binding) -> Self {
        Self {
            element_id: &binding.element_id,
            focus: binding.focus.map(PointV1::from),
        }
    }
}

impl<'a> From<&'a Style> for StyleBorrowed<'a> {
    fn from(style: &'a Style) -> Self {
        Self {
            stroke_color: &style.stroke_color,
            fill_color: &style.fill_color,
            fill_style: style.fill_style.into(),
            stroke_width: style.stroke_width,
            roughness: style.roughness,
            dash: style.dash.into(),
            opacity: style.opacity,
            font_size: style.font_size,
        }
    }
}

impl<'a> From<&'a ElementKind> for KindBorrowed<'a> {
    fn from(kind: &'a ElementKind) -> Self {
        match kind {
            ElementKind::Shape {
                shape,
                rect,
                style,
                bound_text_id,
            } => KindBorrowed::Shape {
                shape: (*shape).into(),
                rect: (*rect).into(),
                style: StyleBorrowed::from(style),
                bound_text_id: bound_text_id.as_deref(),
            },
            ElementKind::Freehand {
                points,
                pressures,
                style,
            } => KindBorrowed::Freehand {
                points: PointSliceBorrowed(points),
                pressures,
                style: StyleBorrowed::from(style),
            },
            ElementKind::Glyph {
                glyph,
                rect,
                style,
                caption_id,
            } => KindBorrowed::Glyph {
                glyph,
                rect: (*rect).into(),
                style: StyleBorrowed::from(style),
                caption_id: caption_id.as_deref(),
            },
            ElementKind::Arrow {
                start,
                end,
                routing,
                start_arrowhead,
                end_arrowhead,
                style,
                waypoints,
                label_id,
            } => KindBorrowed::Arrow {
                start: EndpointBorrowed::from(start),
                end: EndpointBorrowed::from(end),
                routing: (*routing).into(),
                start_arrowhead: *start_arrowhead,
                end_arrowhead: *end_arrowhead,
                style: StyleBorrowed::from(style),
                waypoints: PointSliceBorrowed(waypoints),
                label_id: label_id.as_deref(),
            },
            ElementKind::Text {
                rect,
                text,
                align,
                fixed_width,
                container_id,
                style,
            } => KindBorrowed::Text {
                rect: (*rect).into(),
                text,
                align: (*align).into(),
                fixed_width: *fixed_width,
                container_id: container_id.as_deref(),
                style: StyleBorrowed::from(style),
            },
            ElementKind::StickyStack { rect, style } => KindBorrowed::StickyStack {
                rect: (*rect).into(),
                style: StyleBorrowed::from(style),
            },
            ElementKind::Frame { rect, name } => KindBorrowed::Frame {
                rect: (*rect).into(),
                name,
            },
        }
    }
}

#[derive(Deserialize)]
struct DocumentOwned {
    elements: BTreeMap<String, ElementOwned>,
}

#[derive(Deserialize)]
struct ElementOwned {
    id: String,
    version: u64,
    version_nonce: u32,
    index: String,
    is_deleted: bool,
    updated: u64,
    frame_id: Option<String>,
    kind: KindOwned,
}

#[derive(Deserialize)]
struct EndpointOwned {
    x: f64,
    y: f64,
    binding: Option<BindingOwned>,
}

#[derive(Deserialize)]
struct BindingOwned {
    element_id: String,
    focus: Option<PointV1>,
}

/// Must mirror `StyleBorrowed` field for field.
#[derive(Deserialize)]
struct StyleOwned {
    stroke_color: String,
    fill_color: String,
    fill_style: FillPatternV1,
    stroke_width: f64,
    roughness: u8,
    dash: StrokeDashV1,
    opacity: f64,
    font_size: f64,
}

/// Must mirror `KindBorrowed` variant for variant and field for field.
#[derive(Deserialize)]
enum KindOwned {
    Shape {
        shape: ShapeKindV1,
        rect: RectV1,
        style: StyleOwned,
        bound_text_id: Option<String>,
    },
    Freehand {
        points: Vec<PointV1>,
        pressures: Vec<f64>,
        style: StyleOwned,
    },
    Glyph {
        glyph: String,
        rect: RectV1,
        style: StyleOwned,
        caption_id: Option<String>,
    },
    Arrow {
        start: EndpointOwned,
        end: EndpointOwned,
        routing: RoutingV1,
        start_arrowhead: bool,
        end_arrowhead: bool,
        style: StyleOwned,
        waypoints: Vec<PointV1>,
        label_id: Option<String>,
    },
    Text {
        rect: RectV1,
        text: String,
        align: TextAlignV1,
        fixed_width: bool,
        container_id: Option<String>,
        style: StyleOwned,
    },
    StickyStack {
        rect: RectV1,
        style: StyleOwned,
    },
    Frame {
        rect: RectV1,
        name: String,
    },
}

impl From<DocumentOwned> for Document {
    fn from(doc: DocumentOwned) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            elements: doc
                .elements
                .into_iter()
                .map(|(key, element)| (key, Element::from(element)))
                .collect(),
        }
    }
}

impl From<ElementOwned> for Element {
    fn from(element: ElementOwned) -> Self {
        Self {
            id: element.id,
            version: element.version,
            version_nonce: element.version_nonce,
            index: element.index,
            is_deleted: element.is_deleted,
            updated: element.updated,
            frame_id: element.frame_id,
            kind: ElementKind::from(element.kind),
        }
    }
}

impl From<EndpointOwned> for Endpoint {
    fn from(endpoint: EndpointOwned) -> Self {
        Self {
            x: endpoint.x,
            y: endpoint.y,
            binding: endpoint.binding.map(Binding::from),
        }
    }
}

impl From<BindingOwned> for Binding {
    fn from(binding: BindingOwned) -> Self {
        Self {
            element_id: binding.element_id,
            focus: binding.focus.map(Point::from),
        }
    }
}

impl From<StyleOwned> for Style {
    fn from(style: StyleOwned) -> Self {
        Self {
            stroke_color: style.stroke_color,
            fill_color: style.fill_color,
            fill_style: style.fill_style.into(),
            stroke_width: style.stroke_width,
            roughness: style.roughness,
            dash: style.dash.into(),
            opacity: style.opacity,
            font_size: style.font_size,
        }
    }
}

impl From<KindOwned> for ElementKind {
    fn from(kind: KindOwned) -> Self {
        match kind {
            KindOwned::Shape {
                shape,
                rect,
                style,
                bound_text_id,
            } => ElementKind::Shape {
                shape: shape.into(),
                rect: rect.into(),
                style: style.into(),
                bound_text_id,
            },
            KindOwned::Freehand {
                points,
                pressures,
                style,
            } => ElementKind::Freehand {
                points: points.into_iter().map(Point::from).collect(),
                pressures,
                style: style.into(),
            },
            KindOwned::Glyph {
                glyph,
                rect,
                style,
                caption_id,
            } => ElementKind::Glyph {
                glyph,
                rect: rect.into(),
                style: style.into(),
                caption_id,
            },
            KindOwned::Arrow {
                start,
                end,
                routing,
                start_arrowhead,
                end_arrowhead,
                style,
                waypoints,
                label_id,
            } => ElementKind::Arrow {
                start: start.into(),
                end: end.into(),
                routing: routing.into(),
                start_arrowhead,
                end_arrowhead,
                style: style.into(),
                waypoints: waypoints.into_iter().map(Point::from).collect(),
                label_id,
            },
            KindOwned::Text {
                rect,
                text,
                align,
                fixed_width,
                container_id,
                style,
            } => ElementKind::Text {
                rect: rect.into(),
                text,
                align: align.into(),
                fixed_width,
                container_id,
                style: style.into(),
            },
            KindOwned::StickyStack { rect, style } => ElementKind::StickyStack {
                rect: rect.into(),
                style: style.into(),
            },
            KindOwned::Frame { rect, name } => ElementKind::Frame {
                rect: rect.into(),
                name,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_kinds_document() -> Document {
        let mut doc = Document::new();
        doc.add(
            ElementKind::Shape {
                shape: ShapeKind::Rectangle,
                rect: Rect::new(0.0, 0.0, 100.0, 50.0),
                style: Style::default(),
                bound_text_id: Some("text".into()),
            },
            1,
        );
        doc.add(
            ElementKind::Freehand {
                points: vec![Point::new(0.0, 0.0), Point::new(5.0, 8.0)],
                pressures: vec![0.5, 0.7],
                style: Style::default(),
            },
            2,
        );
        doc.add(
            ElementKind::Glyph {
                glyph: "cloud_run".into(),
                rect: Rect::new(10.0, 10.0, 64.0, 64.0),
                style: Style::default(),
                caption_id: Some("caption".into()),
            },
            3,
        );
        doc.add(
            ElementKind::Arrow {
                start: Endpoint {
                    x: 0.0,
                    y: 0.0,
                    binding: Some(Binding {
                        element_id: "shape".into(),
                        focus: Some(Point::new(0.25, 0.5)),
                    }),
                },
                end: Endpoint::free(40.0, 20.0),
                routing: Routing::Elbow,
                start_arrowhead: true,
                end_arrowhead: false,
                style: Style {
                    dash: StrokeDash::Dashed,
                    fill_style: FillPattern::CrossHatch,
                    ..Style::default()
                },
                waypoints: vec![Point::new(20.0, 10.0)],
                label_id: Some("label".into()),
            },
            4,
        );
        doc.add(
            ElementKind::Text {
                rect: Rect::new(0.0, 0.0, 120.0, 24.0),
                text: "hello\nworld".into(),
                align: TextAlign::Center,
                fixed_width: true,
                container_id: Some("shape".into()),
                style: Style::default(),
            },
            5,
        );
        doc.add(
            ElementKind::StickyStack {
                rect: Rect::new(200.0, 200.0, 120.0, 120.0),
                style: Style::default(),
            },
            6,
        );
        doc.add(
            ElementKind::Frame {
                rect: Rect::new(-20.0, -20.0, 400.0, 300.0),
                name: "us-central1".into(),
            },
            7,
        );
        doc
    }

    #[test]
    fn payload_round_trips_every_kind() {
        let doc = all_kinds_document();
        let payload = encode(&doc).unwrap();
        let back = decode(&payload).unwrap();
        assert_eq!(back, doc);
    }

    #[test]
    fn payload_is_deterministic() {
        let doc = all_kinds_document();
        assert_eq!(encode(&doc).unwrap(), encode(&doc).unwrap());
    }

    #[test]
    fn empty_payload_round_trips() {
        let doc = Document::new();
        assert_eq!(decode(&encode(&doc).unwrap()).unwrap(), doc);
    }

    #[test]
    fn element_list_round_trips_every_kind() {
        let doc = all_kinds_document();
        let elements: Vec<Element> = doc.elements.into_values().collect();
        let payload = encode_elements(&elements).unwrap();
        assert_eq!(decode_elements(&payload).unwrap(), elements);
        assert_eq!(payload, encode_elements(&elements).unwrap());
    }

    #[test]
    fn empty_element_list_round_trips() {
        let payload = encode_elements(&[]).unwrap();
        assert!(decode_elements(&payload).unwrap().is_empty());
    }

    /// Every element kind, every enum variant, every style field and every
    /// optional reference type.
    fn golden_document() -> Document {
        fn style(fill: FillPattern, dash: StrokeDash, roughness: u8) -> Style {
            Style {
                stroke_color: "#112233".into(),
                fill_color: "#aabbcc".into(),
                fill_style: fill,
                stroke_width: 3.5,
                roughness,
                dash,
                opacity: 0.85,
                font_size: 18.0,
            }
        }
        fn element(id: &str, index: &str, kind: ElementKind) -> Element {
            Element {
                id: id.into(),
                version: 7,
                version_nonce: 42,
                index: index.into(),
                is_deleted: false,
                updated: 1_700_000_000_000,
                frame_id: None,
                kind,
            }
        }
        let solid = || style(FillPattern::Hachure, StrokeDash::Solid, 0);
        let dashed = || style(FillPattern::CrossHatch, StrokeDash::Dashed, 1);
        let dotted = || style(FillPattern::Solid, StrokeDash::Dotted, 2);
        let mut doc = Document::new();
        doc.insert(element(
            "shape-rect",
            "a0",
            ElementKind::Shape {
                shape: ShapeKind::Rectangle,
                rect: Rect::new(0.0, 0.0, 100.0, 50.0),
                style: solid(),
                bound_text_id: Some("text-center".into()),
            },
        ));
        doc.insert(element(
            "shape-ellipse",
            "a1",
            ElementKind::Shape {
                shape: ShapeKind::Ellipse,
                rect: Rect::new(10.0, 10.0, 40.0, 20.0),
                style: dashed(),
                bound_text_id: None,
            },
        ));
        doc.insert(element(
            "shape-diamond",
            "a2",
            ElementKind::Shape {
                shape: ShapeKind::Diamond,
                rect: Rect::new(-5.0, -5.0, 30.0, 30.0),
                style: dotted(),
                bound_text_id: None,
            },
        ));
        let mut sticky = element(
            "shape-sticky",
            "a3",
            ElementKind::Shape {
                shape: ShapeKind::Sticky,
                rect: Rect::new(200.0, 200.0, 80.0, 80.0),
                style: solid(),
                bound_text_id: Some("text-left".into()),
            },
        );
        sticky.frame_id = Some("frame".into());
        sticky.is_deleted = true;
        doc.insert(sticky);
        doc.insert(element(
            "freehand",
            "a4",
            ElementKind::Freehand {
                points: vec![Point::new(0.5, 1.25), Point::new(-2.0, 3.75)],
                pressures: vec![0.25, 0.75],
                style: dashed(),
            },
        ));
        doc.insert(element(
            "glyph",
            "a5",
            ElementKind::Glyph {
                glyph: "cloud_run".into(),
                rect: Rect::new(1.0, 2.0, 64.0, 64.0),
                style: dotted(),
                caption_id: Some("text-right".into()),
            },
        ));
        doc.insert(element(
            "arrow-straight",
            "a6",
            ElementKind::Arrow {
                start: Endpoint {
                    x: 0.0,
                    y: 0.0,
                    binding: Some(Binding {
                        element_id: "shape-rect".into(),
                        focus: Some(Point::new(0.25, 0.5)),
                    }),
                },
                end: Endpoint::free(40.0, 20.0),
                routing: Routing::Straight,
                start_arrowhead: false,
                end_arrowhead: true,
                style: solid(),
                waypoints: Vec::new(),
                label_id: None,
            },
        ));
        doc.insert(element(
            "arrow-elbow",
            "a7",
            ElementKind::Arrow {
                start: Endpoint {
                    x: 1.0,
                    y: 2.0,
                    binding: Some(Binding {
                        element_id: "shape-ellipse".into(),
                        focus: None,
                    }),
                },
                end: Endpoint {
                    x: 3.0,
                    y: 4.0,
                    binding: Some(Binding {
                        element_id: "glyph".into(),
                        focus: Some(Point::new(1.0, 0.0)),
                    }),
                },
                routing: Routing::Elbow,
                start_arrowhead: true,
                end_arrowhead: false,
                style: dashed(),
                waypoints: vec![Point::new(20.0, 10.0), Point::new(30.0, 12.5)],
                label_id: Some("text-right".into()),
            },
        ));
        doc.insert(element(
            "arrow-curved",
            "a8",
            ElementKind::Arrow {
                start: Endpoint::free(0.0, 5.0),
                end: Endpoint::free(5.0, 0.0),
                routing: Routing::Curved,
                start_arrowhead: true,
                end_arrowhead: true,
                style: dotted(),
                waypoints: Vec::new(),
                label_id: None,
            },
        ));
        doc.insert(element(
            "text-left",
            "a9",
            ElementKind::Text {
                rect: Rect::new(0.0, 0.0, 120.0, 24.0),
                text: "left".into(),
                align: TextAlign::Left,
                fixed_width: false,
                container_id: Some("shape-rect".into()),
                style: solid(),
            },
        ));
        doc.insert(element(
            "text-center",
            "aa",
            ElementKind::Text {
                rect: Rect::new(0.0, 30.0, 120.0, 24.0),
                text: "center".into(),
                align: TextAlign::Center,
                fixed_width: true,
                container_id: None,
                style: dashed(),
            },
        ));
        doc.insert(element(
            "text-right",
            "ab",
            ElementKind::Text {
                rect: Rect::new(0.0, 60.0, 120.0, 24.0),
                text: "right".into(),
                align: TextAlign::Right,
                fixed_width: false,
                container_id: None,
                style: dotted(),
            },
        ));
        doc.insert(element(
            "sticky-stack",
            "ac",
            ElementKind::StickyStack {
                rect: Rect::new(300.0, 300.0, 120.0, 120.0),
                style: dashed(),
            },
        ));
        doc.insert(element(
            "frame",
            "ad",
            ElementKind::Frame {
                rect: Rect::new(-20.0, -20.0, 400.0, 300.0),
                name: "us-central1".into(),
            },
        ));
        doc
    }

    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// The exact version 1 payload. If this changes, do not update the golden
    /// value: bump `SCHEMA_VERSION`, keep a frozen v1 reader and add a v2
    /// payload. The hash is FNV-1a over the encoded bytes.
    #[test]
    fn v1_payload_matches_golden_bytes() {
        let payload = encode(&golden_document()).unwrap();
        assert_eq!(payload.len(), 1778, "v1 payload length changed");
        assert_eq!(
            fnv1a(&payload),
            0xdd3b_1358_b292_cc9b,
            "v1 payload bytes changed; hex: {}",
            hex(&payload)
        );
    }
}
