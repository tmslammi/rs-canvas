//! The explicit ownership and connection graph behind the compact element
//! lines.
//!
//! The normal read output implies relationships through `in=`, labels and
//! `from>to`. This module makes them explicit for an opt-in read mode:
//!
//! ```text
//! relations: 1 frame, 2 labels, 1 arrow, 1 stack
//! frame e1 "VPC": e2 e3
//! label e4 -> e2
//! container e5 -> e3
//! arrow e6: e2 -> e3 "reads"
//! stack e7
//! ```
//!
//! Frames list their live direct children in document order. A text with a
//! matching owner link is a `label`; a text that only has a `container_id`
//! (for example imported content) is a `container`. Arrows show both
//! endpoints, their label and any non-default routing. Sticky stacks are
//! reported as roots even though the document model has no stack membership
//! field (notes pulled from a stack are ordinary sticky notes).
//!
//! References to deleted or missing elements are not silently dropped: they
//! become `dangling <handle> <relation> -> <missing>` lines. Scoped reads can
//! limit the graph to some elements plus their direct owners and children.
//! Graph construction and line rendering share the same [`IdMap`] handles the
//! element lines use.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::describe::IdMap;
use crate::model::{Document, Element, ElementId, ElementKind, Endpoint, Routing};

/// The relationship section never grows past this many relation lines.
pub const MAX_RELATION_LINES: usize = 100;

/// How a text element belongs to another element.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TextRelationKind {
    /// The owner links back to the text (`bound_text_id`, `caption_id` or
    /// `label_id`).
    Label,
    /// The text has `container_id` but the owner has no matching link.
    Container,
}

impl TextRelationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Label => "label",
            Self::Container => "container",
        }
    }
}

/// One end of an arrow as the relation view sees it.
#[derive(Clone, Debug, PartialEq)]
pub enum ArrowEnd {
    /// An element handle, whether or not the target still exists.
    Bound(ElementId),
    /// A free world point.
    Free { x: f64, y: f64 },
}

/// A frame and its live direct children, in document order.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameRelation {
    pub frame: ElementId,
    pub name: String,
    pub children: Vec<ElementId>,
}

/// A text element and its owning element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextRelation {
    pub text: ElementId,
    pub owner: ElementId,
    pub kind: TextRelationKind,
}

/// A connector with its endpoints, label and routing.
#[derive(Clone, Debug, PartialEq)]
pub struct ArrowRelation {
    pub arrow: ElementId,
    pub start: ArrowEnd,
    pub end: ArrowEnd,
    pub routing: Routing,
    pub label: Option<String>,
}

/// A sticky stack. `children` is empty while the model has no stack membership.
#[derive(Clone, Debug, PartialEq)]
pub struct StackRelation {
    pub stack: ElementId,
    pub children: Vec<ElementId>,
}

/// Which reference broke.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DanglingKind {
    Frame,
    Label,
    Container,
    Start,
    End,
}

impl DanglingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Frame => "frame",
            Self::Label => "label",
            Self::Container => "container",
            Self::Start => "start",
            Self::End => "end",
        }
    }
}

/// A reference to an element that is missing or deleted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DanglingRelation {
    pub source: ElementId,
    pub kind: DanglingKind,
    pub target: ElementId,
}

/// The relationship graph of a document, or of one scope within it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RelationGraph {
    pub frames: Vec<FrameRelation>,
    pub texts: Vec<TextRelation>,
    pub arrows: Vec<ArrowRelation>,
    pub stacks: Vec<StackRelation>,
    pub dangling: Vec<DanglingRelation>,
}

/// Counts for the relation header, before the line cap is applied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelationCounts {
    pub frames: usize,
    pub labels: usize,
    pub arrows: usize,
    pub stacks: usize,
    pub dangling: usize,
}

