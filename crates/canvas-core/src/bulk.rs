//! Bulk board construction for the agent integration.
//!
//! One tool call carries thousands of explicit shapes or a whole parametric
//! grid in tens of tokens. Inserts chain fractional keys from a single
//! `top_index` scan, so a 200k-element build stays O(n) — unlike per-beat
//! `Document::add`, which rescans the store every time (O(n²)).
//!
//! Grids skip connectors at overview zoom on the page (see the LOD path), but
//! every connector is still a bound element: zooming in routes and shows it.

use std::collections::HashMap;

use crate::connector;
use crate::geometry::Rect;
use crate::id::new_element_id;
use crate::index::key_between;
use crate::model::{
    Binding, Document, Element, ElementId, ElementKind, Endpoint, Routing, ShapeKind, Style,
    TextAlign,
};
use crate::text::{layout_text, ApproxMeasurer};

/// Hard ceiling per bulk call: bounds time (~seconds) and memory (~a GB at
/// 500k with full rough caches on the page) before anything commits.
pub const MAX_BULK_ELEMENTS: usize = 500_000;

/// O(1)-amortized key cursor: one `top_index` scan, then chained keys.
pub struct KeyChain {
    last: Option<String>,
}

impl KeyChain {
    pub fn new(doc: &Document) -> Self {
        Self {
            last: doc.top_index(),
        }
    }

    pub fn next_key(&mut self) -> Result<String, String> {
        match key_between(self.last.as_deref(), None) {
            Ok(key) => {
                self.last = Some(key.clone());
                Ok(key)
            }
            Err(_) => Err("index space exhausted; split the board across calls".into()),
        }
    }
}

/// Fill treatment for bulk shapes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BulkFill {
    /// Cross-hatched colour (diagram default).
    #[default]
    Hatch,
    Solid,
    /// No fill: ink outline only.
    None,
}

/// One explicit shape for a bulk call.
pub struct ShapeSpec {
    pub kind: ShapeCode,
    pub rect: Rect,
    /// Fill colour; defaults to the call's shared fill.
    pub fill: Option<String>,
    /// Bound label.
    pub label: Option<String>,
}

/// Shape family by one-letter code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShapeCode {
    Rect,
    Ellipse,
    Diamond,
    Sticky,
    Text,
    Frame,
}

impl ShapeCode {
    pub fn parse(code: &str) -> Option<Self> {
        match code {
            "r" => Some(ShapeCode::Rect),
            "e" => Some(ShapeCode::Ellipse),
            "d" => Some(ShapeCode::Diamond),
            "s" => Some(ShapeCode::Sticky),
            "t" => Some(ShapeCode::Text),
            "f" => Some(ShapeCode::Frame),
            _ => None,
        }
    }
}

/// Shared style for a bulk call; per-shape `fill` overrides.
#[derive(Clone, Debug, Default)]
pub struct BulkStyle {
    pub fill: Option<String>,
    pub ink: Option<String>,
    pub roughness: Option<u8>,
}

/// A parametric grid: a whole board in tens of tokens.
pub struct GridSpec {
    pub count: usize,
    pub cols: Option<usize>,
    pub x0: f64,
    pub y0: f64,
    pub dx: f64,
    pub dy: f64,
    pub w: f64,
    pub h: f64,
    /// Kind codes cycled per node, e.g. "red".
    pub kinds: String,
    /// Fill colours cycled per node.
    pub palette: Vec<String>,
    /// Which neighbours each node wires to.
    pub links: LinkMode,
    pub roughness: u8,
    /// Optional owning frame drawn around the finished grid.
    pub frame: Option<String>,
}

/// Neighbour wiring for grid nodes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LinkMode {
    None,
    Right,
    Down,
    #[default]
    Both,
}

impl LinkMode {
    pub fn parse(mode: &str) -> Option<Self> {
        match mode {
            "none" => Some(LinkMode::None),
            "right" => Some(LinkMode::Right),
            "down" => Some(LinkMode::Down),
            "both" => Some(LinkMode::Both),
            _ => None,
        }
    }
}

/// What a bulk build produced.
#[derive(Clone, Debug, Default)]
pub struct BulkStats {
    pub shapes: usize,
    pub texts: usize,
    pub connects: usize,
    pub bounds: Option<Rect>,
    /// Ids of created shapes in call order (connect indices address these).
    pub shape_ids: Vec<ElementId>,
    /// Every created id (shapes, labels, arrows): the broadcast set.
    pub new_ids: Vec<ElementId>,
}

