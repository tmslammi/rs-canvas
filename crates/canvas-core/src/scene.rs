//! The scene: per-element cached drawing layers with dirty-region updates.
//!
//! `Scene::update` rebuilds only the layers for changed elements and reports
//! the world-space rectangles that must be repainted (old bounds union new
//! bounds, expanded for stroke and jitter overflow). This is what keeps
//! Canvas2D smooth on large documents.

use std::collections::{BTreeSet, HashMap};

use crate::display::DrawOp;
use crate::geometry::Rect;
use crate::model::{Document, Element, ElementId};
use crate::rough::build_element_ops;
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

/// One element's cached drawing instructions.
#[derive(Clone, Debug, PartialEq)]
pub struct SceneLayer {
    pub element_id: ElementId,
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
    pub fn rebuild_all(&mut self, doc: &Document, measurer: &dyn TextMeasurer) -> Vec<Rect> {
        let old: Vec<Rect> = self.layers.iter().map(|l| l.bounds).collect();
        self.layers.clear();
        let ids: BTreeSet<ElementId> = doc.elements.keys().cloned().collect();
        let mut dirty = self.update(doc, &ids, measurer);
        dirty.extend(old);
        dirty
    }

    /// Rebuild layers for the changed ids (added, updated or tombstoned
    /// elements), keep the rest, and report dirty rectangles in world space.
    pub fn update(
        &mut self,
        doc: &Document,
        changed: &BTreeSet<ElementId>,
        measurer: &dyn TextMeasurer,
    ) -> Vec<Rect> {
        let mut dirty: Vec<Rect> = Vec::new();
        let order = doc.ordered_ids();
        let mut position: HashMap<&ElementId, usize> = HashMap::new();
        for (i, id) in order.iter().enumerate() {
            position.insert(id, i);
        }

        for id in changed {
            let existing = self.layers.iter().position(|l| &l.element_id == id);
            match doc.element(id) {
                Some(element) if !element.is_deleted => {
                    let ops = build_element_ops(element, measurer);
                    let bounds = element.bounds().expand(margin(element));
                    if let Some(pos) = existing {
                        dirty.push(self.layers[pos].bounds);
                        self.layers[pos] = SceneLayer {
                            element_id: id.clone(),
                            version: element.version,
                            bounds,
                            ops,
                        };
                    } else {
                        self.layers.push(SceneLayer {
                            element_id: id.clone(),
                            version: element.version,
                            bounds,
                            ops,
                        });
                    }
                    dirty.push(bounds);
                }
                _ => {
                    if let Some(pos) = existing {
                        dirty.push(self.layers[pos].bounds);
                        self.layers.remove(pos);
                    }
                }
            }
        }

        self.layers
            .sort_by_key(|l| position.get(&l.element_id).copied().unwrap_or(usize::MAX));
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
        scene.rebuild_all(&doc, &crate::text::ApproxMeasurer);
        assert_eq!(scene.layers.len(), 1);
        assert_eq!(scene.layers[0].element_id, a);
    }

    #[test]
    fn update_reports_old_and_new_bounds() {
        let mut doc = Document::new();
        let id = doc.add(shape(0.0), 1);
        let mut scene = Scene::new();
        scene.rebuild_all(&doc, &crate::text::ApproxMeasurer);

        doc.element_mut(&id).unwrap().touch(2);
        if let ElementKind::Shape { rect, .. } = &mut doc.element_mut(&id).unwrap().kind {
            rect.x = 500.0;
        }
        let changed: BTreeSet<_> = [id.clone()].into_iter().collect();
        let dirty = scene.update(&doc, &changed, &crate::text::ApproxMeasurer);
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
        scene.rebuild_all(&doc, &crate::text::ApproxMeasurer);
        assert_eq!(scene.layers.len(), 1);

        doc.element_mut(&id).unwrap().is_deleted = true;
        let changed: BTreeSet<_> = [id].into_iter().collect();
        let dirty = scene.update(&doc, &changed, &crate::text::ApproxMeasurer);
        assert!(scene.layers.is_empty());
        assert_eq!(dirty.len(), 1);
    }

    #[test]
    fn visible_filters_by_viewport() {
        let mut doc = Document::new();
        doc.add(shape(0.0), 1);
        doc.add(shape(1000.0), 1);
        let mut scene = Scene::new();
        scene.rebuild_all(&doc, &crate::text::ApproxMeasurer);
        let visible: Vec<_> = scene.visible(Rect::new(0.0, 0.0, 200.0, 200.0)).collect();
        assert_eq!(visible.len(), 1);
    }

    #[test]
    fn layer_order_follows_document_order() {
        let mut doc = Document::new();
        let a = doc.add(shape(0.0), 1);
        let b = doc.add(shape(10.0), 1);
        let mut scene = Scene::new();
        scene.rebuild_all(&doc, &crate::text::ApproxMeasurer);
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
