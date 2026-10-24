//! A coarse uniform grid over obstacle elements, for connector routing.
//!
//! Routing must not scan every element per arrow: at 100k elements that is
//! quadratic. The index is kept in sync with the document by the editor as
//! elements change, and rebuilt wholesale when the document is replaced.

use std::collections::{BTreeMap, BTreeSet};

use crate::geometry::Rect;
use crate::model::{Document, Element, ElementId};

/// World-space size of one grid cell. Obstacles (shapes, glyphs, text) are
/// tens to hundreds of units; 256 keeps most queries to a handful of cells.
const CELL: f64 = 256.0;

/// An element the router should keep away from. Frames are deliberately not
/// obstacles: arrows entering a VPC/region box across its border is normal.
/// Connectors and stacks are not bind targets and never route obstacles.
pub fn is_obstacle(element: &Element) -> bool {
    !element.is_deleted && element.is_bind_target() && !element.is_frame()
}

/// Packed hit-test index: cache-friendly SoA bounds plus a uniform grid.
///
/// Hover, click and marquee selection run on every pointer event, so they may
/// never scan the document: a full scan with per-arrow route recomputation
/// costs ~8ms per mousemove at 100k elements (20ms+ at 200k). This index
/// answers point/rect queries from a handful of grid cells instead.
///
/// Layout is deliberate: bounds live in four parallel `Vec<f32>` (16 bytes
/// per element, one cache line per 4 elements), grid buckets hold `u32`
/// positions into those arrays (no strings, no pointers in the hot loop).
/// Exact geometry still runs per candidate through `hit::hits`, so results
/// match the full scan exactly.
#[derive(Clone, Debug, Default)]
pub struct HitIndex {
    /// Model bounds, SoA: [x0, y0, x1, y1] per element.
    bx0: Vec<f32>,
    by0: Vec<f32>,
    bx1: Vec<f32>,
    by1: Vec<f32>,
    /// Element id per row, in insertion order.
    ids: Vec<ElementId>,
    /// Z-order key per row, for topmost resolution among candidates.
    keys: Vec<String>,
    /// Whether the row is live (edits append; dead rows compact lazily).
    live: Vec<bool>,
    /// Grid cell -> row positions.
    cells: BTreeMap<(i32, i32), Vec<u32>>,
    /// Id -> row, for O(1) invalidation on edit.
    pos: std::collections::HashMap<ElementId, u32>,
    /// Dead-row count since the last compaction.
    dead: usize,
    /// Deferred drift (see `shift_all`): added to bounds and subtracted from
    /// query cells on the fly; flushed by the next rebuild/apply.
    pending: (f64, f64),
}

/// World-space size of one hit-test cell.
const HIT_CELL: f64 = 256.0;
/// Query padding beyond tolerance: covers the widest stroke (64/2) plus
/// jitter margin. Candidates are exact-tested, so padding only costs a few
/// extra candidates, never wrong results.
const HIT_PAD: f64 = 40.0;

pub(crate) fn hit_pad() -> f64 {
    HIT_PAD
}

fn hit_cells(x0: f64, y0: f64, x1: f64, y1: f64) -> impl Iterator<Item = (i32, i32)> {
    let (cx0, cy0) = (
        (x0 / HIT_CELL).floor() as i32,
        (y0 / HIT_CELL).floor() as i32,
    );
    let (cx1, cy1) = (
        (x1 / HIT_CELL).floor() as i32,
        (y1 / HIT_CELL).floor() as i32,
    );
    (cx0..=cx1).flat_map(move |cx| (cy0..=cy1).map(move |cy| (cx, cy)))
}