fn style_for(fill: Option<&str>, defaults: &BulkStyle) -> Style {
    let fill_color = fill
        .or(defaults.fill.as_deref())
        .unwrap_or("transparent")
        .to_string();
    Style {
        fill_color,
        stroke_color: defaults.ink.clone().unwrap_or("#ece4d6".into()),
        roughness: defaults.roughness.unwrap_or(1).min(2),
        ..Style::default()
    }
}

fn insert_element(
    doc: &mut Document,
    chain: &mut KeyChain,
    kind: ElementKind,
    now_ms: u64,
    stats: &mut BulkStats,
) -> Result<ElementId, String> {
    if stats.shapes + stats.texts + stats.connects >= MAX_BULK_ELEMENTS {
        return Err(format!(
            "bulk board exceeds {MAX_BULK_ELEMENTS} elements; split it across calls"
        ));
    }
    let index = chain.next_key()?;
    let id = new_element_id();
    if let Some(bounds) = stats.bounds {
        stats.bounds = Some(bounds.union(&kind_bounds(&kind)));
    } else {
        stats.bounds = Some(kind_bounds(&kind));
    }
    let element = Element::new_with_id(kind, index, now_ms, id.clone());
    doc.insert(element);
    stats.new_ids.push(id.clone());
    Ok(id)
}

fn kind_bounds(kind: &ElementKind) -> Rect {
    match kind {
        ElementKind::Shape { rect, .. }
        | ElementKind::Glyph { rect, .. }
        | ElementKind::Text { rect, .. }
        | ElementKind::StickyStack { rect, .. }
        | ElementKind::Frame { rect, .. } => *rect,
        ElementKind::Freehand { points, .. } => points
            .iter()
            .fold(None, |acc: Option<Rect>, p| {
                let r = Rect::new(p.x, p.y, 0.0, 0.0);
                Some(match acc {
                    Some(a) => a.union(&r),
                    None => r,
                })
            })
            .unwrap_or(Rect::ZERO),
        ElementKind::Arrow { start, end, .. } => Rect::from_points(start.point(), end.point()),
    }
}

/// Build explicit shapes (+ bound labels) into `doc`.
pub fn build_shapes(
    doc: &mut Document,
    chain: &mut KeyChain,
    specs: &[ShapeSpec],
    defaults: &BulkStyle,
    now_ms: u64,
    frame_id: Option<&ElementId>,
    stats: &mut BulkStats,
) -> Result<(), String> {
    for spec in specs {
        if !spec.rect.x.is_finite()
            || !spec.rect.y.is_finite()
            || !spec.rect.w.is_finite()
            || !spec.rect.h.is_finite()
        {
            return Err("bulk shapes need finite x, y, w, h".into());
        }
        if spec.kind != ShapeCode::Text && (spec.rect.w < 8.0 || spec.rect.h < 8.0) {
            return Err("bulk shapes need w, h >= 8".into());
        }
        match spec.kind {
            ShapeCode::Text => {
                let text = spec.label.clone().unwrap_or_default();
                if text.trim().is_empty() {
                    return Err("bulk text needs its t: string".into());
                }
                let ink = defaults.ink.clone().unwrap_or("#ece4d6".into());
                let style = Style {
                    stroke_color: ink,
                    ..Style::default()
                };
                let layout = layout_text(&text, style.font_size, None, &ApproxMeasurer);
                let kind = ElementKind::Text {
                    rect: Rect::new(
                        spec.rect.x,
                        spec.rect.y,
                        (layout.width + 20.0).max(20.0),
                        layout.height,
                    ),
                    text,
                    align: TextAlign::Left,
                    fixed_width: false,
                    container_id: None,
                    style,
                };
                let id = insert_element(doc, chain, kind, now_ms, stats)?;
                stats.texts += 1;
                stats.shape_ids.push(id);
            }
            ShapeCode::Frame => {
                let kind = ElementKind::Frame {
                    rect: spec.rect,
                    name: spec.label.clone().unwrap_or_default(),
                };
                let id = insert_element(doc, chain, kind, now_ms, stats)?;
                stats.shape_ids.push(id);
            }
            _ => {
                let shape = match spec.kind {
                    ShapeCode::Ellipse => ShapeKind::Ellipse,
                    ShapeCode::Diamond => ShapeKind::Diamond,
                    ShapeCode::Sticky => ShapeKind::Sticky,
                    _ => ShapeKind::Rectangle,
                };
                let mut style = style_for(spec.fill.as_deref(), defaults);
                if spec.kind == ShapeCode::Sticky {
                    style = crate::commands::sticky_style(style);
                }
                let kind = ElementKind::Shape {
                    shape,
                    rect: spec.rect,
                    style,
                    bound_text_id: None,
                };
                let id = insert_element(doc, chain, kind, now_ms, stats)?;
                if let Some(label) = spec
                    .label
                    .as_deref()
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                {
                    let ink = if spec.kind == ShapeCode::Sticky {
                        crate::commands::STICKY_INK
                    } else {
                        defaults
                            .ink
                            .as_deref()
                            .unwrap_or(crate::commands::DEFAULT_INK)
                    };
                    let layout = layout_text(label, 20.0, None, &ApproxMeasurer);
                    let label_kind = ElementKind::Text {
                        rect: Rect::new(
                            spec.rect.x + 10.0,
                            spec.rect.y + (spec.rect.h - layout.height) / 2.0,
                            (spec.rect.w - 20.0).max(20.0),
                            layout.height,
                        ),
                        text: label.to_string(),
                        align: TextAlign::Center,
                        fixed_width: false,
                        container_id: None,
                        style: Style {
                            stroke_color: ink.into(),
                            ..Style::default()
                        },
                    };
                    let text_id = insert_element(doc, chain, label_kind, now_ms, stats)?;
                    stats.texts += 1;
                    if let Some(ElementKind::Shape { bound_text_id, .. }) =
                        doc.element_mut(&id).map(|e| &mut e.kind)
                    {
                        *bound_text_id = Some(text_id);
                    }
                }
                if let Some(frame) = frame_id {
                    if let Some(element) = doc.element_mut(&id) {
                        element.frame_id = Some(frame.clone());
                    }
                }
                stats.shapes += 1;
                stats.shape_ids.push(id);
            }
        }
    }
    Ok(())
}

