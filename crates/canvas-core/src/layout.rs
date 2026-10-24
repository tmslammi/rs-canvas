//! Computed layout facts for the agent: overlaps, near misses, containment
//! defects, broken references and alignment, derived purely from bounds.
//!
//! [`analyze`] covers the whole document; [`analyze_scoped`] covers a matched
//! set plus its owners. [`render`] names elements through an [`IdMap`] and
//! caps the output, so `canvas_read(layout: true)` can append facts after its
//! element lines without changing the default read.
//!
//! Connectors are skipped by the pairwise area facts (overlap, near, align):
//! their bounds are routing envelopes, not the ink they draw, and would report
//! every shape inside an elbow's rectangle. They still report `dangling`.
//! Frame ancestry, text containers, bound labels and sticky-stack notes are
//! owned relationships, not accidental overlaps. Text overflow checks shape
//! labels only: glyph captions and connector labels are laid out outside their
//! owners by design, so their bounds are not a typography defect.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;

use crate::describe::IdMap;
use crate::geometry::Rect;
use crate::model::{Document, Element, ElementKind};

/// Facts kept in the report; the rest are counted but not rendered.
pub const MAX_FACTS: usize = 50;
/// Edge-to-edge distance at which two elements count as near.
pub const NEAR_GAP: f64 = 8.0;
/// Edge-to-edge distance at which two elements count as aligned.
pub const ALIGN_TOLERANCE: f64 = 2.0;

/// The facts computed for one read, in stable report order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LayoutReport {
    /// The leading facts, at most [`MAX_FACTS`].
    pub facts: Vec<LayoutFact>,
    /// How many facts were found in total, including truncated ones.
    pub total: usize,
}

/// One geometric observation about the live document.
#[derive(Clone, Debug, PartialEq)]
pub enum LayoutFact {
    Overlap {
        a: String,
        b: String,
        width: f64,
        height: f64,
    },
    Near {
        a: String,
        b: String,
        gap: f64,
    },
    Outside {
        id: String,
        frame: String,
        side: Side,
        amount: f64,
    },
    Orphan {
        id: String,
        owner: String,
        reference: OrphanRef,
        state: RefState,
    },
    Dangling {
        arrow: String,
        end: ArrowEnd,
        target: String,
        state: RefState,
    },
    Align {
        a: String,
        b: String,
        edges: Vec<AlignEdge>,
    },
    TextOverflow {
        text: String,
        container: String,
        side: Side,
        amount: f64,
    },
}

/// A rectangle edge that can be exceeded or shared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Top,
    Right,
    Bottom,
}

/// An edge two elements share within [`ALIGN_TOLERANCE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
    CentreX,
    CentreY,
}

/// A shared edge and `a`'s offset from `b` along it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AlignEdge {
    pub edge: Edge,
    pub offset: f64,
}

/// The reference an orphaned element is missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrphanRef {
    /// A text's `container_id`.
    Owner,
    /// A frame's `frame_id`.
    Frame,
    /// An owner's bound label/caption.
    Label,
}

/// Which arrow endpoint is dangling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrowEnd {
    Start,
    End,
}

/// Why a reference no longer resolves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefState {
    Missing,
    Deleted,
}

/// Analyze every live element in the document.
pub fn analyze(doc: &Document) -> LayoutReport {
    let ids: BTreeSet<String> = doc
        .ordered_live()
        .into_iter()
        .map(|element| element.id.clone())
        .collect();
    analyze_ids(doc, &ids)
}

/// Analyze a matched set plus the owners those elements depend on.
pub fn analyze_scoped(doc: &Document, matched: &BTreeSet<String>) -> LayoutReport {
    let mut ids = matched.clone();
    expand_owners(doc, &mut ids);
    analyze_ids(doc, &ids)
}

