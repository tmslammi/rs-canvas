//! Server-side document work: what the agent can read, and the elements it
//! can create. Everything here goes through `canvas-core`, so headless edits
//! are shaped exactly like the ones the browser makes.

use canvas_core::commands::{
    self, default_shape_size, LabelSpot, DEFAULT_INK, FRAME_H, FRAME_W, GLYPH_SIZE, LABEL_PADDING,
};
use canvas_core::geometry::{Point, Rect};
use canvas_core::model::{Document, ElementId, ShapeKind, Style, TextAlign};
use canvas_core::text::{layout_text, ApproxMeasurer};
use canvas_core::Transaction;

/// Gap between a new element and the thing it is placed beside, in world px.
pub const GAP: f64 = 40.0;
/// Keep-out margin around existing elements during placement.
const MARGIN: f64 = 14.0;
/// Cap on pen-stroke samples so a runaway caller cannot bloat the document.
pub const MAX_STROKE_POINTS: usize = 10_000;
const RING_LIMIT: i32 = 12;

/// One `canvas_draw` request (or one script `draw` beat after resolution).
pub struct DrawRequest {
    pub kind: String,
    pub text: Option<String>,
    /// Frame name.
    pub name: Option<String>,
    /// Glyph library id.
    pub glyph: Option<String>,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub w: Option<f64>,
    pub h: Option<f64>,
    /// Place beside this element id.
    pub near: Option<String>,
    /// right (default), left, top, bottom or inside of `near`.
    pub side: Option<String>,
}

/// Create one element, returning its id. Every mutation is recorded in `tx`.
pub fn draw(
    doc: &mut Document,
    tx: &mut Transaction,
    request: &DrawRequest,
    now_ms: u64,
) -> Result<ElementId, String> {
    let text = request
        .text
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());
    match request.kind.as_str() {
        "rectangle" | "ellipse" | "diamond" | "sticky" => {
            let shape = match request.kind.as_str() {
                "ellipse" => ShapeKind::Ellipse,
                "diamond" => ShapeKind::Diamond,
                "sticky" => ShapeKind::Sticky,
                _ => ShapeKind::Rectangle,
            };
            let (w, h) = size_of(request, default_shape_size(shape));
            let rect = placed_rect(doc, request, (w, h));
            commands::add_shape(
                doc,
                tx,
                shape,
                rect,
                Style::default(),
                text,
                LabelSpot::Centre,
                now_ms,
                &ApproxMeasurer,
            )
        }
        "frame" => {
            let (w, h) = size_of(request, (FRAME_W, FRAME_H));
            let rect = placed_rect(doc, request, (w, h));
            commands::add_frame(
                doc,
                tx,
                rect,
                request.name.as_deref().unwrap_or_default(),
                now_ms,
            )
        }
        "glyph" => {
            let Some(glyph) = request.glyph.as_deref().filter(|glyph| !glyph.is_empty()) else {
                return Err("kind \"glyph\" needs a `glyph` value".into());
            };
            if canvas_core::glyph_lookup(glyph).is_none() {
                return Err(format!("unknown glyph {glyph:?}"));
            }
            let (w, h) = size_of(request, (GLYPH_SIZE, GLYPH_SIZE));
            let rect = placed_rect(doc, request, (w, h));
            commands::add_glyph(
                doc,
                tx,
                glyph,
                rect,
                Style::default(),
                text,
                now_ms,
                &ApproxMeasurer,
            )
        }
        "text" => {
            let Some(text) = text else {
                return Err("kind \"text\" needs a `text` value".into());
            };
            let style = Style {
                stroke_color: DEFAULT_INK.into(),
                ..Style::default()
            };
            let layout = layout_text(text, style.font_size, None, &ApproxMeasurer);
            let w = request
                .w
                .unwrap_or((layout.width + 2.0 * LABEL_PADDING).max(40.0));
            let h = request.h.unwrap_or(layout.height.max(style.font_size * 1.3));
            let rect = placed_rect(doc, request, (w, h));
            commands::add_text(doc, tx, text, rect, TextAlign::Left, style, now_ms)
        }
        other => Err(format!(
            "unknown kind {other:?}; expected rectangle, ellipse, diamond, sticky, frame, glyph or text"
        )),
    }
}

/// One `canvas_pen` request: a stroke path in world coordinates, one pressure
/// per sample, and the ink style to draw it with.
pub struct PenRequest {
    pub points: Vec<Point>,
    /// One entry per point, 0..1; higher pressure widens the stroke. Values
    /// outside the range are clamped.
    pub pressures: Vec<f64>,
    pub style: Style,
}

