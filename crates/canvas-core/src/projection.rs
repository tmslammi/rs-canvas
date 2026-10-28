//! Projection: everything derived from the document for drawing and picking.
//!
//! The scene cache, routing obstacle index and hit-test index are *derived*
//! state. They are never authoritative and never persisted. The editor
//! rebuilds them after committed changes, shifts them in place during preview
//! drags, and otherwise reads them through the queries below.
//!
//! Ownership and invariants:
//!
//! - Only this module mutates the caches. [`Projection::refresh`] rebuilds the
//!   world (document load/replace); [`Projection::rebuild_changed`] updates just
//!   the affected layers and index rows; the preview methods shift cached
//!   layers in place without rebuilding.
//! - The hit and obstacle indexes always describe the committed document plus
//!   any deferred uniform shift, never an in-flight drag preview. Release
//!   settles them through `rebuild_changed`.
//! - Read queries are cheap and borrow-only. The shell uses them for painting,
//!   export and routing; no caller may mutate a cache directly.
//! - Previews do not clone elements or look up per-id maps on the move path:
//!   `shift_slots` addresses stable scene slots resolved once per gesture.

use std::collections::BTreeSet;

use crate::geometry::Rect;
use crate::hit::{elements_in_rect_indexed, hit_test_indexed, hit_test_raw_indexed};
use crate::model::{Document, ElementId};
use crate::scene::{Scene, SceneLayer};
use crate::spatial::{HitIndex, ObstacleIndex};
use crate::text::{ApproxMeasurer, TextMeasurer};

pub(crate) struct Projection {
    scene: Scene,
    /// Spatial index of routing obstacles, kept in sync with the document.
    obstacles: ObstacleIndex,
    /// Packed hit-test index over all live elements: hover, click and marquee
    /// must never scan the document per pointer event.
    hit: HitIndex,
    measurer: Box<dyn TextMeasurer>,
}

impl Projection {
    pub(crate) fn new() -> Self {
        Self {
            scene: Scene::new(),
            obstacles: ObstacleIndex::new(),
            hit: HitIndex::new(),
            measurer: Box::new(ApproxMeasurer),
        }
    }

    /// Install a real text measurer (the wasm façade does this).
    pub(crate) fn set_measurer(&mut self, measurer: Box<dyn TextMeasurer>) {
        self.measurer = measurer;
    }

    pub(crate) fn measurer(&self) -> &dyn TextMeasurer {
        self.measurer.as_ref()
    }

    /// Rebuild the whole scene cache and both indexes.
    pub(crate) fn refresh(&mut self, doc: &Document) -> Vec<Rect> {
        // Loading skips the obstacle build (tens of ms plus megabytes on huge
        // documents); the first edit or route that needs it builds once.
        self.obstacles.invalidate();
        self.hit.rebuild(doc);
        self.scene
            .rebuild_all(doc, self.measurer.as_ref(), &self.obstacles)
    }

    /// Rebuild the changed elements' layers, returning dirty world rects.
    pub(crate) fn rebuild_changed(
        &mut self,
        doc: &Document,
        changed: &BTreeSet<ElementId>,
    ) -> Vec<Rect> {
        self.obstacles.apply(doc, changed);
        self.hit.apply(doc, changed);
        self.scene
            .update(doc, changed, self.measurer.as_ref(), &self.obstacles)
    }

    /// Dirty regions for a preview move: caches already shifted in place, so
    /// this repaints without rebuilding layers or touching indexes.
    pub(crate) fn preview_dirty(&mut self) -> Vec<Rect> {
        self.scene.preview_dirty()
    }

    /// Build draw ops for unbuilt layers touching `area` (and obstacles on
    /// first use). Full-detail paint, repaint and export call this before
    /// drawing ops; the LOD overview never does, so huge boards open without
    /// ever paying the rough-geometry pass for off-screen detail.
    pub(crate) fn ensure_area(&mut self, doc: &Document, area: Rect) {
        if self.scene.unbuilt_count() == 0 {
            return;
        }
        self.obstacles.ensure(doc);
        self.scene
            .ensure_area(doc, area, self.measurer.as_ref(), &self.obstacles);
    }

    /// Scene-layer slots for a set of ids, resolved once per gesture.
    pub(crate) fn slot_subset(&self, ids: &BTreeSet<ElementId>) -> Vec<u32> {
        ids.iter()
            .filter_map(|id| self.scene.slot_for(id))
            .collect()
    }

    /// Rigid-shift every cached point and index cell in place.
    pub(crate) fn shift_all(&mut self, dx: f64, dy: f64) {
        self.scene.shift_all(dx, dy);
        self.obstacles.shift_all(dx, dy);
        self.hit.shift_all(dx, dy);
    }

    pub(crate) fn shift_slots(&mut self, slots: &[u32], dx: f64, dy: f64) {
        self.scene.shift_slots(slots, dx, dy);
    }

    pub(crate) fn set_preview_ops(
        &mut self,
        id: &str,
        ops: Vec<crate::display::DrawOp>,
        bounds: Rect,
    ) {
        self.scene.set_preview_ops(id, ops, bounds);
    }

    pub(crate) fn refresh_preview_ops(
        &mut self,
        id: &str,
        a: crate::geometry::Point,
        b: crate::geometry::Point,
        width: f64,
        bounds: Rect,
    ) {
        self.scene.refresh_preview_ops(id, a, b, width, bounds);
    }

