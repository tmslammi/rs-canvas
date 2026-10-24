//! The scene: per-element cached drawing layers with dirty-region updates.
//!
//! `Scene::update` rebuilds only the layers for changed elements and reports
//! the world-space rectangles that must be repainted (old bounds union new
//! bounds, expanded for stroke and jitter overflow). This is what keeps
//! Canvas2D smooth on large documents. Large changed sets take a bulk rebuild
//! path so mass delete/restore stays linear in the layer count.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::collections::{HashMap, HashSet};

use crate::display::DrawOp;
use crate::geometry::Rect;
use crate::model::{Document, Element, ElementId};
use crate::rough::build_element_ops;
use crate::spatial::ObstacleIndex;
use crate::text::TextMeasurer;

/// Approximate painted bounds without building draw ops: model bounds plus
/// kind-specific margins. Always a superset of the exact layer bounds (safe
/// for culling, LOD rects and dirty regions); tightened to exact when ops
/// are built on demand. What can exceed model bounds: routed-arrow detours
/// around obstacles (covered by the arrow margin), frame name bands, text
/// ascenders and line wrap, sticky pile offsets.
pub(crate) fn approx_bounds(element: &Element) -> Rect {
    use crate::model::ElementKind;
    let (base, margin) = match &element.kind {
        ElementKind::Shape { rect, style, .. } | ElementKind::Glyph { rect, style, .. } => {
            (*rect, style.stroke_width * 2.0 + 4.0)
        }
        ElementKind::Freehand { points, style, .. } => {
            let mut bounds = match points.first() {
                Some(first) => Rect::new(first.x, first.y, 0.0, 0.0),
                None => Rect::ZERO,
            };
            for p in points.iter().skip(1) {
                bounds = bounds.union(&Rect::new(p.x, p.y, 0.0, 0.0));
            }
            (
                bounds,
                style.stroke_width * 2.0 + style.roughness as f64 * 4.0 + 4.0,
            )
        }
        ElementKind::Arrow {
            start, end, style, ..
        } => (
            Rect::from_points(start.point(), end.point()),
            style.stroke_width * 2.0 + 64.0,
        ),
        ElementKind::Text { rect, style, .. } => (*rect, style.font_size * 1.5),
        ElementKind::Frame { rect, .. } => (*rect, 24.0),
        ElementKind::StickyStack { rect, .. } => (*rect, 24.0),
    };
    base.expand(margin)
}

/// Extra margin around exact bounds to cover stroke width and jitter.
fn margin(element: &Element) -> f64 {
    match &element.kind {
        crate::model::ElementKind::Shape { style, .. }
        | crate::model::ElementKind::Freehand { style, .. }
        | crate::model::ElementKind::Arrow { style, .. } => {
            style.stroke_width * 2.0 + style.roughness as f64 * 4.0 + 4.0
        }
        _ => 8.0,
    }
}

/// Layer order key: document z-order, ties broken by id.
fn layer_order(a: &SceneLayer, b: &SceneLayer) -> Ordering {
    a.index
        .cmp(&b.index)
        .then_with(|| a.element_id.cmp(&b.element_id))
}

/// Merge two `(index, id)`-sorted layer vectors in one pass.
fn merge_layers(existing: Vec<SceneLayer>, added: Vec<SceneLayer>) -> Vec<SceneLayer> {
    if added.is_empty() {
        return existing;
    }
    if existing.is_empty() {
        return added;
    }
    let mut merged = Vec::with_capacity(existing.len() + added.len());
    let mut existing = existing.into_iter().peekable();
    let mut added = added.into_iter().peekable();
    loop {
        let take_added = match (existing.peek(), added.peek()) {
            (Some(old), Some(new)) => layer_order(new, old) == Ordering::Less,
            (Some(_), None) => false,
            (None, Some(_)) => true,
            (None, None) => break,
        };
        merged.push(if take_added {
            added.next().expect("peeked")
        } else {
            existing.next().expect("peeked")
        });
    }
    merged
}

/// Bounds of the geometry the draw ops actually touch. This is the truth the
/// renderer paints, which can differ from `Element::bounds` (routed arrows,
/// frame name labels).
pub(crate) fn ops_bounds(ops: &[DrawOp], measurer: &dyn TextMeasurer) -> Option<Rect> {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    let mut include = |x: f64, y: f64| {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    };
    for op in ops {
        match op {
            DrawOp::Path(path) => {
                for p in &path.points {
                    include(p.x, p.y);
                }
            }
            DrawOp::LineBatch { segments, .. } => {
                for [a, b] in segments {
                    include(a.x, a.y);
                    include(b.x, b.y);
                }
            }
            DrawOp::Text(text) => {
                let width = measurer.measure_width(&text.text, text.font_size);
                let x = match text.align {
                    crate::model::TextAlign::Left => text.x,
                    crate::model::TextAlign::Center => text.x - width / 2.0,
                    crate::model::TextAlign::Right => text.x - width,
                };
                include(x, text.y - text.font_size);
                include(x + width, text.y + text.font_size * 0.3);
            }
        }
    }
    if min_x.is_finite() {
        Some(Rect::new(min_x, min_y, max_x - min_x, max_y - min_y))
    } else {
        None
    }
}

/// The painted bounds of an element: its model bounds unioned with the draw
/// ops (frame labels, routed arrows, text extents) plus stroke margin. Used
/// by the scene for dirty regions and by exporters for the output bounds.
pub(crate) fn layer_bounds(element: &Element, ops: &[DrawOp], measurer: &dyn TextMeasurer) -> Rect {
    let geometry = match ops_bounds(ops, measurer) {
        Some(op_bounds) => element.bounds().union(&op_bounds),
        None => element.bounds(),
    };
    geometry.expand(margin(element))
}