/// Render the compact `layout:` block, with handles from `ids`.
pub fn render(report: &LayoutReport, ids: &mut dyn IdMap) -> String {
    if report.total == 0 {
        return "layout: clean\n".to_string();
    }
    let mut out = format!(
        "layout: {} {}\n",
        report.total,
        if report.total == 1 { "fact" } else { "facts" }
    );
    for fact in &report.facts {
        out.push_str(&render_fact(fact, ids));
        out.push('\n');
    }
    if report.total > report.facts.len() {
        let _ = writeln!(out, "layout: +{} more", report.total - report.facts.len());
    }
    out
}

/// Collect facts up to the render cap while counting every one.
#[derive(Default)]
struct CappedFacts {
    facts: Vec<LayoutFact>,
    total: usize,
}

impl CappedFacts {
    fn push(&mut self, fact: LayoutFact) {
        self.total += 1;
        if self.facts.len() < MAX_FACTS {
            self.facts.push(fact);
        }
    }
}

fn expand_owners(doc: &Document, ids: &mut BTreeSet<String>) {
    loop {
        let before = ids.len();
        let frontier: Vec<String> = ids.iter().cloned().collect();
        for id in frontier {
            let Some(element) = doc.element(&id).filter(|element| !element.is_deleted) else {
                continue;
            };
            if let Some(frame) = &element.frame_id {
                ids.insert(frame.clone());
            }
            if let ElementKind::Text {
                container_id: Some(container),
                ..
            } = &element.kind
            {
                ids.insert(container.clone());
            }
        }
        if ids.len() == before {
            break;
        }
    }
}

fn analyze_ids(doc: &Document, ids: &BTreeSet<String>) -> LayoutReport {
    let elements: Vec<&Element> = doc
        .ordered_live()
        .into_iter()
        .filter(|element| ids.contains(&element.id))
        .collect();
    let bounds: Vec<Rect> = elements.iter().map(|element| element.bounds()).collect();
    let related = related_map(doc, &elements);

    let mut overlaps = CappedFacts::default();
    let mut nears = CappedFacts::default();
    let mut aligns = CappedFacts::default();
    for (index, a) in elements.iter().enumerate() {
        let a_bounds = bounds[index];
        for (offset, b) in elements[index + 1..].iter().enumerate() {
            if pair_is_connector(a, b) || related_pair(a, b, &related) {
                continue;
            }
            let b_bounds = bounds[index + 1 + offset];
            if let Some((width, height)) = overlap(a_bounds, b_bounds) {
                overlaps.push(LayoutFact::Overlap {
                    a: a.id.clone(),
                    b: b.id.clone(),
                    width,
                    height,
                });
                continue;
            }
            let gap = gap(a_bounds, b_bounds);
            if gap <= NEAR_GAP {
                nears.push(LayoutFact::Near {
                    a: a.id.clone(),
                    b: b.id.clone(),
                    gap,
                });
            }
            let edges = aligned_edges(a_bounds, b_bounds);
            if !edges.is_empty() {
                aligns.push(LayoutFact::Align {
                    a: a.id.clone(),
                    b: b.id.clone(),
                    edges,
                });
            }
        }
    }

    let mut outside = CappedFacts::default();
    let mut orphans = CappedFacts::default();
    let mut dangling = CappedFacts::default();
    let mut overflow = CappedFacts::default();
    for element in &elements {
        if let Some(fact) = outside_fact(doc, element) {
            outside.push(fact);
        }
        for fact in orphan_facts(doc, element) {
            orphans.push(fact);
        }
        for fact in dangling_facts(doc, element) {
            dangling.push(fact);
        }
        if let Some(fact) = text_overflow_fact(doc, element) {
            overflow.push(fact);
        }
    }

    // Report order groups the facts by kind, so a truncated report always
    // starts with the most structural observations.
    let total = overlaps.total
        + nears.total
        + outside.total
        + orphans.total
        + dangling.total
        + aligns.total
        + overflow.total;
    let mut facts = Vec::with_capacity(total.min(MAX_FACTS));
    for group in [
        overlaps, nears, outside, orphans, dangling, aligns, overflow,
    ] {
        for fact in group.facts {
            if facts.len() == MAX_FACTS {
                break;
            }
            facts.push(fact);
        }
    }
    LayoutReport { facts, total }
}