impl RelationGraph {
    /// Build the whole-board graph, or the graph touching `scope` plus its
    /// direct owners and children when a scope is given.
    pub fn build(doc: &Document, scope: Option<&BTreeSet<ElementId>>) -> Self {
        let live = doc.ordered_live();
        let live_by_id: BTreeMap<&str, &Element> = live
            .iter()
            .map(|element| (element.id.as_str(), *element))
            .collect();
        let order: BTreeMap<&str, usize> = live
            .iter()
            .enumerate()
            .map(|(index, element)| (element.id.as_str(), index))
            .collect();
        let effective = scope.map(|seed| effective_scope(doc, seed));
        let in_scope = |id: &str| effective.as_ref().is_none_or(|ids| ids.contains(id));

        let mut graph = RelationGraph::default();
        let mut seen_dangling = BTreeSet::new();

        // Direct frame membership, and children whose frame is gone.
        let mut children_by_frame: BTreeMap<&str, Vec<ElementId>> = BTreeMap::new();
        for element in &live {
            let Some(frame_id) = &element.frame_id else {
                continue;
            };
            if live_by_id.contains_key(frame_id.as_str()) {
                children_by_frame
                    .entry(frame_id.as_str())
                    .or_default()
                    .push(element.id.clone());
            } else if in_scope(&element.id) {
                push_dangling(
                    &mut graph.dangling,
                    &mut seen_dangling,
                    &element.id,
                    DanglingKind::Frame,
                    frame_id,
                );
            }
        }

        // Ownership edges from both sides, de-duplicated. A matching owner
        // link wins, so each text/owner pair appears exactly once.
        let mut text_edges: BTreeMap<(ElementId, ElementId), TextRelationKind> = BTreeMap::new();
        for element in &live {
            let Some(label) = declared_label(element) else {
                continue;
            };
            if doc
                .element(label)
                .is_some_and(|target| !target.is_deleted && target.is_text())
            {
                text_edges
                    .entry((label.clone(), element.id.clone()))
                    .or_insert(TextRelationKind::Label);
            } else if in_scope(&element.id) {
                push_dangling(
                    &mut graph.dangling,
                    &mut seen_dangling,
                    &element.id,
                    DanglingKind::Label,
                    label,
                );
            }
        }
        for element in &live {
            let ElementKind::Text {
                container_id: Some(owner),
                ..
            } = &element.kind
            else {
                continue;
            };
            if live_by_id.contains_key(owner.as_str()) {
                text_edges
                    .entry((element.id.clone(), owner.clone()))
                    .or_insert(TextRelationKind::Container);
            } else if in_scope(&element.id) {
                push_dangling(
                    &mut graph.dangling,
                    &mut seen_dangling,
                    &element.id,
                    DanglingKind::Container,
                    owner,
                );
            }
        }
        graph.texts = text_edges
            .into_iter()
            .filter(|((text, owner), _)| in_scope(text) || in_scope(owner))
            .map(|((text, owner), kind)| TextRelation { text, owner, kind })
            .collect();
        graph.texts.sort_by_key(|relation| {
            (
                order
                    .get(relation.text.as_str())
                    .copied()
                    .unwrap_or(usize::MAX),
                order
                    .get(relation.owner.as_str())
                    .copied()
                    .unwrap_or(usize::MAX),
            )
        });

        for element in &live {
            if !in_scope(&element.id) {
                continue;
            }
            match &element.kind {
                ElementKind::Frame { name, .. } => graph.frames.push(FrameRelation {
                    frame: element.id.clone(),
                    name: name.clone(),
                    children: children_by_frame
                        .get(element.id.as_str())
                        .cloned()
                        .unwrap_or_default(),
                }),
                ElementKind::Arrow {
                    start,
                    end,
                    routing,
                    label_id,
                    ..
                } => {
                    for (kind, endpoint) in [(DanglingKind::Start, start), (DanglingKind::End, end)]
                    {
                        if let Some(binding) = &endpoint.binding {
                            if !live_by_id.contains_key(binding.element_id.as_str()) {
                                push_dangling(
                                    &mut graph.dangling,
                                    &mut seen_dangling,
                                    &element.id,
                                    kind,
                                    &binding.element_id,
                                );
                            }
                        }
                    }
                    graph.arrows.push(ArrowRelation {
                        arrow: element.id.clone(),
                        start: arrow_end(start),
                        end: arrow_end(end),
                        routing: *routing,
                        label: label_id.as_deref().and_then(|id| live_text_of(doc, id)),
                    });
                }
                ElementKind::StickyStack { .. } => graph.stacks.push(StackRelation {
                    stack: element.id.clone(),
                    children: Vec::new(),
                }),
                _ => {}
            }
        }

        graph.dangling.sort_by(|a, b| {
            let rank = |id: &str| order.get(id).copied().unwrap_or(usize::MAX);
            rank(&a.source)
                .cmp(&rank(&b.source))
                .then_with(|| a.kind.cmp(&b.kind))
                .then_with(|| a.target.cmp(&b.target))
        });
        graph
    }