/// LOD paint color for the zoomed-out fast path: the fill color when the
/// element shades an area, else its ink color. `None` means the element is
/// skipped at overview zoom (text labels, fully transparent elements).
/// Computed once per layer build so per-frame LOD batching never inspects ops.
pub(crate) fn lod_color_for(element: &Element) -> Option<String> {
    use crate::model::ElementKind;
    match &element.kind {
        ElementKind::Shape { style, .. }
        | ElementKind::Glyph { style, .. }
        | ElementKind::StickyStack { style, .. } => {
            if style.fill_color != "transparent" {
                Some(style.fill_color.clone())
            } else if style.stroke_color != "transparent" {
                Some(style.stroke_color.clone())
            } else {
                None
            }
        }
        ElementKind::Freehand { style, .. } => {
            if style.fill_color != "transparent" {
                Some(style.fill_color.clone())
            } else if style.stroke_color != "transparent" {
                Some(style.stroke_color.clone())
            } else {
                None
            }
        }
        ElementKind::Arrow { .. } => {
            // Overview LOD skips connectors: their bounds rectangles would
            // wash over the nodes they wire (and cost half the rect calls on
            // connected boards). Connectivity returns with full ink on zoom-in;
            // node colors carry the overview. See render_lod.
            None
        }
        ElementKind::Frame { .. } => Some("#adb5bd".to_string()),
        ElementKind::Text { .. } => None,
    }
}

/// One element's cached drawing instructions.
#[derive(Clone, Debug, PartialEq)]
pub struct SceneLayer {
    pub element_id: ElementId,
    /// The element's z-order key, kept so `update` can splice in place.
    pub index: String,
    pub version: u64,
    pub bounds: Rect,
    pub ops: Vec<DrawOp>,
    /// Whether `ops`/`bounds` are exactly built (vs approximate bounds with
    /// no ops). Layers start unbuilt; full-detail use builds them on demand.
    pub ops_built: bool,
    /// Palette index for the LOD fast path (`None` = skip at overview zoom:
    /// text labels, fully transparent elements). Integer compare keeps the
    /// per-frame batching loop free of string work; the strings live once in
    /// `Scene::palette`.
    pub lod: Option<u32>,
}

/// Smallest changed set that may take the bulk rebuild path. Below this,
/// per-id splicing is cheap enough and avoids rebuilding the position map.
const BULK_MIN_CHANGED: usize = 128;
/// Estimated per-id splice/reindex pairs (`changed × layers`) above which one
/// rebuild pass beats removing and reinserting layers one by one.
const BULK_WORK_PAIRS: usize = 1 << 21;

/// The renderable scene, kept in document z-order.
#[derive(Clone, Debug, Default)]
pub struct Scene {
    pub layers: Vec<SceneLayer>,
    /// Union of every layer's bounds, maintained incrementally so the
    /// renderer can take the contains-all fast path in O(1) per frame
    /// instead of scanning all layers to discover full coverage.
    bounds_cache: Option<Rect>,
    /// Distinct LOD paint colors across all layers. Grows monotonically
    /// (deleted colors linger as harmless dead entries) so layer indices
    /// stay valid across updates; typical boards hold a handful of entries.
    palette: Vec<String>,
    /// Accumulated drift applied to bounds but not yet to cached ops points
    /// (see `shift_all`). Full-detail rendering flushes it first; the LOD
    /// path reads bounds only and skips the flush, keeping bulk drift at
    /// O(bounds) per frame instead of O(points).
    pending_ops: (f64, f64),
    /// Element id -> position in `layers`. Rebuilt on full rebuilds and large
    /// bulk updates; maintained incrementally on ordinary structural edits
    /// (adds/removes/reorders pay one O(n) reindex, never per frame).
    positions: HashMap<ElementId, u32>,
    /// Layers whose draw ops are not built yet (bounds are approximate).
    /// Full-detail use builds them on demand; the overview never does.
    unbuilt: usize,
    /// Layers whose cached ops are already current for the document, so
    /// `update` must skip rebuilding them (one-shot: consumed on skip).
    /// Populated by `shift_slots` during selection-drag previews; rubber
    /// bands deliberately stay out so release rebuilds them exactly.
    preview_current: HashSet<ElementId>,
    /// Preview-shifted layers whose cached POINTS still need their shift:
    /// bounds move eagerly every preview (culling, LOD and dirty regions
    /// read bounds), points follow lazily — at overview zoom nothing ever
    /// reads them, so bulk drags skip millions of point writes per move.
    /// Flushed with the gesture-total shift before any full-detail use.
    preview_stale: HashSet<ElementId>,
    /// Accumulated shift owed to `preview_stale` layers. Uniform: every
    /// stale layer missed exactly the same preview increments.
    preview_total: (f64, f64),
    /// Old∪new bounds of preview-shifted layers, drained by `update` into
    /// its dirty report (and by `preview_dirty` mid-gesture): repaints cover
    /// both positions with no rebuild.
    pending_dirty: Vec<Rect>,
}

impl Scene {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild every layer. Returns the world rects that changed.
    ///
    /// Fast by design: layers get approximate bounds and LOD colors only —
    /// no draw ops, no text measuring, no obstacle queries. Full-detail use
    /// builds each layer's ops on demand via [`Scene::ensure_area`], so
    /// opening a huge document costs a bounds pass, not a rough-geometry
    /// pass, and the overview never builds ops at all.
    pub fn rebuild_all(
        &mut self,
        doc: &Document,
        _measurer: &dyn TextMeasurer,
        _obstacles: &ObstacleIndex,
    ) -> Vec<Rect> {
        let mut dirty: Vec<Rect> = self.layers.iter().map(|l| l.bounds).collect();
        self.layers.clear();
        self.bounds_cache = None;
        self.positions.clear();
        self.preview_current.clear();
        self.preview_stale.clear();
        self.preview_total = (0.0, 0.0);
        self.pending_dirty.clear();
        // A full rebuild produces ops at current positions: no pending drift.
        self.pending_ops = (0.0, 0.0);
        for element in doc.elements.values() {
            if element.is_deleted {
                continue;
            }
            let bounds = approx_bounds(element);
            let lod = lod_color_for(element).map(|c| self.intern_color(c));
            self.bounds_cache = Some(match self.bounds_cache {
                Some(acc) => acc.union(&bounds),
                None => bounds,
            });
            self.layers.push(SceneLayer {
                element_id: element.id.clone(),
                index: element.index.clone(),
                version: element.version,
                bounds,
                ops: Vec::new(),
                ops_built: false,
                lod,
            });
        }
        self.layers.sort_unstable_by(layer_order);
        self.reindex();
        self.unbuilt = self.layers.len();
        dirty.extend(self.layers.iter().map(|l| l.bounds));
        dirty
    }