fn pair_is_connector(a: &Element, b: &Element) -> bool {
    matches!(a.kind, ElementKind::Arrow { .. }) || matches!(b.kind, ElementKind::Arrow { .. })
}

/// Whether two elements are joined by ownership rather than accident.
fn related_pair(a: &Element, b: &Element, related: &HashMap<String, BTreeSet<String>>) -> bool {
    related
        .get(&a.id)
        .is_some_and(|owners| owners.contains(&b.id))
        || related
            .get(&b.id)
            .is_some_and(|owners| owners.contains(&a.id))
        || sticky_contents(a, b)
}

/// Direct owner and owned-label references, resolved once per read. Walking
/// the text-container and frame chain here keeps the pairwise loop free of
/// document lookups and allocations.
fn related_map(doc: &Document, elements: &[&Element]) -> HashMap<String, BTreeSet<String>> {
    let mut related: HashMap<String, BTreeSet<String>> = HashMap::with_capacity(elements.len());
    for element in elements {
        let owners = related.entry(element.id.clone()).or_default();
        let mut stack = vec![*element];
        let mut seen = BTreeSet::new();
        while let Some(current) = stack.pop() {
            if !seen.insert(current.id.as_str()) {
                continue;
            }
            if let Some(frame) = current.frame_id.as_deref() {
                owners.insert(frame.to_string());
                if let Some(parent) = doc.element(frame) {
                    stack.push(parent);
                }
            }
            if let ElementKind::Text {
                container_id: Some(container),
                ..
            } = &current.kind
            {
                owners.insert(container.clone());
                if let Some(owner) = doc.element(container) {
                    stack.push(owner);
                }
            }
            if let Some(label) = bound_label(current) {
                owners.insert(label.to_string());
            }
        }
    }
    related
}

fn bound_label(element: &Element) -> Option<&str> {
    match &element.kind {
        ElementKind::Shape { bound_text_id, .. } => bound_text_id.as_deref(),
        ElementKind::Glyph { caption_id, .. } => caption_id.as_deref(),
        ElementKind::Arrow { label_id, .. } => label_id.as_deref(),
        _ => None,
    }
}

/// A sticky note sitting on its source stack is a pile, not an overlap.
fn sticky_contents(a: &Element, b: &Element) -> bool {
    (a.is_sticky() && b.is_sticky_stack()) || (a.is_sticky_stack() && b.is_sticky())
}

fn overlap(a: Rect, b: Rect) -> Option<(f64, f64)> {
    let width = a.max_x().min(b.max_x()) - a.min_x().max(b.min_x());
    let height = a.max_y().min(b.max_y()) - a.min_y().max(b.min_y());
    (width > 0.0 && height > 0.0).then_some((width, height))
}

/// Euclidean edge-to-edge distance; zero when the rectangles touch.
fn gap(a: Rect, b: Rect) -> f64 {
    let dx = (b.min_x() - a.max_x()).max(a.min_x() - b.max_x()).max(0.0);
    let dy = (b.min_y() - a.max_y()).max(a.min_y() - b.max_y()).max(0.0);
    (dx * dx + dy * dy).sqrt()
}

fn aligned_edges(a: Rect, b: Rect) -> Vec<AlignEdge> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let a_center = a.center();
    let b_center = b.center();
    [
        (Edge::Left, a.min_x() - b.min_x()),
        (Edge::Right, a.max_x() - b.max_x()),
        (Edge::Top, a.min_y() - b.min_y()),
        (Edge::Bottom, a.max_y() - b.max_y()),
        (Edge::CentreX, a_center.x - b_center.x),
        (Edge::CentreY, a_center.y - b_center.y),
    ]
    .into_iter()
    .filter(|(_, offset)| offset.abs() <= ALIGN_TOLERANCE)
    .map(|(edge, offset)| AlignEdge { edge, offset })
    .collect()
}