/// Create one freehand pen stroke, returning its id. This is the agent's
/// version of the pencil: the same element kind and hand-drawn brush as
/// strokes drawn by hand, with the pressure profile (taper, weight) and ink
/// style (width, colour, roughness, dash, opacity, fill/shading) chosen by
/// the caller.
pub fn pen(
    doc: &mut Document,
    tx: &mut Transaction,
    request: &PenRequest,
    now_ms: u64,
) -> Result<ElementId, String> {
    let points = &request.points;
    if points.len() < 2 {
        return Err("a pen stroke needs at least 2 points".into());
    }
    if points.len() > MAX_STROKE_POINTS {
        return Err(format!(
            "a pen stroke takes at most {MAX_STROKE_POINTS} points; got {}",
            points.len()
        ));
    }
    if points.iter().any(|p| !p.x.is_finite() || !p.y.is_finite()) {
        return Err("pen points must be finite numbers".into());
    }
    if request.pressures.len() != points.len() {
        return Err(format!(
            "a pen stroke needs one pressure per point; got {} points and {} pressures",
            points.len(),
            request.pressures.len()
        ));
    }
    if request.pressures.iter().any(|p| !p.is_finite()) {
        return Err("pen pressures must be finite numbers".into());
    }
    let pressures: Vec<f64> = request
        .pressures
        .iter()
        .map(|pressure| pressure.clamp(0.0, 1.0))
        .collect();
    commands::add_freehand(
        doc,
        tx,
        points.clone(),
        pressures,
        request.style.clone(),
        now_ms,
    )
}

fn size_of(request: &DrawRequest, default: (f64, f64)) -> (f64, f64) {
    (
        request.w.unwrap_or(default.0).max(8.0),
        request.h.unwrap_or(default.1).max(8.0),
    )
}

/// The element's rect: explicit coordinates win; otherwise search for free
/// space beside `near` (or beside the drawing).
fn placed_rect(doc: &Document, request: &DrawRequest, size: (f64, f64)) -> Rect {
    let anchor = request
        .near
        .as_deref()
        .and_then(|id| doc.framing_bounds(id))
        .or_else(|| doc.bounds());
    let (auto_x, auto_y) = match anchor {
        Some(rect) => find_free_spot(doc, size, rect, request.side.as_deref().unwrap_or("right")),
        None => (0.0, 0.0),
    };
    Rect::new(
        request.x.unwrap_or(auto_x),
        request.y.unwrap_or(auto_y),
        size.0,
        size.1,
    )
}

/// Deterministic free-space search: rings of candidate spots around `anchor`,
/// preferring `side`, then the other sides in a fixed order. Falls back to
/// clear of everything.
fn find_free_spot(doc: &Document, size: (f64, f64), anchor: Rect, prefer: &str) -> (f64, f64) {
    let occupied: Vec<Rect> = doc
        .ordered_live()
        .iter()
        .map(|element| element.bounds().expand(MARGIN))
        .collect();
    let fits = |x: f64, y: f64| {
        let rect = Rect::new(x, y, size.0, size.1);
        !occupied.iter().any(|other| other.intersects(&rect))
    };

    if prefer == "inside" {
        let step_x = size.0 + GAP;
        let step_y = size.1 + GAP;
        let cols = ((anchor.w - GAP) / step_x).floor().max(1.0) as i32;
        let rows = ((anchor.h - GAP) / step_y).floor().max(1.0) as i32;
        for row in 0..rows {
            for col in 0..cols {
                let x = anchor.x + GAP + col as f64 * step_x;
                let y = anchor.y + GAP + row as f64 * step_y;
                if x + size.0 > anchor.max_x() - GAP + 1.0
                    || y + size.1 > anchor.max_y() - GAP + 1.0
                {
                    continue;
                }
                if fits(x, y) {
                    return (x, y);
                }
            }
        }
        return (anchor.x + GAP, anchor.y + GAP);
    }

    let order = ["right", "bottom", "left", "top"];
    for ring in 0..RING_LIMIT {
        for side in order {
            // The preferred side gets first refusal; the rest only sweep
            // once rings start expanding.
            if ring == 0 && side != prefer {
                continue;
            }
            for (x, y) in candidates(anchor, size, side, ring) {
                if fits(x, y) {
                    return (x, y);
                }
            }
        }
    }
    let union = occupied
        .iter()
        .copied()
        .reduce(|a, b| a.union(&b))
        .unwrap_or(anchor);
    (union.max_x() + GAP, anchor.y)
}