    /// Build draw ops (and tighten bounds) for every unbuilt layer touching
    /// `area`. Full-detail paint, dirty-region repaint and export call this
    /// before drawing; LOD never does. Idempotent and cheap when done.
    pub fn ensure_area(
        &mut self,
        doc: &Document,
        area: Rect,
        measurer: &dyn TextMeasurer,
        obstacles: &ObstacleIndex,
    ) {
        if self.unbuilt == 0 {
            return;
        }
        for pos in 0..self.layers.len() {
            if self.layers[pos].ops_built || !self.layers[pos].bounds.intersects(&area) {
                continue;
            }
            let id = self.layers[pos].element_id.clone();
            let Some(element) = doc.element(&id).filter(|e| !e.is_deleted) else {
                continue;
            };
            let ops = build_element_ops(element, doc, measurer, obstacles);
            let bounds = layer_bounds(element, &ops, measurer);
            self.bounds_cache = Some(match self.bounds_cache {
                Some(acc) => acc.union(&bounds),
                None => bounds,
            });
            let layer = &mut self.layers[pos];
            layer.version = element.version;
            layer.bounds = bounds;
            layer.ops = ops;
            layer.ops_built = true;
            // Built at current positions: no deferred shift applies.
            self.preview_stale.remove(&layer.element_id.clone());
            self.unbuilt -= 1;
        }
    }

    /// Rebuild the id -> position map after the layer order changed.
    fn reindex(&mut self) {
        self.positions.clear();
        for (pos, layer) in self.layers.iter().enumerate() {
            self.positions.insert(layer.element_id.clone(), pos as u32);
        }
    }

    /// Reindex layers at `start..` (after an insert/remove shifted them).
    fn reindex_from(&mut self, start: usize) {
        for (pos, layer) in self.layers.iter().enumerate().skip(start) {
            self.positions.insert(layer.element_id.clone(), pos as u32);
        }
    }

    /// Map an LOD color to its palette index, interning on first sight.
    fn intern_color(&mut self, color: String) -> u32 {
        if let Some(pos) = self.palette.iter().position(|c| *c == color) {
            return pos as u32;
        }
        let idx = self.palette.len() as u32;
        self.palette.push(color);
        idx
    }

    /// The LOD palette: `layer.lod` indexes into this. Empty when no layer
    /// wants overview painting.
    pub fn palette(&self) -> &[String] {
        &self.palette
    }

    /// Layers still missing exact draw ops.
    pub fn unbuilt_count(&self) -> usize {
        self.unbuilt
    }

    /// Apply any deferred ops drift. Called before full-detail rendering and
    /// before incremental layer rebuilds, so cached points never disagree
    /// with bounds by more than the current frame's LOD pass.
    pub fn flush_pending_ops(&mut self) {
        let (dx, dy) = self.pending_ops;
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        for layer in &mut self.layers {
            for op in &mut layer.ops {
                match op {
                    DrawOp::Path(path) => {
                        for p in &mut path.points {
                            p.x += dx;
                            p.y += dy;
                        }
                    }
                    DrawOp::LineBatch { segments, .. } => {
                        for [a, b] in segments.iter_mut() {
                            a.x += dx;
                            a.y += dy;
                            b.x += dx;
                            b.y += dy;
                        }
                    }
                    DrawOp::Text(text) => {
                        text.x += dx;
                        text.y += dy;
                    }
                }
            }
        }
        self.pending_ops = (0.0, 0.0);
    }

    /// The layer position for an element, or where it would be inserted.
    fn layer_position(&self, index: &str, id: &str) -> Result<usize, usize> {
        self.layers.binary_search_by(|layer| {
            layer
                .index
                .as_str()
                .cmp(index)
                .then_with(|| layer.element_id.as_str().cmp(id))
        })
    }

    fn remove_layer(&mut self, id: &str, dirty: &mut Vec<Rect>) {
        let Some(&pos) = self.positions.get(id) else {
            return;
        };
        let pos = pos as usize;
        if pos >= self.layers.len() || self.layers[pos].element_id != id {
            // Map drifted (should not happen): fall back to a scan.
            if let Some(found) = self.layers.iter().position(|l| l.element_id == id) {
                dirty.push(self.layers[found].bounds);
                self.layers.remove(found);
                self.positions.remove(id);
                self.reindex_from(found);
            }
            return;
        }
        if !self.layers[pos].ops_built {
            self.unbuilt = self.unbuilt.saturating_sub(1);
        }
        dirty.push(self.layers[pos].bounds);
        self.layers.remove(pos);
        self.positions.remove(id);
        self.preview_current.remove(id);
        self.preview_stale.remove(id);
        self.reindex_from(pos);
    }

    /// Rebuild layers for the changed ids (added, updated or tombstoned
    /// elements), keep the rest, and report dirty rectangles in world space.
    ///
    /// Layers stay sorted by `(index, id)`, so an element whose order key did
    /// not change is replaced in place in `O(log n)` with no re-sort. Small
    /// changed sets splice incrementally; large ones (mass delete, undo, paste)
    /// rebuild the layer vector and position map in one pass so cost stays
    /// roughly linear instead of `O(changed × layers)`.
    pub fn update(
        &mut self,
        doc: &Document,
        changed: &BTreeSet<ElementId>,
        measurer: &dyn TextMeasurer,
        obstacles: &ObstacleIndex,
    ) -> Vec<Rect> {
        let bulk = self.should_rebuild_bulk(changed.len());
        self.update_with(doc, changed, measurer, obstacles, bulk)
    }

    fn update_with(
        &mut self,
        doc: &Document,
        changed: &BTreeSet<ElementId>,
        measurer: &dyn TextMeasurer,
        obstacles: &ObstacleIndex,
        bulk: bool,
    ) -> Vec<Rect> {
        // Incremental rebuilds produce ops at current positions: flush any
        // deferred drift first so the rebuilt layer and its neighbours agree.
        // Correctness over speed here; edits are infrequent next to drift
        // frames, and the flush runs at most once per edit burst.
        self.flush_pending_ops();
        // Preview-shifted layers from the in-flight gesture carry over as
        // dirty; the rebuild path below reports the rest.
        let mut dirty: Vec<Rect> = std::mem::take(&mut self.pending_dirty);
        if bulk {
            self.update_bulk(doc, changed, measurer, obstacles, &mut dirty);
        } else {
            self.update_incremental(doc, changed, measurer, obstacles, &mut dirty);
        }
        dirty
    }