fn outside_fact(doc: &Document, element: &Element) -> Option<LayoutFact> {
    let frame_id = element.frame_id.as_ref()?;
    let frame = doc
        .element(frame_id)
        .filter(|frame| !frame.is_deleted && frame.is_frame())?;
    let (side, amount) = overflow(element.bounds(), frame.bounds())?;
    Some(LayoutFact::Outside {
        id: element.id.clone(),
        frame: frame_id.clone(),
        side,
        amount,
    })
}

fn text_overflow_fact(doc: &Document, element: &Element) -> Option<LayoutFact> {
    let ElementKind::Text {
        container_id: Some(container_id),
        ..
    } = &element.kind
    else {
        return None;
    };
    let container = doc
        .element(container_id)
        .filter(|owner| !owner.is_deleted)?;
    // Glyph captions and connector labels intentionally live outside their
    // owner's bounds; only areal shape/sticky containers have a text area to
    // overflow.
    if !matches!(container.kind, ElementKind::Shape { .. }) {
        return None;
    }
    let (side, amount) = overflow(element.bounds(), container.bounds())?;
    Some(LayoutFact::TextOverflow {
        text: element.id.clone(),
        container: container_id.clone(),
        side,
        amount,
    })
}

/// The furthest side on which `inner` escapes `outer`, if any.
fn overflow(inner: Rect, outer: Rect) -> Option<(Side, f64)> {
    let mut worst: Option<(Side, f64)> = None;
    for (side, amount) in [
        (Side::Left, outer.min_x() - inner.min_x()),
        (Side::Top, outer.min_y() - inner.min_y()),
        (Side::Right, inner.max_x() - outer.max_x()),
        (Side::Bottom, inner.max_y() - outer.max_y()),
    ] {
        if amount > 0.0 && worst.is_none_or(|(_, best)| amount > best + 1e-9) {
            worst = Some((side, amount));
        }
    }
    worst
}

fn orphan_facts(doc: &Document, element: &Element) -> Vec<LayoutFact> {
    let mut facts = Vec::new();
    if let Some(frame) = &element.frame_id {
        if let Some(state) = ref_state(doc, frame) {
            facts.push(LayoutFact::Orphan {
                id: element.id.clone(),
                owner: frame.clone(),
                reference: OrphanRef::Frame,
                state,
            });
        }
    }
    if let ElementKind::Text {
        container_id: Some(container),
        ..
    } = &element.kind
    {
        if let Some(state) = ref_state(doc, container) {
            facts.push(LayoutFact::Orphan {
                id: element.id.clone(),
                owner: container.clone(),
                reference: OrphanRef::Owner,
                state,
            });
        }
    }
    if let Some(label) = crate::commands::owned_label(doc, &element.id) {
        if let Some(state) = ref_state(doc, &label) {
            facts.push(LayoutFact::Orphan {
                id: element.id.clone(),
                owner: label,
                reference: OrphanRef::Label,
                state,
            });
        }
    }
    facts
}

fn dangling_facts(doc: &Document, element: &Element) -> Vec<LayoutFact> {
    let ElementKind::Arrow { start, end, .. } = &element.kind else {
        return Vec::new();
    };
    let mut facts = Vec::new();
    for (arrow_end, endpoint) in [(ArrowEnd::Start, start), (ArrowEnd::End, end)] {
        let Some(binding) = &endpoint.binding else {
            continue;
        };
        if let Some(state) = ref_state(doc, &binding.element_id) {
            facts.push(LayoutFact::Dangling {
                arrow: element.id.clone(),
                end: arrow_end,
                target: binding.element_id.clone(),
                state,
            });
        }
    }
    facts
}

fn ref_state(doc: &Document, id: &str) -> Option<RefState> {
    match doc.element(id) {
        None => Some(RefState::Missing),
        Some(element) if element.is_deleted => Some(RefState::Deleted),
        Some(_) => None,
    }
}

