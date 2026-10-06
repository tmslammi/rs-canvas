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
    let mut last_index: Option<String> = None;

    for i in 0..count {
        let col = i % 50;
        let row = i / 50;
        let x = col as f64 * 220.0 + 20.0;
        let y = row as f64 * 170.0 + 20.0;
        let index = next_index(&mut last_index);
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
                    waypoints: Vec::new(),
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

/// The next fractional index key after `last`; kept incremental so building a
/// 200k-element fixture is O(n), not O(n²) (each `Document::top_index` scans
/// the whole store).
fn next_index(last: &mut Option<String>) -> String {
    let key = match &*last {
        Some(top) => crate::index::key_between(Some(top), None).expect("index space"),
        None => crate::index::key_between(None, None).expect("first index"),
    };
    *last = Some(key.clone());
    key
}

/// A deterministic UUID-shaped id, so wire/read benchmarks pay the same id
/// cost a real element does.
fn uuid_like(rng: &mut Lcg) -> String {
    let a = rng.next() as u32;
    let b = rng.next() as u16;
    let c = (rng.next() as u16 & 0x0fff) | 0x4000;
    let d = (rng.next() as u16 & 0x3fff) | 0x8000;
    let e = ((rng.next() << 16) | (rng.next() & 0xffff)) & 0xffff_ffff_ffff;
    format!("{a:08x}-{b:04x}-{c:04x}-{d:04x}-{e:012x}")
}

/// A freehand-heavy sketch like a real drawing: `live` elements on a grid
/// (70% freehand strokes with 20–60 samples, the rest shapes, text and bound
/// connectors) plus `tombstones` deleted elements. Deterministic, for
/// wire-cost benchmarking against `benchmark_fixture`'s architecture board.
pub fn sketch_fixture(live: usize, tombstones: usize) -> Document {
    let mut doc = Document::new();
    let mut rng = Lcg(90210);
    let palette = ["#ffec99", "#b2f2bb", "#a5d8ff", "#ffc9c9", "#d0bfff"];
    let mut last_index: Option<String> = None;
    let mut shapes: Vec<String> = Vec::new();

    for i in 0..live {
        let col = i % 40;
        let row = i / 40;
        let x = col as f64 * 260.0 + 20.0;
        let y = row as f64 * 200.0 + 20.0;
        let index = next_index(&mut last_index);
        let style = Style {
            fill_color: palette[rng.next() as usize % palette.len()].into(),
            fill_style: match rng.next() % 3 {
                0 => FillPattern::Hachure,
                1 => FillPattern::CrossHatch,
                _ => FillPattern::Solid,
            },
            roughness: (rng.next() % 3) as u8,
            stroke_width: 1.0 + (rng.next() % 30) as f64 / 10.0,
            opacity: 0.5 + (rng.next() % 50) as f64 / 100.0,
            ..Style::default()
        };
        let kind = match i % 10 {
            0..=6 => {
                let n = 20 + (rng.next() % 41) as usize;
                let mut points = Vec::with_capacity(n);
                let (mut px, mut py) = (x, y);
                for _ in 0..n {
                    // Fractional steps, like real pointer input: full-precision
                    // floats on the wire, not round numbers.
                    px += ((rng.next() % 24_001) as f64 / 10.0 - 1200.0) / 3.0;
                    py += ((rng.next() % 18_001) as f64 / 10.0 - 600.0) / 3.0;
                    points.push(Point::new(px, py));
                }
                let pressures: Vec<f64> = (0..n)
                    .map(|_| 0.2 + (rng.next() % 800) as f64 / 1000.0)
                    .collect();
                ElementKind::Freehand {
                    points,
                    pressures,
                    style,
                }
            }
            7 => {
                let id = uuid_like(&mut rng);
                let element = Element::new(
                    ElementKind::Shape {
                        shape: match i % 3 {
                            0 => ShapeKind::Rectangle,
                            1 => ShapeKind::Ellipse,
                            _ => ShapeKind::Diamond,
                        },
                        rect: Rect::new(x, y, 140.0, 90.0),
                        style,
                        bound_text_id: None,
                    },
                    index,
                    i as u64,
                )
                .with_id(id.clone());
                shapes.push(id);
                doc.insert(element);
                continue;
            }
            8 if shapes.len() >= 2 => {
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
                    waypoints: Vec::new(),
                    label_id: None,
                }
            }
            _ => ElementKind::Text {
                rect: Rect::new(x, y, 160.0, 26.0),
                text: format!("Note {i}"),
                align: TextAlign::Left,
                fixed_width: true,
                container_id: None,
                style: Style::default(),
            },
        };
        let element = Element::new(kind, index, i as u64).with_id(uuid_like(&mut rng));
        doc.insert(element);
    }

    for t in 0..tombstones {
        let index = next_index(&mut last_index);
        let mut element = Element::new(
            ElementKind::Shape {
                shape: ShapeKind::Rectangle,
                rect: Rect::new(t as f64 * 10.0, 0.0, 100.0, 60.0),
                style: Style::default(),
                bound_text_id: None,
            },
            index,
            0,
        )
        .with_id(uuid_like(&mut rng));
        element.is_deleted = true;
        element.touch(1);
        doc.insert(element);
    }
    doc
}

