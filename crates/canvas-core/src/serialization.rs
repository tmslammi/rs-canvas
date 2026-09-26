//! JSON serialization with a versioned schema and a migration seam.
//!
//! Files and autosaves share one schema. The migration function is the single
//! place older documents are upgraded; unknown future versions are rejected.

use std::fmt;

use serde_json::Value;

use crate::model::{Document, SCHEMA_VERSION};

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
    let version = value
        .get("schemaVersion")
        .and_then(Value::as_u64)
        .ok_or_else(|| SchemaError::Invalid("missing schemaVersion".into()))?
        as u32;
    let value = migrate(value, version)?;
    Ok(serde_json::from_value(value)?)
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
}