    /// Whether a changed set is large enough that splicing each layer
    /// separately would move/reindex the vector far more than one rebuild.
    fn should_rebuild_bulk(&self, changed: usize) -> bool {
        changed >= BULK_MIN_CHANGED
            && changed.saturating_mul(self.layers.len().max(changed)) >= BULK_WORK_PAIRS
    }

    fn update_incremental(
        &mut self,
        doc: &Document,
        changed: &BTreeSet<ElementId>,
        measurer: &dyn TextMeasurer,
        obstacles: &ObstacleIndex,
        dirty: &mut Vec<Rect>,
    ) {
        for id in changed {
            let Some(element) = doc.element(id).filter(|e| !e.is_deleted) else {
                self.remove_layer(id, dirty);
                continue;
            };
            // Already current from the gesture preview: bring deferred
            // points up to date without a rebuild, then skip (one-shot mark,
            // consumed here so later edits rebuild normally). Unbuilt layers
            // fall through: there is nothing current to keep.
            if self.preview_current.remove(id)
                && self
                    .positions
                    .get(id)
                    .copied()
                    .map(|pos| pos as usize)
                    .is_some_and(|pos| {
                        pos < self.layers.len()
                            && self.layers[pos].element_id == *id
                            && self.layers[pos].ops_built
                    })
            {
                if self.preview_stale.remove(id) {
                    let total = self.preview_total;
                    if let Some(&pos) = self.positions.get(id) {
                        let pos = pos as usize;
                        if pos < self.layers.len() && self.layers[pos].element_id == *id {
                            shift_ops_points(&mut self.layers[pos].ops, total.0, total.1);
                        }
                    }
                    if self.preview_stale.is_empty() {
                        self.preview_total = (0.0, 0.0);
                    }
                }
                continue;
            }
            let ops = build_element_ops(element, doc, measurer, obstacles);
            let bounds = layer_bounds(element, &ops, measurer);
            let lod = lod_color_for(element).map(|c| self.intern_color(c));
            // Grow-only cache: unioning the new bounds keeps the cache a
            // superset of the true union (safe for the contains-all fast
            // path; removals/shrinks may only cause a missed fast path).
            self.bounds_cache = Some(match self.bounds_cache {
                Some(acc) => acc.union(&bounds),
                None => bounds,
            });
            let insert_at = match self.layer_position(&element.index, id) {
                Ok(pos) => {
                    dirty.push(self.layers[pos].bounds);
                    self.preview_stale.remove(id);
                    let layer = &mut self.layers[pos];
                    if !layer.ops_built {
                        layer.ops_built = true;
                        self.unbuilt = self.unbuilt.saturating_sub(1);
                    }
                    layer.version = element.version;
                    layer.bounds = bounds;
                    layer.ops = ops;
                    layer.lod = lod;
                    dirty.push(bounds);
                    continue;
                }
                Err(pos) => pos,
            };
            dirty.push(bounds);
            // Stale preview marks must not survive a structural rebuild.
            self.preview_current.remove(id);
            self.preview_stale.remove(id);
            let layer = SceneLayer {
                element_id: id.clone(),
                index: element.index.clone(),
                version: element.version,
                bounds,
                ops,
                ops_built: true,
                lod,
            };
            match self.positions.get(id).copied().map(|pos| pos as usize) {
                Some(old) if old < self.layers.len() && self.layers[old].element_id == *id => {
                    dirty.push(self.layers[old].bounds);
                    if !self.layers[old].ops_built {
                        self.unbuilt = self.unbuilt.saturating_sub(1);
                    }
                    self.layers.remove(old);
                    self.positions.remove(id);
                    let insert_at = if old < insert_at {
                        insert_at - 1
                    } else {
                        insert_at
                    };
                    self.layers.insert(insert_at, layer);
                    self.positions.insert(id.clone(), insert_at as u32);
                    self.reindex_from(insert_at.min(old));
                }
                _ => {
                    self.layers.insert(insert_at, layer);
                    self.positions.insert(id.clone(), insert_at as u32);
                    self.reindex_from(insert_at + 1);
                }
            }
        }
    }

    /// Rebuild the layer vector and position map in one pass for large changed
    /// sets. Keeps unchanged layers (with their cached ops, LOD tags and unbuilt
    /// state), rebuilds changed live elements exactly like the incremental path,
    /// drops tombstones, and merges the new layers into z-order with one linear
    /// merge instead of per-id `Vec::remove`/`insert` memmoves and reindexes.
    fn update_bulk(
        &mut self,
        doc: &Document,
        changed: &BTreeSet<ElementId>,
        measurer: &dyn TextMeasurer,
        obstacles: &ObstacleIndex,
        dirty: &mut Vec<Rect>,
    ) {
        // Layers that a gesture preview already brought up to date are kept
        // as-is: only their deferred ops points need the gesture-total shift.
        // This mirrors the per-id skip in `update_incremental`.
        let preview_total = self.preview_total;
        let mut keep: HashSet<ElementId> = HashSet::new();
        for id in changed {
            if doc.element(id).is_none_or(|e| e.is_deleted) {
                self.preview_current.remove(id);
                self.preview_stale.remove(id);
                continue;
            }
            if self.preview_current.remove(id) {
                let pos = self
                    .positions
                    .get(id)
                    .copied()
                    .map(|pos| pos as usize)
                    .filter(|&pos| {
                        pos < self.layers.len()
                            && self.layers[pos].element_id == *id
                            && self.layers[pos].ops_built
                    });
                if let Some(pos) = pos {
                    if self.preview_stale.remove(id) {
                        shift_ops_points(
                            &mut self.layers[pos].ops,
                            preview_total.0,
                            preview_total.1,
                        );
                    }
                    keep.insert(id.clone());
                }
            }
        }
        if self.preview_stale.is_empty() {
            self.preview_total = (0.0, 0.0);
        }

        // Build the replacement layers before touching the vector so the
        // bound cache and palette can be updated without split borrows.
        let mut added = Vec::with_capacity(changed.len().saturating_sub(keep.len()));
        for id in changed {
            if keep.contains(id) {
                continue;
            }
            self.preview_stale.remove(id);
            let Some(element) = doc.element(id).filter(|e| !e.is_deleted) else {
                continue;
            };
            let ops = build_element_ops(element, doc, measurer, obstacles);
            let bounds = layer_bounds(element, &ops, measurer);
            let lod = lod_color_for(element).map(|c| self.intern_color(c));
            // Grow-only cache, same semantics as the incremental path.
            self.bounds_cache = Some(match self.bounds_cache {
                Some(acc) => acc.union(&bounds),
                None => bounds,
            });
            added.push(SceneLayer {
                element_id: id.clone(),
                index: element.index.clone(),
                version: element.version,
                bounds,
                ops,
                ops_built: true,
                lod,
            });
        }

        // One pass over the old layers: drop changed ids (reporting their old
        // bounds) and keep everything else, including preview marks.
        let mut layers = Vec::with_capacity(self.layers.len() + added.len());
        for layer in std::mem::take(&mut self.layers) {
            if changed.contains(&layer.element_id) && !keep.contains(&layer.element_id) {
                dirty.push(layer.bounds);
            } else {
                layers.push(layer);
            }
        }
        for layer in &added {
            dirty.push(layer.bounds);
        }

        added.sort_unstable_by(layer_order);
        self.layers = merge_layers(layers, added);
        self.reindex();
        self.unbuilt = self.layers.iter().filter(|layer| !layer.ops_built).count();
    }

