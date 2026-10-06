//! The explanation script: ordered beats with handles, executed atomically.
//!
//! One `canvas_script` call can build a whole small diagram: beats reference
//! each other by `as` handle, so the agent never needs ids mid-script. The
//! script runs on a clone of the document and only commits when every beat
//! succeeds, so a failed middle beat cannot leave a half-built drawing.

use std::collections::HashMap;

use canvas_core::geometry::{Point, Rect};
use canvas_core::model::{
    Binding, Document, ElementId, ElementKind, Endpoint, Routing, Style, TextAlign,
};
use canvas_core::{connector, ObstacleIndex};
use rmcp::schemars;

use crate::doc_ops::{self, DrawRequest};

/// Extra room a container leaves around the elements it is drawn `around`.
const AROUND_PAD: f64 = 36.0;
/// Extra room above a frame for its name band.
const AROUND_TOP: f64 = 28.0;

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScriptBeat {
    /// Draw one element; `as` names it for later beats.
    #[serde(default)]
    pub draw: Option<DrawBeat>,
    /// Connect two handles (or existing element ids).
    #[serde(default)]
    pub connect: Option<ConnectBeat>,
    /// Put a cross, check or question mark on a handle or id.
    #[serde(default)]
    pub annotate: Option<AnnotateBeat>,
    /// Dim or emphasize handles or ids.
    #[serde(default)]
    pub emphasis: Option<EmphasisBeat>,
    /// Arrange these handles or ids into a tidy grid.
    #[serde(default)]
    pub tidy: Option<Vec<String>>,
    /// Tombstone these handles or ids (frames take their children).
    #[serde(default)]
    pub clear: Option<Vec<String>>,
    /// Move the camera to this handle or id while the beat plays.
    #[serde(default)]
    pub focus: Option<String>,
    /// Pulse a highlight on this handle or id while the beat plays.
    #[serde(default)]
    pub point: Option<String>,
    /// Caption shown on the canvas while the beat plays.
    #[serde(default)]
    pub caption: Option<String>,
    /// Pause after the beat, in milliseconds (default 900 during playback).
    #[serde(default)]
    pub hold: Option<u64>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnnotateBeat {
    /// Handle or element id to mark.
    pub target: String,
    /// cross, check or question.
    pub mark: String,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmphasisBeat {
    /// Handles or element ids to change.
    pub targets: Vec<String>,
    /// dim or emphasize.
    pub mode: String,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DrawBeat {
    /// Handle for later beats; also returned in the result.
    #[serde(rename = "as", default)]
    pub handle: Option<String>,
    /// rectangle, ellipse, diamond, sticky, frame, glyph or text.
    pub kind: String,
    /// Label text (bound to shapes/stickies; required for text).
    #[serde(default)]
    pub text: Option<String>,
    /// Frame name.
    #[serde(default)]
    pub name: Option<String>,
    /// Glyph library id, for kind "glyph".
    #[serde(default)]
    pub glyph: Option<String>,
    /// Place beside this handle or element id.
    #[serde(default)]
    pub near: Option<String>,
    /// right (default), left, top, bottom or inside of `near`.
    #[serde(default)]
    pub side: Option<String>,
    /// Draw this container around these handles or ids.
    #[serde(default)]
    pub around: Option<Vec<String>>,
    #[serde(default)]
    pub x: Option<f64>,
    #[serde(default)]
    pub y: Option<f64>,
    #[serde(default)]
    pub w: Option<f64>,
    #[serde(default)]
    pub h: Option<f64>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConnectBeat {
    /// Source handle or element id.
    pub from: String,
    /// Target handle or element id.
    pub to: String,
    /// Label drawn at the route's midpoint.
    #[serde(default)]
    pub label: Option<String>,
    /// false draws a plain straight line without an arrowhead.
    #[serde(default)]
    pub arrow: Option<bool>,
}

/// What one script beat produced, and how it should play back.
#[derive(Debug, Default)]
pub struct BeatOutcome {
    /// Elements that appear when this beat plays.
    pub created: Vec<ElementId>,
    /// Camera target resolved for this beat.
    pub focus: Option<ElementId>,
    /// Highlight target resolved for this beat.
    pub point: Option<ElementId>,
    /// Canvas caption for this beat.
    pub caption: Option<String>,
    /// Pause after the beat; filled with the playback default when unset.
    pub hold_ms: Option<u64>,
}

/// What a successful script produced.
#[derive(Debug)]
pub struct ScriptOutcome {
    /// Handles in beat order, with the ids they resolved to.
    pub created: Vec<(String, ElementId)>,
    /// Per-beat playback data, in beat order.
    pub steps: Vec<BeatOutcome>,
    /// True when any beat carries presentation data.
    pub presentation: bool,
    /// Live elements in the document afterwards.
    pub elements: usize,
}

/// Default pause after a beat once a script is being presented.
const DEFAULT_HOLD_MS: u64 = 900;

/// Run `beats` against `doc`; on any failure the document is untouched.
pub fn execute(
    doc: &mut Document,
    beats: &[ScriptBeat],
    now_ms: u64,
) -> Result<ScriptOutcome, String> {
    if beats.is_empty() {
        return Err("a script needs at least one beat".into());
    }
    let presentation = beats.iter().any(|beat| {
        beat.focus.is_some()
            || beat.point.is_some()
            || beat.caption.is_some()
            || beat.hold.is_some()
    });

    let mut working = doc.clone();
    let mut handles: HashMap<String, ElementId> = HashMap::new();
    let mut created: Vec<(String, ElementId)> = Vec::new();
    let mut steps: Vec<BeatOutcome> = Vec::new();

    for (index, beat) in beats.iter().enumerate() {
        let position = index + 1;
        let label = beat_label(beat);
        let fail = |message: String| format!("beat {position} ({label}): {message}");

        let mut step = BeatOutcome {
            hold_ms: beat.hold.or(presentation.then_some(DEFAULT_HOLD_MS)),
            ..BeatOutcome::default()
        };
        step.caption = beat
            .caption
            .as_deref()
            .map(str::trim)
            .filter(|caption| !caption.is_empty())
            .map(str::to_string);

        let actions = [
            beat.draw.is_some(),
            beat.connect.is_some(),
            beat.annotate.is_some(),
            beat.emphasis.is_some(),
            beat.tidy.is_some(),
            beat.clear.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count();
        if actions > 1 {
            return Err(fail("one action per beat".into()));
        }

        if let Some(connect) = &beat.connect {
            let (id, label_id) =
                run_connect(&mut working, &handles, connect, now_ms).map_err(fail)?;
            created.push((format!("connect{position}"), id.clone()));
            step.created.push(id);
            if let Some(label_id) = label_id {
                step.created.push(label_id);
            }
        } else if let Some(annotate) = &beat.annotate {
            let target = resolve(&handles, &working, &annotate.target).map_err(&fail)?;
            let ids = crate::verbs::annotate(&mut working, &target, &annotate.mark, now_ms)
                .map_err(fail)?;
            for id in ids {
                step.created.push(id);
            }
        } else if let Some(emphasis) = &beat.emphasis {
            let targets = emphasis
                .targets
                .iter()
                .map(|reference| resolve(&handles, &working, reference))
                .collect::<Result<Vec<_>, _>>()
                .map_err(&fail)?;
            crate::verbs::emphasis(&mut working, &targets, &emphasis.mode, now_ms).map_err(fail)?;
        } else if let Some(tidy) = &beat.tidy {
            let targets = tidy
                .iter()
                .map(|reference| resolve(&handles, &working, reference))
                .collect::<Result<Vec<_>, _>>()
                .map_err(&fail)?;
            crate::verbs::tidy(&mut working, &targets, now_ms).map_err(fail)?;
        } else if let Some(clear) = &beat.clear {
            let targets = clear
                .iter()
                .map(|reference| resolve(&handles, &working, reference))
                .collect::<Result<Vec<_>, _>>()
                .map_err(&fail)?;
            crate::verbs::clear(&mut working, &targets, now_ms).map_err(fail)?;
        } else if let Some(draw) = &beat.draw {
            if let Some(handle) = &draw.handle {
                if handles.contains_key(handle) {
                    return Err(fail(format!("duplicate handle {handle:?}")));
                }
            }
            let near = draw
                .near
                .as_deref()
                .map(|reference| resolve(&handles, &working, reference))
                .transpose()
                .map_err(&fail)?;
            let mut request = DrawRequest {
                kind: draw.kind.clone(),
                text: draw.text.clone(),
                name: draw.name.clone(),
                glyph: draw.glyph.clone(),
                x: draw.x,
                y: draw.y,
                w: draw.w,
                h: draw.h,
                near,
                side: draw.side.clone(),
            };
            if let Some(around) = &draw.around {
                let rect = around_rect(&handles, &working, around).map_err(&fail)?;
                request.x = Some(rect.x);
                request.y = Some(rect.y);
                request.w = Some(rect.w);
                request.h = Some(rect.h);
            }
            let id = doc_ops::draw(&mut working, &request, now_ms).map_err(fail)?;
            if let Some(handle) = &draw.handle {
                handles.insert(handle.clone(), id.clone());
                created.push((handle.clone(), id.clone()));
            } else {
                created.push((format!("beat{position}"), id.clone()));
            }
            step.created.push(id.clone());
            if let Some(label_id) = linked_text(&working, &id) {
                step.created.push(label_id);
            }
        } else if step.focus.is_none()
            && step.point.is_none()
            && step.caption.is_none()
            && beat.hold.is_none()
        {
            return Err(format!(
                "beat {position}: expected `draw`, `connect` or presentation fields"
            ));
        }
        // Resolve presentation targets after the beat's own elements exist, so
        // a beat can focus or point at something it just drew.
        step.focus = beat
            .focus
            .as_deref()
            .map(|reference| resolve(&handles, &working, reference))
            .transpose()
            .map_err(&fail)?;
        step.point = beat
            .point
            .as_deref()
            .map(|reference| resolve(&handles, &working, reference))
            .transpose()
            .map_err(&fail)?;
        steps.push(step);
    }

    let elements = working.live_count();
    *doc = working;
    Ok(ScriptOutcome {
        created,
        steps,
        presentation,
        elements,
    })
}

/// The label/caption element a container links to, if any.
fn linked_text(doc: &Document, id: &str) -> Option<ElementId> {
    match &doc.element(id)?.kind {
        ElementKind::Shape { bound_text_id, .. } => bound_text_id.clone(),
        ElementKind::Glyph { caption_id, .. } => caption_id.clone(),
        ElementKind::Arrow { label_id, .. } => label_id.clone(),
        _ => None,
    }
}

fn run_connect(
    doc: &mut Document,
    handles: &HashMap<String, ElementId>,
    beat: &ConnectBeat,
    now_ms: u64,
) -> Result<(ElementId, Option<ElementId>), String> {
    let source = resolve(handles, doc, &beat.from)?;
    let target = resolve(handles, doc, &beat.to)?;
    if source == target {
        return Err("cannot connect an element to itself".into());
    }
    let source_bounds = doc
        .framing_bounds(&source)
        .ok_or_else(|| format!("unknown reference {:?}", beat.from))?;
    let target_bounds = doc
        .framing_bounds(&target)
        .ok_or_else(|| format!("unknown reference {:?}", beat.to))?;
    let arrow = beat.arrow.unwrap_or(true);
    let routing = if arrow {
        Routing::Elbow
    } else {
        Routing::Straight
    };

    let mut start = Endpoint {
        x: source_bounds.center().x,
        y: source_bounds.center().y,
        binding: Some(Binding {
            element_id: source,
            focus: None,
        }),
    };
    let mut end = Endpoint {
        x: target_bounds.center().x,
        y: target_bounds.center().y,
        binding: Some(Binding {
            element_id: target,
            focus: None,
        }),
    };
    let (start_point, _) = connector::resolve_endpoint(&start, end.point(), doc);
    let (end_point, _) = connector::resolve_endpoint(&end, start_point, doc);
    start.x = start_point.x;
    start.y = start_point.y;
    end.x = end_point.x;
    end.y = end_point.y;

    let id = doc.add(
        ElementKind::Arrow {
            start: start.clone(),
            end: end.clone(),
            routing,
            start_arrowhead: false,
            end_arrowhead: arrow,
            style: Style::default(),
            waypoints: Vec::new(),
            label_id: None,
        },
        now_ms,
    );

    let mut linked_label = None;
    if let Some(label) = beat
        .label
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        let mut obstacles = ObstacleIndex::new();
        obstacles.rebuild(doc);
        let route = connector::route_avoiding(&start, &end, routing, &[], doc, &obstacles);
        let midpoint = polyline_midpoint(&route).unwrap_or_else(|| source_bounds.center());
        let text_id = doc_ops::add_text(doc, label, TextAlign::Center, "#ece4d6", now_ms);
        if let Some(text_rect) = doc.element(&text_id).map(|element| element.bounds()) {
            if let Some(element) = doc.element_mut(&text_id) {
                if let ElementKind::Text {
                    rect, container_id, ..
                } = &mut element.kind
                {
                    *rect = Rect::new(
                        midpoint.x - text_rect.w / 2.0,
                        midpoint.y - text_rect.h / 2.0,
                        text_rect.w,
                        text_rect.h,
                    );
                    *container_id = Some(id.clone());
                }
            }
        }
        if let Some(element) = doc.element_mut(&id) {
            if let ElementKind::Arrow { label_id, .. } = &mut element.kind {
                *label_id = Some(text_id.clone());
            }
        }
        linked_label = Some(text_id);
    }
    Ok((id, linked_label))
}

fn resolve(
    handles: &HashMap<String, ElementId>,
    doc: &Document,
    reference: &str,
) -> Result<ElementId, String> {
    if let Some(id) = handles.get(reference) {
        return Ok(id.clone());
    }
    if doc
        .element(reference)
        .is_some_and(|element| !element.is_deleted)
    {
        return Ok(reference.to_string());
    }
    Err(format!(
        "unknown reference {reference:?} (no handle or element with that id)"
    ))
}

fn around_rect(
    handles: &HashMap<String, ElementId>,
    doc: &Document,
    around: &[String],
) -> Result<Rect, String> {
    if around.is_empty() {
        return Err("`around` needs at least one reference".into());
    }
    let mut union: Option<Rect> = None;
    for reference in around {
        let id = resolve(handles, doc, reference)?;
        let bounds = doc
            .framing_bounds(&id)
            .ok_or_else(|| format!("unknown reference {reference:?}"))?;
        union = Some(match union {
            Some(current) => current.union(&bounds),
            None => bounds,
        });
    }
    let union = union.expect("checked non-empty");
    let mut rect = union.expand(AROUND_PAD);
    rect.y -= AROUND_TOP;
    rect.h += AROUND_TOP;
    Ok(rect)
}

fn polyline_midpoint(points: &[Point]) -> Option<Point> {
    if points.is_empty() {
        return None;
    }
    let mut total = 0.0;
    for pair in points.windows(2) {
        total += pair[0].distance(pair[1]);
    }
    let mut remaining = total / 2.0;
    for pair in points.windows(2) {
        let length = pair[0].distance(pair[1]);
        if remaining <= length && length > 0.0 {
            let t = remaining / length;
            return Some(Point::new(
                pair[0].x + (pair[1].x - pair[0].x) * t,
                pair[0].y + (pair[1].y - pair[0].y) * t,
            ));
        }
        remaining -= length;
    }
    Some(*points.last().expect("non-empty"))
}

fn beat_label(beat: &ScriptBeat) -> String {
    if let Some(connect) = &beat.connect {
        return format!("connect {} -> {}", connect.from, connect.to);
    }
    if let Some(annotate) = &beat.annotate {
        return format!("annotate {} ({})", annotate.target, annotate.mark);
    }
    if let Some(emphasis) = &beat.emphasis {
        return format!(
            "emphasis {} ({})",
            emphasis.targets.join(","),
            emphasis.mode
        );
    }
    if let Some(tidy) = &beat.tidy {
        return format!("tidy {} refs", tidy.len());
    }
    if let Some(clear) = &beat.clear {
        return format!("clear {} refs", clear.len());
    }
    if let Some(draw) = &beat.draw {
        return match &draw.handle {
            Some(handle) => format!("draw {handle:?} ({})", draw.kind),
            None => format!("draw ({})", draw.kind),
        };
    }
    "empty".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use canvas_core::describe::describe;

    fn beat(json: serde_json::Value) -> ScriptBeat {
        serde_json::from_value(json).expect("valid beat")
    }

    fn draw(handle: &str, kind: &str) -> ScriptBeat {
        beat(serde_json::json!({ "draw": { "as": handle, "kind": kind } }))
    }

    fn labelled_draw(handle: &str, kind: &str, text: &str) -> ScriptBeat {
        beat(serde_json::json!({ "draw": { "as": handle, "kind": kind, "text": text } }))
    }

    fn named_frame(handle: &str, name: &str) -> ScriptBeat {
        beat(serde_json::json!({ "draw": { "as": handle, "kind": "frame", "name": name } }))
    }

    fn connect(from: &str, to: &str, label: Option<&str>) -> ScriptBeat {
        beat(serde_json::json!({ "connect": { "from": from, "to": to, "label": label } }))
    }

    #[test]
    fn a_script_builds_a_small_architecture_in_one_pass() {
        let mut doc = Document::new();
        let outcome = execute(
            &mut doc,
            &[
                labelled_draw("client", "sticky", "Client"),
                named_frame("vpc", "VPC"),
                labelled_draw("db", "sticky", "DB"),
                connect("client", "db", Some("queries")),
            ],
            1,
        )
        .unwrap();

        assert_eq!(outcome.created.len(), 4);
        let described = describe(&doc);
        assert!(described.contains("frame"), "{described}");
        assert!(described.contains("\"VPC\""), "{described}");
        assert!(described.contains("\"queries\""), "{described}");
    }

    #[test]
    fn handles_resolve_across_beats_and_return_in_order() {
        let mut doc = Document::new();
        let outcome =
            execute(&mut doc, &[draw("a", "rectangle"), draw("b", "ellipse")], 1).unwrap();
        assert_eq!(outcome.created[0].0, "a");
        assert_eq!(outcome.created[1].0, "b");
        assert!(doc.element(&outcome.created[0].1).is_some());
    }

    #[test]
    fn frame_around_children_contains_them() {
        let mut doc = Document::new();
        let outcome = execute(
            &mut doc,
            &[
                labelled_draw("a", "sticky", "A"),
                labelled_draw("b", "sticky", "B"),
                beat(serde_json::json!({
                    "draw": { "as": "box", "kind": "frame", "name": "Group", "around": ["a", "b"] }
                })),
            ],
            1,
        )
        .unwrap();
        let frame_id = &outcome.created[2].1;
        let frame = doc.framing_bounds(frame_id).unwrap();
        for handle in ["a", "b"] {
            let id = &outcome
                .created
                .iter()
                .find(|(name, _)| name == handle)
                .unwrap()
                .1;
            let bounds = doc.framing_bounds(id).unwrap();
            assert!(bounds.x >= frame.x && bounds.max_x() <= frame.max_x());
            assert!(bounds.y >= frame.y && bounds.max_y() <= frame.max_y());
        }
    }

    #[test]
    fn auto_placement_never_overlaps_and_is_deterministic() {
        let beats = [
            draw("a", "sticky"),
            draw("b", "sticky"),
            draw("c", "sticky"),
            draw("d", "rectangle"),
        ];
        let layout = |beats: &[ScriptBeat]| {
            let mut doc = Document::new();
            execute(&mut doc, beats, 1).unwrap();
            let mut rects: Vec<(f64, f64, f64, f64)> = doc
                .ordered_live()
                .iter()
                .map(|element| {
                    let b = element.bounds();
                    (b.x, b.y, b.w, b.h)
                })
                .collect();
            rects.sort_by(|a, b| a.partial_cmp(b).unwrap());
            rects
        };
        let first = layout(&beats);
        let second = layout(&beats);
        assert_eq!(first, second, "same input must produce the same layout");
        for (index, rect) in first.iter().enumerate() {
            for other in first.iter().skip(index + 1) {
                let a = Rect::new(rect.0, rect.1, rect.2, rect.3);
                let b = Rect::new(other.0, other.1, other.2, other.3);
                assert!(!a.intersects(&b), "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn a_failed_beat_leaves_the_document_untouched() {
        let mut doc = Document::new();
        let before = doc.live_count();
        let error = execute(
            &mut doc,
            &[
                draw("a", "sticky"),
                connect("a", "ghost", None),
                draw("b", "sticky"),
            ],
            1,
        )
        .unwrap_err();
        assert!(error.contains("beat 2"), "{error}");
        assert!(error.contains("ghost"), "{error}");
        assert_eq!(doc.live_count(), before, "partial script leaked");
    }

    #[test]
    fn error_messages_name_the_beat() {
        let mut doc = Document::new();
        let mut bad = draw("a", "hexagon");
        bad.draw.as_mut().unwrap().handle = Some("a".into());
        let error = execute(&mut doc, &[bad], 1).unwrap_err();
        assert!(
            error.contains("beat 1") && error.contains("hexagon"),
            "{error}"
        );

        let error = execute(&mut doc, &[draw("a", "sticky"), draw("a", "sticky")], 1).unwrap_err();
        assert!(error.contains("duplicate handle"), "{error}");
    }

    #[test]
    fn connects_can_reference_existing_document_ids() {
        let mut doc = Document::new();
        let existing = doc_ops::draw(
            &mut doc,
            &DrawRequest {
                kind: "rectangle".into(),
                text: None,
                name: None,
                glyph: None,
                x: Some(0.0),
                y: Some(0.0),
                w: None,
                h: None,
                near: None,
                side: None,
            },
            1,
        )
        .unwrap();
        let outcome = execute(
            &mut doc,
            &[draw("new", "sticky"), connect("new", &existing, None)],
            2,
        )
        .unwrap();
        let described = describe(&doc);
        assert!(described.contains(&format!(">{existing}")), "{described}");
        assert_eq!(outcome.created.len(), 2);
    }

    #[test]
    fn empty_scripts_are_rejected() {
        let mut doc = Document::new();
        assert!(execute(&mut doc, &[], 1).is_err());
    }

    #[test]
    fn presentation_beats_yield_a_step_timeline() {
        let mut first = labelled_draw("a", "sticky", "Client");
        first.caption = Some("First".into());
        first.focus = Some("a".into());
        first.point = Some("a".into());
        first.hold = Some(1_500);
        let second = draw("b", "sticky");
        let walk = beat(serde_json::json!({
            "focus": "a",
            "caption": "Back to the first",
            "hold": 600
        }));

        let mut doc = Document::new();
        let outcome = execute(&mut doc, &[first, second, walk], 1).unwrap();
        assert!(outcome.presentation);
        assert_eq!(outcome.steps.len(), 3);
        assert_eq!(outcome.steps[0].created.len(), 2, "sticky plus its label");
        assert_eq!(outcome.steps[0].caption.as_deref(), Some("First"));
        assert!(outcome.steps[0].focus.is_some() && outcome.steps[0].point.is_some());
        assert_eq!(outcome.steps[0].hold_ms, Some(1_500));
        assert_eq!(
            outcome.steps[1].hold_ms,
            Some(DEFAULT_HOLD_MS),
            "beats without an explicit hold get the playback default"
        );
        assert!(outcome.steps[2].created.is_empty());
        assert_eq!(
            outcome.steps[2].focus.as_deref(),
            outcome.steps[0].created.first().map(String::as_str)
        );
    }

    #[test]
    fn pure_draw_scripts_have_no_playback_pacing() {
        let mut doc = Document::new();
        let outcome = execute(&mut doc, &[draw("a", "sticky"), draw("b", "sticky")], 1).unwrap();
        assert!(!outcome.presentation);
        assert!(outcome.steps.iter().all(|step| step.hold_ms.is_none()));
    }

    #[test]
    fn standalone_presentation_beats_need_a_target_or_caption() {
        let mut doc = Document::new();
        let empty = beat(serde_json::json!({}));
        let error = execute(&mut doc, &[empty], 1).unwrap_err();
        assert!(error.contains("beat 1"), "{error}");
    }

    #[test]
    fn discussion_verbs_are_expressible_as_beats() {
        let mut doc = Document::new();
        let outcome = execute(
            &mut doc,
            &[
                labelled_draw("a", "sticky", "A"),
                labelled_draw("b", "sticky", "B"),
                beat(serde_json::json!({ "annotate": { "target": "b", "mark": "cross" } })),
                beat(serde_json::json!({ "emphasis": { "targets": ["a"], "mode": "dim" } })),
                beat(serde_json::json!({ "tidy": ["a", "b"] })),
                beat(serde_json::json!({ "clear": ["a"] })),
            ],
            1,
        )
        .unwrap();

        // a + label + b + label, then two cross strokes.
        assert_eq!(outcome.steps[2].created.len(), 2, "a cross is two strokes");
        let described = describe(&doc);
        assert!(described.contains("stroke"), "{described}");
        // The clear beat tombstoned a and its label; b and the mark remain.
        let id_for = |handle: &str| {
            outcome
                .created
                .iter()
                .find(|(name, _)| name == handle)
                .map(|(_, id)| id.clone())
                .expect("handle in outcome")
        };
        assert!(doc.element(&id_for("a")).unwrap().is_deleted);
        assert!(!doc.element(&id_for("b")).unwrap().is_deleted);
        assert!(outcome.steps[2]
            .created
            .iter()
            .all(|id| doc.element(id).is_some_and(|element| !element.is_deleted)));
    }

    #[test]
    fn a_beat_with_two_actions_is_rejected() {
        let mut doc = Document::new();
        let double = beat(serde_json::json!({
            "draw": { "as": "a", "kind": "sticky" },
            "tidy": ["a"]
        }));
        let error = execute(&mut doc, &[double], 1).unwrap_err();
        assert!(error.contains("one action per beat"), "{error}");
        assert_eq!(doc.live_count(), 0);
    }
}
