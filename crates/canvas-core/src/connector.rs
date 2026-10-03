//! Connector routing: anchor resolution, elbow/curved paths.
//!
//! Endpoints bind to elements (`Binding`); each render and hit-test resolves
//! the actual point from the element's current bounds, so connectors follow
//! the shapes they connect.

use crate::geometry::{Point, Rect};
use crate::model::{Document, ElementId, Endpoint, Routing};
use crate::spatial::ObstacleIndex;

/// How far a route stays away from obstacle bounds, in world units.
const OBSTACLE_CLEARANCE: f64 = 8.0;

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

/// The routed polyline for a connector, honouring waypoints but ignoring
/// other elements. This is the cheap route used by hit-testing; rendering
/// uses [`route_avoiding`] so the drawn path steps around obstacles.
pub fn route(
    start: &Endpoint,
    end: &Endpoint,
    routing: Routing,
    waypoints: &[Point],
    doc: &Document,
) -> Vec<Point> {
    let (start_point, start_side) = resolve_endpoint(start, end.point(), doc);
    let (end_point, end_side) = resolve_endpoint(end, start.point(), doc);
    route_points(
        start_point,
        start_side,
        end_point,
        end_side,
        routing,
        waypoints,
    )
}

/// The routed polyline the renderer draws: the waypoint route when the user
/// placed waypoints, otherwise the elbow route adjusted to step around the
/// obstacles it would cross.
///
/// The elements bound to the connector's endpoints are never obstacles — a
/// route may leave and arrive at them — and neither are frames (crossing a
/// VPC border to reach a node inside is normal). Obstacle avoidance is
/// best-effort: when every candidate still crosses something, the plain
/// route is returned rather than refusing to draw.
pub fn route_avoiding(
    start: &Endpoint,
    end: &Endpoint,
    routing: Routing,
    waypoints: &[Point],
    doc: &Document,
    obstacles: &ObstacleIndex,
) -> Vec<Point> {
    let (start_point, start_side) = resolve_endpoint(start, end.point(), doc);
    let (end_point, end_side) = resolve_endpoint(end, start.point(), doc);
    let excluded: Vec<&ElementId> = [&start.binding, &end.binding]
        .into_iter()
        .flatten()
        .map(|binding| &binding.element_id)
        .collect();
    if waypoints.is_empty() && matches!(routing, Routing::Elbow) {
        return avoid_obstacles(
            start_point,
            start_side,
            end_point,
            end_side,
            &excluded,
            obstacles,
        );
    }
    route_points(
        start_point,
        start_side,
        end_point,
        end_side,
        routing,
        waypoints,
    )
}

fn route_points(
    a: Point,
    a_side: Option<AnchorSide>,
    b: Point,
    b_side: Option<AnchorSide>,
    routing: Routing,
    waypoints: &[Point],
) -> Vec<Point> {
    if waypoints.is_empty() {
        return match routing {
            Routing::Straight => vec![a, b],
            Routing::Elbow => elbow_route(a, a_side, b, b_side),
            Routing::Curved => curved_route(a, a_side, b, b_side),
        };
    }
    match routing {
        Routing::Elbow => elbow_through(a, a_side, b, b_side, waypoints),
        // Curves and lines through waypoints stay polylines: the waypoints
        // are user-authored bends, not samples.
        Routing::Straight | Routing::Curved => {
            let mut out = Vec::with_capacity(waypoints.len() + 2);
            out.push(a);
            out.extend_from_slice(waypoints);
            out.push(b);
            dedupe(out)
        }
    }
}