    /// Rigid bulk translation for the all-elements-moving workload: shift
    /// every cached bound in place and defer the ops-point shift until full
    /// detail is actually needed. Bounds move eagerly (O(layers), contiguous
    /// Vec, ~0.2ms at 100k) so the LOD overview — which reads bounds only —
    /// stays exact every frame with no RNG, no hachure rebuild, no
    /// allocation. Cached points accumulate into `pending_ops` and are
    /// flushed once by `flush_pending_ops` before the next full-detail draw
    /// or incremental rebuild. The caller must shift the document the same
    /// way; bound connectors shift with their targets so bindings stay valid.
    pub fn shift_all(&mut self, dx: f64, dy: f64) {
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        for layer in &mut self.layers {
            layer.bounds.x += dx;
            layer.bounds.y += dy;
        }
        self.pending_ops.0 += dx;
        self.pending_ops.1 += dy;
        if let Some(cached) = self.bounds_cache.as_mut() {
            cached.x += dx;
            cached.y += dy;
        }
    }
}

/// Shift every cached draw point of one layer's ops by `(dx, dy)`.
fn shift_ops_points(ops: &mut [DrawOp], dx: f64, dy: f64) {
    for op in ops {
        match op {
            DrawOp::Path(path) => {
                for p in &mut path.points {
                    p.x += dx;
                    p.y += dy;
                }
            }
            DrawOp::LineBatch { segments, .. } => {
                for [a, b] in segments.iter_mut() {
                    a.x += dx;
                    a.y += dy;
                    b.x += dx;
                    b.y += dy;
                }
            }
            DrawOp::Text(text) => {
                text.x += dx;
                text.y += dy;
            }
        }
    }
}

impl Scene {
    /// Slot-based subset shift: rigid in-place shift addressed
    /// by layer positions resolved once per gesture (see
    /// `Editor::drag_slots`). Bounds move eagerly; cached points follow
    /// lazily via [`Scene::flush_preview_stale`] (at overview zoom nothing
    /// ever reads them). No map lookups, no id clones per move.
    pub fn shift_slots(&mut self, slots: &[u32], dx: f64, dy: f64) {
        if dx == 0.0 && dy == 0.0 || slots.is_empty() {
            return;
        }
        self.flush_pending_ops();
        for &slot in slots {
            let pos = slot as usize;
            if pos >= self.layers.len() {
                continue;
            }
            let layer = &mut self.layers[pos];
            self.pending_dirty.push(layer.bounds);
            layer.bounds.x += dx;
            layer.bounds.y += dy;
            self.pending_dirty.push(layer.bounds);
            // Probe before cloning: members persist for the gesture, so only
            // the first shift per layer allocates.
            if !self.preview_current.contains(layer.element_id.as_str()) {
                self.preview_current.insert(layer.element_id.clone());
            }
            if !self.preview_stale.contains(layer.element_id.as_str()) {
                self.preview_stale.insert(layer.element_id.clone());
            }
        }
        self.preview_total.0 += dx;
        self.preview_total.1 += dy;
        if let Some(cached) = self.bounds_cache.as_mut() {
            let shift = Rect::new(dx.min(0.0), dy.min(0.0), dx.abs(), dy.abs());
            *cached = cached.union(&Rect::new(
                cached.x + shift.x,
                cached.y + shift.y,
                cached.w + shift.w,
                cached.h + shift.h,
            ));
        }
    }

    /// Shift the cached points of every stale layer by the gesture total and
    /// unmark them. `area` bounds the work: only stale layers intersecting it
    /// are flushed (full-detail paint passes exactly what they redraw).
    pub fn flush_preview_stale(&mut self, area: Rect) {
        self.flush_pending_ops();
        if self.preview_stale.is_empty() {
            return;
        }
        let (dx, dy) = self.preview_total;
        let mut done: Vec<ElementId> = Vec::new();
        for id in &self.preview_stale {
            let Some(&pos) = self.positions.get(id) else {
                continue;
            };
            let pos = pos as usize;
            if pos >= self.layers.len() || self.layers[pos].element_id != *id {
                continue;
            }
            if !self.layers[pos].bounds.intersects(&area) {
                continue;
            }
            shift_ops_points(&mut self.layers[pos].ops, dx, dy);
            done.push(id.clone());
        }
        for id in done {
            self.preview_stale.remove(&id);
        }
        if self.preview_stale.is_empty() {
            self.preview_total = (0.0, 0.0);
        }
    }

