//! Hit testing: which element is under a world point.

use crate::geometry::{Point, Rect};
use crate::model::{Document, Element, ElementId, ElementKind, ShapeKind};

/// Resolve a bound text to the element that owns it; other ids pass through.
pub fn resolve_container(doc: &Document, id: &str) -> ElementId {
    match doc.element(id).map(|e| &e.kind) {
        Some(ElementKind::Text {
            container_id: Some(container),
            ..
        }) if doc
            .element(container)
            .map(|e| !e.is_deleted)
            .unwrap_or(false) =>
        {
            container.clone()
        }
        _ => id.to_string(),
    }
}

/// The topmost live element under `point`, or `None`. `tolerance` is in
/// world units (screen tolerance divided by zoom).
///
/// Bound text (glyph captions, shape labels, connector labels) resolves to
/// the element that owns it: clicking a component's caption selects the
/// component.
pub fn hit_test(doc: &Document, point: Point, tolerance: f64) -> Option<ElementId> {
    let id = hit_test_raw(doc, point, tolerance)?;
    Some(resolve_container(doc, &id))
}

/// Hit testing without the bound-text-to-container mapping (text editing,
/// text tool), so the caption itself can be edited in place.
///
/// Scans the flat store and keeps the topmost match by `(index, id)`; this
/// needs no z-order sort, and every element is bounds-culled before its
/// geometry is examined.
pub fn hit_test_raw(doc: &Document, point: Point, tolerance: f64) -> Option<ElementId> {
    let mut best: Option<&Element> = None;
    for element in doc.elements.values() {
        if element.is_deleted || invisible(element) {
            continue;
        }
        if !hits(doc, element, point, tolerance) {
            continue;
        }
        let topmost = best.is_none_or(|b| {
            (element.index.as_str(), element.id.as_str()) > (b.index.as_str(), b.id.as_str())
        });
        if topmost {
            best = Some(element);
        }
    }
    best.map(|element| element.id.clone())
}

/// Fully transparent elements never intercept input — there is no paint
/// under the cursor for the user to aim at.
pub(crate) fn invisible(element: &Element) -> bool {
    crate::editor::element_style(element)
        .map(|style| style.opacity <= 0.0)
        .unwrap_or(false)
}

/// Live elements whose bounds intersect the rectangle, in z-order.
pub fn elements_in_rect(doc: &Document, rect: Rect) -> Vec<ElementId> {
    doc.ordered_live()
        .into_iter()
        .filter(|e| !invisible(e))
        .filter(|e| {
            let bounds = e.bounds();
            bounds.intersects(&rect) || bounds.expand(4.0).intersects(&rect)
        })
        .map(|e| e.id.clone())
        .collect()
}

/// Indexed hit testing for pointer-hot paths (hover, click, marquee): the
/// same results as the full scan, from grid candidates only. `tolerance` is
/// in world units, as in [`hit_test_raw`].
pub fn hit_test_raw_indexed(
    doc: &Document,
    index: &crate::spatial::HitIndex,
    point: Point,
    tolerance: f64,
) -> Option<ElementId> {
    // Same reject the exact test uses (model bounds + max stroke slack), so
    // the candidate set is a superset of the scan's and results are identical.
    let pad = tolerance + crate::spatial::hit_pad();
    let area = Rect::new(point.x - pad, point.y - pad, pad * 2.0, pad * 2.0);
    let mut best: Option<(String, String)> = None; // (index key, id)
    for row in index.query(area) {
        let Some((id, key, _)) = index.row(row) else {
            continue;
        };
        let Some(element) = doc.element(id).filter(|e| !e.is_deleted) else {
            continue;
        };
        if !hits(doc, element, point, tolerance) {
            continue;
        }
        let topmost = best
            .as_ref()
            .is_none_or(|(best_key, best_id)| (key, id) > (best_key.as_str(), best_id.as_str()));
        if topmost {
            best = Some((key.to_string(), id.to_string()));
        }
    }
    best.map(|(_, id)| id)
}