impl HitIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.ids.len() - self.dead
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Build over every live, visible element.
    pub fn rebuild(&mut self, doc: &Document) {
        self.clear();
        self.pending = (0.0, 0.0);
        for element in doc.elements.values() {
            if element.is_deleted || super::hit::invisible(element) {
                continue;
            }
            self.push(element.id.clone(), element.index.clone(), element.bounds());
        }
    }

    fn clear(&mut self) {
        self.bx0.clear();
        self.by0.clear();
        self.bx1.clear();
        self.by1.clear();
        self.ids.clear();
        self.keys.clear();
        self.live.clear();
        self.cells.clear();
        self.pos.clear();
        self.dead = 0;
    }

    fn push(&mut self, id: ElementId, key: String, bounds: Rect) {
        let row = self.ids.len() as u32;
        self.pos.insert(id.clone(), row);
        self.ids.push(id);
        self.keys.push(key);
        self.live.push(true);
        self.bx0.push(bounds.x as f32);
        self.by0.push(bounds.y as f32);
        self.bx1.push(bounds.max_x() as f32);
        self.by1.push(bounds.max_y() as f32);
        for cell in hit_cells(bounds.x, bounds.y, bounds.max_x(), bounds.max_y()) {
            self.cells.entry(cell).or_default().push(row);
        }
    }

    /// Rebuild the rows for `changed` (added, updated or tombstoned).
    /// Flushes deferred drift first: edits address current positions.
    pub fn apply(&mut self, doc: &Document, changed: &std::collections::BTreeSet<ElementId>) {
        self.flush_pending();
        for id in changed {
            if let Some(&row) = self.pos.get(id) {
                let row = row as usize;
                if self.live[row] {
                    self.live[row] = false;
                    self.dead += 1;
                }
            }
            self.pos.remove(id);
            if let Some(element) = doc.element(id).filter(|e| !e.is_deleted) {
                if super::hit::invisible(element) {
                    continue;
                }
                self.push(id.clone(), element.index.clone(), element.bounds());
            }
        }
        if self.dead > self.ids.len() / 2 && !self.ids.is_empty() {
            self.compact();
        }
    }

    /// Drop dead rows and repack every array (amortized: runs only after the
    /// dead set outgrows the live set).
    fn compact(&mut self) {
        let mut compacted = HitIndex::new();
        for row in 0..self.ids.len() {
            if !self.live[row] {
                continue;
            }
            compacted.push(
                std::mem::take(&mut self.ids[row]),
                std::mem::take(&mut self.keys[row]),
                Rect::new(
                    self.bx0[row] as f64,
                    self.by0[row] as f64,
                    (self.bx1[row] - self.bx0[row]) as f64,
                    (self.by1[row] - self.by0[row]) as f64,
                ),
            );
        }
        *self = compacted;
    }

    /// O(1) bulk drift: bounds and cells stay put; queries compensate.
    /// Rendering never queries hits, so shift+render loops pay nothing here.
    pub fn shift_all(&mut self, dx: f64, dy: f64) {
        if !dx.is_finite() || !dy.is_finite() || (dx == 0.0 && dy == 0.0) {
            return;
        }
        self.pending.0 += dx;
        self.pending.1 += dy;
    }

    fn flush_pending(&mut self) {
        let (dx, dy) = self.pending;
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        self.pending = (0.0, 0.0);
        if self.ids.is_empty() {
            return;
        }
        let (dx, dy) = (dx as f32, dy as f32);
        for row in 0..self.ids.len() {
            self.bx0[row] += dx;
            self.by0[row] += dy;
            self.bx1[row] += dx;
            self.by1[row] += dy;
        }
        // Cells moved: rebucket live rows only.
        self.cells.clear();
        for row in 0..self.ids.len() {
            if !self.live[row] {
                continue;
            }
            let row_u = row as u32;
            for cell in hit_cells(
                self.bx0[row] as f64,
                self.by0[row] as f64,
                self.bx1[row] as f64,
                self.by1[row] as f64,
            ) {
                self.cells.entry(cell).or_default().push(row_u);
            }
        }
    }

    /// Row positions whose cached bounds intersect `area` (model bounds, same
    /// reject the exact test uses). Applies deferred drift both sides.
    pub fn query(&self, area: Rect) -> Vec<u32> {
        let (dx, dy) = self.pending;
        let lookup = Rect::new(area.x - dx, area.y - dy, area.w, area.h);
        let mut out: Vec<u32> = Vec::new();
        for cell in hit_cells(lookup.x, lookup.y, lookup.max_x(), lookup.max_y()) {
            if let Some(rows) = self.cells.get(&cell) {
                out.extend(rows.iter().copied());
            }
        }
        // Filter by (drifted) bounds; dedupe multi-cell rows.
        out.sort_unstable();
        out.dedup();
        let (ax0, ay0, ax1, ay1) = (
            area.x as f32,
            area.y as f32,
            area.max_x() as f32,
            area.max_y() as f32,
        );
        out.into_iter()
            .filter(|&row| {
                let row = row as usize;
                row < self.live.len()
                    && self.live[row]
                    && (self.bx0[row] + dx as f32) <= ax1
                    && (self.bx1[row] + dx as f32) >= ax0
                    && (self.by0[row] + dy as f32) <= ay1
                    && (self.by1[row] + dy as f32) >= ay0
            })
            .collect()
    }

    /// Id, z-key and drifted model bounds for a query row.
    pub fn row(&self, row: u32) -> Option<(&str, &str, Rect)> {
        let row = row as usize;
        if row >= self.ids.len() || !self.live[row] {
            return None;
        }
        let (dx, dy) = self.pending;
        Some((
            self.ids[row].as_str(),
            self.keys[row].as_str(),
            Rect::new(
                self.bx0[row] as f64 + dx,
                self.by0[row] as f64 + dy,
                (self.bx1[row] - self.bx0[row]) as f64,
                (self.by1[row] - self.by0[row]) as f64,
            ),
        ))
    }
}

