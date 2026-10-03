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

#[derive(Clone, Debug, Default)]
pub struct ObstacleIndex {
    buckets: BTreeMap<(i32, i32), Vec<ElementId>>,
    bounds: BTreeMap<ElementId, Rect>,
}

impl ObstacleIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the index over every obstacle in the document.
    pub fn rebuild(&mut self, doc: &Document) {
        self.buckets.clear();
        self.bounds.clear();
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
    pub fn apply(&mut self, doc: &Document, changed: &BTreeSet<ElementId>) {
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

    /// Obstacles whose bounds intersect `area`, in id order.
    pub fn query(&self, area: Rect) -> Vec<(ElementId, Rect)> {
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
}