/// Indexed [`hit_test`] with the bound-text-to-container mapping.
pub fn hit_test_indexed(
    doc: &Document,
    index: &crate::spatial::HitIndex,
    point: Point,
    tolerance: f64,
) -> Option<ElementId> {
    hit_test_raw_indexed(doc, index, point, tolerance).map(|id| resolve_container(doc, &id))
}

/// Indexed [`elements_in_rect`]: identical results from grid candidates.
pub fn elements_in_rect_indexed(
    doc: &Document,
    index: &crate::spatial::HitIndex,
    rect: Rect,
) -> Vec<ElementId> {
    let pad = crate::spatial::hit_pad() + 4.0;
    let area = rect.expand(pad);
    let mut ids: Vec<ElementId> = Vec::new();
    for row in index.query(area) {
        let Some((id, _, _)) = index.row(row) else {
            continue;
        };
        let Some(element) = doc.element(id).filter(|e| !e.is_deleted) else {
            continue;
        };
        if invisible(element) {
            continue;
        }
        let bounds = element.bounds();
        if bounds.intersects(&rect) || bounds.expand(4.0).intersects(&rect) {
            ids.push(element.id.clone());
        }
    }
    // Restore z-order: candidates arrive in row (insertion) order.
    ids.sort_by(|a, b| {
        let (ka, kb) = (
            doc.element(a).map(|e| e.index.as_str()).unwrap_or(""),
            doc.element(b).map(|e| e.index.as_str()).unwrap_or(""),
        );
        ka.cmp(kb).then_with(|| a.cmp(b))
    });
    ids
}

pub fn hits(doc: &Document, element: &Element, p: Point, tolerance: f64) -> bool {
    if element.is_deleted || invisible(element) {
        return false;
    }
    match &element.kind {
        ElementKind::Shape {
            shape, rect, style, ..
        } => {
            if !rect.expand(tolerance + style.stroke_width).contains(p) {
                return false;
            }
            match shape {
                ShapeKind::Rectangle => rect.expand(tolerance).contains(p),
                ShapeKind::Sticky => rect.expand(tolerance).contains(p),
                ShapeKind::Diamond => point_in_diamond(*rect, p, tolerance),
                ShapeKind::Ellipse => point_in_ellipse(*rect, p, tolerance),
            }
        }
        ElementKind::Freehand { points, style, .. } => {
            let slack = tolerance + style.stroke_width / 2.0;
            if !element.bounds().expand(slack).contains(p) {
                return false;
            }
            near_polyline(points, p, slack)
        }
        ElementKind::Arrow {
            start,
            end,
            routing,
            style,
            waypoints,
            ..
        } => {
            let slack = tolerance + style.stroke_width / 2.0;
            // Cheap reject: plain and waypoint routes stay inside the
            // element bounds, so a point outside them can never hit.
            if !element.bounds().expand(slack).contains(p) {
                return false;
            }
            let route = crate::connector::route(start, end, *routing, waypoints, doc);
            if !polyline_bounds(&route).expand(slack).contains(p) {
                return false;
            }
            near_polyline(&route, p, slack)
        }
        ElementKind::Text { rect, .. } | ElementKind::Glyph { rect, .. } => {
            rect.expand(tolerance).contains(p)
        }
        ElementKind::StickyStack { rect, .. } => rect.expand(tolerance).contains(p),
        ElementKind::Frame { rect, .. } => {
            near_rect_border(*rect, p, tolerance.max(4.0)) || frame_name_hit(*rect, p)
        }
    }
}

fn polyline_bounds(points: &[Point]) -> Rect {
    let Some(first) = points.first() else {
        return Rect::ZERO;
    };
    let mut min_x = first.x;
    let mut min_y = first.y;
    let mut max_x = first.x;
    let mut max_y = first.y;
    for p in points {
        min_x = min_x.min(p.x);
        min_y = min_y.min(p.y);
        max_x = max_x.max(p.x);
        max_y = max_y.max(p.y);
    }
    Rect::new(min_x, min_y, max_x - min_x, max_y - min_y)
}

