//! Hit testing: which element is under a world point.

use crate::geometry::{Point, Rect};
use crate::model::{Document, Element, ElementId, ElementKind, ShapeKind};

/// The topmost live element under `point`, or `None`. `tolerance` is in
/// world units (screen tolerance divided by zoom).
pub fn hit_test(doc: &Document, point: Point, tolerance: f64) -> Option<ElementId> {
    for id in doc.ordered_ids().into_iter().rev() {
        let Some(element) = doc.element(&id) else {
            continue;
        };
        if element.is_deleted {
            continue;
        }
        if hits(doc, element, point, tolerance) {
            return Some(id);
        }
    }
    None
}

/// Live elements whose bounds intersect the rectangle, in z-order.
pub fn elements_in_rect(doc: &Document, rect: Rect) -> Vec<ElementId> {
    doc.ordered_live()
        .into_iter()
        .filter(|e| e.bounds().intersects(&rect) || e.bounds().expand(4.0).intersects(&rect))
        .map(|e| e.id.clone())
        .collect()
}

pub fn hits(doc: &Document, element: &Element, p: Point, tolerance: f64) -> bool {
    match &element.kind {
        ElementKind::Shape { shape, rect, .. } => match shape {
            ShapeKind::Rectangle => rect.expand(tolerance).contains(p),
            ShapeKind::Diamond => point_in_diamond(*rect, p, tolerance),
            ShapeKind::Ellipse => point_in_ellipse(*rect, p, tolerance),
        },
        ElementKind::Freehand { points, style, .. } => {
            near_polyline(points, p, tolerance + style.stroke_width / 2.0)
        }
        ElementKind::Arrow {
            start,
            end,
            routing,
            style,
            ..
        } => {
            let route = crate::connector::route(start, end, *routing, doc);
            near_polyline(&route, p, tolerance + style.stroke_width / 2.0)
        }
        ElementKind::Text { rect, .. } | ElementKind::Glyph { rect, .. } => {
            rect.expand(tolerance).contains(p)
        }
        ElementKind::Frame { rect, .. } => near_rect_border(*rect, p, tolerance.max(4.0)),
    }
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
}
