//! The drawing as compact text for the agent: one line per element with
//! bounds, membership and labels.
//!
//! This is the LLM-facing projection of a `Document` — the model reads it to
//! learn handles and available space before drawing. It is deliberately
//! separate from the JSON wire format: the wire is for machines, this is for
//! tokens. Handles come from an [`IdMap`]; the server's alias registry maps
//! element UUIDs to stable short names (`e12`), while tests and file-facing
//! code use [`UuidMap`].
//!
//! Line format:
//!
//! ```text
//! <handle> <kind> <x>,<y> <w>x<h>[ in=<handle>][ extras]
//! ```
//!
//! Kinds: `rect`, `ellipse`, `diamond`, `sticky`, `stroke`, `glyph`, `text`,
//! `arrow`, `frame`, `stack`. Extras: a quoted label/name/text, `n=` and `w=`
//! for strokes, `from>to` and non-default routing for arrows, the glyph id
//! for glyphs, `fill=` for shaded strokes.

use std::fmt::Write as _;

use crate::model::{Document, Element, ElementKind, Endpoint, Routing, ShapeKind};

/// Maps element ids to the names the agent sees.
pub trait IdMap {
    fn map(&mut self, id: &str) -> String;
}

/// Identity mapping: element UUIDs appear verbatim.
pub struct UuidMap;

impl IdMap for UuidMap {
    fn map(&mut self, id: &str) -> String {
        id.to_string()
    }
}

/// The drawing as compact lines for the agent.
pub fn describe(doc: &Document) -> String {
    let mut ids = UuidMap;
    describe_with(doc, &mut ids)
}

/// The drawing as compact lines, with every element id passed through `ids`.
pub fn describe_with(doc: &Document, ids: &mut dyn IdMap) -> String {
    let live = doc.ordered_live();
    let mut out = String::new();
    let _ = writeln!(out, "{} elements", live.len());
    for element in &live {
        out.push_str(&describe_element_with(doc, element, ids));
        out.push('\n');
    }
    out
}

/// One element as a compact line, UUIDs verbatim.
pub fn describe_element(doc: &Document, element: &Element) -> String {
    let mut ids = UuidMap;
    describe_element_with(doc, element, &mut ids)
}

/// One element as a compact line, ids mapped through `ids`.
pub fn describe_element_with(doc: &Document, element: &Element, ids: &mut dyn IdMap) -> String {
    let bounds = element.bounds();
    let mut line = format!(
        "{} {} {:.0},{:.0} {:.0}x{:.0}",
        ids.map(&element.id),
        kind_name(element),
        bounds.x,
        bounds.y,
        bounds.w,
        bounds.h
    );
    if let Some(frame) = &element.frame_id {
        let _ = write!(line, " in={}", ids.map(frame));
    }
    match &element.kind {
        ElementKind::Shape { bound_text_id, .. } => {
            if let Some(text) = bound_text_id.as_ref().and_then(|id| text_of(doc, id)) {
                let _ = write!(line, " {text:?}");
            }
        }
        ElementKind::Glyph {
            glyph, caption_id, ..
        } => {
            let _ = write!(line, " {glyph}");
            if let Some(text) = caption_id.as_ref().and_then(|id| text_of(doc, id)) {
                let _ = write!(line, " {text:?}");
            }
        }
        ElementKind::Text { text, .. } => {
            let _ = write!(line, " {text:?}");
        }
        ElementKind::Frame { name, .. } => {
            if !name.is_empty() {
                let _ = write!(line, " {name:?}");
            }
        }
        ElementKind::Arrow {
            start,
            end,
            routing,
            label_id,
            ..
        } => {
            let _ = write!(
                line,
                " {}>{}",
                endpoint_name(start, ids),
                endpoint_name(end, ids)
            );
            if !matches!(routing, Routing::Elbow) {
                let _ = write!(line, " {}", routing_name(*routing));
            }
            if let Some(text) = label_id.as_ref().and_then(|id| text_of(doc, id)) {
                let _ = write!(line, " {text:?}");
            }
        }
        ElementKind::Freehand { points, style, .. } => {
            let _ = write!(line, " n={}", points.len());
            if (style.stroke_width - 2.0).abs() > f64::EPSILON {
                let _ = write!(line, " w={:.1}", style.stroke_width);
            }
            if style.fill_color != "transparent" {
                let _ = write!(line, " fill={:?}", style.fill_color);
            }
        }
        ElementKind::StickyStack { .. } => {}
    }
    line
}

/// One element as the structured read projection (schema version 1).
///
/// Carries the same identity and geometry as the compact line: handle, raw
/// id, kind, `[x, y, w, h]`, label and owning frame handle. Fields that do not
/// apply are `null`; the caller attaches the exact element JSON for detail
/// reads.
pub fn structured_element(
    doc: &Document,
    element: &Element,
    ids: &mut dyn IdMap,
) -> serde_json::Value {
    let bounds = element.bounds();
    let handle = ids.map(&element.id);
    let frame = element.frame_id.as_deref().map(|id| ids.map(id));
    serde_json::json!({
        "handle": handle,
        "id": element.id,
        "kind": kind_name(element),
        "bounds": [bounds.x, bounds.y, bounds.w, bounds.h],
        "label": label_of(doc, element),
        "frame": frame,
    })
}

/// The agent-facing label/name/text of an element, if it has one.
pub fn label_of(doc: &Document, element: &Element) -> Option<String> {
    match &element.kind {
        ElementKind::Shape { bound_text_id, .. } => {
            bound_text_id.as_ref().and_then(|id| text_of(doc, id))
        }
        ElementKind::Glyph { caption_id, .. } => {
            caption_id.as_ref().and_then(|id| text_of(doc, id))
        }
        ElementKind::Text { text, .. } => Some(text.clone()),
        ElementKind::Frame { name, .. } => (!name.is_empty()).then(|| name.clone()),
        ElementKind::Arrow { label_id, .. } => label_id.as_ref().and_then(|id| text_of(doc, id)),
        ElementKind::Freehand { .. } | ElementKind::StickyStack { .. } => None,
    }
}

