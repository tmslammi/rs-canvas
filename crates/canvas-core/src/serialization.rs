//! JSON serialization with a versioned schema and a migration seam.
//!
//! Files and autosaves share one schema. The migration function is the single
//! place older documents are upgraded; unknown future versions are rejected.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde_json::Value;

use crate::geometry::Rect;
use crate::model::{Document, Element, ElementId, ElementKind, SCHEMA_VERSION};

#[derive(Debug)]
pub enum SchemaError {
    Json(serde_json::Error),
    /// A version this build does not understand (from the future).
    UnsupportedVersion(u32),
    /// A document that is structurally unusable.
    Invalid(String),
}

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SchemaError::Json(e) => write!(f, "invalid document JSON: {e}"),
            SchemaError::UnsupportedVersion(v) => {
                write!(
                    f,
                    "document schema version {v} is newer than this app supports"
                )
            }
            SchemaError::Invalid(msg) => write!(f, "invalid document: {msg}"),
        }
    }
}

impl std::error::Error for SchemaError {}

impl From<serde_json::Error> for SchemaError {
    fn from(value: serde_json::Error) -> Self {
        SchemaError::Json(value)
    }
}

pub fn to_json(doc: &Document) -> Result<String, SchemaError> {
    Ok(serde_json::to_string(doc)?)
}

pub fn to_json_pretty(doc: &Document) -> Result<String, SchemaError> {
    Ok(serde_json::to_string_pretty(doc)?)
}

pub fn from_json(input: &str) -> Result<Document, SchemaError> {
    let value: Value = serde_json::from_str(input)?;
    from_value(value)
}

/// Load a document from an already-parsed JSON value, with the same schema
/// check, migration and sanitizing as `from_json`. Used by the wire protocol,
/// where the document arrives as an embedded JSON object.
pub fn from_value(value: Value) -> Result<Document, SchemaError> {
    let version = value
        .get("schemaVersion")
        .and_then(Value::as_u64)
        .ok_or_else(|| SchemaError::Invalid("missing schemaVersion".into()))?
        as u32;
    let value = migrate(value, version)?;
    let mut doc: Document = serde_json::from_value(value)?;
    for (id, element) in &doc.elements {
        if id != &element.id {
            return Err(SchemaError::Invalid(format!(
                "element key {id:?} does not match its id"
            )));
        }
        crate::index::validate_order_key(&element.index).map_err(|_| {
            SchemaError::Invalid(format!("element {id:?} has an invalid order key"))
        })?;
    }
    sanitize(&mut doc);
    Ok(doc)
}

/// Repair references and geometry a hostile or hand-edited file may carry,
/// so the editor never holds a dangling id or an inverted rect. Tombstones
/// stay tombstones; links pointing at missing or deleted targets are
/// cleared rather than rejected so the rest of the document survives.
fn sanitize(doc: &mut Document) {
    let live: BTreeSet<ElementId> = doc
        .elements
        .values()
        .filter(|e| !e.is_deleted)
        .map(|e| e.id.clone())
        .collect();
    let live_text: BTreeSet<ElementId> = doc
        .elements
        .values()
        .filter(|e| !e.is_deleted && e.is_text())
        .map(|e| e.id.clone())
        .collect();
    let live_containers: BTreeSet<ElementId> = doc
        .elements
        .values()
        .filter(|e| {
            !e.is_deleted
                && matches!(
                    e.kind,
                    ElementKind::Shape { .. }
                        | ElementKind::Glyph { .. }
                        | ElementKind::Arrow { .. }
                )
        })
        .map(|e| e.id.clone())
        .collect();
    let live_frames: BTreeSet<ElementId> = doc
        .elements
        .values()
        .filter(|e| !e.is_deleted && e.is_frame())
        .map(|e| e.id.clone())
        .collect();
    // Bound text belongs to its container: a binding that points at a label
    // really means the container. Retarget before the dead-binding sweep so
    // documents written before bound text stopped being a target stay wired.
    let bound_text_containers: BTreeMap<ElementId, ElementId> = doc
        .elements
        .values()
        .filter(|e| !e.is_deleted && e.is_text())
        .filter_map(|e| match &e.kind {
            ElementKind::Text {
                container_id: Some(container_id),
                ..
            } if live.contains(container_id) => Some((e.id.clone(), container_id.clone())),
            _ => None,
        })
        .collect();
    for element in doc.elements.values_mut() {
        // Negative extents are normalized rather than rejected: they draw
        // the same shape with flipped axes.
        if let Some(rect) = rect_mut(element) {
            if rect.w < 0.0 {
                rect.x += rect.w;
                rect.w = -rect.w;
            }
            if rect.h < 0.0 {
                rect.y += rect.h;
                rect.h = -rect.h;
            }
        }
        match &mut element.kind {
            ElementKind::Shape { bound_text_id, .. } => clear_dead(bound_text_id, &live_text),
            ElementKind::Glyph { caption_id, .. } => clear_dead(caption_id, &live_text),
            ElementKind::Arrow {
                start,
                end,
                label_id,
                waypoints,
                ..
            } => {
                clear_dead(label_id, &live_text);
                waypoints.retain(|point| point.x.is_finite() && point.y.is_finite());
                for endpoint in [start, end] {
                    if let Some(binding) = &mut endpoint.binding {
                        if let Some(container_id) = bound_text_containers.get(&binding.element_id) {
                            binding.element_id = container_id.clone();
                        }
                        if !live.contains(&binding.element_id) {
                            endpoint.binding = None;
                        }
                    }
                }
            }
            ElementKind::Text { container_id, .. } => clear_dead(container_id, &live_containers),
            _ => {}
        }
        if let Some(frame_id) = &element.frame_id {
            if *frame_id == element.id || !live_frames.contains(frame_id) {
                element.frame_id = None;
            }
        }
    }
}

