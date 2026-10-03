//! The scene: per-element cached drawing layers with dirty-region updates.
//!
//! `Scene::update` rebuilds only the layers for changed elements and reports
//! the world-space rectangles that must be repainted (old bounds union new
//! bounds, expanded for stroke and jitter overflow). This is what keeps
//! Canvas2D smooth on large documents.

use std::collections::BTreeSet;

use crate::display::DrawOp;
use crate::geometry::Rect;
use crate::model::{Document, Element, ElementId};
use crate::rough::build_element_ops;
use crate::spatial::ObstacleIndex;
use crate::text::TextMeasurer;

/// Extra margin around an element's bounds to cover stroke width and jitter.
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

/// Bounds of the geometry the draw ops actually touch. This is the truth the
/// renderer paints, which can differ from `Element::bounds` (routed arrows,
/// frame name labels).
fn ops_bounds(ops: &[DrawOp], measurer: &dyn TextMeasurer) -> Option<Rect> {
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

/// One element's cached drawing instructions.
#[derive(Clone, Debug, PartialEq)]
pub struct SceneLayer {
    pub element_id: ElementId,
    /// The element's z-order key, kept so `update` can splice in place.
    pub index: String,
    pub version: u64,
    pub bounds: Rect,
    pub ops: Vec<DrawOp>,
}

/// The renderable scene, kept in document z-order.
#[derive(Clone, Debug, Default)]
pub struct Scene {
    pub layers: Vec<SceneLayer>,
}

impl Scene {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild every layer. Returns the world rects that changed.
    pub fn rebuild_all(
        &mut self,
        doc: &Document,
        measurer: &dyn TextMeasurer,
        obstacles: &ObstacleIndex,
    ) -> Vec<Rect> {
        let mut dirty: Vec<Rect> = self.layers.iter().map(|l| l.bounds).collect();
        self.layers.clear();
        for element in doc.elements.values() {
            if element.is_deleted {
                continue;
            }
            let ops = build_element_ops(element, doc, measurer, obstacles);
            let bounds = layer_bounds(element, &ops, measurer);
            self.layers.push(SceneLayer {
                element_id: element.id.clone(),
                index: element.index.clone(),
                version: element.version,
                bounds,
                ops,
            });
        }
        self.layers.sort_unstable_by(|a, b| {
            a.index
                .cmp(&b.index)
                .then_with(|| a.element_id.cmp(&b.element_id))
        });
        dirty.extend(self.layers.iter().map(|l| l.bounds));
        dirty
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
        if let Some(pos) = self.layers.iter().position(|l| l.element_id == id) {
            dirty.push(self.layers[pos].bounds);
            self.layers.remove(pos);
        }
    }

    /// Rebuild layers for the changed ids (added, updated or tombstoned
    /// elements), keep the rest, and report dirty rectangles in world space.
    ///
    /// Layers stay sorted by `(index, id)`, so an element whose order key did
    /// not change is replaced in place in `O(log n)` with no re-sort. Only
    /// structural changes (adds, deletes, reorders) touch the vector shape.
    pub fn update(
        &mut self,
        doc: &Document,
        changed: &BTreeSet<ElementId>,
        measurer: &dyn TextMeasurer,
        obstacles: &ObstacleIndex,
    ) -> Vec<Rect> {
        let mut dirty: Vec<Rect> = Vec::new();
        for id in changed {
            let Some(element) = doc.element(id).filter(|e| !e.is_deleted) else {
                self.remove_layer(id, &mut dirty);
                continue;
            };
            let ops = build_element_ops(element, doc, measurer, obstacles);
            let bounds = layer_bounds(element, &ops, measurer);
            let insert_at = match self.layer_position(&element.index, id) {
                Ok(pos) => {
                    dirty.push(self.layers[pos].bounds);
                    let layer = &mut self.layers[pos];
                    layer.version = element.version;
                    layer.bounds = bounds;
                    layer.ops = ops;
                    dirty.push(bounds);
                    continue;
                }
                Err(pos) => pos,
            };
            dirty.push(bounds);
            let layer = SceneLayer {
                element_id: id.clone(),
                index: element.index.clone(),
                version: element.version,
                bounds,
                ops,
            };
            match self.layers.iter().position(|l| l.element_id == *id) {
                Some(old) => {
                    dirty.push(self.layers[old].bounds);
                    self.layers.remove(old);
                    let insert_at = if old < insert_at {
                        insert_at - 1
                    } else {
                        insert_at
                    };
                    self.layers.insert(insert_at, layer);
                }
                None => self.layers.insert(insert_at, layer),
            }
        }
        dirty
    }

    /// Layers whose bounds intersect the viewport, in z-order.
    pub fn visible(&self, viewport: Rect) -> impl Iterator<Item = &SceneLayer> {
        self.layers
            .iter()
            .filter(move |layer| layer.bounds.intersects(&viewport))
    }

    pub fn layer_for(&self, id: &str) -> Option<&SceneLayer> {
        self.layers.iter().find(|l| l.element_id == id)
    }

    /// Union of every layer's bounds.
    pub fn bounds(&self) -> Option<Rect> {
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
}