/// An orthogonal route from `a` to `b` that passes through each waypoint.
/// The first leg leaves along `a`'s anchor side, the last leg arrives along
/// `b`'s; intermediate legs turn on the dominant axis.
fn elbow_through(
    a: Point,
    a_side: Option<AnchorSide>,
    b: Point,
    b_side: Option<AnchorSide>,
    waypoints: &[Point],
) -> Vec<Point> {
    let mut out = vec![a];
    let mut prev = a;
    let mut prev_side = a_side;
    for waypoint in waypoints {
        let corner = match prev_side {
            Some(side) if side.is_vertical() => Point::new(prev.x, waypoint.y),
            Some(_) => Point::new(waypoint.x, prev.y),
            None => {
                if (waypoint.x - prev.x).abs() >= (waypoint.y - prev.y).abs() {
                    Point::new(waypoint.x, prev.y)
                } else {
                    Point::new(prev.x, waypoint.y)
                }
            }
        };
        out.push(corner);
        out.push(*waypoint);
        prev = *waypoint;
        prev_side = None;
    }
    let corner = match b_side {
        Some(side) if side.is_vertical() => Point::new(b.x, prev.y),
        Some(_) => Point::new(prev.x, b.y),
        None => {
            if (b.x - prev.x).abs() >= (b.y - prev.y).abs() {
                Point::new(b.x, prev.y)
            } else {
                Point::new(prev.x, b.y)
            }
        }
    };
    out.push(corner);
    out.push(b);
    dedupe(out)
}