fn render_fact(fact: &LayoutFact, ids: &mut dyn IdMap) -> String {
    match fact {
        LayoutFact::Overlap {
            a,
            b,
            width,
            height,
        } => format!(
            "overlap {} {} {}x{}",
            ids.map(a),
            ids.map(b),
            number(*width),
            number(*height)
        ),
        LayoutFact::Near { a, b, gap } => {
            format!("near {} {} gap={}", ids.map(a), ids.map(b), number(*gap))
        }
        LayoutFact::Outside {
            id,
            frame,
            side,
            amount,
        } => format!(
            "outside {} in={} {}={}",
            ids.map(id),
            ids.map(frame),
            side.name(),
            number(*amount)
        ),
        LayoutFact::Orphan {
            id,
            owner,
            reference,
            state,
        } => format!(
            "orphan {} {}={} ({})",
            ids.map(id),
            reference.name(),
            ids.map(owner),
            state.name()
        ),
        LayoutFact::Dangling {
            arrow,
            end,
            target,
            state,
        } => format!(
            "dangling {} {}->{} ({})",
            ids.map(arrow),
            end.name(),
            ids.map(target),
            state.name()
        ),
        LayoutFact::Align { a, b, edges } => {
            let edges: Vec<String> = edges
                .iter()
                .map(|edge| format!("{}={}", edge.edge.name(), number(edge.offset)))
                .collect();
            format!("align {} {} {}", ids.map(a), ids.map(b), edges.join(" "))
        }
        LayoutFact::TextOverflow {
            text,
            container,
            side,
            amount,
        } => format!(
            "text-overflow {} in={} {}={}",
            ids.map(text),
            ids.map(container),
            side.name(),
            number(*amount)
        ),
    }
}

/// One decimal when needed, whole numbers otherwise; never `-0`.
fn number(value: f64) -> String {
    let rounded = (value * 10.0).round() / 10.0;
    let rounded = if rounded.abs() < 0.05 { 0.0 } else { rounded };
    if (rounded - rounded.round()).abs() < 0.05 {
        format!("{rounded:.0}")
    } else {
        format!("{rounded:.1}")
    }
}

impl Side {
    fn name(self) -> &'static str {
        match self {
            Side::Left => "left",
            Side::Top => "top",
            Side::Right => "right",
            Side::Bottom => "bottom",
        }
    }
}

impl Edge {
    fn name(self) -> &'static str {
        match self {
            Edge::Left => "left",
            Edge::Right => "right",
            Edge::Top => "top",
            Edge::Bottom => "bottom",
            Edge::CentreX => "centre-x",
            Edge::CentreY => "centre-y",
        }
    }
}

impl OrphanRef {
    fn name(self) -> &'static str {
        match self {
            OrphanRef::Owner => "owner",
            OrphanRef::Frame => "frame",
            OrphanRef::Label => "label",
        }
    }
}

impl ArrowEnd {
    fn name(self) -> &'static str {
        match self {
            ArrowEnd::Start => "start",
            ArrowEnd::End => "end",
        }
    }
}