    /// Counts for the header, before the line cap.
    pub fn counts(&self) -> RelationCounts {
        RelationCounts {
            frames: self.frames.len(),
            labels: self.texts.len(),
            arrows: self.arrows.len(),
            stacks: self.stacks.len(),
            dangling: self.dangling.len(),
        }
    }

    /// Render the relation section, capped at [`MAX_RELATION_LINES`].
    pub fn render(&self, ids: &mut dyn IdMap) -> String {
        self.render_capped(ids, MAX_RELATION_LINES)
    }

    /// Render the relation section with an explicit line cap.
    pub fn render_capped(&self, ids: &mut dyn IdMap, max_lines: usize) -> String {
        let counts = self.counts();
        let mut lines = Vec::new();
        for frame in &self.frames {
            let mut line = format!("frame {}", ids.map(&frame.frame));
            if !frame.name.is_empty() {
                let _ = write!(line, " {:?}", frame.name);
            }
            append_handles(&mut line, &frame.children, ids);
            lines.push(line);
        }
        for text in &self.texts {
            lines.push(format!(
                "{} {} -> {}",
                text.kind.as_str(),
                ids.map(&text.text),
                ids.map(&text.owner)
            ));
        }
        for arrow in &self.arrows {
            let mut line = format!(
                "arrow {}: {} -> {}",
                ids.map(&arrow.arrow),
                render_end(&arrow.start, ids),
                render_end(&arrow.end, ids)
            );
            if arrow.routing != Routing::Elbow {
                let _ = write!(line, " {}", routing_name(arrow.routing));
            }
            if let Some(label) = &arrow.label {
                let _ = write!(line, " {label:?}");
            }
            lines.push(line);
        }
        for stack in &self.stacks {
            let mut line = format!("stack {}", ids.map(&stack.stack));
            append_handles(&mut line, &stack.children, ids);
            lines.push(line);
        }
        for dangling in &self.dangling {
            lines.push(format!(
                "dangling {} {} -> {}",
                ids.map(&dangling.source),
                dangling.kind.as_str(),
                ids.map(&dangling.target)
            ));
        }

        let mut out = format!("relations: {}\n", summary(counts));
        for line in lines.iter().take(max_lines) {
            out.push_str(line);
            out.push('\n');
        }
        if lines.len() > max_lines {
            let _ = writeln!(out, "+{} more", lines.len() - max_lines);
        }
        out
    }
}

fn effective_scope(doc: &Document, seed: &BTreeSet<ElementId>) -> BTreeSet<ElementId> {
    let live = doc.ordered_live();
    let mut ids: BTreeSet<ElementId> = seed
        .iter()
        .filter(|id| doc.element(id).is_some_and(|element| !element.is_deleted))
        .cloned()
        .collect();
    let seed_ids = ids.clone();
    for id in &seed_ids {
        let Some(element) = doc.element(id) else {
            continue;
        };
        if let Some(frame_id) = &element.frame_id {
            if doc
                .element(frame_id)
                .is_some_and(|frame| !frame.is_deleted && frame.is_frame())
            {
                ids.insert(frame_id.clone());
            }
        }
        if element.is_frame() {
            for child in &live {
                if child.frame_id.as_deref() == Some(id.as_str()) {
                    ids.insert(child.id.clone());
                }
            }
        }
        if let Some(label) = declared_label(element) {
            if doc
                .element(label)
                .is_some_and(|text| !text.is_deleted && text.is_text())
            {
                ids.insert(label.clone());
            }
        }
        for child in &live {
            if matches!(
                &child.kind,
                ElementKind::Text {
                    container_id: Some(owner),
                    ..
                } if owner == id
            ) {
                ids.insert(child.id.clone());
            }
        }
        if let ElementKind::Text {
            container_id: Some(owner),
            ..
        } = &element.kind
        {
            if doc
                .element(owner)
                .is_some_and(|container| !container.is_deleted)
            {
                ids.insert(owner.clone());
            }
        }
    }
    let direct = ids.clone();
    for element in &live {
        let ElementKind::Arrow { start, end, .. } = &element.kind else {
            continue;
        };
        let touches_scope = [start, end].iter().any(|endpoint| {
            endpoint
                .binding
                .as_ref()
                .is_some_and(|binding| direct.contains(&binding.element_id))
        });
        if touches_scope {
            ids.insert(element.id.clone());
        }
    }
    ids
}