/// Route `a` -> `b` around the obstacles its elbow route would cross.
fn avoid_obstacles(
    a: Point,
    a_side: Option<AnchorSide>,
    b: Point,
    b_side: Option<AnchorSide>,
    excluded: &[&ElementId],
    obstacles: &ObstacleIndex,
) -> Vec<Point> {
    let base = elbow_corners(a, a_side, b, b_side);
    let base_box = polyline_bounds(&base);
    let area = base_box.expand(OBSTACLE_CLEARANCE);
    let relevant = |(id, _): &(ElementId, Rect)| !excluded.contains(&id);
    let blockers: Vec<Rect> = obstacles
        .query(area)
        .into_iter()
        .filter(relevant)
        .map(|(_, rect)| rect.expand(OBSTACLE_CLEARANCE))
        .filter(|rect| polyline_crosses(&base, *rect))
        .collect();
    if blockers.is_empty() {
        return base;
    }
    let union = blockers
        .into_iter()
        .reduce(|acc, rect| acc.union(&rect))
        .unwrap_or(base_box);

    // Candidate detours: shift the elbow's middle line beyond the blocked
    // union, in both orientations. Useless orientations are filtered by the
    // clearance test; ties prefer paths that leave and arrive along the
    // anchor sides.
    let mut candidates: Vec<Vec<Point>> = Vec::new();
    for middle in [
        union.y - OBSTACLE_CLEARANCE,
        union.max_y() + OBSTACLE_CLEARANCE,
    ] {
        candidates.push(dedupe(vec![
            a,
            Point::new(a.x, middle),
            Point::new(b.x, middle),
            b,
        ]));
    }
    for middle in [
        union.x - OBSTACLE_CLEARANCE,
        union.max_x() + OBSTACLE_CLEARANCE,
    ] {
        candidates.push(dedupe(vec![
            a,
            Point::new(middle, a.y),
            Point::new(middle, b.y),
            b,
        ]));
    }
    candidates.retain(|candidate| candidate.len() >= 2);

    // Every obstacle that could touch any candidate (plus clearance) decides
    // whether the candidate is clear.
    let mut reach = base_box;
    for candidate in &candidates {
        reach = reach.union(&polyline_bounds(candidate));
    }
    let all: Vec<Rect> = obstacles
        .query(reach.expand(OBSTACLE_CLEARANCE * 2.0))
        .into_iter()
        .filter(relevant)
        .map(|(_, rect)| rect.expand(OBSTACLE_CLEARANCE))
        .collect();

    candidates.sort_by(|left, right| {
        let respect = |path: &[Point]| !respects_sides(path, a_side, b_side);
        let left_worse = respect(left);
        let right_worse = respect(right);
        left_worse.cmp(&right_worse).then_with(|| {
            polyline_length(left)
                .partial_cmp(&polyline_length(right))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    });
    candidates
        .into_iter()
        .find(|candidate| !all.iter().any(|rect| polyline_crosses(candidate, *rect)))
        .unwrap_or(base)
}

/// Whether a path leaves `a` and arrives at `b` against their anchor sides.
fn respects_sides(path: &[Point], a_side: Option<AnchorSide>, b_side: Option<AnchorSide>) -> bool {
    let first = path
        .get(1)
        .map(|next| *next - path[0])
        .unwrap_or(Point::ZERO);
    let last = path
        .get(path.len().saturating_sub(2))
        .map(|prev| path[path.len() - 1] - *prev)
        .unwrap_or(Point::ZERO);
    let leaves = a_side.is_none_or(|side| first.length() < 1e-9 || first.dot(side.focus()) >= 0.0);
    let arrives = b_side.is_none_or(|side| last.length() < 1e-9 || last.dot(side.focus()) >= 0.0);
    leaves && arrives
}

fn polyline_crosses(points: &[Point], rect: Rect) -> bool {
    points
        .windows(2)
        .any(|segment| segment_intersects_rect(segment[0], segment[1], rect))
}

fn polyline_length(points: &[Point]) -> f64 {
    points.windows(2).map(|w| w[0].distance(w[1])).sum()
}

fn polyline_bounds(points: &[Point]) -> Rect {
    let Some(first) = points.first() else {
        return Rect::ZERO;
    };
    let (mut min_x, mut min_y) = (first.x, first.y);
    let (mut max_x, mut max_y) = (first.x, first.y);
    for p in points {
        min_x = min_x.min(p.x);
        min_y = min_y.min(p.y);
        max_x = max_x.max(p.x);
        max_y = max_y.max(p.y);
    }
    Rect::new(min_x, min_y, max_x - min_x, max_y - min_y)
}

/// Liang-Barsky segment vs. axis-aligned rectangle.
fn segment_intersects_rect(a: Point, b: Point, rect: Rect) -> bool {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let mut t0 = 0.0;
    let mut t1 = 1.0;
    let clips = [
        (-dx, a.x - rect.x),
        (dx, rect.max_x() - a.x),
        (-dy, a.y - rect.y),
        (dy, rect.max_y() - a.y),
    ];
    for (p, q) in clips {
        if p.abs() < 1e-12 {
            if q < 0.0 {
                return false;
            }
            continue;
        }
        let t = q / p;
        if p < 0.0 {
            if t > t1 {
                return false;
            }
            if t > t0 {
                t0 = t;
            }
        } else {
            if t < t0 {
                return false;
            }
            if t < t1 {
                t1 = t;
            }
        }
    }
    true
}

/// How far a same-side elbow runs past the anchors before turning, so the
/// path continues outward (below both shapes, say) instead of cutting back
/// between them.
const OUTWARD_MARGIN: f64 = 24.0;

/// Corner points for an elbow route. When both anchors face the same
/// direction the middle line is pushed beyond them — bottom-to-bottom
/// connectors route under both shapes rather than through them.
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
            let my = match (a_side, b_side) {
                (Some(AnchorSide::S), Some(AnchorSide::S)) => a.y.max(b.y) + OUTWARD_MARGIN,
                (Some(AnchorSide::N), Some(AnchorSide::N)) => a.y.min(b.y) - OUTWARD_MARGIN,
                _ => (a.y + b.y) / 2.0,
            };
            vec![a, Point::new(a.x, my), Point::new(b.x, my), b]
        }
        (false, false) => {
            let mx = match (a_side, b_side) {
                (Some(AnchorSide::E), Some(AnchorSide::E)) => a.x.max(b.x) + OUTWARD_MARGIN,
                (Some(AnchorSide::W), Some(AnchorSide::W)) => a.x.min(b.x) - OUTWARD_MARGIN,
                _ => (a.x + b.x) / 2.0,
            };
            vec![a, Point::new(mx, a.y), Point::new(mx, b.y), b]
        }
        (true, false) => vec![a, Point::new(a.x, b.y), b],
        (false, true) => vec![a, Point::new(b.x, a.y), b],
    };
    prune_collinear(dedupe(raw))
}