/// The text of a text element, if `id` names one.
pub fn text_of(doc: &Document, id: &str) -> Option<String> {
    match &doc.element(id)?.kind {
        ElementKind::Text { text, .. } => Some(text.clone()),
        _ => None,
    }
}

fn kind_name(element: &Element) -> &'static str {
    match &element.kind {
        ElementKind::Shape { shape, .. } => match shape {
            ShapeKind::Rectangle => "rect",
            ShapeKind::Ellipse => "ellipse",
            ShapeKind::Diamond => "diamond",
            ShapeKind::Sticky => "sticky",
        },
        ElementKind::Freehand { .. } => "stroke",
        ElementKind::Glyph { .. } => "glyph",
        ElementKind::Arrow { .. } => "arrow",
        ElementKind::Text { .. } => "text",
        ElementKind::StickyStack { .. } => "stack",
        ElementKind::Frame { .. } => "frame",
    }
}

fn routing_name(routing: Routing) -> &'static str {
    match routing {
        Routing::Straight => "straight",
        Routing::Elbow => "elbow",
        Routing::Curved => "curved",
    }
}

fn endpoint_name(endpoint: &Endpoint, ids: &mut dyn IdMap) -> String {
    match &endpoint.binding {
        Some(binding) => ids.map(&binding.element_id),
        None => format!("({:.0},{:.0})", endpoint.x, endpoint.y),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;
    use crate::model::{Binding, Style, TextAlign};
    use std::collections::HashMap;

    struct Short(HashMap<String, String>);

    impl IdMap for Short {
        fn map(&mut self, id: &str) -> String {
            if let Some(handle) = self.0.get(id) {
                return handle.clone();
            }
            let handle = format!("e{}", self.0.len() + 1);
            self.0.insert(id.to_string(), handle.clone());
            handle
        }
    }

    fn labelled_board() -> Document {
        let mut doc = Document::new();
        let frame = doc.add(
            ElementKind::Frame {
                rect: Rect::new(0.0, 0.0, 400.0, 300.0),
                name: "VPC".into(),
            },
            1,
        );
        let text = doc.add(
            ElementKind::Text {
                rect: Rect::new(10.0, 10.0, 40.0, 20.0),
                text: "DB".into(),
                align: TextAlign::Center,
                fixed_width: false,
                container_id: None,
                style: Style::default(),
            },
            2,
        );
        let sticky = doc.add(
            ElementKind::Shape {
                shape: ShapeKind::Sticky,
                rect: Rect::new(10.0, 10.0, 140.0, 140.0),
                style: Style::default(),
                bound_text_id: Some(text.clone()),
            },
            3,
        );
        doc.element_mut(&sticky).unwrap().frame_id = Some(frame.clone());
        let arrow = doc.add(
            ElementKind::Arrow {
                start: Endpoint {
                    x: 0.0,
                    y: 0.0,
                    binding: Some(Binding {
                        element_id: frame.clone(),
                        focus: None,
                    }),
                },
                end: Endpoint {
                    x: 0.0,
                    y: 0.0,
                    binding: Some(Binding {
                        element_id: sticky.clone(),
                        focus: None,
                    }),
                },
                routing: Routing::Elbow,
                start_arrowhead: false,
                end_arrowhead: true,
                style: Style::default(),
                waypoints: Vec::new(),
                label_id: None,
            },
            4,
        );
        let _ = arrow;
        doc
    }

    #[test]
    fn lines_carry_kind_bounds_labels_and_connections() {
        let doc = labelled_board();
        let ids: Vec<String> = doc.ordered_live().iter().map(|e| e.id.clone()).collect();
        let described = describe(&doc);
        assert!(described.contains(&format!("{} sticky", ids[2])));
        assert!(described.contains("\"DB\""), "{described}");
        assert!(described.contains(&format!("{} arrow", ids[3])));
        assert!(
            described.contains(&format!("{}>{}", ids[0], ids[2])),
            "arrow endpoints render as handles: {described}"
        );
        assert!(described.contains(&format!("in={}", ids[0])));
        assert!(described.contains("\"VPC\""));
    }

    #[test]
    fn handles_replace_uuids_when_a_map_is_given() {
        let doc = labelled_board();
        let mut ids = Short(HashMap::new());
        let described = describe_with(&doc, &mut ids);
        assert!(described.starts_with("4 elements\n"));
        assert!(described.contains("e3 sticky"), "{described}");
        assert!(described.contains("e1>e3"), "{described}");
        assert!(
            !described.contains('-'),
            "no UUID dashes survive: {described}"
        );

        // Stable across calls: the same element keeps its handle.
        let again = describe_with(&doc, &mut ids);
        assert_eq!(described, again);
    }

    #[test]
    fn strokes_carry_sample_count_and_only_non_default_ink() {
        let mut doc = Document::new();
        let style = Style {
            stroke_width: 6.0,
            fill_color: "#ffec99".into(),
            ..Style::default()
        };
        doc.add(
            ElementKind::Freehand {
                points: vec![crate::geometry::Point::new(0.0, 0.0); 7],
                pressures: vec![0.5; 7],
                style,
            },
            1,
        );
        let described = describe(&doc);
        assert!(described.contains(" stroke "), "{described}");
        assert!(described.contains("n=7"), "{described}");
        assert!(described.contains("w=6.0"), "{described}");
        assert!(described.contains("fill=\"#ffec99\""), "{described}");
    }
}