fn declared_label(element: &Element) -> Option<&ElementId> {
    match &element.kind {
        ElementKind::Shape { bound_text_id, .. } => bound_text_id.as_ref(),
        ElementKind::Glyph { caption_id, .. } => caption_id.as_ref(),
        ElementKind::Arrow { label_id, .. } => label_id.as_ref(),
        _ => None,
    }
}

fn live_text_of(doc: &Document, id: &str) -> Option<String> {
    match &doc.element(id).filter(|element| !element.is_deleted)?.kind {
        ElementKind::Text { text, .. } => Some(text.clone()),
        _ => None,
    }
}

fn arrow_end(endpoint: &Endpoint) -> ArrowEnd {
    match &endpoint.binding {
        Some(binding) => ArrowEnd::Bound(binding.element_id.clone()),
        None => ArrowEnd::Free {
            x: endpoint.x,
            y: endpoint.y,
        },
    }
}

fn push_dangling(
    dangling: &mut Vec<DanglingRelation>,
    seen: &mut BTreeSet<(ElementId, DanglingKind, ElementId)>,
    source: &str,
    kind: DanglingKind,
    target: &str,
) {
    let key = (source.to_string(), kind, target.to_string());
    if seen.insert(key) {
        dangling.push(DanglingRelation {
            source: source.to_string(),
            kind,
            target: target.to_string(),
        });
    }
}

fn append_handles(line: &mut String, children: &[ElementId], ids: &mut dyn IdMap) {
    if children.is_empty() {
        return;
    }
    line.push(':');
    for child in children {
        line.push(' ');
        line.push_str(&ids.map(child));
    }
}

fn render_end(end: &ArrowEnd, ids: &mut dyn IdMap) -> String {
    match end {
        ArrowEnd::Bound(id) => ids.map(id),
        ArrowEnd::Free { x, y } => format!("({x:.0},{y:.0})"),
    }
}

fn routing_name(routing: Routing) -> &'static str {
    match routing {
        Routing::Straight => "straight",
        Routing::Elbow => "elbow",
        Routing::Curved => "curved",
    }
}

fn summary(counts: RelationCounts) -> String {
    let mut parts = Vec::new();
    if counts.frames > 0 {
        parts.push(counted(counts.frames, "frame", "frames"));
    }
    if counts.labels > 0 {
        parts.push(counted(counts.labels, "label", "labels"));
    }
    if counts.arrows > 0 {
        parts.push(counted(counts.arrows, "arrow", "arrows"));
    }
    if counts.stacks > 0 {
        parts.push(counted(counts.stacks, "stack", "stacks"));
    }
    if counts.dangling > 0 {
        parts.push(counted(counts.dangling, "dangling", "dangling"));
    }
    if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join(", ")
    }
}

