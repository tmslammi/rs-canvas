//! Discussion verbs: annotate, emphasis, tidy and clear — the moves you make
//! when thinking together, not just building. Everything here is a normal
//! document mutation, so it syncs, merges, saves and undoes like any edit.

use canvas_core::commands::{self, owned_label};
use canvas_core::geometry::{Point, Rect};
use canvas_core::model::{Document, ElementId, ElementKind, Style, TextAlign};
use canvas_core::text::{layout_text, ApproxMeasurer};
use canvas_core::{connector, element_style, element_style_mut, translate_element, Transaction};

use crate::doc_ops::GAP;

const CROSS_INK: &str = "#ff8787";
const CHECK_INK: &str = "#69db7c";
const QUESTION_INK: &str = "#ffc078";
const ANNOTATION_WIDTH: f64 = 3.0;
const DIM_OPACITY: f64 = 0.3;
const EMPHASIS_WIDTH: f64 = 3.5;

/// Put a cross, check or question mark on an element. Returns the elements
/// that make up the mark (a cross is two strokes).
pub fn annotate(
    doc: &mut Document,
    tx: &mut Transaction,
    target: &str,
    mark: &str,
    now_ms: u64,
) -> Result<Vec<ElementId>, String> {
    let bounds = doc
        .framing_bounds(target)
        .ok_or_else(|| format!("unknown element {target:?}"))?;
    let size = (bounds.w.min(bounds.h) * 0.45).clamp(24.0, 80.0);
    let center = bounds.center();
    let rect = Rect::new(center.x - size / 2.0, center.y - size / 2.0, size, size);
    match mark {
        "cross" => Ok(vec![
            stroke(
                doc,
                tx,
                &[
                    Point::new(rect.x, rect.y),
                    Point::new(rect.max_x(), rect.max_y()),
                ],
                CROSS_INK,
                now_ms,
            )?,
            stroke(
                doc,
                tx,
                &[
                    Point::new(rect.max_x(), rect.y),
                    Point::new(rect.x, rect.max_y()),
                ],
                CROSS_INK,
                now_ms,
            )?,
        ]),
        "check" => Ok(vec![stroke(
            doc,
            tx,
            &[
                Point::new(rect.x, rect.y + rect.h * 0.55),
                Point::new(rect.x + rect.w * 0.35, rect.max_y()),
                Point::new(rect.max_x(), rect.y),
            ],
            CHECK_INK,
            now_ms,
        )?]),
        "question" => {
            let style = Style {
                stroke_color: QUESTION_INK.into(),
                font_size: size,
                ..Style::default()
            };
            let layout = layout_text("?", style.font_size, None, &ApproxMeasurer);
            Ok(vec![commands::add_text(
                doc,
                tx,
                "?",
                Rect::new(
                    center.x - layout.width / 2.0,
                    center.y - layout.height / 2.0,
                    layout.width.max(16.0),
                    layout.height,
                ),
                TextAlign::Center,
                style,
                now_ms,
            )?])
        }
        other => Err(format!(
            "unknown mark {other:?}; expected cross, check or question"
        )),
    }
}

fn stroke(
    doc: &mut Document,
    tx: &mut Transaction,
    points: &[Point],
    ink: &str,
    now_ms: u64,
) -> Result<ElementId, String> {
    let style = Style {
        stroke_color: ink.into(),
        stroke_width: ANNOTATION_WIDTH,
        ..Style::default()
    };
    let pressures = vec![0.5; points.len()];
    commands::add_freehand(doc, tx, points.to_vec(), pressures, style, now_ms)
}