    /// Shift the cached points of every stale layer by the gesture total.
    /// For exports, which draw all layers regardless of viewport.
    pub fn flush_all_preview_stale(&mut self) {
        if self.preview_stale.is_empty() {
            return;
        }
        let (dx, dy) = self.preview_total;
        let ids: Vec<ElementId> = self.preview_stale.iter().cloned().collect();
        for id in ids {
            let Some(&pos) = self.positions.get(&id) else {
                continue;
            };
            let pos = pos as usize;
            if pos >= self.layers.len() || self.layers[pos].element_id != id {
                continue;
            }
            shift_ops_points(&mut self.layers[pos].ops, dx, dy);
            self.preview_stale.remove(&id);
        }
        self.preview_total = (0.0, 0.0);
    }

    /// Layers whose bounds intersect the viewport, in z-order.
    pub fn visible(&self, viewport: Rect) -> impl Iterator<Item = &SceneLayer> {
        self.layers
            .iter()
            .filter(move |layer| layer.bounds.intersects(&viewport))
    }

    /// Replace one layer's ops with a caller-built preview (first rubber
    /// band of a gesture; the displaced real ops are dropped and rebuilt
    /// exactly on release). Deliberately outside `preview_current`.
    pub fn set_preview_ops(&mut self, id: &str, ops: Vec<DrawOp>, bounds: Rect) {
        let Some(&pos) = self.positions.get(id) else {
            return;
        };
        let pos = pos as usize;
        if pos >= self.layers.len() || self.layers[pos].element_id != id {
            return;
        }
        let layer = &mut self.layers[pos];
        self.pending_dirty.push(layer.bounds);
        layer.ops = ops;
        layer.bounds = bounds;
        self.pending_dirty.push(bounds);
        // Swapped-in preview ops are already current: never shift or skip.
        self.preview_stale.remove(id);
        if let Some(cached) = self.bounds_cache.as_mut() {
            *cached = cached.union(&bounds);
        }
    }

    /// Refresh installed preview ops in place: same op shape, new points, no
    /// allocation past residual capacity. Only valid after `set_preview_ops`.
    pub fn refresh_preview_ops(
        &mut self,
        id: &str,
        start: crate::geometry::Point,
        end: crate::geometry::Point,
        width: f64,
        bounds: Rect,
    ) {
        let Some(&pos) = self.positions.get(id) else {
            return;
        };
        let pos = pos as usize;
        if pos >= self.layers.len() || self.layers[pos].element_id != id {
            return;
        }
        let layer = &mut self.layers[pos];
        self.pending_dirty.push(layer.bounds);
        crate::rough::rubber_band_refresh(&mut layer.ops, start, end, width);
        layer.bounds = bounds;
        self.pending_dirty.push(bounds);
        if let Some(cached) = self.bounds_cache.as_mut() {
            *cached = cached.union(&bounds);
        }
    }

    /// Drop the preview-skip mark for one layer so the next `update`
    /// rebuilds it exactly (used when release-time changes supersede the
    /// rigid preview, e.g. reflowed labels).
    pub fn unmark_preview(&mut self, id: &str) {
        self.preview_current.remove(id);
    }

    /// Drain preview dirty rects for a mid-gesture repaint (no rebuild).
    pub fn preview_dirty(&mut self) -> Vec<Rect> {
        std::mem::take(&mut self.pending_dirty)
    }

    /// Drop preview state after a cancelled gesture; the caller rebuilds the
    /// affected layers exactly through `update`.
    pub fn discard_preview(&mut self) {
        self.preview_current.clear();
        self.preview_stale.clear();
        self.preview_total = (0.0, 0.0);
        self.pending_dirty.clear();
    }

    /// Iterate without per-layer intersection tests when the caller already
    /// knows every layer is visible (overview fast path). The caller must
    /// have checked `cached_bounds` against the viewport.
    pub fn all_layers(&self) -> impl Iterator<Item = &SceneLayer> {
        self.layers.iter()
    }

    pub fn layer_for(&self, id: &str) -> Option<&SceneLayer> {
        self.positions
            .get(id)
            .copied()
            .map(|pos| pos as usize)
            .filter(|&pos| pos < self.layers.len() && self.layers[pos].element_id == id)
            .map(|pos| &self.layers[pos])
    }

    /// Layer position for an id, if the map agrees with the vector.
    pub fn slot_for(&self, id: &str) -> Option<u32> {
        self.positions.get(id).copied().filter(|&pos| {
            let pos = pos as usize;
            pos < self.layers.len() && self.layers[pos].element_id == id
        })
    }

    /// Cached union of every layer's bounds (`None` when empty). Grow-only
    /// between rebuilds, so always a superset of the true union: safe for
    /// the renderer's contains-all fast path.
    pub fn cached_bounds(&self) -> Option<Rect> {
        self.bounds_cache
    }

