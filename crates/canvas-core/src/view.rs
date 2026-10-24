//! View state: everything the user is looking at that is not the document.
//!
//! Camera and navigation, the active tool and its lock, style defaults, grid
//! preferences, viewport size, and the selection live here. None of it is ever
//! persisted or merged: ADR-0002 keeps the document/view-state split strict,
//! and ADR-0005/0006 let the agent send transient view commands without
//! touching the document.
//!
//! Two invariants matter to the editor:
//!
//! - `selection` only contains live element ids. Commits, remote merges and
//!   document replacement call [`ViewState::retain_live_selection`] or
//!   [`ViewState::clear_selection`].
//! - `selection_bounds_cache` is derived. It is computed on selection change
//!   and shifted (never recomputed) during a preview drag, so the overlay can
//!   read selection bounds per frame without scanning thousands of elements.
//!   Any structural document change invalidates it.

use std::collections::BTreeSet;

use crate::camera::Camera;
use crate::geometry::{Point, Rect};
use crate::model::{Document, Element, ElementId, Style};
use crate::nav::NavigationState;
use crate::tool::Tool;
use crate::GRID_SIZE;

pub(crate) struct ViewState {
    pub(crate) camera: Camera,
    pub(crate) nav: NavigationState,
    pub(crate) tool: Tool,
    pub(crate) tool_locked: bool,
    pub(crate) style: Style,
    /// Whether the optional grid is drawn (view state; the shell persists it).
    pub(crate) grid_visible: bool,
    /// Whether pointer-driven edits snap to the grid.
    pub(crate) snap_to_grid: bool,
    /// World-space grid spacing (the snap step when snapping is enabled).
    pub(crate) grid_size: f64,
    pub(crate) selection: BTreeSet<ElementId>,
    /// Cached selection bounds: shifted (not recomputed) on preview moves,
    /// cleared on any selection or structural change. Overlay reads this per
    /// frame during drags; recomputing over thousands of elements per move
    /// is what made big drags stutter the marching ants.
    pub(crate) selection_bounds_cache: Option<Rect>,
    pub(crate) viewport_size: (f64, f64),
}

impl ViewState {
    pub(crate) fn new() -> Self {
        Self {
            camera: Camera::default(),
            nav: NavigationState::new(),
            tool: Tool::default(),
            tool_locked: false,
            style: Style::default(),
            grid_visible: false,
            snap_to_grid: false,
            grid_size: GRID_SIZE,
            selection: BTreeSet::new(),
            selection_bounds_cache: None,
            viewport_size: (800.0, 600.0),
        }
    }

    /// Replace the selection and recompute its cached bounds.
    pub(crate) fn select(&mut self, ids: impl IntoIterator<Item = ElementId>, doc: &Document) {
        self.selection = ids.into_iter().collect();
        self.refresh_selection_bounds(doc);
    }

    pub(crate) fn clear_selection(&mut self) {
        self.selection.clear();
        self.selection_bounds_cache = None;
    }

    /// Drop selected ids that are no longer live after a merge or replacement.
    pub(crate) fn retain_live_selection(&mut self, doc: &Document) {
        self.selection
            .retain(|id| doc.element(id).is_some_and(|element| !element.is_deleted));
        self.selection_bounds_cache = None;
    }

    /// Recompute cached selection bounds from the document.
    pub(crate) fn refresh_selection_bounds(&mut self, doc: &Document) {
        self.selection_bounds_cache = self.selection_bounds_uncached(doc);
    }

    pub(crate) fn invalidate_selection_bounds(&mut self) {
        self.selection_bounds_cache = None;
    }

    /// Bounds of the current selection, in world space.
    pub(crate) fn selection_bounds(&self, doc: &Document) -> Option<Rect> {
        self.selection_bounds_cache
            .or_else(|| self.selection_bounds_uncached(doc))
    }

    fn selection_bounds_uncached(&self, doc: &Document) -> Option<Rect> {
        self.selection
            .iter()
            .filter_map(|id| doc.element(id))
            .map(Element::bounds)
            .reduce(|a, b| a.union(&b))
    }

    /// Translate the cached bounds by a preview delta. Keeps the per-move
    /// overlay read O(1); see the module invariants.
    pub(crate) fn shift_selection_bounds(&mut self, dx: f64, dy: f64) {
        if let Some(cached) = self.selection_bounds_cache.as_mut() {
            cached.x += dx;
            cached.y += dy;
        }
    }

    /// World-space tolerance for a screen-pixel radius.
    pub(crate) fn world_tolerance(&self, screen_px: f64) -> f64 {
        screen_px / self.camera.zoom
    }

    /// The nearest grid point when snapping is enabled.
    pub(crate) fn snap(&self, point: Point) -> Point {
        if !self.snap_to_grid {
            return point;
        }
        let size = self.grid_size;
        Point::new(
            (point.x / size).round() * size,
            (point.y / size).round() * size,
        )
    }

    /// A drag delta adjusted so the moving set's top-left corner lands on the
    /// grid (relative positions inside the set stay intact).
    pub(crate) fn snap_delta(&self, originals: &[Element], dx: f64, dy: f64) -> (f64, f64) {
        if !self.snap_to_grid || originals.is_empty() {
            return (dx, dy);
        }
        let Some(bounds) = originals
            .iter()
            .map(Element::bounds)
            .reduce(|a, b| a.union(&b))
        else {
            return (dx, dy);
        };
        let target = self.snap(Point::new(bounds.x + dx, bounds.y + dy));
        (target.x - bounds.x, target.y - bounds.y)
    }
}
