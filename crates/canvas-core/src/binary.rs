//! Version 1 binary payload: a compact, explicit schema for the document
//! model.
//!
//! The JSON representation is shaped for people and the wire (field names,
//! internally tagged enums, `flatten`), which makes it a poor fit for
//! positional binary codecs. This module keeps a frozen, deliberately plain
//! mirror of the model for persistence:
//!
//! * structs become positional fields in declaration order;
//! * enums become a variant index plus positional fields;
//! * `Option` becomes a presence tag, never an omitted field.
//!
//! Field and variant order is the schema: append only in a new version. The
//! borrowed types encode a live document without cloning; the owned types
//! decode straight into the model without an intermediate JSON tree.
//!
//! `SCHEMA_VERSION` 1 is the first binary schema. When the model changes, add a
//! frozen copy of these types for reading version 1 instead of editing them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::geometry::{Point, Rect};
use crate::model::{
    Binding, Document, Element, ElementKind, Endpoint, Routing, ShapeKind, Style, TextAlign,
    SCHEMA_VERSION,
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
    focus: Option<Point>,
}

/// Variant and field order is the version 1 schema.
#[derive(Serialize)]
enum KindBorrowed<'a> {
    Shape {
        shape: ShapeKind,
        rect: Rect,
        style: &'a Style,
        bound_text_id: Option<&'a str>,
    },
    Freehand {
        points: &'a [Point],
        pressures: &'a [f64],
        style: &'a Style,
    },
    Glyph {
        glyph: &'a str,
        rect: Rect,
        style: &'a Style,
        caption_id: Option<&'a str>,
    },
    Arrow {
        start: EndpointBorrowed<'a>,
        end: EndpointBorrowed<'a>,
        routing: Routing,
        start_arrowhead: bool,
        end_arrowhead: bool,
        style: &'a Style,
        waypoints: &'a [Point],
        label_id: Option<&'a str>,
    },
    Text {
        rect: Rect,
        text: &'a str,
        align: TextAlign,
        fixed_width: bool,
        container_id: Option<&'a str>,
        style: &'a Style,
    },
    StickyStack {
        rect: Rect,
        style: &'a Style,
    },
    Frame {
        rect: Rect,
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
            focus: binding.focus,
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
                shape: *shape,
                rect: *rect,
                style,
                bound_text_id: bound_text_id.as_deref(),
            },
            ElementKind::Freehand {
                points,
                pressures,
                style,
            } => KindBorrowed::Freehand {
                points,
                pressures,
                style,
            },
            ElementKind::Glyph {
                glyph,
                rect,
                style,
                caption_id,
            } => KindBorrowed::Glyph {
                glyph,
                rect: *rect,
                style,
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
                routing: *routing,
                start_arrowhead: *start_arrowhead,
                end_arrowhead: *end_arrowhead,
                style,
                waypoints,
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
                rect: *rect,
                text,
                align: *align,
                fixed_width: *fixed_width,
                container_id: container_id.as_deref(),
                style,
            },
            ElementKind::StickyStack { rect, style } => {
                KindBorrowed::StickyStack { rect: *rect, style }
            }
            ElementKind::Frame { rect, name } => KindBorrowed::Frame { rect: *rect, name },
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
    focus: Option<Point>,
}

/// Must mirror `KindBorrowed` variant for variant and field for field.
#[derive(Deserialize)]
enum KindOwned {
    Shape {
        shape: ShapeKind,
        rect: Rect,
        style: Style,
        bound_text_id: Option<String>,
    },
    Freehand {
        points: Vec<Point>,
        pressures: Vec<f64>,
        style: Style,
    },
    Glyph {
        glyph: String,
        rect: Rect,
        style: Style,
        caption_id: Option<String>,
    },
    Arrow {
        start: EndpointOwned,
        end: EndpointOwned,
        routing: Routing,
        start_arrowhead: bool,
        end_arrowhead: bool,
        style: Style,
        waypoints: Vec<Point>,
        label_id: Option<String>,
    },
    Text {
        rect: Rect,
        text: String,
        align: TextAlign,
        fixed_width: bool,
        container_id: Option<String>,
        style: Style,
    },
    StickyStack {
        rect: Rect,
        style: Style,
    },
    Frame {
        rect: Rect,
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
            focus: binding.focus,
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
                shape,
                rect,
                style,
                bound_text_id,
            },
            KindOwned::Freehand {
                points,
                pressures,
                style,
            } => ElementKind::Freehand {
                points,
                pressures,
                style,
            },
            KindOwned::Glyph {
                glyph,
                rect,
                style,
                caption_id,
            } => ElementKind::Glyph {
                glyph,
                rect,
                style,
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
                routing,
                start_arrowhead,
                end_arrowhead,
                style,
                waypoints,
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
                rect,
                text,
                align,
                fixed_width,
                container_id,
                style,
            },
            KindOwned::StickyStack { rect, style } => ElementKind::StickyStack { rect, style },
            KindOwned::Frame { rect, name } => ElementKind::Frame { rect, name },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;
    use crate::model::{FillPattern, StrokeDash};

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
}
