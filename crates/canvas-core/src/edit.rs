//! Semantic revisions shared by native callers and the browser's document model.
//! Updates preserve identity and return reversible deltas, including followers.

use std::collections::BTreeSet;

use crate::model::{Binding, Document, ElementId, ElementKind, StylePatch, TextAlign};
use crate::text::{layout_text, ApproxMeasurer};
use crate::{connector, element_style_mut, translate_element, Delta, ObstacleIndex, Point, Rect};

#[derive(Default)]
pub struct ElementUpdate {
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub w: Option<f64>,
    pub h: Option<f64>,
    /// Shape label, glyph caption, connector label, frame name or text content.
    pub text: Option<String>,
    pub style: Option<StylePatch>,
    /// Some(None) detaches; Some(Some(id)) establishes ownership.
    pub frame: Option<Option<ElementId>>,
    pub from: Option<ElementId>,
    pub to: Option<ElementId>,
}

/// Revise one live element atomically. Invalid fields leave the document untouched.
pub fn update_element(
    doc: &mut Document,
    id: &str,
    update: &ElementUpdate,
    now: u64,
) -> Result<Vec<Delta>, String> {
    let original = doc
        .element(id)
        .filter(|e| !e.is_deleted)
        .ok_or_else(|| format!("unknown or deleted element {id:?}"))?;
    for value in [update.x, update.y, update.w, update.h]
        .into_iter()
        .flatten()
    {
        if !value.is_finite() {
            return Err("geometry must be finite".into());
        }
    }
    if let Some(style) = &update.style {
        if [style.stroke_width, style.opacity, style.font_size]
            .into_iter()
            .flatten()
            .any(|value| !value.is_finite())
        {
            return Err("style numbers must be finite".into());
        }
    }
    if [update.w, update.h].into_iter().flatten().any(|n| n < 8.0) {
        return Err("width and height must be at least 8 world pixels".into());
    }
    if (update.w.is_some() || update.h.is_some())
        && matches!(
            original.kind,
            ElementKind::Arrow { .. } | ElementKind::Freehand { .. }
        )
    {
        return Err(
            "resize supports shapes, glyphs, text, frames and stacks; reconnect connectors instead"
                .into(),
        );
    }
    if update.text.is_some()
        && matches!(
            original.kind,
            ElementKind::Freehand { .. } | ElementKind::StickyStack { .. }
        )
    {
        return Err("this element does not have text".into());
    }
    if (update.from.is_some() || update.to.is_some())
        && !matches!(original.kind, ElementKind::Arrow { .. })
    {
        return Err("from/to apply only to connectors".into());
    }
    if let Some(Some(frame)) = &update.frame {
        if !doc
            .element(frame)
            .is_some_and(|e| !e.is_deleted && e.is_frame())
        {
            return Err(format!("frame {frame:?} must name a live frame"));
        }
        let mut parent = Some(frame.as_str());
        let mut seen = BTreeSet::new();
        while let Some(current) = parent {
            if current == id || !seen.insert(current) {
                return Err("frame membership would create a cycle".into());
            }
            parent = doc.element(current).and_then(|e| e.frame_id.as_deref());
        }
    }
    for endpoint in [&update.from, &update.to].into_iter().flatten() {
        if !doc
            .element(endpoint)
            .is_some_and(|e| !e.is_deleted && e.is_bind_target())
        {
            return Err(format!(
                "connector endpoint {endpoint:?} must name a live shape, glyph or frame"
            ));
        }
    }
    let mut working = doc.clone();
    let bounds = original.bounds();
    let dx = update.x.unwrap_or(bounds.x) - bounds.x;
    let dy = update.y.unwrap_or(bounds.y) - bounds.y;
    let mut followers = BTreeSet::from([id.to_string()]);
    loop {
        let old_len = followers.len();
        for element in working.ordered_live() {
            let child = element
                .frame_id
                .as_ref()
                .is_some_and(|frame| followers.contains(frame));
            let label = matches!(&element.kind, ElementKind::Text { container_id: Some(owner), .. } if followers.contains(owner));
            if child || label {
                followers.insert(element.id.clone());
            }
        }
        if followers.len() == old_len {
            break;
        }
    }
    if dx != 0.0 || dy != 0.0 {
        for follower in &followers {
            translate_element(working.element_mut(follower).unwrap(), dx, dy);
        }
    }
    let element = working.element_mut(id).unwrap();
    match &mut element.kind {
        ElementKind::Shape { rect, .. }
        | ElementKind::Glyph { rect, .. }
        | ElementKind::Text { rect, .. }
        | ElementKind::Frame { rect, .. }
        | ElementKind::StickyStack { rect, .. } => {
            if let Some(w) = update.w {
                rect.w = w;
            }
            if let Some(h) = update.h {
                rect.h = h;
            }
        }
        _ => {}
    }
    if update.w.is_some() {
        if let ElementKind::Text { fixed_width, .. } = &mut element.kind {
            *fixed_width = true;
        }
    }
    if let Some(frame) = &update.frame {
        element.frame_id = frame.clone();
    }
    if let Some(style) = &update.style {
        let target_style = element_style_mut(element).ok_or("frames do not have style fields")?;
        style.apply_to(target_style);
    }
    if let ElementKind::Arrow { start, end, .. } = &mut element.kind {
        if let Some(from) = &update.from {
            start.binding = Some(Binding {
                element_id: from.clone(),
                focus: None,
            });
        }
        if let Some(to) = &update.to {
            end.binding = Some(Binding {
                element_id: to.clone(),
                focus: None,
            });
        }
        if start.binding.as_ref().map(|b| &b.element_id)
            == end.binding.as_ref().map(|b| &b.element_id)
            && start.binding.is_some()
        {
            return Err("cannot connect an element to itself".into());
        }
    }
    if let Some(text) = &update.text {
        set_text(&mut working, id, text, now)?;
    }
    // Text style follows its owner's ink/font; fill and stroke thickness stay independent.
    if let Some(patch) = &update.style {
        if let Some(label) = owned_label(&working, id) {
            if let Some(style) = working.element_mut(&label).and_then(element_style_mut) {
                if let Some(color) = &patch.stroke_color {
                    style.stroke_color = color.clone();
                }
                if let Some(font) = patch.font_size {
                    style.font_size = font.clamp(8.0, 96.0);
                }
                if let Some(opacity) = patch.opacity {
                    style.opacity = opacity.clamp(0.05, 1.0);
                }
            }
        }
    }
    // Reroute affected connectors and reflow all affected labels after geometry settles.
    let arrows: Vec<String> = working
        .ordered_live()
        .into_iter()
        .filter_map(|e| match &e.kind {
            ElementKind::Arrow { start, end, .. }
                if followers.contains(&e.id)
                    || [&start.binding, &end.binding]
                        .into_iter()
                        .flatten()
                        .any(|b| followers.contains(&b.element_id)) =>
            {
                Some(e.id.clone())
            }
            _ => None,
        })
        .collect();
    for arrow in &arrows {
        refresh_connector(&mut working, arrow);
    }
    for owner in followers.iter().chain(arrows.iter()) {
        reflow_label(&mut working, owner);
    }
    // Editing a bound text directly must keep it placed with its owner.
    if let ElementKind::Text {
        container_id: Some(owner),
        ..
    } = &working.element(id).unwrap().kind
    {
        let owner = owner.clone();
        reflow_label(&mut working, &owner);
    } else if matches!(working.element(id).unwrap().kind, ElementKind::Text { .. }) {
        reflow_label(&mut working, id);
    }
    if let Some(height) = update.h {
        if let ElementKind::Text {
            rect,
            container_id: None,
            ..
        } = &mut working.element_mut(id).unwrap().kind
        {
            rect.h = height;
        }
    }
    let mut deltas = Vec::new();
    for (element_id, after) in &mut working.elements {
        match doc.element(element_id) {
            Some(before) if !before.content_eq(after) => {
                after.touch(now);
                deltas.push(Delta::Update {
                    before: before.clone(),
                    after: after.clone(),
                });
            }
            None => deltas.push(Delta::Add {
                element: after.clone(),
            }),
            _ => {}
        }
    }
    for delta in &deltas {
        doc.apply(delta).map_err(|e| e.to_string())?;
    }
    Ok(deltas)
}