fn frame_name_hit(rect: Rect, p: Point) -> bool {
    let band = Rect::new(rect.x, rect.y - 18.0, rect.w.clamp(120.0, 240.0), 18.0);
    band.contains(p)
}

fn point_in_diamond(rect: Rect, p: Point, tolerance: f64) -> bool {
    let c = rect.center();
    let rx = rect.w / 2.0 + tolerance;
    let ry = rect.h / 2.0 + tolerance;
    if rx <= 0.0 || ry <= 0.0 {
        return false;
    }
    (p.x - c.x).abs() / rx + (p.y - c.y).abs() / ry <= 1.0
}

fn point_in_ellipse(rect: Rect, p: Point, tolerance: f64) -> bool {
    let c = rect.center();
    let rx = rect.w / 2.0 + tolerance;
    let ry = rect.h / 2.0 + tolerance;
    if rx <= 0.0 || ry <= 0.0 {
        return false;
    }
    let nx = (p.x - c.x) / rx;
    let ny = (p.y - c.y) / ry;
    nx * nx + ny * ny <= 1.0
}

fn near_rect_border(rect: Rect, p: Point, tolerance: f64) -> bool {
    if !rect.expand(tolerance).contains(p) {
        return false;
    }
    let inner = Rect::new(
        rect.x + tolerance,
        rect.y + tolerance,
        (rect.w - 2.0 * tolerance).max(0.0),
        (rect.h - 2.0 * tolerance).max(0.0),
    );
    !inner.contains(p)
}

fn near_polyline(points: &[Point], p: Point, tolerance: f64) -> bool {
    if points.is_empty() {
        return false;
    }
    if points.len() == 1 {
        return p.distance(points[0]) <= tolerance;
    }
    points
        .windows(2)
        .any(|w| distance_to_segment(p, w[0], w[1]) <= tolerance)
}