/// A dense architecture-like board: `nodes` shapes on a square grid, each
/// wired to its right and/or lower neighbour by a bound elbow connector.
/// `arrows_per_node` picks how many of those two links are drawn (0..=2),
/// so a 100k-node board can carry 100k–200k connectors.
pub fn connected_fixture(nodes: usize, arrows_per_node: usize) -> Document {
    let mut doc = Document::new();
    let mut rng = Lcg(4242);
    let palette = ["#ffec99", "#b2f2bb", "#a5d8ff", "#ffc9c9", "#d0bfff"];
    let cols = (nodes as f64).sqrt().ceil().max(1.0) as usize;
    let (spacing_x, spacing_y) = (220.0, 170.0);
    let arrows_per_node = arrows_per_node.min(2);
    let mut last_index: Option<String> = None;

    let mut node_ids: Vec<String> = Vec::with_capacity(nodes);
    for i in 0..nodes {
        let col = i % cols;
        let row = i / cols;
        let x = col as f64 * spacing_x + 20.0;
        let y = row as f64 * spacing_y + 20.0;
        let index = next_index(&mut last_index);
        let style = Style {
            fill_color: palette[rng.next() as usize % palette.len()].into(),
            fill_style: crate::model::FillPattern::Solid,
            roughness: 1,
            ..Style::default()
        };
        let kind = ElementKind::Shape {
            shape: match i % 3 {
                0 => ShapeKind::Rectangle,
                1 => ShapeKind::Ellipse,
                _ => ShapeKind::Diamond,
            },
            rect: Rect::new(x, y, 140.0, 90.0),
            style,
            bound_text_id: None,
        };
        let element = Element::new(kind, index, i as u64).with_id(format!("perf-{i:06}"));
        node_ids.push(element.id.clone());
        doc.insert(element);
    }

    let mut arrow_index = 0usize;
    for i in 0..nodes {
        let mut targets: Vec<usize> = Vec::with_capacity(2);
        if arrows_per_node >= 1 && (i + 1) % cols != 0 && i + 1 < nodes {
            targets.push(i + 1);
        }
        if arrows_per_node >= 2 && i + cols < nodes {
            targets.push(i + cols);
        }
        for target in targets {
            let from_rect = doc
                .element(&node_ids[i])
                .map(|e| e.bounds())
                .unwrap_or(Rect::ZERO);
            let to_rect = doc
                .element(&node_ids[target])
                .map(|e| e.bounds())
                .unwrap_or(Rect::ZERO);
            let index = next_index(&mut last_index);
            let connector = Element::new(
                ElementKind::Arrow {
                    start: crate::model::Endpoint {
                        x: from_rect.center().x,
                        y: from_rect.center().y,
                        binding: Some(crate::model::Binding {
                            element_id: node_ids[i].clone(),
                            focus: None,
                        }),
                    },
                    end: crate::model::Endpoint {
                        x: to_rect.center().x,
                        y: to_rect.center().y,
                        binding: Some(crate::model::Binding {
                            element_id: node_ids[target].clone(),
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
                index,
                arrow_index as u64,
            )
            .with_id(format!("perf-arrow-{arrow_index:06}"));
            arrow_index += 1;
            doc.insert(connector);
        }
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
    fn sketch_fixture_is_freehand_heavy_and_carries_tombstones() {
        let doc = sketch_fixture(500, 100);
        assert_eq!(doc.len(), 600);
        assert_eq!(doc.live_count(), 500);
        let strokes = doc
            .ordered_live()
            .iter()
            .filter(|e| matches!(e.kind, ElementKind::Freehand { .. }))
            .count();
        assert!(
            strokes > 300,
            "expected a freehand-heavy mix, got {strokes}"
        );
    }

    #[test]
    fn connected_fixture_wires_its_nodes() {
        // 100 nodes in a 10x10 grid: 9 right links per row and 9 down links
        // per column, so 180 connectors at full density.
        let doc = connected_fixture(100, 2);
        assert_eq!(doc.live_count(), 100 + 180);
        let doc = connected_fixture(100, 1);
        assert_eq!(doc.live_count(), 100 + 90);
        let doc = connected_fixture(100, 0);
        assert_eq!(doc.live_count(), 100);
    }

    #[test]
    fn culling_excludes_most_of_a_ten_thousand_element_document() {
        let doc = benchmark_fixture(10_000);
        let mut scene = crate::scene::Scene::new();
        scene.rebuild_all(&doc, &ApproxMeasurer, &crate::spatial::ObstacleIndex::new());
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
        scene.rebuild_all(&doc, &ApproxMeasurer, &crate::spatial::ObstacleIndex::new());
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
            scene.update(
                &doc,
                &changed,
                &ApproxMeasurer,
                &crate::spatial::ObstacleIndex::new(),
            );
        }
        assert_eq!(scene.layers.len(), 200, "one layer per live element");
    }
}