pub fn owned_label(doc: &Document, id: &str) -> Option<ElementId> {
    match &doc.element(id)?.kind {
        ElementKind::Shape { bound_text_id, .. } => bound_text_id.clone(),
        ElementKind::Glyph { caption_id, .. } => caption_id.clone(),
        ElementKind::Arrow { label_id, .. } => label_id.clone(),
        _ => None,
    }
}

fn set_text(doc: &mut Document, id: &str, value: &str, now: u64) -> Result<(), String> {
    match &mut doc.element_mut(id).unwrap().kind {
        ElementKind::Frame { name, .. } => {
            *name = value.to_string();
            return Ok(());
        }
        ElementKind::Text { text, .. } => {
            *text = value.to_string();
            return Ok(());
        }
        _ => {}
    }
    if let Some(label) = owned_label(doc, id) {
        if let Some(element) = doc.element_mut(&label) {
            if let ElementKind::Text { text, .. } = &mut element.kind {
                *text = value.to_string();
            }
        }
    } else if !value.is_empty() {
        let style = crate::editor::element_style(doc.element(id).unwrap())
            .cloned()
            .unwrap_or_default();
        let label = doc.add(
            ElementKind::Text {
                rect: Rect::new(0.0, 0.0, 1.0, 1.0),
                text: value.to_string(),
                align: TextAlign::Center,
                fixed_width: false,
                container_id: Some(id.to_string()),
                style,
            },
            now,
        );
        match &mut doc.element_mut(id).unwrap().kind {
            ElementKind::Shape { bound_text_id, .. } => *bound_text_id = Some(label),
            ElementKind::Glyph { caption_id, .. } => *caption_id = Some(label),
            ElementKind::Arrow { label_id, .. } => *label_id = Some(label),
            _ => return Err("this element does not have a label".into()),
        }
    }
    Ok(())
}