#[derive(Clone, Debug, Default)]
pub struct ObstacleIndex {
    buckets: BTreeMap<(i32, i32), Vec<ElementId>>,
    bounds: BTreeMap<ElementId, Rect>,
    /// Accumulated drift applied to the document/scene but not yet to these
    /// bounds/buckets (see `shift_all`). O(1) per tick: pure shift+render
    /// loops never query obstacles, so they pay nothing here. Flushed on the
    /// next `apply`/`rebuild`; `query` adds it on the fly while pending.
    pending: (f64, f64),
    /// Whether `bounds`/`buckets` are built. Loading a huge document skips
    /// the build (tens of ms plus megabytes); the first edit or route that
    /// needs obstacles builds them once via [`ObstacleIndex::ensure`].
    /// Queries on an unbuilt index return empty, as before.
    built: bool,
}

impl ObstacleIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the index over every obstacle in the document.
    pub fn rebuild(&mut self, doc: &Document) {
        self.buckets.clear();
        self.bounds.clear();
        // Rebuilt from the (already shifted) document: positions exact.
        self.pending = (0.0, 0.0);
        self.built = true;
        for element in doc.elements.values() {
            if is_obstacle(element) {
                let rect = element.bounds();
                self.bounds.insert(element.id.clone(), rect);
                self.bucket(&element.id, rect);
            }
        }
    }

    /// Number of indexed obstacles (tests, diagnostics).
    pub fn len(&self) -> usize {
        self.bounds.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bounds.is_empty()
    }

    fn cell_range(rect: Rect) -> (i32, i32, i32, i32) {
        (
            (rect.x / CELL).floor() as i32,
            (rect.y / CELL).floor() as i32,
            (rect.max_x() / CELL).floor() as i32,
            (rect.max_y() / CELL).floor() as i32,
        )
    }

    fn bucket(&mut self, id: &str, rect: Rect) {
        let (x0, y0, x1, y1) = Self::cell_range(rect);
        for cx in x0..=x1 {
            for cy in y0..=y1 {
                self.buckets
                    .entry((cx, cy))
                    .or_default()
                    .push(id.to_string());
            }
        }
    }

    fn unbucket(&mut self, id: &str, rect: Rect) {
        let (x0, y0, x1, y1) = Self::cell_range(rect);
        for cx in x0..=x1 {
            for cy in y0..=y1 {
                if let Some(ids) = self.buckets.get_mut(&(cx, cy)) {
                    ids.retain(|existing| existing != id);
                    if ids.is_empty() {
                        self.buckets.remove(&(cx, cy));
                    }
                }
            }
        }
    }

    /// Insert, update or remove the elements in `changed` to match the doc.
    /// Builds the index first when loading skipped it (see `ensure`).
    pub fn apply(&mut self, doc: &Document, changed: &BTreeSet<ElementId>) {
        self.ensure(doc);
        self.flush_pending();
        for id in changed {
            if let Some(old) = self.bounds.remove(id) {
                self.unbucket(id, old);
            }
            let Some(element) = doc.element(id) else {
                continue;
            };
            if is_obstacle(element) {
                let rect = element.bounds();
                self.bounds.insert(id.clone(), rect);
                self.bucket(id, rect);
            }
        }
    }

    /// Build the index when loading skipped it. One O(n) pass, once: edits
    /// and routes before the first build all funnel through here.
    pub fn ensure(&mut self, doc: &Document) {
        if !self.built {
            self.rebuild(doc);
        }
    }

    /// Mark the index stale (document replaced wholesale): the next use
    /// rebuilds it. Keeps huge-document loads to a bounds pass.
    pub fn invalidate(&mut self) {
        self.built = false;
        self.bounds.clear();
        self.buckets.clear();
        self.pending = (0.0, 0.0);
    }

    /// Apply any deferred drift to bounds and buckets. Called before
    /// incremental updates and full rebuilds consume the index.
    fn flush_pending(&mut self) {
        let (dx, dy) = self.pending;
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        self.pending = (0.0, 0.0);
        if self.bounds.is_empty() {
            return;
        }
        for rect in self.bounds.values_mut() {
            rect.x += dx;
            rect.y += dy;
        }
        self.rebucket();
    }

    /// Rigid bulk shift for the all-elements-moving workload: O(1).
    /// Accumulates drift without touching bounds or buckets; rendering never
    /// queries obstacles, so shift+render loops pay nothing here. The next
    /// `apply`/`rebuild` flushes once; a `query` while pending scans bounds
    /// with the offset added (exact, O(n), rare). Uniform translation keeps
    /// relative layout identical, so deferred flushing is exact.
    pub fn shift_all(&mut self, dx: f64, dy: f64) {
        if !dx.is_finite() || !dy.is_finite() || (dx == 0.0 && dy == 0.0) {
            return;
        }
        // No-op when empty so a drift over an empty index stays exact-zero
        // and never trips the pending path.
        if self.bounds.is_empty() && self.pending == (0.0, 0.0) {
            return;
        }
        self.pending.0 += dx;
        self.pending.1 += dy;
    }

    fn rebucket(&mut self) {
        self.buckets.clear();
        let bounds: Vec<(ElementId, Rect)> =
            self.bounds.iter().map(|(id, r)| (id.clone(), *r)).collect();
        for (id, rect) in bounds {
            self.bucket(&id, rect);
        }
    }

    /// Obstacles whose bounds intersect `area`, in id order.
    pub fn query(&self, area: Rect) -> Vec<(ElementId, Rect)> {
        let (dx, dy) = self.pending;
        if dx != 0.0 || dy != 0.0 {
            // Deferred drift outstanding: scan with the offset applied and
            // return true positions. Exact; runs at most until the next
            // `apply`/`rebuild` flushes (drift loops never query).
            return self
                .bounds
                .iter()
                .filter_map(|(id, rect)| {
                    let shifted = Rect::new(rect.x + dx, rect.y + dy, rect.w, rect.h);
                    if shifted.intersects(&area) {
                        Some((id.clone(), shifted))
                    } else {
                        None
                    }
                })
                .collect();
        }
        let (x0, y0, x1, y1) = Self::cell_range(area);
        let mut ids: BTreeSet<&ElementId> = BTreeSet::new();
        for cx in x0..=x1 {
            for cy in y0..=y1 {
                if let Some(bucket) = self.buckets.get(&(cx, cy)) {
                    ids.extend(bucket.iter());
                }
            }
        }
        ids.into_iter()
            .filter_map(|id| {
                self.bounds
                    .get(id)
                    .filter(|rect| rect.intersects(&area))
                    .map(|rect| (id.clone(), *rect))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Point;
    use crate::model::{ElementKind, ShapeKind, Style};

    fn shape(doc: &mut Document, rect: Rect) -> ElementId {
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
    fn query_finds_intersecting_obstacles_only() {
        let mut doc = Document::new();
        let near = shape(&mut doc, Rect::new(0.0, 0.0, 100.0, 50.0));
        let far = shape(&mut doc, Rect::new(1000.0, 1000.0, 100.0, 50.0));
        let mut index = ObstacleIndex::new();
        index.rebuild(&doc);
        let found: Vec<ElementId> = index
            .query(Rect::new(50.0, 25.0, 10.0, 10.0))
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(found, vec![near]);
        assert!(index.query(Rect::new(500.0, 500.0, 10.0, 10.0)).is_empty());
        assert!(!found.contains(&far));
    }

    #[test]
    fn apply_tracks_moves_adds_and_removes() {
        let mut doc = Document::new();
        let id = shape(&mut doc, Rect::new(0.0, 0.0, 100.0, 50.0));
        let mut index = ObstacleIndex::new();
        index.rebuild(&doc);
        assert_eq!(index.len(), 1);
        let probe = Rect::new(50.0, 25.0, 1.0, 1.0);
        assert_eq!(index.query(probe).len(), 1);

        // Move it far away.
        if let ElementKind::Shape { rect, .. } = &mut doc.element_mut(&id).unwrap().kind {
            rect.x = 5000.0;
        }
        let changed: BTreeSet<ElementId> = [id.clone()].into_iter().collect();
        index.apply(&doc, &changed);
        assert!(index.query(probe).is_empty());
        assert_eq!(
            index.query(Rect::new(5050.0, 25.0, 1.0, 1.0)).len(),
            1,
            "the moved obstacle is found at its new place"
        );

        // Remove it.
        doc.element_mut(&id).unwrap().is_deleted = true;
        index.apply(&doc, &changed);
        assert!(index.is_empty());
        assert_eq!(index.query(Rect::new(5050.0, 25.0, 1.0, 1.0)).len(), 0);

        // Bounds lookup sanity.
        let _ = Point::ZERO;
    }

    #[test]
    fn bulk_shift_defers_work_until_flush() {
        let mut doc = Document::new();
        let id = shape(&mut doc, Rect::new(0.0, 0.0, 100.0, 50.0));
        let mut index = ObstacleIndex::new();
        index.rebuild(&doc);
        // O(1): bounds untouched, drift accumulated.
        index.shift_all(1000.0, 0.0);
        assert_eq!(index.bounds.get(&id).unwrap().x, 0.0);
        // Queries observe true (shifted) positions while pending.
        assert!(index.query(Rect::new(50.0, 25.0, 1.0, 1.0)).is_empty());
        assert_eq!(index.query(Rect::new(1050.0, 25.0, 1.0, 1.0)).len(), 1);
        // The next apply flushes once and restores bucketed queries.
        if let ElementKind::Shape { rect, .. } = &mut doc.element_mut(&id).unwrap().kind {
            rect.x += 1000.0;
        }
        let changed: BTreeSet<ElementId> = [id.clone()].into_iter().collect();
        index.apply(&doc, &changed);
        assert_eq!(index.bounds.get(&id).unwrap().x, 1000.0);
        assert_eq!(index.query(Rect::new(1050.0, 25.0, 1.0, 1.0)).len(), 1);
    }
}