/// Change emphasis through the style model: `dim` fades, `emphasize` restores
/// opacity and thickens the stroke. Elements without a style (frames) are
/// skipped.
pub fn emphasis(
    doc: &mut Document,
    tx: &mut Transaction,
    ids: &[String],
    mode: &str,
    now_ms: u64,
) -> Result<usize, String> {
    if ids.is_empty() {
        return Err("emphasis needs at least one element".into());
    }
    let (opacity, width) = match mode {
        "dim" => (DIM_OPACITY, None),
        "emphasize" => (1.0, Some(EMPHASIS_WIDTH)),
        other => return Err(format!("unknown mode {other:?}; expected dim or emphasize")),
    };
    let mut changed = 0;
    for id in ids {
        let Some(element) = doc.element(id).filter(|element| !element.is_deleted) else {
            return Err(format!("unknown element {id:?}"));
        };
        if element_style(element).is_none() {
            continue;
        }
        tx.update(doc, id, now_ms, |element| {
            if let Some(style) = element_style_mut(element) {
                style.opacity = opacity;
                if let Some(width) = width {
                    style.stroke_width = width;
                }
            }
        })
        .map_err(|error| error.to_string())?;
        changed += 1;
    }
    Ok(changed)
}

/// Arrange a set into a tidy grid at their current top-left corner, in
/// reading order. Bound labels travel with their element and bound arrows
/// re-resolve so they stay attached. Arrows themselves are skipped — they
/// move with their endpoints.
pub fn tidy(
    doc: &mut Document,
    tx: &mut Transaction,
    ids: &[String],
    now_ms: u64,
) -> Result<usize, String> {
    if ids.is_empty() {
        return Err("tidy needs at least one element".into());
    }
    let mut targets: Vec<(ElementId, Rect)> = Vec::new();
    for id in ids {
        let element = doc
            .element(id)
            .filter(|element| !element.is_deleted)
            .ok_or_else(|| format!("unknown element {id:?}"))?;
        if matches!(element.kind, ElementKind::Arrow { .. }) {
            continue;
        }
        targets.push((id.clone(), element.bounds()));
    }
    if targets.is_empty() {
        return Err("nothing to tidy: arrows move with their endpoints".into());
    }
    targets.sort_by(|a, b| {
        (a.1.y, a.1.x)
            .partial_cmp(&(b.1.y, b.1.x))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let cell_w = targets.iter().map(|(_, rect)| rect.w).fold(0.0, f64::max);
    let cell_h = targets.iter().map(|(_, rect)| rect.h).fold(0.0, f64::max);
    let origin = targets
        .iter()
        .map(|(_, rect)| (rect.x, rect.y))
        .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1)))
        .expect("checked non-empty");
    let columns = (targets.len() as f64).sqrt().ceil().max(1.0) as usize;

    let mut moved: Vec<(ElementId, f64, f64)> = Vec::new();
    for (index, (id, rect)) in targets.iter().enumerate() {
        let col = index % columns;
        let row = index / columns;
        let target_x = origin.0 + col as f64 * (cell_w + GAP);
        let target_y = origin.1 + row as f64 * (cell_h + GAP);
        let (dx, dy) = (target_x - rect.x, target_y - rect.y);
        if dx.abs() < 1e-9 && dy.abs() < 1e-9 {
            continue;
        }
        moved.push((id.clone(), dx, dy));
    }

    for (id, dx, dy) in &moved {
        let label = owned_label(doc, id);
        tx.update(doc, id, now_ms, |element| {
            translate_element(element, *dx, *dy);
        })
        .map_err(|error| error.to_string())?;
        if let Some(label) = label {
            tx.update(doc, &label, now_ms, |element| {
                translate_element(element, *dx, *dy);
            })
            .map_err(|error| error.to_string())?;
        }
    }

    let moved_ids: Vec<ElementId> = moved.iter().map(|(id, _, _)| id.clone()).collect();
    let arrows: Vec<ElementId> = doc
        .ordered_live()
        .iter()
        .filter(|element| matches!(element.kind, ElementKind::Arrow { .. }))
        .map(|element| element.id.clone())
        .collect();
    for arrow_id in arrows {
        let bound_to_moved = doc
            .element(&arrow_id)
            .is_some_and(|arrow| match &arrow.kind {
                ElementKind::Arrow { start, end, .. } => [&start.binding, &end.binding]
                    .into_iter()
                    .flatten()
                    .any(|binding| moved_ids.contains(&binding.element_id)),
                _ => false,
            });
        if bound_to_moved {
            refresh_arrow(doc, tx, &arrow_id, now_ms)?;
        }
    }
    Ok(moved.len())
}

