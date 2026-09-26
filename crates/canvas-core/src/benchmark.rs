//! A deterministic large-document fixture for scale verification.

use crate::geometry::{Point, Rect};
use crate::model::{
    Document, Element, ElementKind, FillPattern, Routing, ShapeKind, Style, TextAlign,
};

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

/// A grid of mixed elements: shapes, freehand strokes, connectors and text.
pub fn benchmark_fixture(count: usize) -> Document {
    let mut doc = Document::new();
    let mut rng = Lcg(2026);
    let palette = ["#ffec99", "#b2f2bb", "#a5d8ff", "#ffc9c9", "#d0bfff"];
    let mut shapes: Vec<String> = Vec::new();

    for i in 0..count {
        let col = i % 50;
        let row = i / 50;
        let x = col as f64 * 220.0 + 20.0;
        let y = row as f64 * 170.0 + 20.0;
        let index = match doc.top_index() {
            Some(top) => crate::index::key_between(Some(&top), None).expect("index space"),
            None => crate::index::key_between(None, None).expect("first index"),
        };
        let style = Style {
            fill_color: palette[rng.next() as usize % palette.len()].into(),
            fill_style: match rng.next() % 3 {
                0 => FillPattern::Hachure,
                1 => FillPattern::CrossHatch,
                _ => FillPattern::Solid,
            },
            roughness: (rng.next() % 3) as u8,
            ..Style::default()
        };
        let kind = match i % 10 {
            9 if shapes.len() >= 2 => {
                let from = shapes[shapes.len() - 1].clone();
                let to = shapes[shapes.len() - 2].clone();
                let from_rect = doc.element(&from).map(|e| e.bounds()).unwrap_or(Rect::ZERO);
                let to_rect = doc.element(&to).map(|e| e.bounds()).unwrap_or(Rect::ZERO);
                ElementKind::Arrow {
                    start: crate::model::Endpoint {
                        x: from_rect.center().x,
                        y: from_rect.center().y,
                        binding: Some(crate::model::Binding {
                            element_id: from,
                            focus: None,
                        }),
                    },
                    end: crate::model::Endpoint {
                        x: to_rect.center().x,
                        y: to_rect.center().y,
                        binding: Some(crate::model::Binding {
                            element_id: to,
                            focus: None,
                        }),
                    },
                    routing: Routing::Elbow,
                    start_arrowhead: false,
                    end_arrowhead: true,
                    style: Style::default(),
                    label_id: None,
                }
            }
            8 => ElementKind::Freehand {
                points: (0..6)
                    .map(|k| Point::new(x + k as f64 * 12.0, y + (k % 2) as f64 * 18.0))
                    .collect(),
                pressures: Vec::new(),
                style: style.clone(),
            },
            5 => ElementKind::Text {
                rect: Rect::new(x, y, 140.0, 26.0),
                text: format!("Node {i}"),
                align: TextAlign::Left,
                fixed_width: true,
                container_id: None,
                style: Style::default(),
            },
            _ => ElementKind::Shape {
                shape: match i % 3 {
                    0 => ShapeKind::Rectangle,
                    1 => ShapeKind::Ellipse,
                    _ => ShapeKind::Diamond,
                },
                rect: Rect::new(x, y, 140.0, 90.0),
                style,
                bound_text_id: None,
            },
        };
        let element = Element::new(kind, index, i as u64).with_id(format!("bench-{i:05}"));
        if !matches!(element.kind, ElementKind::Arrow { .. }) {
            shapes.push(element.id.clone());
        }
        doc.insert(element);
    }
    doc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::ApproxMeasurer;

    #[test]
    fn fixture_has_the_requested_size() {
        let doc = benchmark_fixture(500);
        assert_eq!(doc.ordered_live().len(), 500);
    }

    #[test]
    fn culling_excludes_most_of_a_ten_thousand_element_document() {
        let doc = benchmark_fixture(10_000);
        let mut scene = crate::scene::Scene::new();
        scene.rebuild_all(&doc, &ApproxMeasurer);
        assert_eq!(scene.layers.len(), 10_000);

        // A single-cell viewport at the origin.
        let visible: Vec<_> = scene.visible(Rect::new(0.0, 0.0, 400.0, 300.0)).collect();
        assert!(!visible.is_empty());
        assert!(
            visible.len() < 50,
            "culled to a small window: {}",
            visible.len()
        );
    }

    #[test]
    fn updates_keep_the_layer_cache_bounded() {
        use std::collections::BTreeSet;
        let mut doc = benchmark_fixture(200);
        let mut scene = crate::scene::Scene::new();
        scene.rebuild_all(&doc, &ApproxMeasurer);
        for step in 0..100u64 {
            let id = format!("bench-{:05}", step % 200);
            let mut element = doc.element(&id).unwrap().clone();
            if let ElementKind::Shape { rect, .. } = &mut element.kind {
                rect.x += 1.0;
            } else {
                element.updated += 1;
            }
            element.touch(step);
            doc.insert(element);
            let changed: BTreeSet<String> = [id].into_iter().collect();
            scene.update(&doc, &changed, &ApproxMeasurer);
        }
        assert_eq!(scene.layers.len(), 200, "one layer per live element");
    }
}