fn counted(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Point, Rect};
    use crate::model::{Binding, Endpoint, ShapeKind, Style, TextAlign};
    use std::collections::HashMap;

    struct Short(HashMap<String, String>);

    impl IdMap for Short {
        fn map(&mut self, id: &str) -> String {
            if let Some(handle) = self.0.get(id) {
                return handle.clone();
            }
            let handle = format!("e{}", self.0.len() + 1);
            self.0.insert(id.to_string(), handle.clone());
            handle
        }
    }

    fn insert(doc: &mut Document, id: &str, index: &str, kind: ElementKind) {
        let mut element = Element::new(kind, index.to_string(), 1);
        element.id = id.to_string();
        doc.insert(element);
    }

    fn shape(x: f64) -> ElementKind {
        ElementKind::Shape {
            shape: ShapeKind::Rectangle,
            rect: Rect::new(x, 0.0, 100.0, 60.0),
            style: Style::default(),
            bound_text_id: None,
        }
    }

    fn text(text: &str, container: Option<&str>) -> ElementKind {
        ElementKind::Text {
            rect: Rect::new(0.0, 0.0, 40.0, 20.0),
            text: text.into(),
            align: TextAlign::Center,
            fixed_width: false,
            container_id: container.map(str::to_string),
            style: Style::default(),
        }
    }

    fn arrow(
        start: Endpoint,
        end: Endpoint,
        routing: Routing,
        label_id: Option<&str>,
    ) -> ElementKind {
        ElementKind::Arrow {
            start,
            end,
            routing,
            start_arrowhead: false,
            end_arrowhead: true,
            style: Style::default(),
            waypoints: Vec::new(),
            label_id: label_id.map(str::to_string),
        }
    }

    fn bound(id: &str) -> Endpoint {
        Endpoint {
            x: 0.0,
            y: 0.0,
            binding: Some(Binding {
                element_id: id.into(),
                focus: Some(Point::new(0.5, 0.5)),
            }),
        }
    }

    fn frame(name: &str) -> ElementKind {
        ElementKind::Frame {
            rect: Rect::new(0.0, 0.0, 400.0, 300.0),
            name: name.into(),
        }
    }

    fn board() -> Document {
        let mut doc = Document::new();
        insert(&mut doc, "s1", "a2", shape(0.0));
        insert(&mut doc, "s2", "a3", shape(180.0));
        insert(&mut doc, "f1", "a1", frame("VPC"));
        insert(&mut doc, "t1", "a5", text("API", Some("s1")));
        insert(&mut doc, "t2", "a6", text("extra", Some("s2")));
        insert(&mut doc, "t3", "a8", text("reads", Some("a1")));
        insert(
            &mut doc,
            "a1",
            "a7",
            arrow(bound("s1"), bound("s2"), Routing::Straight, Some("t3")),
        );
        insert(
            &mut doc,
            "d1",
            "aa",
            arrow(
                bound("s1"),
                Endpoint {
                    x: 0.0,
                    y: 0.0,
                    binding: Some(Binding {
                        element_id: "ghost".into(),
                        focus: None,
                    }),
                },
                Routing::Elbow,
                None,
            ),
        );
        insert(
            &mut doc,
            "st",
            "a9",
            ElementKind::StickyStack {
                rect: Rect::new(500.0, 0.0, 140.0, 140.0),
                style: Style::default(),
            },
        );
        doc.element_mut("s1").unwrap().frame_id = Some("f1".into());
        doc.element_mut("s2").unwrap().frame_id = Some("f1".into());
        if let ElementKind::Shape { bound_text_id, .. } = &mut doc.element_mut("s1").unwrap().kind {
            *bound_text_id = Some("t1".into());
        }
        if let ElementKind::Arrow { label_id, .. } = &mut doc.element_mut("a1").unwrap().kind {
            *label_id = Some("t3".into());
        }
        doc
    }

    #[test]
    fn graph_covers_frames_labels_containers_arrows_stacks_and_dangling_refs() {
        let doc = board();
        let graph = RelationGraph::build(&doc, None);

        assert_eq!(graph.counts().frames, 1);
        assert_eq!(graph.counts().labels, 3);
        assert_eq!(graph.counts().arrows, 2);
        assert_eq!(graph.counts().stacks, 1);
        assert_eq!(graph.counts().dangling, 1);
        assert_eq!(graph.frames[0].children, vec!["s1", "s2"]);
        assert_eq!(graph.frames[0].name, "VPC");

        let kinds: Vec<_> = graph
            .texts
            .iter()
            .map(|text| (text.text.as_str(), text.owner.as_str(), text.kind))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("t1", "s1", TextRelationKind::Label),
                ("t2", "s2", TextRelationKind::Container),
                ("t3", "a1", TextRelationKind::Label),
            ]
        );

        let first = &graph.arrows[0];
        assert_eq!(first.arrow, "a1");
        assert_eq!(first.start, ArrowEnd::Bound("s1".into()));
        assert_eq!(first.end, ArrowEnd::Bound("s2".into()));
        assert_eq!(first.routing, Routing::Straight);
        assert_eq!(first.label.as_deref(), Some("reads"));

        assert_eq!(graph.stacks[0].stack, "st");
        assert!(graph.stacks[0].children.is_empty());
        assert_eq!(
            graph.dangling[0],
            DanglingRelation {
                source: "d1".into(),
                kind: DanglingKind::End,
                target: "ghost".into(),
            }
        );
    }

    #[test]
    fn rendering_uses_handles_for_every_relation() {
        let doc = board();
        let graph = RelationGraph::build(&doc, None);
        let mut ids = Short(HashMap::new());
        let rendered = graph.render(&mut ids);

        assert_eq!(
            rendered,
            concat!(
                "relations: 1 frame, 3 labels, 2 arrows, 1 stack, 1 dangling\n",
                "frame e1 \"VPC\": e2 e3\n",
                "label e4 -> e2\n",
                "container e5 -> e3\n",
                "label e6 -> e7\n",
                "arrow e7: e2 -> e3 straight \"reads\"\n",
                "arrow e8: e2 -> e9\n",
                "stack e10\n",
                "dangling e8 end -> e9\n",
            )
        );
    }

    #[test]
    fn scoped_graph_adds_direct_owners_children_and_incident_arrows() {
        let mut doc = Document::new();
        insert(&mut doc, "s1", "a2", shape(0.0));
        insert(&mut doc, "s2", "a3", shape(180.0));
        insert(&mut doc, "outside", "a6", shape(400.0));
        insert(&mut doc, "f1", "a1", frame("VPC"));
        insert(&mut doc, "t1", "a4", text("one", Some("s1")));
        insert(&mut doc, "t2", "a5", text("two", Some("s2")));
        insert(
            &mut doc,
            "x1",
            "a7",
            arrow(bound("s1"), bound("outside"), Routing::Elbow, None),
        );
        doc.element_mut("s1").unwrap().frame_id = Some("f1".into());
        doc.element_mut("s2").unwrap().frame_id = Some("f1".into());
        if let ElementKind::Shape { bound_text_id, .. } = &mut doc.element_mut("s1").unwrap().kind {
            *bound_text_id = Some("t1".into());
        }

        let scope = BTreeSet::from(["s1".to_string()]);
        let graph = RelationGraph::build(&doc, Some(&scope));
        let mut ids = Short(HashMap::new());
        assert_eq!(
            graph.render(&mut ids),
            concat!(
                "relations: 1 frame, 1 label, 1 arrow\n",
                "frame e1 \"VPC\": e2 e3\n",
                "label e4 -> e2\n",
                "arrow e5: e2 -> e6\n",
            )
        );
    }

    #[test]
    fn missing_and_deleted_targets_are_reported_not_dropped() {
        let mut doc = Document::new();
        insert(&mut doc, "f1", "a1", frame("Gone"));
        insert(&mut doc, "c1", "a2", shape(0.0));
        insert(&mut doc, "owner", "a3", shape(180.0));
        insert(
            &mut doc,
            "a1",
            "a4",
            arrow(
                Endpoint::free(0.0, 0.0),
                bound("deleted-shape"),
                Routing::Elbow,
                None,
            ),
        );
        doc.element_mut("f1").unwrap().is_deleted = true;
        doc.element_mut("c1").unwrap().frame_id = Some("f1".into());
        if let ElementKind::Shape { bound_text_id, .. } =
            &mut doc.element_mut("owner").unwrap().kind
        {
            *bound_text_id = Some("missing-label".into());
        }

        let graph = RelationGraph::build(&doc, None);
        assert!(graph.frames.is_empty());
        assert_eq!(
            graph
                .dangling
                .iter()
                .map(|d| (d.source.as_str(), d.kind, d.target.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("c1", DanglingKind::Frame, "f1"),
                ("owner", DanglingKind::Label, "missing-label"),
                ("a1", DanglingKind::End, "deleted-shape"),
            ]
        );
    }

    #[test]
    fn deleted_children_do_not_appear_in_a_frame() {
        let mut doc = Document::new();
        insert(&mut doc, "f1", "a1", frame("VPC"));
        insert(&mut doc, "c1", "a2", shape(0.0));
        doc.element_mut("c1").unwrap().frame_id = Some("f1".into());
        doc.element_mut("c1").unwrap().is_deleted = true;

        let graph = RelationGraph::build(&doc, None);
        assert_eq!(graph.frames[0].children, Vec::<ElementId>::new());
        assert!(graph.dangling.is_empty());
    }

    #[test]
    fn line_cap_reports_the_remainder() {
        let mut doc = Document::new();
        insert(&mut doc, "f1", "a1", frame("one"));
        insert(&mut doc, "f2", "a2", frame("two"));
        insert(&mut doc, "f3", "a3", frame("three"));

        let graph = RelationGraph::build(&doc, None);
        let mut ids = Short(HashMap::new());
        assert_eq!(
            graph.render_capped(&mut ids, 2),
            concat!(
                "relations: 3 frames\n",
                "frame e1 \"one\"\n",
                "frame e2 \"two\"\n",
                "+1 more\n",
            )
        );
    }

    #[test]
    fn free_arrow_ends_render_as_points() {
        let mut doc = Document::new();
        insert(&mut doc, "s1", "a2", shape(0.0));
        insert(
            &mut doc,
            "a1",
            "a1",
            arrow(
                Endpoint::free(10.4, 20.6),
                bound("s1"),
                Routing::Curved,
                None,
            ),
        );

        let graph = RelationGraph::build(&doc, None);
        let mut ids = Short(HashMap::new());
        assert!(graph
            .render(&mut ids)
            .contains("arrow e1: (10,21) -> e2 curved"));
    }
}
