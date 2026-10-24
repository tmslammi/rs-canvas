//! The seed demo: a small, proper cloud architecture that exercises the
//! glyph library, frames, connectors and labels. Loaded from the toolbar —
//! the app boots empty.

use crate::connector;
use crate::geometry::{Point, Rect};
use crate::model::{Binding, Document, ElementKind, Endpoint, Routing, Style, TextAlign};

/// Build the demo architecture deterministically.
pub fn demo_document() -> Document {
    let mut doc = Document::new();
    let frame_id = doc.add(
        ElementKind::Frame {
            rect: Rect::new(520.0, 120.0, 780.0, 600.0),
            name: "VPC prod".into(),
        },
        0,
    );

    let user = glyph(&mut doc, "user", 100.0, 360.0, "User", None);
    let load_balancer = glyph(
        &mut doc,
        "load_balancing",
        320.0,
        360.0,
        "Load Balancing",
        None,
    );
    let gke = glyph(&mut doc, "gke", 640.0, 220.0, "GKE", Some(&frame_id));
    let sql = glyph(
        &mut doc,
        "cloud_sql",
        640.0,
        480.0,
        "Cloud SQL",
        Some(&frame_id),
    );
    let storage = glyph(
        &mut doc,
        "cloud_storage",
        1060.0,
        220.0,
        "Cloud Storage",
        Some(&frame_id),
    );
    let pubsub = glyph(
        &mut doc,
        "pubsub",
        1060.0,
        480.0,
        "Pub/Sub",
        Some(&frame_id),
    );

    connect(&mut doc, &user, &load_balancer, None);
    connect(&mut doc, &load_balancer, &gke, Some("https"));
    connect(&mut doc, &gke, &sql, Some("jdbc"));
    connect(&mut doc, &gke, &storage, Some("objects"));
    connect(&mut doc, &gke, &pubsub, Some("events"));

    doc
}

#[allow(clippy::too_many_arguments)]
fn glyph(
    doc: &mut Document,
    glyph_id: &str,
    x: f64,
    y: f64,
    name: &str,
    frame: Option<&str>,
) -> String {
    let id = doc.add(
        ElementKind::Glyph {
            glyph: glyph_id.into(),
            rect: Rect::new(x, y, 96.0, 96.0),
            style: Style::default(),
            caption_id: None,
        },
        0,
    );
    if let Some(frame) = frame {
        if let Some(element) = doc.element_mut(&id) {
            element.frame_id = Some(frame.to_string());
        }
    }
    let caption = doc.add(
        ElementKind::Text {
            rect: Rect::new(x - 48.0, y + 100.0, 192.0, 26.0),
            text: name.into(),
            align: TextAlign::Center,
            fixed_width: true,
            container_id: Some(id.clone()),
            style: Style::default(),
        },
        0,
    );
    if let Some(element) = doc.element_mut(&id) {
        if let ElementKind::Glyph { caption_id, .. } = &mut element.kind {
            *caption_id = Some(caption);
        }
    }
    id
}

fn connect(doc: &mut Document, from: &str, to: &str, label: Option<&str>) -> String {
    let from_rect = doc.element(from).map(|e| e.bounds()).unwrap_or(Rect::ZERO);
    let to_rect = doc.element(to).map(|e| e.bounds()).unwrap_or(Rect::ZERO);
    let arrow = doc.add(
        ElementKind::Arrow {
            start: Endpoint {
                x: from_rect.center().x,
                y: from_rect.center().y,
                binding: Some(Binding {
                    element_id: from.into(),
                    focus: None,
                }),
            },
            end: Endpoint {
                x: to_rect.center().x,
                y: to_rect.center().y,
                binding: Some(Binding {
                    element_id: to.into(),
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
        0,
    );
    if let Some(text) = label {
        let midpoint = doc
            .element(&arrow)
            .and_then(|element| match &element.kind {
                ElementKind::Arrow {
                    start,
                    end,
                    routing,
                    waypoints,
                    ..
                } => Some(connector::route(start, end, *routing, waypoints, doc)),
                _ => None,
            })
            .map(|route| connector::point_at(&route, 0.5))
            .unwrap_or(Point::ZERO);
        let label_id = doc.add(
            ElementKind::Text {
                rect: Rect::new(midpoint.x - 60.0, midpoint.y - 34.0, 120.0, 26.0),
                text: text.into(),
                align: TextAlign::Center,
                fixed_width: true,
                container_id: Some(arrow.clone()),
                style: Style::default(),
            },
            0,
        );
        if let Some(element) = doc.element_mut(&arrow) {
            if let ElementKind::Arrow { label_id: slot, .. } = &mut element.kind {
                *slot = Some(label_id);
            }
        }
    }
    arrow
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::ApproxMeasurer;

    #[test]
    fn demo_is_a_connected_architecture() {
        let doc = demo_document();
        let live = doc.ordered_live();
        let glyphs = live
            .iter()
            .filter(|e| matches!(e.kind, ElementKind::Glyph { .. }))
            .count();
        let arrows = live
            .iter()
            .filter(|e| matches!(e.kind, ElementKind::Arrow { .. }))
            .count();
        let frames = live.iter().filter(|e| e.is_frame()).count();
        assert_eq!(glyphs, 6);
        assert_eq!(arrows, 5);
        assert_eq!(frames, 1);

        // Every element renders and the scene builds without gaps.
        let mut scene = crate::scene::Scene::new();
        let mut obstacles = crate::spatial::ObstacleIndex::new();
        obstacles.rebuild(&doc);
        scene.rebuild_all(&doc, &ApproxMeasurer, &obstacles);
        assert_eq!(scene.layers.len(), live.len());
        // Ops build on demand: ensure the whole board, then check.
        let everywhere = crate::geometry::Rect::new(-1e9, -1e9, 2e9, 2e9);
        scene.ensure_area(&doc, everywhere, &ApproxMeasurer, &obstacles);
        for layer in &scene.layers {
            assert!(
                !layer.ops.is_empty(),
                "{} renders nothing",
                layer.element_id
            );
        }
    }

    #[test]
    fn demo_connectors_are_bound_and_labelled() {
        let doc = demo_document();
        let labelled = doc
            .ordered_live()
            .into_iter()
            .filter(|e| {
                matches!(
                    &e.kind,
                    ElementKind::Arrow {
                        label_id: Some(_),
                        ..
                    }
                )
            })
            .count();
        assert_eq!(labelled, 4);

        let bound = doc
            .ordered_live()
            .into_iter()
            .filter(|e| {
                matches!(
                    &e.kind,
                    ElementKind::Arrow {
                        start: Endpoint {
                            binding: Some(_),
                            ..
                        },
                        end: Endpoint {
                            binding: Some(_),
                            ..
                        },
                        ..
                    }
                )
            })
            .count();
        assert_eq!(bound, 5, "every connector binds at both ends");
    }
}