fn refresh_connector(doc: &mut Document, id: &str) {
    let arrow = doc.element(id).unwrap().clone();
    let ElementKind::Arrow { start, end, .. } = &arrow.kind else {
        return;
    };
    let (a, _) = connector::resolve_endpoint(start, end.point(), doc);
    let (b, _) = connector::resolve_endpoint(end, a, doc);
    if let ElementKind::Arrow { start, end, .. } = &mut doc.element_mut(id).unwrap().kind {
        start.x = a.x;
        start.y = a.y;
        end.x = b.x;
        end.y = b.y;
    }
}

fn reflow_label(doc: &mut Document, owner: &str) {
    let Some(element) = doc.element(owner).cloned() else {
        return;
    };
    // An owned text is laid out through its owner, never as a free text again.
    if matches!(
        element.kind,
        ElementKind::Text {
            container_id: Some(_),
            ..
        }
    ) {
        return;
    }
    let label = owned_label(doc, owner).or_else(|| element.is_text().then(|| owner.to_string()));
    let Some(label) = label else {
        return;
    };
    let Some(text_element) = doc.element(&label).cloned() else {
        return;
    };
    let ElementKind::Text {
        text,
        style,
        fixed_width,
        rect: text_rect,
        ..
    } = &text_element.kind
    else {
        return;
    };
    let wrap = match &element.kind {
        ElementKind::Shape { rect, .. } => Some((rect.w - 20.0).max(1.0)),
        ElementKind::Text { .. } if *fixed_width => Some(text_rect.w),
        _ => None,
    };
    let layout = layout_text(text, style.font_size, wrap, &ApproxMeasurer);
    let w = if matches!(element.kind, ElementKind::Shape { .. }) {
        // The browser measures the real font. Give it the owner's complete
        // text area rather than the heuristic's narrower longest-line width.
        (element.bounds().w - 20.0).max(1.0)
    } else if *fixed_width {
        text_rect.w
    } else if matches!(
        element.kind,
        ElementKind::Arrow { .. } | ElementKind::Glyph { .. }
    ) {
        layout.width.max(1.0) + 20.0
    } else {
        layout.width.max(1.0)
    };
    let h = layout.height.max(style.font_size * 1.3);
    let bounds = element.bounds();
    let position = match &element.kind {
        ElementKind::Glyph { .. } => Point::new(bounds.center().x, bounds.max_y() + 10.0 + h / 2.0),
        ElementKind::Arrow {
            start,
            end,
            routing,
            waypoints,
            ..
        } => {
            let mut obstacles = ObstacleIndex::new();
            obstacles.rebuild(doc);
            let route = connector::route_avoiding(start, end, *routing, waypoints, doc, &obstacles);
            midpoint(&route).unwrap_or(bounds.center())
        }
        ElementKind::Text { .. } => Point::new(text_rect.x + w / 2.0, text_rect.y + h / 2.0),
        _ => bounds.center(),
    };
    if let ElementKind::Text { rect, .. } = &mut doc.element_mut(&label).unwrap().kind {
        *rect = Rect::new(position.x - w / 2.0, position.y - h / 2.0, w, h);
    }
}