/// Connect shape indices (into this call's shapes, in order) with bound
/// elbow connectors, mirroring the script `connect` beat.
pub fn connect_indices(
    doc: &mut Document,
    chain: &mut KeyChain,
    pairs: &[(usize, usize)],
    shape_ids: &[ElementId],
    now_ms: u64,
    stats: &mut BulkStats,
) -> Result<(), String> {
    for (position, (from, to)) in pairs.iter().enumerate() {
        let source = shape_ids
            .get(*from)
            .ok_or_else(|| format!("bulk connect #{position}: shape index {from} is out of range"))?
            .clone();
        let target = shape_ids
            .get(*to)
            .ok_or_else(|| format!("bulk connect #{position}: shape index {to} is out of range"))?
            .clone();
        if source == target {
            return Err(format!(
                "bulk connect #{position}: cannot link a shape to itself"
            ));
        }
        let source_rect = doc
            .element(&source)
            .map(|e| e.bounds())
            .ok_or_else(|| format!("bulk connect #{position}: shape {from} vanished"))?;
        let target_rect = doc
            .element(&target)
            .map(|e| e.bounds())
            .ok_or_else(|| format!("bulk connect #{position}: shape {to} vanished"))?;
        let mut start = Endpoint {
            x: source_rect.center().x,
            y: source_rect.center().y,
            binding: Some(Binding {
                element_id: source,
                focus: None,
            }),
        };
        let mut end = Endpoint {
            x: target_rect.center().x,
            y: target_rect.center().y,
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
        let kind = ElementKind::Arrow {
            start,
            end,
            routing: Routing::Elbow,
            start_arrowhead: false,
            end_arrowhead: true,
            style: Style::default(),
            waypoints: Vec::new(),
            label_id: None,
        };
        insert_element(doc, chain, kind, now_ms, stats)?;
        stats.connects += 1;
    }
    Ok(())
}

/// Expand a parametric grid into shapes (+ links), in row-major order.
pub fn build_grid(
    doc: &mut Document,
    chain: &mut KeyChain,
    grid: &GridSpec,
    defaults: &BulkStyle,
    now_ms: u64,
    stats: &mut BulkStats,
) -> Result<(), String> {
    if grid.count == 0 {
        return Err("bulk grid needs count >= 1".into());
    }
    for value in [grid.x0, grid.y0, grid.dx, grid.dy, grid.w, grid.h] {
        if !value.is_finite() {
            return Err("bulk grid needs finite geometry".into());
        }
    }
    if grid.dx <= 0.0 || grid.dy <= 0.0 || grid.w < 8.0 || grid.h < 8.0 {
        return Err("bulk grid needs dx, dy > 0 and w, h >= 8".into());
    }
    let cols = grid
        .cols
        .unwrap_or((grid.count as f64).sqrt().ceil() as usize)
        .max(1);
    let kinds: Vec<char> = if grid.kinds.is_empty() {
        vec!['r']
    } else {
        grid.kinds.chars().collect()
    };
    let base = stats.shape_ids.len();
    let frame_id = if let Some(name) = grid.frame.as_deref() {
        let rows = grid.count.div_ceil(cols);
        let rect = Rect::new(
            grid.x0,
            grid.y0,
            (cols as f64) * grid.dx + grid.w,
            (rows as f64) * grid.dy + grid.h,
        )
        .expand(24.0);
        let id = insert_element(
            doc,
            chain,
            ElementKind::Frame {
                rect,
                name: name.to_string(),
            },
            now_ms,
            stats,
        )?;
        Some(id)
    } else {
        None
    };
    for i in 0..grid.count {
        let col = i % cols;
        let row = i / cols;
        let kind = match kinds[i % kinds.len()] {
            'e' => ShapeCode::Ellipse,
            'd' => ShapeCode::Diamond,
            's' => ShapeCode::Sticky,
            _ => ShapeCode::Rect,
        };
        let fill = grid.palette.get(i % grid.palette.len().max(1)).cloned();
        let spec = ShapeSpec {
            kind,
            rect: Rect::new(
                grid.x0 + col as f64 * grid.dx,
                grid.y0 + row as f64 * grid.dy,
                grid.w,
                grid.h,
            ),
            fill,
            label: None,
        };
        let before = stats.shape_ids.len();
        build_shapes(
            doc,
            chain,
            std::slice::from_ref(&spec),
            defaults,
            now_ms,
            frame_id.as_ref(),
            stats,
        )?;
        debug_assert_eq!(stats.shape_ids.len(), before + 1);
    }
    let rows = grid.count.div_ceil(cols);
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for i in 0..grid.count {
        let col = i % cols;
        let row = i / cols;
        let right = matches!(grid.links, LinkMode::Right | LinkMode::Both)
            && col + 1 < cols
            && i + 1 < grid.count
            && (i + 1) / cols == row;
        // Guard the row wrap on ragged final rows.
        if right && row < rows {
            pairs.push((base + i, base + i + 1));
        }
        if matches!(grid.links, LinkMode::Down | LinkMode::Both) && i + cols < grid.count {
            pairs.push((base + i, base + i + cols));
        }
    }
    connect_indices(doc, chain, &pairs, &stats.shape_ids.clone(), now_ms, stats)?;
    Ok(())
}

/// Whole-document summary in a few hundred tokens at any scale.
pub fn summarize(doc: &Document) -> serde_json::Value {
    let mut rectangles = 0;
    let mut ellipses = 0;
    let mut diamonds = 0;
    let mut stickies = 0;
    let mut freehand = 0;
    let mut arrows = 0;
    let mut texts = 0;
    let mut frames = 0;
    let mut glyphs = 0;
    let mut fills: HashMap<String, usize> = HashMap::new();
    for element in doc.elements.values().filter(|e| !e.is_deleted) {
        match &element.kind {
            ElementKind::Shape { shape, style, .. } => {
                match shape {
                    ShapeKind::Rectangle => rectangles += 1,
                    ShapeKind::Ellipse => ellipses += 1,
                    ShapeKind::Diamond => diamonds += 1,
                    ShapeKind::Sticky => stickies += 1,
                }
                if style.fill_color != "transparent" {
                    *fills.entry(style.fill_color.clone()).or_default() += 1;
                }
            }
            ElementKind::Freehand { .. } => freehand += 1,
            ElementKind::Arrow { .. } => arrows += 1,
            ElementKind::Text { .. } => texts += 1,
            ElementKind::Frame { .. } => frames += 1,
            ElementKind::Glyph { .. } => glyphs += 1,
            ElementKind::StickyStack { .. } => stickies += 1,
        }
    }
    let mut fills: Vec<(String, usize)> = fills.into_iter().collect();
    fills.sort_by_key(|a| std::cmp::Reverse(a.1));
    fills.truncate(8);
    serde_json::json!({
        "live": doc.live_count(),
        "rectangles": rectangles, "ellipses": ellipses, "diamonds": diamonds,
        "stickies": stickies, "freehand": freehand, "arrows": arrows,
        "texts": texts, "frames": frames, "glyphs": glyphs,
        "bounds": doc.bounds().map(|r| [r.x, r.y, r.w, r.h]),
        "top_fills": fills.iter().map(|(c, n)| serde_json::json!([c, n])).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Document {
        Document::new()
    }

    #[test]
    fn chained_keys_stay_ordered_without_rescans() {
        let doc = doc();
        let mut chain = KeyChain::new(&doc);
        let mut keys = Vec::new();
        for _ in 0..500 {
            keys.push(chain.next_key().expect("key space"));
        }
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "chained keys arrive in order");
        assert!(keys.windows(2).all(|w| w[0] != w[1]));
    }

    #[test]
    fn bulk_shapes_and_connects_link_up() {
        let before = crate::id::element_id_generations();
        let mut doc = doc();
        let mut chain = KeyChain::new(&doc);
        let mut stats = BulkStats::default();
        let specs = (0..4)
            .map(|i| ShapeSpec {
                kind: ShapeCode::Rect,
                rect: Rect::new(i as f64 * 200.0, 0.0, 140.0, 90.0),
                fill: None,
                label: if i == 0 { Some("DB".into()) } else { None },
            })
            .collect::<Vec<_>>();
        build_shapes(
            &mut doc,
            &mut chain,
            &specs,
            &BulkStyle::default(),
            1,
            None,
            &mut stats,
        )
        .unwrap();
        connect_indices(
            &mut doc,
            &mut chain,
            &[(0, 1), (1, 2)],
            &stats.shape_ids.clone(),
            1,
            &mut stats,
        )
        .unwrap();
        assert_eq!(stats.shapes, 4);
        assert_eq!(stats.texts, 1);
        assert_eq!(stats.connects, 2);
        assert_eq!(doc.live_count(), 4 + 1 + 2);
        assert!(stats.bounds.is_some());
        assert_eq!(
            crate::id::element_id_generations() - before,
            stats.new_ids.len(),
            "bulk draws exactly one id per element, never one it discards"
        );
    }

    #[test]
    fn bulk_stickies_use_the_shared_solid_style() {
        let mut doc = doc();
        let mut chain = KeyChain::new(&doc);
        let mut stats = BulkStats::default();
        build_shapes(
            &mut doc,
            &mut chain,
            &[ShapeSpec {
                kind: ShapeCode::Sticky,
                rect: Rect::new(0.0, 0.0, 140.0, 140.0),
                fill: None,
                label: Some("Cache".into()),
            }],
            &BulkStyle::default(),
            1,
            None,
            &mut stats,
        )
        .unwrap();
        let sticky = doc.element(&stats.shape_ids[0]).expect("sticky inserted");
        let crate::model::ElementKind::Shape { style, .. } = &sticky.kind else {
            panic!("expected a sticky shape");
        };
        assert_eq!(style.fill_style, crate::model::FillPattern::Solid);
        assert_eq!(style.fill_color, crate::commands::STICKY_FILL);
        assert_eq!(style.stroke_color, crate::commands::STICKY_INK);
    }

    #[test]
    fn grid_wires_right_and_down_without_row_wrap() {
        let mut doc = doc();
        let mut chain = KeyChain::new(&doc);
        let mut stats = BulkStats::default();
        // 10 nodes in 4 cols: row 0 has 4, row 1 has 4, row 2 has 2.
        // Right links: 3 + 3 + 1 = 7. Down links: 4 + 2 = 6.
        build_grid(
            &mut doc,
            &mut chain,
            &GridSpec {
                count: 10,
                cols: Some(4),
                x0: 0.0,
                y0: 0.0,
                dx: 220.0,
                dy: 170.0,
                w: 140.0,
                h: 90.0,
                kinds: "red".into(),
                palette: vec!["#a5d8ff".into()],
                links: LinkMode::Both,
                roughness: 1,
                frame: None,
            },
            &BulkStyle::default(),
            1,
            &mut stats,
        )
        .unwrap();
        assert_eq!(stats.shapes, 10);
        assert_eq!(stats.connects, 13, "7 right + 6 down, no wrap links");
        assert_eq!(doc.live_count(), 23);
    }

    #[test]
    fn summary_counts_a_mixed_board() {
        let mut doc = doc();
        let mut chain = KeyChain::new(&doc);
        let mut stats = BulkStats::default();
        build_shapes(
            &mut doc,
            &mut chain,
            &[ShapeSpec {
                kind: ShapeCode::Ellipse,
                rect: Rect::new(0.0, 0.0, 140.0, 90.0),
                fill: Some("#ffc9c9".into()),
                label: None,
            }],
            &BulkStyle::default(),
            1,
            None,
            &mut stats,
        )
        .unwrap();
        let summary = summarize(&doc);
        assert_eq!(summary["live"], 1);
        assert_eq!(summary["ellipses"], 1);
        assert_eq!(
            summary["bounds"],
            serde_json::json!([0.0, 0.0, 140.0, 90.0])
        );
    }
}