/// Candidate spots on one side of the anchor at a given ring: the straight
/// spot first, then lateral offsets in an alternating direction.
fn candidates(anchor: Rect, size: (f64, f64), side: &str, ring: i32) -> Vec<(f64, f64)> {
    let dx = size.0 + GAP;
    let dy = size.1 + GAP;
    let ring = ring as f64;
    let (base_x, base_y, step_x, step_y) = match side {
        "left" => (anchor.x - size.0 - GAP - ring * dx, anchor.y, 0.0, dy),
        "top" => (anchor.x, anchor.y - size.1 - GAP - ring * dy, dx, 0.0),
        "bottom" => (anchor.x, anchor.max_y() + GAP + ring * dy, dx, 0.0),
        _ => (anchor.max_x() + GAP + ring * dx, anchor.y, 0.0, dy),
    };
    let mut spots = vec![(base_x, base_y)];
    for k in 1..=ring as i32 {
        spots.push((base_x + step_x * k as f64, base_y + step_y * k as f64));
        if k > 0 {
            spots.push((base_x - step_x * k as f64, base_y - step_y * k as f64));
        }
    }
    spots
}

#[cfg(test)]
mod tests {
    use super::*;
    use canvas_core::ElementKind;

    fn draw(doc: &mut Document, request: &DrawRequest, now: u64) -> Result<ElementId, String> {
        super::draw(doc, &mut Transaction::begin(), request, now)
    }

    fn pen(doc: &mut Document, request: &PenRequest, now: u64) -> Result<ElementId, String> {
        super::pen(doc, &mut Transaction::begin(), request, now)
    }

    fn request(kind: &str) -> DrawRequest {
        DrawRequest {
            kind: kind.into(),
            text: None,
            name: None,
            glyph: None,
            x: None,
            y: None,
            w: None,
            h: None,
            near: None,
            side: None,
        }
    }

    #[test]
    fn draw_places_beside_the_existing_drawing() {
        let mut doc = Document::new();
        let first = draw(&mut doc, &request("rectangle"), 1).unwrap();
        let second = draw(&mut doc, &request("rectangle"), 2).unwrap();
        let first_bounds = doc.framing_bounds(&first).unwrap();
        let second_bounds = doc.framing_bounds(&second).unwrap();
        assert!(second_bounds.x >= first_bounds.max_x());
        assert_eq!(first_bounds.y, second_bounds.y);
    }

    fn pen_request(points: Vec<Point>) -> PenRequest {
        PenRequest {
            pressures: vec![0.5; points.len()],
            points,
            style: Style::default(),
        }
    }

    #[test]
    fn pen_creates_a_freehand_stroke_through_the_given_points() {
        let mut doc = Document::new();
        let points = vec![
            Point::new(0.0, 0.0),
            Point::new(30.0, 10.0),
            Point::new(60.0, 0.0),
        ];
        let id = pen(&mut doc, &pen_request(points.clone()), 1).unwrap();
        let element = doc.element(&id).unwrap();
        match &element.kind {
            ElementKind::Freehand {
                points: stored,
                pressures,
                ..
            } => {
                assert_eq!(stored, &points);
                assert_eq!(pressures.len(), points.len());
            }
            other => panic!("expected freehand, got {other:?}"),
        }
        let bounds = element.bounds();
        assert_eq!(
            (bounds.x, bounds.y, bounds.w, bounds.h),
            (0.0, 0.0, 60.0, 10.0)
        );
    }

    #[test]
    fn pen_stores_the_pressure_profile_and_ink_style() {
        let mut doc = Document::new();
        let request = PenRequest {
            points: vec![
                Point::new(0.0, 0.0),
                Point::new(30.0, 10.0),
                Point::new(60.0, 0.0),
            ],
            pressures: vec![0.1, 1.4, -0.5],
            style: Style {
                stroke_color: "#ff8787".into(),
                fill_color: "#ffec99".into(),
                fill_style: canvas_core::model::FillPattern::CrossHatch,
                stroke_width: 6.0,
                roughness: 2,
                dash: canvas_core::model::StrokeDash::Solid,
                opacity: 0.4,
                font_size: 20.0,
            },
        };
        let id = pen(&mut doc, &request, 1).unwrap();
        let ElementKind::Freehand {
            pressures, style, ..
        } = &doc.element(&id).unwrap().kind
        else {
            panic!("expected freehand");
        };
        assert_eq!(pressures, &vec![0.1, 1.0, 0.0], "pressures clamp to 0..1");
        assert_eq!(style.stroke_width, 6.0);
        assert_eq!(style.stroke_color, "#ff8787");
        assert_eq!(style.fill_color, "#ffec99");
        assert_eq!(style.opacity, 0.4);
    }

    #[test]
    fn pen_rejects_degenerate_oversized_or_mismatched_strokes() {
        let mut doc = Document::new();
        let err = pen(&mut doc, &pen_request(vec![Point::new(1.0, 1.0)]), 1).unwrap_err();
        assert!(err.contains("at least 2 points"), "{err}");

        let too_many: Vec<Point> = (0..=MAX_STROKE_POINTS)
            .map(|i| Point::new(i as f64, 0.0))
            .collect();
        let err = pen(&mut doc, &pen_request(too_many), 1).unwrap_err();
        assert!(err.contains("at most"), "{err}");

        let mut mismatched = pen_request(vec![Point::new(0.0, 0.0), Point::new(1.0, 1.0)]);
        mismatched.pressures = vec![0.5];
        let err = pen(&mut doc, &mismatched, 1).unwrap_err();
        assert!(err.contains("one pressure per point"), "{err}");
        assert_eq!(doc.ordered_live().len(), 0, "nothing was drawn");
    }

