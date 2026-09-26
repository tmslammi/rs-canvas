//! Connector routing: anchor resolution, elbow/curved paths.
//!
//! Endpoints bind to elements (`Binding`); each render and hit-test resolves
//! the actual point from the element's current bounds, so connectors follow
//! the shapes they connect.

use crate::geometry::{Point, Rect};
use crate::model::{Document, Endpoint, Routing};

/// The four side anchors of an element's bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnchorSide {
    N,
    E,
    S,
    W,
}

impl AnchorSide {
    pub const ALL: [AnchorSide; 4] = [AnchorSide::N, AnchorSide::E, AnchorSide::S, AnchorSide::W];

    /// Normalized focus vector for this side.
    pub fn focus(self) -> Point {
        match self {
            AnchorSide::N => Point::new(0.0, -1.0),
            AnchorSide::E => Point::new(1.0, 0.0),
            AnchorSide::S => Point::new(0.0, 1.0),
            AnchorSide::W => Point::new(-1.0, 0.0),
        }
    }

    pub fn from_focus(focus: Point) -> AnchorSide {
        AnchorSide::ALL
            .into_iter()
            .max_by(|a, b| {
                let da = a.focus().dot(focus);
                let db = b.focus().dot(focus);
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or(AnchorSide::E)
    }

    pub fn point(self, rect: Rect) -> Point {
        let c = rect.center();
        let f = self.focus();
        Point::new(c.x + f.x * rect.w / 2.0, c.y + f.y * rect.h / 2.0)
    }

    pub fn is_vertical(self) -> bool {
        matches!(self, AnchorSide::N | AnchorSide::S)
    }
}

/// All anchor points of a rectangle.
pub fn anchor_points(rect: Rect) -> Vec<(AnchorSide, Point)> {
    AnchorSide::ALL
        .into_iter()
        .map(|side| (side, side.point(rect)))
        .collect()
}

/// The nearest anchor of a rectangle to a target point.
pub fn nearest_anchor(rect: Rect, target: Point) -> (AnchorSide, Point) {
    anchor_points(rect)
        .into_iter()
        .min_by(|(_, a), (_, b)| {
            a.distance(target)
                .partial_cmp(&b.distance(target))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or((AnchorSide::E, rect.center()))
}

/// Resolve an endpoint against the document: bound endpoints follow their
/// element; `other` is the opposite endpoint used for auto-anchor selection.
pub fn resolve_endpoint(
    endpoint: &Endpoint,
    other: Point,
    doc: &Document,
) -> (Point, Option<AnchorSide>) {
    let Some(binding) = &endpoint.binding else {
        return (endpoint.point(), None);
    };
    let Some(element) = doc.element(&binding.element_id).filter(|e| !e.is_deleted) else {
        return (endpoint.point(), None);
    };
    let rect = element.bounds();
    let side = match binding.focus {
        Some(focus) => AnchorSide::from_focus(focus),
        None => nearest_anchor(rect, other).0,
    };
    (side.point(rect), Some(side))
}

/// Resolve just the point (convenience for rendering and hit-testing).
pub fn endpoint_point(endpoint: &Endpoint, other: Point, doc: &Document) -> Point {
    resolve_endpoint(endpoint, other, doc).0
}

/// The routed polyline for a connector.
pub fn route(start: &Endpoint, end: &Endpoint, routing: Routing, doc: &Document) -> Vec<Point> {
    let (start_point, start_side) = resolve_endpoint(start, end.point(), doc);
    let (end_point, end_side) = resolve_endpoint(end, start.point(), doc);
    match routing {
        Routing::Straight => vec![start_point, end_point],
        Routing::Elbow => elbow_route(start_point, start_side, end_point, end_side),
        Routing::Curved => curved_route(start_point, start_side, end_point, end_side),
    }
}

/// Corner points for an elbow route.
pub fn elbow_corners(
    a: Point,
    a_side: Option<AnchorSide>,
    b: Point,
    b_side: Option<AnchorSide>,
) -> Vec<Point> {
    let a_vertical = a_side.map(|s| s.is_vertical()).unwrap_or(false);
    let b_vertical = b_side.map(|s| s.is_vertical()).unwrap_or(false);
    let raw = match (a_vertical, b_vertical) {
        (true, true) => {
            let my = (a.y + b.y) / 2.0;
            vec![a, Point::new(a.x, my), Point::new(b.x, my), b]
        }
        (false, false) => {
            let mx = (a.x + b.x) / 2.0;
            vec![a, Point::new(mx, a.y), Point::new(mx, b.y), b]
        }
        (true, false) => vec![a, Point::new(a.x, b.y), b],
        (false, true) => vec![a, Point::new(b.x, a.y), b],
    };
    dedupe(raw)
}

fn elbow_route(
    a: Point,
    a_side: Option<AnchorSide>,
    b: Point,
    b_side: Option<AnchorSide>,
) -> Vec<Point> {
    elbow_corners(a, a_side, b, b_side)
}

fn curved_route(
    a: Point,
    a_side: Option<AnchorSide>,
    b: Point,
    b_side: Option<AnchorSide>,
) -> Vec<Point> {
    let corners = elbow_corners(a, a_side, b, b_side);
    let control = if corners.len() >= 4 {
        corners[1].lerp(corners[corners.len() - 2], 0.5)
    } else {
        corners.get(1).copied().unwrap_or_else(|| a.lerp(b, 0.5))
    };
    let samples = 24;
    (0..=samples)
        .map(|i| {
            let t = i as f64 / samples as f64;
            quadratic(a, control, b, t)
        })
        .collect()
}

fn quadratic(a: Point, control: Point, b: Point, t: f64) -> Point {
    let one = 1.0 - t;
    Point::new(
        one * one * a.x + 2.0 * one * t * control.x + t * t * b.x,
        one * one * a.y + 2.0 * one * t * control.y + t * t * b.y,
    )
}

fn dedupe(points: Vec<Point>) -> Vec<Point> {
    let mut out: Vec<Point> = Vec::with_capacity(points.len());
    for p in points {
        if out
            .last()
            .map(|last| last.distance(p) > 0.01)
            .unwrap_or(true)
        {
            out.push(p);
        }
    }
    out
}

/// The point along a polyline at parameter t in [0, 1] by arc length.
pub fn point_at(points: &[Point], t: f64) -> Point {
    if points.is_empty() {
        return Point::ZERO;
    }
    if points.len() == 1 {
        return points[0];
    }
    let lengths: Vec<f64> = points.windows(2).map(|w| w[0].distance(w[1])).collect();
    let total: f64 = lengths.iter().sum();
    if total <= 1e-9 {
        return points[0];
    }
    let target = total * t.clamp(0.0, 1.0);
    let mut travelled = 0.0;
    for (i, length) in lengths.iter().enumerate() {
        if travelled + length >= target {
            let local = if *length > 1e-9 {
                (target - travelled) / length
            } else {
                0.0
            };
            return points[i].lerp(points[i + 1], local);
        }
        travelled += length;
    }
    points[points.len() - 1]
}

/// Arrowhead triangle for a tip, pointing away from `from`.
pub fn arrowhead(tip: Point, from: Point, stroke_width: f64) -> Vec<Point> {
    let direction = tip - from;
    let length = direction.length();
    if length < 1e-6 {
        return Vec::new();
    }
    let unit = direction.scale(1.0 / length);
    let normal = Point::new(-unit.y, unit.x);
    let head_length = 8.0 + stroke_width * 2.5;
    let head_width = 4.0 + stroke_width * 1.5;
    let base = tip - unit.scale(head_length);
    vec![
        tip,
        base + normal.scale(head_width),
        base - normal.scale(head_width),
    ]
}

/// Auto-anchor a free endpoint drag to a nearby element, if any. `approach`
/// is the other endpoint, used to break distance ties toward the incoming
/// direction (so dropping at a shape's centre connects the facing side).
pub fn binding_candidate(
    doc: &Document,
    point: Point,
    tolerance: f64,
    approach: Option<Point>,
) -> Option<(String, AnchorSide, Point)> {
    for id in doc.ordered_ids().into_iter().rev() {
        let Some(element) = doc.element(&id) else {
            continue;
        };
        if element.is_deleted || element.is_frame() {
            continue;
        }
        let rect = element.bounds();
        if rect.expand(tolerance).contains(point) {
            let approach_dir = approach
                .map(|start| {
                    let d = start - rect.center();
                    let len = d.length();
                    if len > 1e-6 {
                        d.scale(1.0 / len)
                    } else {
                        Point::ZERO
                    }
                })
                .unwrap_or(Point::ZERO);
            let (side, anchor) = anchor_points(rect)
                .into_iter()
                .max_by(|(sa, pa), (sb, pb)| {
                    let bias = |side: AnchorSide| {
                        if matches!(side, AnchorSide::E | AnchorSide::W) {
                            1.0
                        } else {
                            0.0
                        }
                    };
                    let score_a =
                        -pa.distance(point) + approach_dir.dot(sa.focus()) * 30.0 + bias(*sa);
                    let score_b =
                        -pb.distance(point) + approach_dir.dot(sb.focus()) * 30.0 + bias(*sb);
                    score_a
                        .partial_cmp(&score_b)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap_or((AnchorSide::E, rect.center()));
            return Some((id, side, anchor));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;
    use crate::model::{Binding, ElementKind, ShapeKind, Style};

    fn shape(doc: &mut Document, rect: Rect) -> String {
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
    fn side_points_are_on_the_bounds() {
        let rect = Rect::new(0.0, 0.0, 100.0, 50.0);
        assert_eq!(AnchorSide::N.point(rect), Point::new(50.0, 0.0));
        assert_eq!(AnchorSide::E.point(rect), Point::new(100.0, 25.0));
        assert_eq!(AnchorSide::S.point(rect), Point::new(50.0, 50.0));
        assert_eq!(AnchorSide::W.point(rect), Point::new(0.0, 25.0));
    }

    #[test]
    fn nearest_anchor_picks_the_closest_side() {
        let rect = Rect::new(0.0, 0.0, 100.0, 50.0);
        assert_eq!(
            nearest_anchor(rect, Point::new(10.0, 25.0)).0,
            AnchorSide::W
        );
        assert_eq!(
            nearest_anchor(rect, Point::new(95.0, 40.0)).0,
            AnchorSide::E
        );
    }

    #[test]
    fn bound_endpoints_follow_their_element() {
        let mut doc = Document::new();
        let id = shape(&mut doc, Rect::new(0.0, 0.0, 100.0, 50.0));
        let endpoint = Endpoint {
            x: 0.0,
            y: 0.0,
            binding: Some(Binding {
                element_id: id.clone(),
                focus: Some(AnchorSide::E.focus()),
            }),
        };
        let (point, side) = resolve_endpoint(&endpoint, Point::new(500.0, 25.0), &doc);
        assert_eq!(side, Some(AnchorSide::E));
        assert_eq!(point, Point::new(100.0, 25.0));

        if let ElementKind::Shape { rect, .. } = &mut doc.element_mut(&id).unwrap().kind {
            rect.x += 40.0;
        }
        let (moved, _) = resolve_endpoint(&endpoint, Point::new(500.0, 25.0), &doc);
        assert_eq!(moved, Point::new(140.0, 25.0));
    }

    #[test]
    fn elbow_routes_are_orthogonal() {
        let a = Point::new(100.0, 25.0);
        let b = Point::new(300.0, 200.0);
        let corners = elbow_corners(a, Some(AnchorSide::E), b, Some(AnchorSide::W));
        assert!(corners.len() >= 3);
        for pair in corners.windows(2) {
            let horizontal = (pair[0].y - pair[1].y).abs() < 1e-9;
            let vertical = (pair[0].x - pair[1].x).abs() < 1e-9;
            assert!(horizontal || vertical, "segment not orthogonal: {pair:?}");
        }
        assert_eq!(corners[0], a);
        assert_eq!(corners[corners.len() - 1], b);
    }

    #[test]
    fn curved_routes_start_and_end_at_anchors() {
        let a = Point::new(0.0, 0.0);
        let b = Point::new(200.0, 100.0);
        let points = curved_route(a, Some(AnchorSide::S), b, Some(AnchorSide::N));
        assert!(points.len() > 8);
        assert_eq!(points[0], a);
        assert_eq!(points[points.len() - 1], b);
    }

    #[test]
    fn arrowhead_points_back_along_the_line() {
        let head = arrowhead(Point::new(100.0, 0.0), Point::new(0.0, 0.0), 2.0);
        assert_eq!(head.len(), 3);
        assert_eq!(head[0], Point::new(100.0, 0.0));
        assert!(head[1].x < 100.0);
        assert!(head[2].x < 100.0);
        assert!(head[1].y.signum() != head[2].y.signum());
    }

    #[test]
    fn point_at_midpoint_by_arc_length() {
        let points = vec![
            Point::new(0.0, 0.0),
            Point::new(100.0, 0.0),
            Point::new(100.0, 100.0),
        ];
        assert_eq!(point_at(&points, 0.5), Point::new(100.0, 0.0));
        assert_eq!(point_at(&points, 0.0), Point::new(0.0, 0.0));
        assert_eq!(point_at(&points, 1.0), Point::new(100.0, 100.0));
    }

    #[test]
    fn route_uses_bindings_and_routing() {
        let mut doc = Document::new();
        let a = shape(&mut doc, Rect::new(0.0, 0.0, 100.0, 100.0));
        let b = shape(&mut doc, Rect::new(300.0, 0.0, 100.0, 100.0));
        let start = Endpoint {
            x: 100.0,
            y: 50.0,
            binding: Some(Binding {
                element_id: a,
                focus: Some(AnchorSide::E.focus()),
            }),
        };
        let end = Endpoint {
            x: 300.0,
            y: 50.0,
            binding: Some(Binding {
                element_id: b,
                focus: Some(AnchorSide::W.focus()),
            }),
        };
        let points = route(&start, &end, Routing::Elbow, &doc);
        assert_eq!(points[0], Point::new(100.0, 50.0));
        assert_eq!(points[points.len() - 1], Point::new(300.0, 50.0));
    }

    #[test]
    fn binding_candidate_finds_shapes_under_a_point() {
        let mut doc = Document::new();
        let id = shape(&mut doc, Rect::new(0.0, 0.0, 100.0, 50.0));
        let candidate = binding_candidate(
            &doc,
            Point::new(95.0, 45.0),
            6.0,
            Some(Point::new(0.0, 0.0)),
        );
        assert!(candidate.is_some());
        let (found, side, point) = candidate.unwrap();
        assert_eq!(found, id);
        assert_eq!(side, AnchorSide::E);
        assert_eq!(point, Point::new(100.0, 25.0));
        assert!(binding_candidate(&doc, Point::new(500.0, 500.0), 6.0, None).is_none());
    }

    #[test]
    fn binding_candidate_prefers_the_facing_side_on_centre_drops() {
        let mut doc = Document::new();
        shape(&mut doc, Rect::new(300.0, 0.0, 100.0, 100.0));
        // Dropping at the centre: coming from the left, connect the west side.
        let (_, side, point) = binding_candidate(
            &doc,
            Point::new(350.0, 50.0),
            6.0,
            Some(Point::new(50.0, 50.0)),
        )
        .unwrap();
        assert_eq!(side, AnchorSide::W);
        assert_eq!(point, Point::new(300.0, 50.0));
    }
}