fn clear_dead(reference: &mut Option<ElementId>, valid: &BTreeSet<ElementId>) {
    if reference.as_ref().is_some_and(|id| !valid.contains(id)) {
        *reference = None;
    }
}

fn rect_mut(element: &mut Element) -> Option<&mut Rect> {
    match &mut element.kind {
        ElementKind::Shape { rect, .. }
        | ElementKind::Glyph { rect, .. }
        | ElementKind::Text { rect, .. }
        | ElementKind::StickyStack { rect, .. }
        | ElementKind::Frame { rect, .. } => Some(rect),
        _ => None,
    }
}

/// Upgrade a document from an older schema version.
///
/// Version 1 is the first schema; the match exists so every future bump has a
/// single, testable home.
fn migrate(value: Value, version: u32) -> Result<Value, SchemaError> {
    match version {
        1 => Ok(value),
        v if v < SCHEMA_VERSION => Err(SchemaError::Invalid(format!(
            "no migration path from schema version {v}"
        ))),
        v => Err(SchemaError::UnsupportedVersion(v)),
    }
}

/// Convenience: serialize a document to pretty JSON for download.
pub fn to_file_json(doc: &Document) -> Result<String, SchemaError> {
    to_json_pretty(doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Point, Rect};
    use crate::model::{ElementKind, Endpoint, FillPattern, Routing, ShapeKind, Style, TextAlign};

    fn full_document() -> Document {
        let mut doc = Document::new();
        doc.add(
            ElementKind::Shape {
                shape: ShapeKind::Rectangle,
                rect: Rect::new(0.0, 0.0, 100.0, 50.0),
                style: Style::default(),
                bound_text_id: None,
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
                caption_id: None,
            },
            3,
        );
        doc.add(
            ElementKind::Arrow {
                start: Endpoint::free(0.0, 0.0),
                end: Endpoint::free(40.0, 20.0),
                routing: Routing::Elbow,
                start_arrowhead: false,
                end_arrowhead: true,
                style: Style {
                    dash: crate::model::StrokeDash::Dashed,
                    fill_style: FillPattern::CrossHatch,
                    ..Style::default()
                },
                waypoints: Vec::new(),
                label_id: None,
            },
            4,
        );
        doc.add(
            ElementKind::Text {
                rect: Rect::new(0.0, 0.0, 120.0, 24.0),
                text: "hello\nworld".into(),
                align: TextAlign::Center,
                fixed_width: true,
                container_id: None,
                style: Style::default(),
            },
            5,
        );
        doc.add(
            ElementKind::Frame {
                rect: Rect::new(-20.0, -20.0, 400.0, 300.0),
                name: "us-central1".into(),
            },
            6,
        );
        doc
    }

    #[test]
    fn load_rejects_mismatched_element_identity() {
        let mut value = serde_json::to_value(full_document()).unwrap();
        let elements = value["elements"].as_object_mut().unwrap();
        let id = elements.keys().next().unwrap().clone();
        let element = elements.remove(&id).unwrap();
        elements.insert("other-key".into(), element);
        assert!(matches!(from_value(value), Err(SchemaError::Invalid(_))));
    }

    #[test]
    fn load_rejects_invalid_order_keys() {
        for index in ["", "b0", "aé", "a!", "a0!"] {
            let mut value = serde_json::to_value(full_document()).unwrap();
            let element = value["elements"]
                .as_object_mut()
                .unwrap()
                .values_mut()
                .next()
                .unwrap();
            element["index"] = Value::from(index);
            assert!(
                matches!(from_value(value), Err(SchemaError::Invalid(_))),
                "{index}"
            );
        }
    }

    #[test]
    fn round_trip_is_identity() {
        let doc = full_document();
        let json = to_json(&doc).unwrap();
        let back = from_json(&json).unwrap();
        assert_eq!(doc, back);
    }

    #[test]
    fn serialization_is_deterministic() {
        let doc = full_document();
        assert_eq!(to_json(&doc).unwrap(), to_json(&doc).unwrap());
    }

    #[test]
    fn schema_version_is_present() {
        let json = to_json(&Document::new()).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["schemaVersion"], 1);
    }

    #[test]
    fn unknown_future_version_is_rejected() {
        let doc = Document::new();
        let mut value: Value = serde_json::from_str(&to_json(&doc).unwrap()).unwrap();
        value["schemaVersion"] = Value::from(99);
        let err = from_json(&value.to_string()).unwrap_err();
        assert!(matches!(err, SchemaError::UnsupportedVersion(99)));
    }

    #[test]
    fn missing_version_is_rejected() {
        let err = from_json(r#"{"elements":{}}"#).unwrap_err();
        assert!(matches!(err, SchemaError::Invalid(_)));
    }

    #[test]
    fn version_zero_has_no_migration_path() {
        let value = r#"{"schemaVersion":0,"elements":{}}"#;
        let err = from_json(value).unwrap_err();
        assert!(matches!(err, SchemaError::Invalid(_)));
    }

    #[test]
    fn element_ids_round_trip_with_all_fields() {
        let doc = full_document();
        let json = to_json(&doc).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        let elements = value["elements"].as_object().unwrap();
        assert_eq!(elements.len(), 6);
        for element in elements.values() {
            assert!(element["id"].is_string());
            assert!(element["version"].is_u64());
            assert!(element["versionNonce"].is_u64());
            assert!(element["index"].is_string());
            assert_eq!(element["isDeleted"], false);
            assert!(element["updated"].is_u64());
            assert!(element["type"].is_string());
        }
    }

    #[test]
    fn load_repairs_dangling_references() {
        let mut doc = Document::new();
        let shape_id = doc.add(
            ElementKind::Shape {
                shape: ShapeKind::Rectangle,
                rect: Rect::new(0.0, 0.0, 100.0, 50.0),
                style: Style::default(),
                bound_text_id: Some("ghost-text".into()),
            },
            1,
        );
        let arrow_id = doc.add(
            ElementKind::Arrow {
                start: Endpoint {
                    x: 0.0,
                    y: 0.0,
                    binding: Some(crate::model::Binding {
                        element_id: "ghost".into(),
                        focus: None,
                    }),
                },
                end: Endpoint::free(50.0, 50.0),
                routing: Routing::Straight,
                start_arrowhead: false,
                end_arrowhead: true,
                style: Style::default(),
                waypoints: Vec::new(),
                label_id: None,
            },
            2,
        );
        let text_id = doc.add(
            ElementKind::Text {
                rect: Rect::new(0.0, 0.0, 10.0, 10.0),
                text: "orphan".into(),
                align: TextAlign::Left,
                fixed_width: false,
                container_id: Some("ghost".into()),
                style: Style::default(),
            },
            3,
        );
        doc.element_mut(&shape_id).unwrap().frame_id = Some("ghost-frame".into());
        let flipped_id = doc.add(
            ElementKind::Shape {
                shape: ShapeKind::Rectangle,
                rect: Rect::new(200.0, 200.0, -40.0, -30.0),
                style: Style::default(),
                bound_text_id: None,
            },
            4,
        );

        let back = from_json(&to_json(&doc).unwrap()).unwrap();
        let shape = back.element(&shape_id).unwrap();
        assert!(
            matches!(
                &shape.kind,
                ElementKind::Shape {
                    bound_text_id: None,
                    ..
                }
            ),
            "dangling bound_text_id survived load"
        );
        assert!(shape.frame_id.is_none(), "dangling frame_id survived load");
        let ElementKind::Arrow { start, .. } = &back.element(&arrow_id).unwrap().kind else {
            panic!("arrow kind changed")
        };
        assert!(start.binding.is_none(), "dangling binding survived load");
        let ElementKind::Text { container_id, .. } = &back.element(&text_id).unwrap().kind else {
            panic!("text kind changed")
        };
        assert!(
            container_id.is_none(),
            "dangling container_id survived load"
        );
        let ElementKind::Shape { rect, .. } = &back.element(&flipped_id).unwrap().kind else {
            panic!("shape kind changed")
        };
        assert_eq!(*rect, Rect::new(160.0, 170.0, 40.0, 30.0));
    }

    #[test]
    fn load_preserves_live_bound_references() {
        let mut doc = Document::new();
        let shape_id = doc.add(
            ElementKind::Shape {
                shape: ShapeKind::Rectangle,
                rect: Rect::new(0.0, 0.0, 100.0, 50.0),
                style: Style::default(),
                bound_text_id: None,
            },
            1,
        );
        let text_id = doc.add(
            ElementKind::Text {
                rect: Rect::new(0.0, 0.0, 10.0, 10.0),
                text: "label".into(),
                align: TextAlign::Center,
                fixed_width: false,
                container_id: Some(shape_id.clone()),
                style: Style::default(),
            },
            2,
        );
        if let ElementKind::Shape { bound_text_id, .. } =
            &mut doc.element_mut(&shape_id).unwrap().kind
        {
            *bound_text_id = Some(text_id.clone());
        }

        let back = from_json(&to_json(&doc).unwrap()).unwrap();
        let ElementKind::Shape { bound_text_id, .. } = &back.element(&shape_id).unwrap().kind
        else {
            panic!("shape kind changed")
        };
        assert_eq!(bound_text_id.as_deref(), Some(text_id.as_str()));
        let ElementKind::Text { container_id, .. } = &back.element(&text_id).unwrap().kind else {
            panic!("text kind changed")
        };
        assert_eq!(container_id.as_deref(), Some(shape_id.as_str()));
    }

    #[test]
    fn load_retargets_bindings_away_from_bound_text() {
        let mut doc = Document::new();
        let shape_id = doc.add(
            ElementKind::Shape {
                shape: ShapeKind::Rectangle,
                rect: Rect::new(0.0, 0.0, 100.0, 50.0),
                style: Style::default(),
                bound_text_id: None,
            },
            1,
        );
        let text_id = doc.add(
            ElementKind::Text {
                rect: Rect::new(10.0, 10.0, 40.0, 10.0),
                text: "hub".into(),
                align: TextAlign::Center,
                fixed_width: false,
                container_id: Some(shape_id.clone()),
                style: Style::default(),
            },
            2,
        );
        // A document written while bound text was still a legal target.
        let arrow_id = doc.add(
            ElementKind::Arrow {
                start: Endpoint {
                    x: 60.0,
                    y: 25.0,
                    binding: Some(crate::model::Binding {
                        element_id: text_id.clone(),
                        focus: None,
                    }),
                },
                end: Endpoint::free(200.0, 200.0),
                routing: Routing::Straight,
                start_arrowhead: false,
                end_arrowhead: true,
                style: Style::default(),
                waypoints: Vec::new(),
                label_id: None,
            },
            3,
        );

        let back = from_json(&to_json(&doc).unwrap()).unwrap();
        let ElementKind::Arrow { start, .. } = &back.element(&arrow_id).unwrap().kind else {
            panic!("arrow kind changed")
        };
        assert_eq!(
            start.binding.as_ref().map(|b| b.element_id.as_str()),
            Some(shape_id.as_str()),
            "binding still points at the label instead of its container"
        );
    }
}