    #[test]
    fn placement_never_overlaps_existing_elements() {
        let mut doc = Document::new();
        for _ in 0..12 {
            draw(&mut doc, &request("sticky"), 1).unwrap();
        }
        let live: Vec<Rect> = doc
            .ordered_live()
            .iter()
            .map(|element| element.bounds())
            .collect();
        for (index, rect) in live.iter().enumerate() {
            for other in live.iter().skip(index + 1) {
                assert!(
                    !rect.intersects(other),
                    "elements {index} overlap: {rect:?} and {other:?}"
                );
            }
        }
    }

    #[test]
    fn draw_inside_a_frame_keeps_children_together() {
        let mut doc = Document::new();
        let frame = draw(&mut doc, &request("frame"), 1).unwrap();
        for _ in 0..3 {
            let mut inside = request("sticky");
            inside.near = Some(frame.clone());
            inside.side = Some("inside".into());
            draw(&mut doc, &inside, 1).unwrap();
        }
        let frame_rect = doc.framing_bounds(&frame).unwrap();
        let stickies: Vec<Rect> = doc
            .ordered_live()
            .iter()
            .filter(|element| element.id != frame)
            .map(|element| element.bounds())
            .collect();
        assert_eq!(stickies.len(), 3);
        for sticky in stickies {
            assert!(
                sticky.x >= frame_rect.x && sticky.max_x() <= frame_rect.max_x(),
                "{sticky:?} escaped the frame"
            );
        }
    }

    #[test]
    fn draw_honours_explicit_coordinates_and_near_side() {
        let mut doc = Document::new();
        let anchor = draw(&mut doc, &request("rectangle"), 1).unwrap();

        let mut left = request("ellipse");
        left.near = Some(anchor.clone());
        left.side = Some("left".into());
        let left_id = draw(&mut doc, &left, 2).unwrap();
        assert!(
            doc.framing_bounds(&left_id).unwrap().max_x() <= doc.framing_bounds(&anchor).unwrap().x
        );

        let mut explicit = request("sticky");
        explicit.x = Some(-500.0);
        explicit.y = Some(-500.0);
        let explicit_id = draw(&mut doc, &explicit, 3).unwrap();
        let rect = doc.framing_bounds(&explicit_id).unwrap();
        assert_eq!((rect.x, rect.y), (-500.0, -500.0));
    }

    #[test]
    fn sticky_text_is_linked_and_centred() {
        let mut doc = Document::new();
        let mut sticky = request("sticky");
        sticky.text = Some("Cache".into());
        let id = draw(&mut doc, &sticky, 1).unwrap();
        let shape = doc.element(&id).unwrap().clone();
        let ElementKind::Shape { bound_text_id, .. } = &shape.kind else {
            panic!("expected a sticky shape");
        };
        let text_id = bound_text_id.as_ref().expect("bound text");
        assert_eq!(
            canvas_core::describe::text_of(&doc, text_id).as_deref(),
            Some("Cache")
        );
        let ElementKind::Text {
            container_id,
            align,
            ..
        } = &doc.element(text_id).unwrap().kind
        else {
            panic!("expected text");
        };
        assert_eq!(container_id.as_deref(), Some(id.as_str()));
        assert_eq!(*align, TextAlign::Center);
    }

    #[test]
    fn glyph_captions_sit_below_the_glyph() {
        let mut doc = Document::new();
        let mut glyph = request("glyph");
        glyph.glyph = Some("cloud_run".into());
        glyph.text = Some("Cloud Run".into());
        let id = draw(&mut doc, &glyph, 1).unwrap();
        let ElementKind::Glyph { caption_id, .. } = &doc.element(&id).unwrap().kind else {
            panic!("expected a glyph");
        };
        let caption_id = caption_id.as_ref().expect("caption");
        let glyph_rect = doc.framing_bounds(&id).unwrap();
        let caption_rect = doc.framing_bounds(caption_id).unwrap();
        assert!(caption_rect.y >= glyph_rect.max_y());
    }

    #[test]
    fn draw_rejects_unknown_kinds_glyphs_and_empty_text_elements() {
        let mut doc = Document::new();
        assert!(draw(&mut doc, &request("hexagon"), 1).is_err());
        assert!(draw(&mut doc, &request("text"), 1).is_err());
        let mut glyph = request("glyph");
        glyph.glyph = Some("not_a_glyph".into());
        assert!(draw(&mut doc, &glyph, 1).is_err());
    }
}