/// Drop interior vertices that lie on a straight run, so an elbow route that
/// happens to be straight is two points rather than three collinear ones.
fn prune_collinear(points: Vec<Point>) -> Vec<Point> {
    if points.len() < 3 {
        return points;
    }
    let mut out: Vec<Point> = Vec::with_capacity(points.len());
    for point in points {
        while out.len() >= 2 {
            let previous = out[out.len() - 2];
            let last = out[out.len() - 1];
            let horizontal = (previous.y - last.y).abs() < 1e-9 && (last.y - point.y).abs() < 1e-9;
            let vertical = (previous.x - last.x).abs() < 1e-9 && (last.x - point.x).abs() < 1e-9;
            if horizontal || vertical {
                out.pop();
            } else {
                break;
            }
        }
        out.push(point);
    }
    out
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

/// Where a new waypoint grabbed on the route belongs in the waypoint list:
/// after every existing waypoint whose route position precedes `point`.
pub fn waypoint_insertion_index(route: &[Point], waypoints: &[Point], point: Point) -> usize {
    let vertex_index = |target: Point| {
        route
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                a.distance(target)
                    .partial_cmp(&b.distance(target))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(index, _)| index)
            .unwrap_or(0)
    };
    let at = vertex_index(point);
    waypoints
        .iter()
        .filter(|waypoint| vertex_index(**waypoint) <= at)
        .count()
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
/// `exclude` skips the element being edited — an arrow's bounds grow as it is
/// drawn, so without the exclusion an endpoint dropped inside its own bounds
/// would bind the connector to itself.
///
/// Frames only bind when the point is near their border, so a drop inside a
/// frame still reaches the elements it contains.
pub fn binding_candidate(
    doc: &Document,
    point: Point,
    tolerance: f64,
    approach: Option<Point>,
    exclude: Option<&str>,
) -> Option<(String, AnchorSide, Point)> {
    let mut best: Option<&crate::model::Element> = None;
    for element in doc.elements.values() {
        if element.is_deleted || !element.is_bind_target() || exclude == Some(element.id.as_str()) {
            continue;
        }
        let in_reach = if element.is_frame() {
            crate::hit::hits(doc, element, point, tolerance)
        } else {
            element.bounds().expand(tolerance).contains(point)
        };
        if !in_reach {
            continue;
        }
        let topmost = best.is_none_or(|b| {
            (element.index.as_str(), element.id.as_str()) > (b.index.as_str(), b.id.as_str())
        });
        if topmost {
            best = Some(element);
        }
    }
    let element = best?;
    let rect = element.bounds();
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
            let score_a = -pa.distance(point) + approach_dir.dot(sa.focus()) * 30.0 + bias(*sa);
            let score_b = -pb.distance(point) + approach_dir.dot(sb.focus()) * 30.0 + bias(*sb);
            score_a
                .partial_cmp(&score_b)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or((AnchorSide::E, rect.center()));
    Some((element.id.clone(), side, anchor))
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
        let points = route(&start, &end, Routing::Elbow, &[], &doc);
        assert_eq!(points[0], Point::new(100.0, 50.0));
        assert_eq!(points[points.len() - 1], Point::new(300.0, 50.0));
    }

    #[test]
    fn routes_pass_through_waypoints() {
        let mut doc = Document::new();
        let a = shape(&mut doc, Rect::new(0.0, 0.0, 100.0, 100.0));
        let b = shape(&mut doc, Rect::new(400.0, 0.0, 100.0, 100.0));
        let start = Endpoint {
            x: 100.0,
            y: 50.0,
            binding: Some(Binding {
                element_id: a,
                focus: Some(AnchorSide::E.focus()),
            }),
        };
        let end = Endpoint {
            x: 400.0,
            y: 50.0,
            binding: Some(Binding {
                element_id: b,
                focus: Some(AnchorSide::W.focus()),
            }),
        };
        let waypoint = Point::new(250.0, 200.0);
        let points = route(&start, &end, Routing::Elbow, &[waypoint], &doc);
        assert!(
            points.contains(&waypoint),
            "the route goes through the waypoint: {points:?}"
        );
        for pair in points.windows(2) {
            let horizontal = (pair[0].y - pair[1].y).abs() < 1e-9;
            let vertical = (pair[0].x - pair[1].x).abs() < 1e-9;
            assert!(horizontal || vertical, "segment not orthogonal: {pair:?}");
        }
    }

    #[test]
    fn avoiding_routes_step_around_a_blocker() {
        let mut doc = Document::new();
        let a = shape(&mut doc, Rect::new(0.0, 0.0, 100.0, 100.0));
        let b = shape(&mut doc, Rect::new(600.0, 0.0, 100.0, 100.0));
        let blocker = shape(&mut doc, Rect::new(300.0, -40.0, 120.0, 180.0));
        let start = Endpoint {
            x: 100.0,
            y: 50.0,
            binding: Some(Binding {
                element_id: a,
                focus: Some(AnchorSide::E.focus()),
            }),
        };
        let end = Endpoint {
            x: 600.0,
            y: 50.0,
            binding: Some(Binding {
                element_id: b,
                focus: Some(AnchorSide::W.focus()),
            }),
        };
        let index = crate::spatial::ObstacleIndex::new();
        let mut index = index;
        index.rebuild(&doc);
        let points = route_avoiding(&start, &end, Routing::Elbow, &[], &doc, &index);
        assert_eq!(points[0], Point::new(100.0, 50.0));
        assert_eq!(points[points.len() - 1], Point::new(600.0, 50.0));
        let blocker_rect = doc.element(&blocker).unwrap().bounds();
        for pair in points.windows(2) {
            assert!(
                !segment_intersects_rect(pair[0], pair[1], blocker_rect),
                "segment {pair:?} crosses the blocker"
            );
        }
        assert!(
            points
                .iter()
                .any(|p| p.y < blocker_rect.y || p.y > blocker_rect.max_y()),
            "the route detours around the blocker: {points:?}"
        );
    }

    #[test]
    fn avoiding_routes_ignore_the_bound_elements() {
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
        let mut index = crate::spatial::ObstacleIndex::new();
        index.rebuild(&doc);
        let points = route_avoiding(&start, &end, Routing::Elbow, &[], &doc, &index);
        // The bound shapes are not obstacles, so nothing detours: the route
        // stays on the straight line between the anchors.
        assert!(
            points.iter().all(|p| (p.y - 50.0).abs() < 1e-9),
            "{points:?}"
        );
        assert_eq!(points.first(), Some(&Point::new(100.0, 50.0)));
        assert_eq!(points.last(), Some(&Point::new(300.0, 50.0)));
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
            None,
        );
        assert!(candidate.is_some());
        let (found, side, point) = candidate.unwrap();
        assert_eq!(found, id);
        assert_eq!(side, AnchorSide::E);
        assert_eq!(point, Point::new(100.0, 25.0));
        assert!(binding_candidate(&doc, Point::new(500.0, 500.0), 6.0, None, None).is_none());
        assert!(
            binding_candidate(&doc, Point::new(95.0, 45.0), 6.0, None, Some(&id)).is_none(),
            "the excluded element is never a binding candidate"
        );
    }

    #[test]
    fn binding_candidate_binds_to_frames_only_near_their_border() {
        let mut doc = Document::new();
        let frame = doc.add(
            ElementKind::Frame {
                rect: Rect::new(0.0, 0.0, 200.0, 150.0),
                name: "VPC".into(),
            },
            0,
        );
        let (found, side, point) =
            binding_candidate(&doc, Point::new(202.0, 75.0), 6.0, None, None)
                .expect("a drop on the frame border binds to the frame");
        assert_eq!(found, frame);
        assert_eq!(side, AnchorSide::E);
        assert_eq!(point, Point::new(200.0, 75.0));
        assert!(
            binding_candidate(&doc, Point::new(100.0, 75.0), 6.0, None, None).is_none(),
            "empty space inside a frame is not a binding target"
        );
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
            None,
        )
        .unwrap();
        assert_eq!(side, AnchorSide::W);
        assert_eq!(point, Point::new(300.0, 50.0));
    }
}