impl RefState {
    fn name(self) -> &'static str {
        match self {
            RefState::Missing => "missing",
            RefState::Deleted => "deleted",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Point;
    use crate::model::{Binding, Endpoint, Routing, ShapeKind, Style, TextAlign};
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

    fn rect_shape(doc: &mut Document, rect: Rect) -> String {
        doc.add(
            ElementKind::Shape {
                shape: ShapeKind::Rectangle,
                rect,
                style: Style::default(),
                bound_text_id: None,
            },
            1,
        )
    }

    fn frame(doc: &mut Document, rect: Rect) -> String {
        doc.add(
            ElementKind::Frame {
                rect,
                name: "frame".into(),
            },
            1,
        )
    }

    fn label(doc: &mut Document, rect: Rect, owner: &str) -> String {
        doc.add(
            ElementKind::Text {
                rect,
                text: "label".into(),
                align: TextAlign::Center,
                fixed_width: false,
                container_id: Some(owner.to_string()),
                style: Style::default(),
            },
            1,
        )
    }

    fn arrow(doc: &mut Document, end_target: &str) -> String {
        doc.add(
            ElementKind::Arrow {
                start: Endpoint::free(0.0, 0.0),
                end: Endpoint {
                    x: 10.0,
                    y: 10.0,
                    binding: Some(Binding {
                        element_id: end_target.to_string(),
                        focus: Some(Point::ZERO),
                    }),
                },
                routing: Routing::Straight,
                start_arrowhead: false,
                end_arrowhead: true,
                style: Style::default(),
                waypoints: Vec::new(),
                label_id: None,
            },
            1,
        )
    }

    fn has_edge(edges: &[AlignEdge], edge: Edge) -> Option<f64> {
        edges
            .iter()
            .find(|candidate| candidate.edge == edge)
            .map(|candidate| candidate.offset)
    }

    #[test]
    fn reports_overlap_near_outside_orphans_and_dangling_arrows() {
        let mut doc = Document::new();
        let a = rect_shape(&mut doc, Rect::new(0.0, 0.0, 100.0, 60.0));
        let b = rect_shape(&mut doc, Rect::new(50.0, 20.0, 100.0, 60.0));
        let near_a = rect_shape(&mut doc, Rect::new(300.0, 0.0, 100.0, 60.0));
        let near_b = rect_shape(&mut doc, Rect::new(408.0, 0.0, 100.0, 60.0));
        let frame = frame(&mut doc, Rect::new(0.0, 200.0, 200.0, 100.0));
        let outside = rect_shape(&mut doc, Rect::new(190.0, 220.0, 30.0, 30.0));
        doc.element_mut(&outside).unwrap().frame_id = Some(frame.clone());

        let gone = rect_shape(&mut doc, Rect::new(500.0, 500.0, 100.0, 60.0));
        let orphan = label(&mut doc, Rect::new(500.0, 500.0, 40.0, 20.0), &gone);
        doc.element_mut(&gone).unwrap().is_deleted = true;

        let target = rect_shape(&mut doc, Rect::new(700.0, 500.0, 100.0, 60.0));
        doc.element_mut(&target).unwrap().is_deleted = true;
        let dangling = arrow(&mut doc, &target);

        let report = analyze(&doc);
        assert!(
            report.facts.contains(&LayoutFact::Overlap {
                a: a.clone(),
                b: b.clone(),
                width: 50.0,
                height: 40.0,
            }),
            "{:?}",
            report.facts
        );
        assert!(
            report.facts.contains(&LayoutFact::Near {
                a: near_a.clone(),
                b: near_b.clone(),
                gap: 8.0,
            }),
            "{:?}",
            report.facts
        );
        let outside_fact = report
            .facts
            .iter()
            .find(|fact| matches!(fact, LayoutFact::Outside { id, .. } if id == &outside))
            .expect("out-of-frame element reported");
        assert_eq!(
            outside_fact,
            &LayoutFact::Outside {
                id: outside.clone(),
                frame: frame.clone(),
                side: Side::Right,
                amount: 20.0,
            }
        );
        assert!(
            report.facts.contains(&LayoutFact::Orphan {
                id: orphan.clone(),
                owner: gone.clone(),
                reference: OrphanRef::Owner,
                state: RefState::Deleted,
            }),
            "{:?}",
            report.facts
        );
        assert!(
            report.facts.contains(&LayoutFact::Dangling {
                arrow: dangling.clone(),
                end: ArrowEnd::End,
                target: target.clone(),
                state: RefState::Deleted,
            }),
            "{:?}",
            report.facts
        );
    }

    #[test]
    fn frame_children_labels_and_sticky_notes_are_not_overlaps() {
        let mut doc = Document::new();
        let outer = frame(&mut doc, Rect::new(0.0, 0.0, 400.0, 300.0));
        let inner = frame(&mut doc, Rect::new(20.0, 20.0, 100.0, 100.0));
        doc.element_mut(&inner).unwrap().frame_id = Some(outer.clone());
        let grandchild = rect_shape(&mut doc, Rect::new(30.0, 30.0, 40.0, 40.0));
        doc.element_mut(&grandchild).unwrap().frame_id = Some(inner.clone());

        let shape = rect_shape(&mut doc, Rect::new(200.0, 10.0, 100.0, 60.0));
        doc.element_mut(&shape).unwrap().frame_id = Some(outer.clone());
        let label = label(&mut doc, Rect::new(210.0, 20.0, 50.0, 20.0), &shape);
        if let ElementKind::Shape { bound_text_id, .. } = &mut doc.element_mut(&shape).unwrap().kind
        {
            *bound_text_id = Some(label.clone());
        }

        let stack = doc.add(
            ElementKind::StickyStack {
                rect: Rect::new(0.0, 400.0, 140.0, 140.0),
                style: Style::default(),
            },
            1,
        );
        let sticky = doc.add(
            ElementKind::Shape {
                shape: ShapeKind::Sticky,
                rect: Rect::new(10.0, 410.0, 120.0, 120.0),
                style: Style::default(),
                bound_text_id: None,
            },
            1,
        );

        let x = rect_shape(&mut doc, Rect::new(1000.0, 1000.0, 100.0, 60.0));
        let y = rect_shape(&mut doc, Rect::new(1050.0, 1020.0, 100.0, 60.0));

        let report = analyze(&doc);
        let overlap_ids: Vec<(&str, &str)> = report
            .facts
            .iter()
            .filter_map(|fact| match fact {
                LayoutFact::Overlap { a, b, .. } => Some((a.as_str(), b.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            overlap_ids,
            vec![(x.as_str(), y.as_str())],
            "only the unrelated shapes overlap: {:?}",
            report.facts
        );
        // Ownership still lets the frame/stack pair through near/align checks,
        // but containment pairs must not surface as area overlaps.
        for (a, b) in [
            (&outer, &inner),
            (&outer, &grandchild),
            (&inner, &grandchild),
            (&outer, &shape),
            (&outer, &label),
            (&shape, &label),
            (&stack, &sticky),
        ] {
            assert!(
                !report.facts.iter().any(|fact| matches!(
                    fact,
                    LayoutFact::Overlap { a: left, b: right, .. }
                        if (left == a && right == b) || (left == b && right == a)
                )),
                "{a} and {b} are an owned pair: {:?}",
                report.facts
            );
        }
    }

    #[test]
    fn alignment_reports_shared_edges_with_offsets() {
        let mut doc = Document::new();
        let a = rect_shape(&mut doc, Rect::new(0.0, 0.0, 100.0, 60.0));
        let b = rect_shape(&mut doc, Rect::new(1.0, 100.0, 100.0, 60.0));
        let far = rect_shape(&mut doc, Rect::new(3.0, 300.0, 100.0, 60.0));

        let report = analyze(&doc);
        let aligned = report
            .facts
            .iter()
            .find_map(|fact| match fact {
                LayoutFact::Align {
                    a: left,
                    b: right,
                    edges,
                } if left == &a && right == &b => Some(edges),
                _ => None,
            })
            .expect("a and b align");
        assert_eq!(has_edge(aligned, Edge::Left), Some(-1.0));
        assert_eq!(has_edge(aligned, Edge::Right), Some(-1.0));
        assert_eq!(has_edge(aligned, Edge::CentreX), Some(-1.0));
        assert_eq!(has_edge(aligned, Edge::Top), None);
        assert_eq!(has_edge(aligned, Edge::Bottom), None);

        let far_aligns = report.facts.iter().any(|fact| {
            matches!(
                fact,
                LayoutFact::Align { a: left, b: right, .. }
                    if (left == &a && right == &far) || (left == &far && right == &a)
            )
        });
        assert!(!far_aligns, "a 3-unit offset is past tolerance");
    }

    #[test]
    fn text_overflow_uses_the_text_container() {
        let mut doc = Document::new();
        let container = rect_shape(&mut doc, Rect::new(0.0, 0.0, 100.0, 60.0));
        let text = label(&mut doc, Rect::new(10.0, 10.0, 40.0, 80.0), &container);
        let contained = label(&mut doc, Rect::new(10.0, 10.0, 40.0, 20.0), &container);

        // Captions below glyphs and labels beside connectors are placed
        // outside their owner on purpose and are not overflow defects.
        let glyph = doc.add(
            ElementKind::Glyph {
                glyph: "cloud_run".into(),
                rect: Rect::new(200.0, 200.0, 50.0, 50.0),
                style: Style::default(),
                caption_id: None,
            },
            1,
        );
        let caption = label(&mut doc, Rect::new(190.0, 260.0, 70.0, 20.0), &glyph);
        if let ElementKind::Glyph { caption_id, .. } = &mut doc.element_mut(&glyph).unwrap().kind {
            *caption_id = Some(caption.clone());
        }
        let arrow_target = rect_shape(&mut doc, Rect::new(400.0, 400.0, 50.0, 50.0));
        let arrow = arrow(&mut doc, &arrow_target);
        let arrow_label = label(&mut doc, Rect::new(500.0, 500.0, 60.0, 20.0), &arrow);
        if let ElementKind::Arrow { label_id, .. } = &mut doc.element_mut(&arrow).unwrap().kind {
            *label_id = Some(arrow_label.clone());
        }

        let report = analyze(&doc);
        let overflow: Vec<_> = report
            .facts
            .iter()
            .filter(|fact| matches!(fact, LayoutFact::TextOverflow { .. }))
            .collect();
        assert_eq!(
            overflow,
            vec![&LayoutFact::TextOverflow {
                text,
                container,
                side: Side::Bottom,
                amount: 30.0,
            }],
            "{:?}",
            report.facts
        );
        let _ = (contained, caption, arrow_label);
    }

    #[test]
    fn scoped_analysis_adds_owners_but_not_unrelated_elements() {
        let mut doc = Document::new();
        let frame = frame(&mut doc, Rect::new(0.0, 0.0, 100.0, 100.0));
        let container = rect_shape(&mut doc, Rect::new(90.0, 10.0, 50.0, 50.0));
        doc.element_mut(&container).unwrap().frame_id = Some(frame.clone());
        let matched = label(&mut doc, Rect::new(95.0, 20.0, 20.0, 20.0), &container);
        let other = rect_shape(&mut doc, Rect::new(300.0, 300.0, 50.0, 50.0));
        let overlapping_other = rect_shape(&mut doc, Rect::new(320.0, 320.0, 50.0, 50.0));

        let matched_ids = BTreeSet::from([matched.clone()]);
        let report = analyze_scoped(&doc, &matched_ids);
        assert!(
            report.facts.contains(&LayoutFact::Outside {
                id: container.clone(),
                frame: frame.clone(),
                side: Side::Right,
                amount: 40.0,
            }),
            "the text owner joins the scope recursively, revealing its overflow: {:?}",
            report.facts
        );
        assert!(
            !report.facts.iter().any(|fact| matches!(
                fact,
                LayoutFact::Overlap { a, b, .. }
                    if (a == &other && b == &overlapping_other)
                        || (a == &overlapping_other && b == &other)
            )),
            "elements outside the scope stay out: {:?}",
            report.facts
        );
    }

    #[test]
    fn render_is_clean_when_empty() {
        let preserved = render(&LayoutReport::default(), &mut Short(HashMap::new()));
        assert_eq!(preserved, "layout: clean\n");
    }

    #[test]
    fn render_truncates_at_the_cap_and_uses_handles() {
        let mut doc = Document::new();
        for index in 0..55 {
            rect_shape(&mut doc, Rect::new(index as f64, index as f64, 100.0, 60.0));
        }
        let report = analyze(&doc);
        assert_eq!(report.facts.len(), MAX_FACTS);
        assert_eq!(report.total, 55 * 54 / 2, "every overlapping pair counts");
        let text = render(&report, &mut Short(HashMap::new()));
        assert!(text.starts_with("layout: 1485 facts\n"));
        assert!(text.contains("overlap e1 e2 "), "{text}");
        assert_eq!(
            text.lines().count(),
            1 + MAX_FACTS + 1,
            "header, capped facts, and the truncation line"
        );
        assert!(
            text.ends_with(&format!(
                "layout: +{} more\n",
                report.total - report.facts.len()
            )),
            "{text}"
        );
    }
}