fn distance_to_segment(p: Point, a: Point, b: Point) -> f64 {
    let ab = b - a;
    let len_sq = ab.dot(ab);
    if len_sq < 1e-12 {
        return p.distance(a);
    }
    let t = ((p - a).dot(ab) / len_sq).clamp(0.0, 1.0);
    p.distance(a + ab.scale(t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ElementKind, ShapeKind, Style};

    fn rect_element(doc: &mut Document, rect: Rect) -> ElementId {
        doc.add(
            ElementKind::Shape {
                shape: ShapeKind::Rectangle,
                rect,
                style: Style::default(),
                bound_text_id: None,
            },
            0,
        )
    }

    #[test]
    fn rectangle_hits_inside_only() {
        let mut doc = Document::new();
        let id = rect_element(&mut doc, Rect::new(0.0, 0.0, 100.0, 50.0));
        assert_eq!(
            hit_test(&doc, Point::new(50.0, 25.0), 5.0),
            Some(id.clone())
        );
        assert_eq!(hit_test(&doc, Point::new(150.0, 25.0), 5.0), None);
    }

    #[test]
    fn topmost_element_wins() {
        let mut doc = Document::new();
        let _bottom = rect_element(&mut doc, Rect::new(0.0, 0.0, 100.0, 100.0));
        let top = rect_element(&mut doc, Rect::new(50.0, 50.0, 100.0, 100.0));
        assert_eq!(hit_test(&doc, Point::new(75.0, 75.0), 0.0), Some(top));
    }

    #[test]
    fn diamond_corners_miss() {
        let mut doc = Document::new();
        doc.add(
            ElementKind::Shape {
                shape: ShapeKind::Diamond,
                rect: Rect::new(0.0, 0.0, 100.0, 100.0),
                style: Style::default(),
                bound_text_id: None,
            },
            0,
        );
        assert!(hit_test(&doc, Point::new(50.0, 50.0), 0.0).is_some());
        assert!(hit_test(&doc, Point::new(5.0, 5.0), 0.0).is_none());
    }

    #[test]
    fn ellipse_corners_miss_centre_hits() {
        let mut doc = Document::new();
        doc.add(
            ElementKind::Shape {
                shape: ShapeKind::Ellipse,
                rect: Rect::new(0.0, 0.0, 100.0, 50.0),
                style: Style::default(),
                bound_text_id: None,
            },
            0,
        );
        assert!(hit_test(&doc, Point::new(50.0, 25.0), 0.0).is_some());
        assert!(hit_test(&doc, Point::new(3.0, 3.0), 0.0).is_none());
    }

    #[test]
    fn freehand_hits_near_the_stroke() {
        let mut doc = Document::new();
        doc.add(
            ElementKind::Freehand {
                points: vec![Point::new(0.0, 0.0), Point::new(100.0, 0.0)],
                pressures: Vec::new(),
                style: Style::default(),
            },
            0,
        );
        assert!(hit_test(&doc, Point::new(50.0, 1.0), 3.0).is_some());
        assert!(hit_test(&doc, Point::new(50.0, 40.0), 3.0).is_none());
    }

    #[test]
    fn bound_text_selects_its_container() {
        use crate::model::{ElementKind, TextAlign};
        let mut doc = Document::new();
        let glyph = doc.add(
            ElementKind::Glyph {
                glyph: "cloud_storage".into(),
                rect: Rect::new(0.0, 0.0, 96.0, 96.0),
                style: Style::default(),
                caption_id: None,
            },
            0,
        );
        let caption = doc.add(
            ElementKind::Text {
                rect: Rect::new(-48.0, 100.0, 192.0, 26.0),
                text: "Cloud Storage".into(),
                align: TextAlign::Center,
                fixed_width: true,
                container_id: Some(glyph.clone()),
                style: Style::default(),
            },
            0,
        );
        if let ElementKind::Glyph { caption_id, .. } = &mut doc.element_mut(&glyph).unwrap().kind {
            *caption_id = Some(caption.clone());
        }

        let on_caption = Point::new(0.0, 110.0);
        assert_eq!(hit_test(&doc, on_caption, 2.0), Some(glyph.clone()));
        assert_eq!(hit_test_raw(&doc, on_caption, 2.0), Some(caption.clone()));

        // A dangling container falls back to the text itself.
        doc.element_mut(&glyph).unwrap().is_deleted = true;
        assert_eq!(hit_test(&doc, on_caption, 2.0), Some(caption));
    }

    #[test]
    fn frames_hit_on_their_border_only() {
        let mut doc = Document::new();
        doc.add(
            ElementKind::Frame {
                rect: Rect::new(0.0, 0.0, 200.0, 150.0),
                name: "VPC".into(),
            },
            0,
        );
        assert!(hit_test(&doc, Point::new(0.0, 75.0), 2.0).is_some());
        assert!(hit_test(&doc, Point::new(100.0, 75.0), 2.0).is_none());
    }

    #[test]
    fn marquee_selects_intersecting_elements() {
        let mut doc = Document::new();
        let a = rect_element(&mut doc, Rect::new(0.0, 0.0, 50.0, 50.0));
        let b = rect_element(&mut doc, Rect::new(200.0, 200.0, 50.0, 50.0));
        let hits = elements_in_rect(&doc, Rect::new(-10.0, -10.0, 100.0, 100.0));
        assert_eq!(hits, vec![a]);
        assert!(!hits.contains(&b));
    }

    #[test]
    fn invisible_elements_do_not_intercept_hits() {
        let mut doc = Document::new();
        let id = rect_element(&mut doc, Rect::new(0.0, 0.0, 50.0, 50.0));
        let point = Point::new(25.0, 25.0);
        assert_eq!(hit_test(&doc, point, 2.0), Some(id.clone()));
        if let ElementKind::Shape { style, .. } = &mut doc.element_mut(&id).unwrap().kind {
            style.opacity = 0.0;
        }
        assert_eq!(hit_test(&doc, point, 2.0), None);
        assert_eq!(hit_test_raw(&doc, point, 2.0), None);
    }
}