fn midpoint(points: &[Point]) -> Option<Point> {
    let mut remaining = points.windows(2).map(|p| p[0].distance(p[1])).sum::<f64>() / 2.0;
    for pair in points.windows(2) {
        let distance = pair[0].distance(pair[1]);
        if remaining <= distance && distance > 0.0 {
            let t = remaining / distance;
            return Some(Point::new(
                pair[0].x + (pair[1].x - pair[0].x) * t,
                pair[0].y + (pair[1].y - pair[0].y) * t,
            ));
        }
        remaining -= distance;
    }
    points.first().copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Endpoint, ShapeKind, Style};

    fn shape(doc: &mut Document, x: f64) -> String {
        doc.add(
            ElementKind::Shape {
                shape: ShapeKind::Rectangle,
                rect: Rect::new(x, 0.0, 160.0, 100.0),
                style: Style::default(),
                bound_text_id: None,
            },
            1,
        )
    }

    fn connect(doc: &mut Document, from: &str, to: &str) -> String {
        doc.add(
            ElementKind::Arrow {
                start: Endpoint {
                    x: 160.0,
                    y: 50.0,
                    binding: Some(Binding {
                        element_id: from.into(),
                        focus: None,
                    }),
                },
                end: Endpoint {
                    x: 300.0,
                    y: 50.0,
                    binding: Some(Binding {
                        element_id: to.into(),
                        focus: None,
                    }),
                },
                routing: crate::Routing::Elbow,
                start_arrowhead: false,
                end_arrowhead: true,
                style: Style::default(),
                waypoints: Vec::new(),
                label_id: None,
            },
            1,
        )
    }

    #[test]
    fn rename_resize_and_move_preserve_owner_and_label_identity() {
        let mut doc = Document::new();
        let id = shape(&mut doc, 0.0);
        update_element(
            &mut doc,
            &id,
            &ElementUpdate {
                text: Some("Worker".into()),
                ..Default::default()
            },
            2,
        )
        .unwrap();
        let label = owned_label(&doc, &id).unwrap();
        let before = doc.clone();
        let deltas = update_element(
            &mut doc,
            &id,
            &ElementUpdate {
                text: Some("At-least-once worker".into()),
                x: Some(200.0),
                w: Some(240.0),
                style: Some(StylePatch {
                    font_size: Some(24.0),
                    ..Default::default()
                }),
                ..Default::default()
            },
            3,
        )
        .unwrap();
        assert_eq!(owned_label(&doc, &id), Some(label.clone()));
        assert_eq!(
            doc.element(&label).unwrap().bounds().center(),
            doc.element(&id).unwrap().bounds().center()
        );
        assert_eq!(
            crate::describe::text_of(&doc, &label).unwrap(),
            "At-least-once worker"
        );
        for delta in deltas.iter().rev() {
            doc.apply(&delta.invert()).unwrap();
        }
        assert_eq!(
            doc, before,
            "returned deltas reverse the complete semantic edit"
        );
    }

    #[test]
    fn moving_frame_carries_children_and_reroutes_labeled_connector() {
        let mut doc = Document::new();
        let frame = doc.add(
            ElementKind::Frame {
                rect: Rect::new(-20.0, -20.0, 240.0, 150.0),
                name: "Workers".into(),
            },
            1,
        );
        let a = shape(&mut doc, 0.0);
        let b = shape(&mut doc, 400.0);
        doc.element_mut(&a).unwrap().frame_id = Some(frame.clone());
        update_element(
            &mut doc,
            &a,
            &ElementUpdate {
                text: Some("A".into()),
                ..Default::default()
            },
            2,
        )
        .unwrap();
        let arrow = connect(&mut doc, &a, &b);
        update_element(
            &mut doc,
            &arrow,
            &ElementUpdate {
                text: Some("jobs".into()),
                ..Default::default()
            },
            3,
        )
        .unwrap();
        let old_arrow = doc.element(&arrow).unwrap().clone();
        let old_label = doc
            .element(&owned_label(&doc, &arrow).unwrap())
            .unwrap()
            .bounds();
        update_element(
            &mut doc,
            &frame,
            &ElementUpdate {
                y: Some(180.0),
                ..Default::default()
            },
            4,
        )
        .unwrap();
        assert_eq!(doc.element(&a).unwrap().bounds().y, 200.0);
        assert_eq!(doc.element(&b).unwrap().bounds().y, 0.0);
        assert_ne!(doc.element(&arrow).unwrap().kind, old_arrow.kind);
        assert_ne!(
            doc.element(&owned_label(&doc, &arrow).unwrap())
                .unwrap()
                .bounds(),
            old_label
        );
        assert_eq!(
            doc.element(&owned_label(&doc, &a).unwrap())
                .unwrap()
                .bounds()
                .center(),
            doc.element(&a).unwrap().bounds().center()
        );
    }

    #[test]
    fn reconnect_preserves_connector_and_reflows_its_label() {
        let mut doc = Document::new();
        let a = shape(&mut doc, 0.0);
        let b = shape(&mut doc, 300.0);
        let c = shape(&mut doc, 700.0);
        let arrow = connect(&mut doc, &a, &b);
        update_element(
            &mut doc,
            &arrow,
            &ElementUpdate {
                text: Some("ack".into()),
                ..Default::default()
            },
            2,
        )
        .unwrap();
        let label = owned_label(&doc, &arrow).unwrap();
        let old = doc.element(&label).unwrap().bounds();
        update_element(
            &mut doc,
            &arrow,
            &ElementUpdate {
                to: Some(c.clone()),
                ..Default::default()
            },
            3,
        )
        .unwrap();
        assert_eq!(owned_label(&doc, &arrow), Some(label.clone()));
        let ElementKind::Arrow { end, .. } = &doc.element(&arrow).unwrap().kind else {
            panic!()
        };
        assert_eq!(end.binding.as_ref().unwrap().element_id, c);
        assert_ne!(doc.element(&label).unwrap().bounds(), old);
    }

    #[test]
    fn invalid_changes_are_atomic_and_frame_cycles_are_rejected() {
        let mut doc = Document::new();
        let frame = doc.add(
            ElementKind::Frame {
                rect: Rect::new(0.0, 0.0, 400.0, 200.0),
                name: "A".into(),
            },
            1,
        );
        let child = doc.add(
            ElementKind::Frame {
                rect: Rect::new(20.0, 20.0, 200.0, 100.0),
                name: "B".into(),
            },
            1,
        );
        doc.element_mut(&child).unwrap().frame_id = Some(frame.clone());
        let before = doc.clone();
        for update in [
            ElementUpdate {
                frame: Some(Some(child)),
                ..Default::default()
            },
            ElementUpdate {
                x: Some(f64::NAN),
                ..Default::default()
            },
            ElementUpdate {
                x: Some(123.0),
                style: Some(StylePatch::default()),
                ..Default::default()
            },
        ] {
            assert!(update_element(&mut doc, &frame, &update, 2).is_err());
            assert_eq!(doc, before);
        }
    }
}