    /// Union of every layer's bounds.
    pub fn bounds(&self) -> Option<Rect> {
        // Prefer the cache; fall back to a scan for scenes built before the
        // cache existed (tests constructing layers by hand).
        if let Some(cached) = self.bounds_cache {
            return Some(cached);
        }
        self.layers
            .iter()
            .map(|l| l.bounds)
            .reduce(|acc, r| acc.union(&r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Point;
    use crate::model::{ElementKind, ShapeKind, Style};

    fn shape(x: f64) -> ElementKind {
        ElementKind::Shape {
            shape: ShapeKind::Rectangle,
            rect: Rect::new(x, 0.0, 50.0, 40.0),
            style: Style::default(),
            bound_text_id: None,
        }
    }

    #[test]
    fn rebuild_all_caches_live_elements_only() {
        let mut doc = Document::new();
        let a = doc.add(shape(0.0), 1);
        let b = doc.add(shape(100.0), 1);
        doc.element_mut(&b).unwrap().is_deleted = true;

        let mut scene = Scene::new();
        scene.rebuild_all(&doc, &crate::text::ApproxMeasurer, &ObstacleIndex::new());
        assert_eq!(scene.layers.len(), 1);
        assert_eq!(scene.layers[0].element_id, a);
    }

    #[test]
    fn update_reports_old_and_new_bounds() {
        let mut doc = Document::new();
        let id = doc.add(shape(0.0), 1);
        let mut scene = Scene::new();
        scene.rebuild_all(&doc, &crate::text::ApproxMeasurer, &ObstacleIndex::new());

        doc.element_mut(&id).unwrap().touch(2);
        if let ElementKind::Shape { rect, .. } = &mut doc.element_mut(&id).unwrap().kind {
            rect.x = 500.0;
        }
        let changed: BTreeSet<_> = [id.clone()].into_iter().collect();
        let dirty = scene.update(
            &doc,
            &changed,
            &crate::text::ApproxMeasurer,
            &ObstacleIndex::new(),
        );
        assert_eq!(dirty.len(), 2, "old and new bounds are both dirty");
        let covers_old = dirty.iter().any(|r| r.contains(Point::new(25.0, 20.0)));
        let covers_new = dirty.iter().any(|r| r.contains(Point::new(525.0, 20.0)));
        assert!(covers_old && covers_new);
    }

    #[test]
    fn update_removes_layers_for_deleted_elements() {
        let mut doc = Document::new();
        let id = doc.add(shape(0.0), 1);
        let mut scene = Scene::new();
        scene.rebuild_all(&doc, &crate::text::ApproxMeasurer, &ObstacleIndex::new());
        assert_eq!(scene.layers.len(), 1);

        doc.element_mut(&id).unwrap().is_deleted = true;
        let changed: BTreeSet<_> = [id].into_iter().collect();
        let dirty = scene.update(
            &doc,
            &changed,
            &crate::text::ApproxMeasurer,
            &ObstacleIndex::new(),
        );
        assert!(scene.layers.is_empty());
        assert_eq!(dirty.len(), 1);
    }

    #[test]
    fn visible_filters_by_viewport() {
        let mut doc = Document::new();
        doc.add(shape(0.0), 1);
        doc.add(shape(1000.0), 1);
        let mut scene = Scene::new();
        scene.rebuild_all(&doc, &crate::text::ApproxMeasurer, &ObstacleIndex::new());
        let visible: Vec<_> = scene.visible(Rect::new(0.0, 0.0, 200.0, 200.0)).collect();
        assert_eq!(visible.len(), 1);
    }

    #[test]
    fn layer_order_follows_document_order() {
        let mut doc = Document::new();
        let a = doc.add(shape(0.0), 1);
        let b = doc.add(shape(10.0), 1);
        let mut scene = Scene::new();
        scene.rebuild_all(&doc, &crate::text::ApproxMeasurer, &ObstacleIndex::new());
        assert_eq!(
            scene
                .layers
                .iter()
                .map(|l| l.element_id.clone())
                .collect::<Vec<_>>(),
            vec![a, b]
        );
    }

    #[test]
    fn rebuild_is_lazy_and_approx_bounds_cover_exact() {
        // Mixed fixtures incl. routed arrows, labels and frames: approximate
        // bounds must cover the exact painted bounds of every layer, so
        // culling and LOD rects built before ops exist stay correct.
        for mut doc in [
            crate::benchmark_fixture(500),
            crate::benchmark::connected_fixture(200, 2),
            crate::demo_document(),
        ] {
            // Cover frames/labels/arrows, not just shapes.
            let mut scene = Scene::new();
            scene.rebuild_all(&doc, &crate::text::ApproxMeasurer, &ObstacleIndex::new());
            assert_eq!(scene.unbuilt_count(), scene.layers.len());
            assert!(scene
                .layers
                .iter()
                .all(|l| !l.ops_built && l.ops.is_empty()));
            let approx: Vec<Rect> = scene.layers.iter().map(|l| l.bounds).collect();
            // True paint extents, built directly (bypasses the scene cache),
            // keyed by id: layers run in z-order, the store in id order.
            let obstacles = ObstacleIndex::new();
            let mut paint: std::collections::HashMap<String, Rect> =
                std::collections::HashMap::new();
            for element in doc.elements.values().filter(|e| !e.is_deleted) {
                let ops = crate::rough::build_element_ops(
                    element,
                    &doc,
                    &crate::text::ApproxMeasurer,
                    &obstacles,
                );
                let mut truth = element.bounds();
                if let Some(op_bounds) = super::ops_bounds(&ops, &crate::text::ApproxMeasurer) {
                    truth = truth.union(&op_bounds);
                }
                paint.insert(element.id.clone(), truth);
            }
            let everywhere = Rect::new(-1e9, -1e9, 2e9, 2e9);
            scene.ensure_area(
                &doc,
                everywhere,
                &crate::text::ApproxMeasurer,
                &ObstacleIndex::new(),
            );
            assert_eq!(scene.unbuilt_count(), 0);
            for (layer, old) in scene.layers.iter().zip(approx.iter()) {
                assert!(layer.ops_built, "{}", layer.element_id);
                let truth = paint.get(&layer.element_id).expect("painted");
                assert!(
                    old.x <= truth.x + 1e-6
                        && old.y <= truth.y + 1e-6
                        && old.max_x() + 1e-6 >= truth.max_x()
                        && old.max_y() + 1e-6 >= truth.max_y(),
                    "{}: approx {old:?} must cover paint {truth:?}",
                    layer.element_id
                );
            }
            // Second ensure is a no-op.
            scene.ensure_area(
                &doc,
                everywhere,
                &crate::text::ApproxMeasurer,
                &ObstacleIndex::new(),
            );
            let _ = &mut doc;
        }
    }

    /// Normalize dirty rects for order-insensitive comparison.
    fn dirty_key(rects: &[Rect]) -> Vec<(u64, u64, u64, u64)> {
        let mut keys: Vec<_> = rects
            .iter()
            .map(|r| (r.x.to_bits(), r.y.to_bits(), r.w.to_bits(), r.h.to_bits()))
            .collect();
        keys.sort_unstable();
        keys
    }

    /// A mixed mass edit: tombstones, moved geometry, changed z-order and new
    /// elements. Exercises every structural branch the bulk path must mirror.
    fn mass_edit(doc: &mut Document) {
        let ids: Vec<ElementId> = doc.ordered_live().iter().map(|e| e.id.clone()).collect();
        for (i, id) in ids.iter().enumerate() {
            let element = doc.element_mut(id).expect("live");
            element.touch(100);
            match i % 9 {
                0 | 4 => element.is_deleted = true,
                2 => {
                    if let ElementKind::Shape { rect, .. } = &mut element.kind {
                        rect.x += 700.0;
                        rect.y += 300.0;
                    }
                }
                _ => {}
            }
        }
        // Swap order keys pairwise so some layers move instead of just rebuild.
        for pair in ids.chunks(2).filter(|pair| pair.len() == 2) {
            let a_index = doc.element(&pair[0]).expect("live").index.clone();
            let b_index = doc.element(&pair[1]).expect("live").index.clone();
            doc.element_mut(&pair[0]).expect("live").index = b_index;
            doc.element_mut(&pair[1]).expect("live").index = a_index;
        }
        for i in 0..12 {
            doc.add(shape(5000.0 + i as f64), 101);
        }
    }

    #[test]
    fn bulk_update_matches_incremental_layers_and_dirty() {
        let base = crate::benchmark_fixture(300);
        let mut doc = base.clone();
        mass_edit(&mut doc);
        let changed: BTreeSet<ElementId> = doc.elements.keys().cloned().collect();
        let measurer = crate::text::ApproxMeasurer;
        let obstacles = ObstacleIndex::new();

        let mut incremental = Scene::new();
        incremental.rebuild_all(&base, &measurer, &obstacles);
        let mut bulk = Scene::new();
        bulk.rebuild_all(&base, &measurer, &obstacles);

        let incremental_dirty =
            incremental.update_with(&doc, &changed, &measurer, &obstacles, false);
        let bulk_dirty = bulk.update_with(&doc, &changed, &measurer, &obstacles, true);

        assert_eq!(bulk.layers, incremental.layers);
        assert_eq!(bulk.unbuilt_count(), incremental.unbuilt_count());
        assert_eq!(bulk.cached_bounds(), incremental.cached_bounds());
        assert_eq!(bulk.palette(), incremental.palette());
        assert_eq!(dirty_key(&bulk_dirty), dirty_key(&incremental_dirty));
        for (pos, layer) in bulk.layers.iter().enumerate() {
            assert_eq!(
                bulk.slot_for(&layer.element_id),
                Some(pos as u32),
                "position map agrees with z-order"
            );
        }
    }

    #[test]
    fn bulk_update_keeps_preview_layers_current() {
        const N: usize = 240;
        const MOVED: usize = 180;
        let base = crate::benchmark_fixture(N);
        let mut doc = base.clone();
        let ids: Vec<ElementId> = doc
            .ordered_live()
            .iter()
            .take(MOVED)
            .map(|e| e.id.clone())
            .collect();
        let measurer = crate::text::ApproxMeasurer;
        let obstacles = ObstacleIndex::new();
        let everywhere = Rect::new(-1e9, -1e9, 2e9, 2e9);

        let mut incremental = Scene::new();
        incremental.rebuild_all(&base, &measurer, &obstacles);
        incremental.ensure_area(&base, everywhere, &measurer, &obstacles);
        let mut bulk = Scene::new();
        bulk.rebuild_all(&base, &measurer, &obstacles);
        bulk.ensure_area(&base, everywhere, &measurer, &obstacles);
        let slots: Vec<u32> = ids
            .iter()
            .map(|id| incremental.slot_for(id).expect("preview slot"))
            .collect();
        incremental.shift_slots(&slots, 12.0, -7.0);
        bulk.shift_slots(&slots, 12.0, -7.0);

        // Release-time document: the previewed motion, one previewed delete.
        for (i, id) in ids.iter().enumerate() {
            let element = doc.element_mut(id).expect("live");
            if i == 0 {
                element.is_deleted = true;
            } else {
                crate::translate_element(element, 12.0, -7.0);
                element.touch(50);
            }
        }
        let changed: BTreeSet<ElementId> = ids.iter().cloned().collect();
        let incremental_dirty =
            incremental.update_with(&doc, &changed, &measurer, &obstacles, false);
        let bulk_dirty = bulk.update_with(&doc, &changed, &measurer, &obstacles, true);

        assert_eq!(bulk.layers, incremental.layers);
        assert_eq!(dirty_key(&bulk_dirty), dirty_key(&incremental_dirty));
        assert_eq!(bulk.unbuilt_count(), 0);
        // Previewed layers were kept as-is: no exact rebuild bumped their
        // version, and the deleted one is gone.
        for id in ids.iter().skip(1) {
            let layer = bulk.layer_for(id).expect("preview layer kept");
            assert_eq!(layer.version, base.element(id).unwrap().version);
        }
        assert!(bulk.layer_for(&ids[0]).is_none());
    }

    #[test]
    fn mass_delete_and_restore_keep_z_order_and_dirty_coverage() {
        let base = crate::benchmark_fixture(400);
        let mut doc = base.clone();
        let ids: Vec<ElementId> = doc.ordered_live().iter().map(|e| e.id.clone()).collect();
        let measurer = crate::text::ApproxMeasurer;
        let obstacles = ObstacleIndex::new();
        let mut scene = Scene::new();
        scene.rebuild_all(&base, &measurer, &obstacles);
        let old_bounds: Vec<Rect> = scene.layers.iter().map(|l| l.bounds).collect();
        let changed: BTreeSet<ElementId> = ids.iter().cloned().collect();

        for id in &ids {
            doc.element_mut(id).unwrap().is_deleted = true;
        }
        let dirty = scene.update(&doc, &changed, &measurer, &obstacles);
        assert!(scene.layers.is_empty());
        assert_eq!(scene.unbuilt_count(), 0);
        for bounds in old_bounds {
            assert!(dirty.contains(&bounds), "deleted layer bounds are dirty");
        }

        // Undo restores the original store; layers come back in document order.
        doc = base.clone();
        let dirty = scene.update(&doc, &changed, &measurer, &obstacles);
        let expected: Vec<ElementId> = base.ordered_live().iter().map(|e| e.id.clone()).collect();
        assert_eq!(
            scene
                .layers
                .iter()
                .map(|l| l.element_id.clone())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(scene.unbuilt_count(), 0);
        for layer in &scene.layers {
            assert!(layer.ops_built);
            assert!(dirty.contains(&layer.bounds), "restored bounds are dirty");
        }
    }
}