/// Tombstone a set of elements (frames take their children; containers take
/// their labels). The tombstone is what the merge model propagates.
pub fn clear(
    doc: &mut Document,
    tx: &mut Transaction,
    ids: &[String],
    now_ms: u64,
) -> Result<usize, String> {
    Ok(commands::delete(doc, tx, ids, now_ms)?.len())
}

fn refresh_arrow(
    doc: &mut Document,
    tx: &mut Transaction,
    arrow_id: &str,
    now_ms: u64,
) -> Result<(), String> {
    let Some(arrow) = doc.element(arrow_id).cloned() else {
        return Ok(());
    };
    let ElementKind::Arrow { start, end, .. } = &arrow.kind else {
        return Ok(());
    };
    let (start_point, _) = connector::resolve_endpoint(start, end.point(), doc);
    let (end_point, _) = connector::resolve_endpoint(end, start_point, doc);
    tx.update(doc, arrow_id, now_ms, |element| {
        if let ElementKind::Arrow { start, end, .. } = &mut element.kind {
            start.x = start_point.x;
            start.y = start_point.y;
            end.x = end_point.x;
            end.y = end_point.y;
        }
    })
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc_ops::{self, DrawRequest};

    fn draw(doc: &mut Document, kind: &str, text: Option<&str>) -> ElementId {
        doc_ops::draw(
            doc,
            &mut Transaction::begin(),
            &DrawRequest {
                kind: kind.into(),
                text: text.map(str::to_string),
                name: None,
                glyph: None,
                x: None,
                y: None,
                w: None,
                h: None,
                near: None,
                side: None,
            },
            1,
        )
        .unwrap()
    }

    fn annotate(
        doc: &mut Document,
        target: &str,
        mark: &str,
        now: u64,
    ) -> Result<Vec<ElementId>, String> {
        super::annotate(doc, &mut Transaction::begin(), target, mark, now)
    }

    fn emphasis(doc: &mut Document, ids: &[String], mode: &str, now: u64) -> Result<usize, String> {
        super::emphasis(doc, &mut Transaction::begin(), ids, mode, now)
    }

    fn tidy(doc: &mut Document, ids: &[String], now: u64) -> Result<usize, String> {
        super::tidy(doc, &mut Transaction::begin(), ids, now)
    }

    fn clear(doc: &mut Document, ids: &[String], now: u64) -> Result<usize, String> {
        super::clear(doc, &mut Transaction::begin(), ids, now)
    }

    #[test]
    fn annotations_are_document_elements_with_ink() {
        let mut doc = Document::new();
        let shape = draw(&mut doc, "sticky", Some("API"));
        let cross = annotate(&mut doc, &shape, "cross", 2).unwrap();
        assert_eq!(cross.len(), 2, "a cross is two strokes");
        for id in &cross {
            let element = doc.element(id).unwrap();
            let ElementKind::Freehand { style, .. } = &element.kind else {
                panic!("expected a stroke");
            };
            assert_eq!(style.stroke_color, CROSS_INK);
        }
        let check = annotate(&mut doc, &shape, "check", 3).unwrap();
        assert_eq!(check.len(), 1);
        let question = annotate(&mut doc, &shape, "question", 4).unwrap();
        assert!(matches!(
            doc.element(&question[0]).unwrap().kind,
            ElementKind::Text { .. }
        ));
        assert!(annotate(&mut doc, &shape, "star", 5).is_err());
        assert!(annotate(&mut doc, "ghost", "cross", 5).is_err());
    }

    #[test]
    fn emphasis_goes_through_the_style_model() {
        let mut doc = Document::new();
        let shape = draw(&mut doc, "rectangle", None);
        assert_eq!(
            emphasis(&mut doc, std::slice::from_ref(&shape), "dim", 2).unwrap(),
            1
        );
        let style = canvas_core::element_style(doc.element(&shape).unwrap()).unwrap();
        assert_eq!(style.opacity, DIM_OPACITY);
        assert_eq!(
            emphasis(&mut doc, std::slice::from_ref(&shape), "emphasize", 3).unwrap(),
            1
        );
        let style = canvas_core::element_style(doc.element(&shape).unwrap()).unwrap();
        assert_eq!(style.opacity, 1.0);
        assert_eq!(style.stroke_width, EMPHASIS_WIDTH);
        assert!(emphasis(&mut doc, &[shape], "shout", 4).is_err());
    }

    #[test]
    fn tidy_arranges_without_overlaps_and_keeps_arrows_bound() {
        let mut doc = Document::new();
        let a = draw(&mut doc, "sticky", Some("A"));
        let b = draw(&mut doc, "sticky", Some("B"));
        let c = draw(&mut doc, "sticky", Some("C"));
        // Connect a -> b so tidy has an arrow to keep attached.
        let arrow_id = doc.add(
            ElementKind::Arrow {
                start: canvas_core::model::Endpoint {
                    x: 0.0,
                    y: 0.0,
                    binding: Some(canvas_core::model::Binding {
                        element_id: a.clone(),
                        focus: None,
                    }),
                },
                end: canvas_core::model::Endpoint {
                    x: 0.0,
                    y: 0.0,
                    binding: Some(canvas_core::model::Binding {
                        element_id: b.clone(),
                        focus: None,
                    }),
                },
                routing: canvas_core::model::Routing::Elbow,
                start_arrowhead: false,
                end_arrowhead: true,
                style: Style::default(),
                waypoints: Vec::new(),
                label_id: None,
            },
            2,
        );

        let moved = tidy(&mut doc, &[a.clone(), b.clone(), c.clone()], 3).unwrap();
        assert!(moved >= 1);
        let rects: Vec<Rect> = [&a, &b, &c]
            .iter()
            .map(|id| doc.framing_bounds(id).unwrap())
            .collect();
        for (index, rect) in rects.iter().enumerate() {
            for other in rects.iter().skip(index + 1) {
                assert!(!rect.intersects(other), "{rect:?} overlaps {other:?}");
            }
        }
        // The arrow is still bound to both endpoints.
        let described = canvas_core::describe::describe(&doc);
        assert!(described.contains(&format!("{a}>{b}")), "{described}");
        let _ = arrow_id;
    }

    #[test]
    fn clear_tombstones_elements_and_their_labels() {
        let mut doc = Document::new();
        let shape = draw(&mut doc, "sticky", Some("API"));
        let before = doc.live_count();
        let cleared = clear(&mut doc, std::slice::from_ref(&shape), 2).unwrap();
        assert_eq!(cleared, 2, "the sticky and its bound label");
        assert_eq!(doc.live_count(), before - 2);
        assert!(doc.element(&shape).unwrap().is_deleted);
        assert!(clear(&mut doc, &[shape], 3).is_err(), "already gone");
        assert!(clear(&mut doc, &["ghost".into()], 3).is_err());
    }

    #[test]
    fn clearing_a_frame_takes_its_children() {
        let mut doc = Document::new();
        let frame = draw(&mut doc, "frame", None);
        let child = draw(&mut doc, "sticky", None);
        doc.element_mut(&child).unwrap().frame_id = Some(frame.clone());
        let cleared = clear(&mut doc, std::slice::from_ref(&frame), 2).unwrap();
        assert_eq!(cleared, 2);
        assert!(doc.element(&child).unwrap().is_deleted);
    }

    #[test]
    fn clearing_takes_arrows_bound_to_the_cleared() {
        use canvas_core::model::{Binding, ElementKind, Endpoint, Routing, Style};
        let mut doc = Document::new();
        let shape = draw(&mut doc, "rectangle", None);
        let other = draw(&mut doc, "rectangle", None);
        let arrow = doc.add(
            ElementKind::Arrow {
                start: Endpoint {
                    x: 0.0,
                    y: 0.0,
                    binding: Some(Binding {
                        element_id: shape.clone(),
                        focus: None,
                    }),
                },
                end: Endpoint {
                    x: 500.0,
                    y: 0.0,
                    binding: Some(Binding {
                        element_id: other.clone(),
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
            2,
        );
        let cleared = clear(&mut doc, std::slice::from_ref(&shape), 3).unwrap();
        assert_eq!(cleared, 2, "the shape and its bound arrow");
        assert!(doc.element(&arrow).unwrap().is_deleted);
        assert!(!doc.element(&other).unwrap().is_deleted);
    }
}
