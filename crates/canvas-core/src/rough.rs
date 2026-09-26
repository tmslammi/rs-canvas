//! The rough engine: deterministic hand-drawn geometry.
//!
//! Given an element ID as seed, the same document always renders the same
//! strokes. Shapes are drawn as lightly jittered polylines with an extra
//! sketchy pass at higher roughness levels; fills are hachure / cross-hatch
//! line batches clipped to the shape polygon, or a solid polygon fill.

use crate::display::{Dash, DrawOp, FillStyle, PathOp, StrokeStyle, TextOp};
use crate::geometry::{Point, Rect};
use crate::model::{Element, ElementKind, FillPattern, ShapeKind, Style, TextAlign};
use crate::text::{layout_text, TextMeasurer};

/// A tiny deterministic RNG (splitmix64).
pub struct RoughRng {
    state: u64,
}

impl RoughRng {
    pub fn seeded(seed: u64) -> Self {
        Self {
            state: seed ^ 0x9E37_79B9_7F4A_7C15,
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in [-1, 1).
    pub fn signed(&mut self) -> f64 {
        self.next_f64() * 2.0 - 1.0
    }
}

/// FNV-1a; stable across runs and platforms.
pub fn hash_str(s: &str) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn jitter_magnitude(roughness: u8) -> f64 {
    match roughness {
        0 => 0.6,
        1 => 1.6,
        _ => 3.2,
    }
}

fn perp(a: Point, b: Point) -> Point {
    let d = b - a;
    let len = d.length();
    if len < 1e-9 {
        Point::ZERO
    } else {
        Point::new(-d.y / len, d.x / len)
    }
}

/// Build jittered strokes for a polyline. Returns one path for the main
/// stroke (closed via the `closed` flag at draw time) plus, at higher
/// roughness, short overlapping second-pass segments.
pub fn rough_polyline(
    points: &[Point],
    closed: bool,
    roughness: u8,
    rng: &mut RoughRng,
) -> (Vec<Point>, Vec<Vec<Point>>) {
    let n = points.len();
    if n < 2 {
        return (Vec::new(), Vec::new());
    }
    let mag = jitter_magnitude(roughness);
    let offsets: Vec<Point> = (0..n)
        .map(|_| Point::new(rng.signed() * mag, rng.signed() * mag))
        .collect();

    let edge_count = if closed { n } else { n - 1 };
    let mut main = Vec::with_capacity(edge_count * 2 + 1);
    for i in 0..edge_count {
        if i == 0 {
            main.push(points[0] + offsets[0]);
        }
        let a = points[i] + offsets[i];
        let b = points[(i + 1) % n] + offsets[(i + 1) % n];
        let bow = perp(a, b).scale(rng.signed() * mag * 0.8);
        main.push(a.lerp(b, 0.5) + bow);
        main.push(b);
    }

    let mut extras = Vec::new();
    if roughness > 0 {
        let small = mag * 0.5;
        for i in 0..edge_count {
            if rng.next_f64() < 0.35 {
                continue;
            }
            let a = points[i] + offsets[i];
            let b = points[(i + 1) % n] + offsets[(i + 1) % n];
            let t0 = 0.12 + rng.next_f64() * 0.2;
            let t1 = (t0 + 0.45 + rng.next_f64() * 0.3).min(0.95);
            let seg = vec![
                a.lerp(b, t0) + Point::new(rng.signed() * small, rng.signed() * small),
                a.lerp(b, t1) + Point::new(rng.signed() * small, rng.signed() * small),
            ];
            extras.push(seg);
        }
    }
    (main, extras)
}

/// The polygon used for fills and rough outline of a shape.
pub fn shape_polygon(shape: ShapeKind, rect: Rect) -> Vec<Point> {
    match shape {
        ShapeKind::Rectangle => vec![
            Point::new(rect.x, rect.y),
            Point::new(rect.max_x(), rect.y),
            Point::new(rect.max_x(), rect.max_y()),
            Point::new(rect.x, rect.max_y()),
        ],
        ShapeKind::Diamond => {
            let c = rect.center();
            vec![
                Point::new(c.x, rect.y),
                Point::new(rect.max_x(), c.y),
                Point::new(c.x, rect.max_y()),
                Point::new(rect.x, c.y),
            ]
        }
        ShapeKind::Ellipse => {
            let c = rect.center();
            let rx = rect.w / 2.0;
            let ry = rect.h / 2.0;
            let n = 48;
            (0..n)
                .map(|i| {
                    let t = i as f64 / n as f64 * std::f64::consts::TAU;
                    Point::new(c.x + rx * t.cos(), c.y + ry * t.sin())
                })
                .collect()
        }
    }
}

/// Hachure lines at `angle_deg` clipped to the polygon (even-odd rule).
pub fn hachure_lines(
    polygon: &[Point],
    angle_deg: f64,
    gap: f64,
    rng: &mut RoughRng,
) -> Vec<[Point; 2]> {
    if polygon.len() < 3 || gap <= 0.5 {
        return Vec::new();
    }
    let angle = angle_deg.to_radians();
    let (sin, cos) = angle.sin_cos();
    let rotated: Vec<Point> = polygon
        .iter()
        .map(|p| Point::new(p.x * cos + p.y * sin, -p.x * sin + p.y * cos))
        .collect();
    let min_y = rotated.iter().map(|p| p.y).fold(f64::INFINITY, f64::min);
    let max_y = rotated
        .iter()
        .map(|p| p.y)
        .fold(f64::NEG_INFINITY, f64::max);

    let mut segments = Vec::new();
    let mut y = (min_y / gap).ceil() * gap;
    while y <= max_y {
        let mut xs: Vec<f64> = Vec::new();
        for i in 0..rotated.len() {
            let p1 = rotated[i];
            let p2 = rotated[(i + 1) % rotated.len()];
            if (p1.y <= y && p2.y > y) || (p2.y <= y && p1.y > y) {
                let t = (y - p1.y) / (p2.y - p1.y);
                xs.push(p1.x + t * (p2.x - p1.x));
            }
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mut i = 0;
        while i + 1 < xs.len() {
            let (x0, x1) = (xs[i], xs[i + 1]);
            if x1 - x0 > 0.75 {
                let p0 = Point::new(x0 * cos - y * sin, x0 * sin + y * cos);
                let p1 = Point::new(x1 * cos - y * sin, x1 * sin + y * cos);
                segments.push([
                    Point::new(p0.x, p0.y + rng.signed() * 0.4),
                    Point::new(p1.x, p1.y + rng.signed() * 0.4),
                ]);
            }
            i += 2;
        }
        y += gap;
    }
    segments
}

fn stroke_of(style: &Style, width: f64) -> StrokeStyle {
    StrokeStyle {
        color: style.stroke_color.clone(),
        width,
        dash: match style.dash {
            crate::model::StrokeDash::Solid => Dash::Solid,
            crate::model::StrokeDash::Dashed => Dash::Dashed,
            crate::model::StrokeDash::Dotted => Dash::Dotted,
        },
        opacity: style.opacity,
    }
}

/// Rough outline ops for a polygon.
pub fn outline_ops(polygon: &[Point], style: &Style, seed: u64, closed: bool) -> Vec<DrawOp> {
    if style.stroke_color == "transparent" || polygon.len() < 2 {
        return Vec::new();
    }
    let mut rng = RoughRng::seeded(seed);
    let (main, extras) = rough_polyline(polygon, closed, style.roughness, &mut rng);
    let stroke = stroke_of(style, style.stroke_width);
    let mut ops = Vec::new();
    if main.len() >= 2 {
        ops.push(DrawOp::Path(PathOp {
            points: main,
            closed,
            stroke: Some(stroke.clone()),
            fill: None,
        }));
    }
    for seg in extras {
        ops.push(DrawOp::Path(PathOp {
            points: seg,
            closed: false,
            stroke: Some(stroke.clone()),
            fill: None,
        }));
    }
    ops
}

/// Fill ops for a polygon per the style's fill pattern.
pub fn fill_ops(polygon: &[Point], style: &Style, seed: u64) -> Vec<DrawOp> {
    if style.fill_color == "transparent" || polygon.len() < 3 {
        return Vec::new();
    }
    let mut rng = RoughRng::seeded(seed ^ 0x51D3);
    let gap = (4.0 + style.stroke_width).max(5.0);
    match style.fill_style {
        FillPattern::Solid => vec![DrawOp::Path(PathOp {
            points: polygon.to_vec(),
            closed: true,
            stroke: None,
            fill: Some(FillStyle::Solid {
                color: style.fill_color.clone(),
                opacity: style.opacity,
            }),
        })],
        FillPattern::Hachure => {
            let segments = hachure_lines(polygon, 41.0, gap, &mut rng);
            batch(segments, style)
        }
        FillPattern::CrossHatch => {
            let mut segments = hachure_lines(polygon, 41.0, gap, &mut rng);
            segments.extend(hachure_lines(polygon, -49.0, gap, &mut rng));
            batch(segments, style)
        }
    }
}

fn batch(segments: Vec<[Point; 2]>, style: &Style) -> Vec<DrawOp> {
    if segments.is_empty() {
        return Vec::new();
    }
    vec![DrawOp::LineBatch {
        segments,
        stroke: StrokeStyle {
            color: style.fill_color.clone(),
            width: 1.0,
            dash: Dash::Solid,
            opacity: style.opacity,
        },
    }]
}

/// Build the drawing instructions for one element.
pub fn build_element_ops(element: &Element, measurer: &dyn TextMeasurer) -> Vec<DrawOp> {
    let seed = hash_str(&element.id);
    match &element.kind {
        ElementKind::Shape {
            shape, rect, style, ..
        } => {
            let polygon = shape_polygon(*shape, *rect);
            let mut ops = fill_ops(&polygon, style, seed);
            ops.extend(outline_ops(&polygon, style, seed, true));
            ops
        }
        ElementKind::Freehand { points, style, .. } => {
            if points.len() < 2 || style.stroke_color == "transparent" {
                return Vec::new();
            }
            vec![DrawOp::Path(PathOp {
                points: points.clone(),
                closed: false,
                stroke: Some(stroke_of(style, style.stroke_width)),
                fill: None,
            })]
        }
        ElementKind::Arrow {
            start, end, style, ..
        } => {
            if style.stroke_color == "transparent" {
                return Vec::new();
            }
            outline_ops(&[start.point(), end.point()], style, seed ^ 0xA22, false)
        }
        ElementKind::Frame { rect, .. } => {
            let polygon = shape_polygon(ShapeKind::Rectangle, *rect);
            let mut rng = RoughRng::seeded(seed ^ 0xF4A3E);
            let (main, _) = rough_polyline(&polygon, true, 0, &mut rng);
            let stroke = StrokeStyle {
                color: "#adb5bd".into(),
                width: 1.5,
                dash: Dash::Dashed,
                opacity: 1.0,
            };
            vec![DrawOp::Path(PathOp {
                points: main,
                closed: true,
                stroke: Some(stroke),
                fill: None,
            })]
        }
        // Glyph rendering arrives with the shape library ticket.
        ElementKind::Glyph { .. } => Vec::new(),
        ElementKind::Text {
            rect,
            text,
            align,
            container_id,
            fixed_width,
            style,
        } => {
            if text.is_empty() {
                return Vec::new();
            }
            let wrap_width = if *fixed_width || container_id.is_some() {
                Some(rect.w.max(1.0))
            } else {
                None
            };
            let layout = layout_text(text, style.font_size, wrap_width, measurer);
            let mut ops = Vec::new();
            for (i, line) in layout.lines.iter().enumerate() {
                if line.is_empty() {
                    continue;
                }
                let baseline = rect.y + layout.line_height * (i as f64 + 0.75);
                let x = match align {
                    TextAlign::Left => rect.x,
                    TextAlign::Center => rect.x + rect.w / 2.0,
                    TextAlign::Right => rect.max_x(),
                };
                ops.push(DrawOp::Text(TextOp {
                    x,
                    y: baseline,
                    text: line.clone(),
                    font_size: style.font_size,
                    color: style.stroke_color.clone(),
                    opacity: style.opacity,
                    align: *align,
                }));
            }
            ops
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_deterministic() {
        let mut a = RoughRng::seeded(42);
        let mut b = RoughRng::seeded(42);
        for _ in 0..10 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn jitter_stays_near_the_geometry() {
        let rect = Rect::new(0.0, 0.0, 100.0, 60.0);
        let polygon = shape_polygon(ShapeKind::Rectangle, rect);
        let mut rng = RoughRng::seeded(7);
        let (main, extras) = rough_polyline(&polygon, true, 2, &mut rng);
        let bounds = rect.expand(12.0);
        for p in &main {
            assert!(bounds.contains(*p), "{p:?} escaped {bounds:?}");
        }
        for seg in &extras {
            for p in seg {
                assert!(bounds.contains(*p), "{p:?} escaped {bounds:?}");
            }
        }
        assert!(main.len() >= polygon.len() * 2 - 1);
    }

    #[test]
    fn hachure_lines_lie_inside_the_polygon() {
        let rect = Rect::new(10.0, 20.0, 80.0, 40.0);
        let polygon = shape_polygon(ShapeKind::Rectangle, rect);
        let mut rng = RoughRng::seeded(3);
        let segments = hachure_lines(&polygon, 41.0, 6.0, &mut rng);
        assert!(!segments.is_empty());
        let margin = rect.expand(1.0);
        for [a, b] in &segments {
            assert!(margin.contains(*a) && margin.contains(*b));
        }
    }

    #[test]
    fn hachure_angle_is_respected() {
        let rect = Rect::new(0.0, 0.0, 100.0, 100.0);
        let polygon = shape_polygon(ShapeKind::Rectangle, rect);
        let mut rng = RoughRng::seeded(11);
        let segments = hachure_lines(&polygon, 41.0, 10.0, &mut rng);
        for [a, b] in &segments {
            let d = *b - *a;
            assert!(d.x.abs() > 0.5, "hachure should be diagonal-ish: {d:?}");
        }
    }

    #[test]
    fn ellipse_polygon_is_rounded() {
        let polygon = shape_polygon(ShapeKind::Ellipse, Rect::new(0.0, 0.0, 100.0, 50.0));
        assert!(polygon.len() > 16);
        for p in &polygon {
            assert!((0.0..=100.0).contains(&p.x));
            assert!((0.0..=50.0).contains(&p.y));
        }
    }

    #[test]
    fn rendering_a_document_is_deterministic() {
        use crate::model::{Element, ElementKind, ShapeKind, Style};
        use crate::text::ApproxMeasurer;
        let make = || {
            let mut element = Element::new(
                ElementKind::Shape {
                    shape: ShapeKind::Diamond,
                    rect: Rect::new(5.0, 5.0, 50.0, 50.0),
                    style: Style::default(),
                    bound_text_id: None,
                },
                "a0".into(),
                0,
            );
            element.id = "stable-id".into();
            build_element_ops(&element, &ApproxMeasurer)
        };
        assert_eq!(make(), make());
    }
}