    pub(crate) fn unmark_preview(&mut self, id: &str) {
        self.scene.unmark_preview(id);
    }

    pub(crate) fn discard_preview(&mut self) {
        self.scene.discard_preview();
    }

    pub(crate) fn flush_pending_ops(&mut self) {
        self.scene.flush_pending_ops();
    }

    pub(crate) fn flush_preview_stale(&mut self, area: Rect) {
        self.scene.flush_preview_stale(area);
    }

    pub(crate) fn flush_all_preview_stale(&mut self) {
        self.scene.flush_all_preview_stale();
    }

    // -- read-only queries ------------------------------------------------

    pub(crate) fn layers(&self) -> &[SceneLayer] {
        &self.scene.layers
    }

    pub(crate) fn all_layers(&self) -> impl Iterator<Item = &SceneLayer> {
        self.scene.all_layers()
    }

    pub(crate) fn visible_layers(&self, viewport: Rect) -> impl Iterator<Item = &SceneLayer> {
        self.scene.visible(viewport)
    }

    pub(crate) fn palette(&self) -> &[String] {
        self.scene.palette()
    }

    /// Cached union of layer bounds (approximate; exact per layer).
    pub(crate) fn cached_bounds(&self) -> Option<Rect> {
        self.scene.cached_bounds()
    }

    /// Painted bounds of the whole document.
    pub(crate) fn bounds(&self) -> Option<Rect> {
        self.scene.bounds()
    }

    /// Point hit test over the committed-document index.
    pub(crate) fn hit_test(
        &self,
        doc: &Document,
        world: crate::geometry::Point,
        tolerance: f64,
    ) -> Option<ElementId> {
        hit_test_indexed(doc, &self.hit, world, tolerance)
    }

    /// Point hit test that may resolve to a frame (double-click targeting).
    pub(crate) fn hit_test_raw(
        &self,
        doc: &Document,
        world: crate::geometry::Point,
        tolerance: f64,
    ) -> Option<ElementId> {
        hit_test_raw_indexed(doc, &self.hit, world, tolerance)
    }

    pub(crate) fn elements_in_rect(&self, doc: &Document, rect: Rect) -> Vec<ElementId> {
        elements_in_rect_indexed(doc, &self.hit, rect)
    }

    /// Candidate rows for a spatial query, for connection-node picking.
    pub(crate) fn hit_query(&self, area: Rect) -> Vec<u32> {
        self.hit.query(area)
    }

    pub(crate) fn hit_row(&self, row: u32) -> Option<(&str, &str, Rect)> {
        self.hit.row(row)
    }

    /// Read-only routing index, for connector geometry queries.
    pub(crate) fn obstacles(&self) -> &ObstacleIndex {
        &self.obstacles
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Point;
    use crate::model::{ElementKind, ShapeKind, Style};

    fn shape_doc() -> (Document, ElementId) {
        let mut doc = Document::new();
        let id = doc.add(
            ElementKind::Shape {
                shape: ShapeKind::Rectangle,
                rect: Rect::new(10.0, 20.0, 100.0, 50.0),
                style: Style::default(),
                bound_text_id: None,
            },
            1,
        );
        (doc, id)
    }

    #[test]
    fn hit_and_marquee_queries_follow_the_committed_document() {
        let (doc, id) = shape_doc();
        let mut projection = Projection::new();
        projection.refresh(&doc);

        assert_eq!(
            projection.hit_test(&doc, Point::new(60.0, 45.0), 4.0),
            Some(id.clone())
        );
        assert_eq!(
            projection.hit_test(&doc, Point::new(500.0, 500.0), 4.0),
            None
        );
        assert_eq!(
            projection.elements_in_rect(&doc, Rect::new(0.0, 0.0, 200.0, 200.0)),
            vec![id.clone()]
        );
        assert!(projection
            .elements_in_rect(&doc, Rect::new(300.0, 300.0, 50.0, 50.0))
            .is_empty());
    }

    #[test]
    fn scene_queries_describe_the_built_layers() {
        let (doc, id) = shape_doc();
        let mut projection = Projection::new();
        projection.refresh(&doc);

        let layers = projection.layers();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].element_id, id);
        assert_eq!(projection.all_layers().count(), 1);

        let bounds = projection.bounds().expect("one painted layer");
        assert!(bounds.contains(Point::new(60.0, 45.0)));
        assert_eq!(projection.cached_bounds(), Some(bounds));
        assert_eq!(
            projection
                .visible_layers(bounds)
                .map(|layer| layer.element_id.as_str())
                .collect::<Vec<_>>(),
            vec![id.as_str()]
        );
    }

    #[test]
    fn preview_shift_moves_scene_queries_but_not_hit_queries() {
        let (doc, id) = shape_doc();
        let mut projection = Projection::new();
        projection.refresh(&doc);
        let before = projection.layers()[0].bounds.x;
        let slot = projection.slot_subset(&[id.clone()].into_iter().collect());
        projection.shift_slots(&slot, 200.0, 0.0);

        let layer = projection
            .layers()
            .iter()
            .find(|layer| layer.element_id == id)
            .expect("previewed layer");
        assert_eq!(layer.bounds.x, before + 200.0, "the preview cache shifted");

        // Hit/obstacle indexes describe committed geometry only: previews
        // settle them on release through `rebuild_changed`.
        assert_eq!(
            projection.hit_test(&doc, Point::new(60.0, 45.0), 4.0),
            Some(id)
        );
    }
}
