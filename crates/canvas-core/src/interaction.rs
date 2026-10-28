//! In-flight gesture state.
//!
//! This module owns everything between pointer-down and pointer-up: which
//! gesture is running, its open transaction, the marquee, pointer pressure,
//! hover decorations, and the drag-preview caches. The committed document is
//! untouched for the whole preview; only release writes it back and commits
//! the one gesture transaction.
//!
//! Drag fast path invariants (paid for by `massive-bench` and `perf`):
//!
//! - [`InteractionController::preview_move`] never clones elements and never
//!   looks up a per-id map. `drag_working` is contiguous preview truth
//!   translated in place; `drag_slots` are scene slots resolved once at
//!   gesture start; attached connectors refresh their cached rubber-band ops
//!   in place after the first move.
//! - `drag_applied` is the total snapped delta already previewed. Each move
//!   translates by the difference only.
//! - Cancelling unshifts the scene preview and drops the transaction; the
//!   document never saw the gesture. Releasing writes `drag_working` back in
//!   one pass and then builds the undo transaction from pre-gesture copies.
//!
//! The editor owns command semantics on release; this module owns the
//! lifetime and shape of the preview caches.

use std::collections::BTreeSet;

use crate::geometry::{Point, Rect};
use crate::history::Transaction;
use crate::model::{Element, ElementId, Style};
use crate::projection::Projection;
use crate::view::ViewState;

/// Which end of a connector an `ArrowEndpoint` drag is moving.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EndpointEnd {
    Start,
    End,
}

/// What an `ArrowWaypoint` drag is moving.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum WaypointDrag {
    Existing {
        index: usize,
    },
    /// A segment midpoint: the first real move inserts two waypoints (a bow)
    /// and then slides them perpendicular to the segment.
    Bow {
        start: Point,
        end: Point,
        /// Where the pointer grabbed the handle; a click with no movement
        /// must not change the route.
        grab: Point,
        insert_at: usize,
        /// Filled in after insertion: first waypoint index, and whether the
        /// segment is horizontal.
        inserted: Option<(usize, bool)>,
    },
}

/// Attached (non-selected) arrow preview state, computed once when a move
/// gesture starts. Ends bound to selected targets advance arithmetically
/// with the drag (uniform translation never flips the anchor side); other
/// ends freeze. The document is untouched until release.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AttachedDrag {
    pub(crate) id: ElementId,
    pub(crate) a: Point,
    pub(crate) b: Point,
    pub(crate) a_follows: bool,
    pub(crate) b_follows: bool,
    pub(crate) style: Style,
    /// Pre-gesture element for the release undo step.
    pub(crate) pre: Element,
    /// Whether this arrow's rubber band is installed (later moves refresh
    /// the layer's points in place instead of rebuilding).
    pub(crate) built: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) enum Interaction {
    #[default]
    Idle,
    Drawing {
        id: ElementId,
        start: Point,
    },
    Marquee {
        start: Point,
    },
    Moving {
        start: Point,
        originals: Vec<Element>,
        /// Arrows bound to moved elements but not moved themselves: stretched
        /// by the drag, rubber-banded per move, re-routed on release.
        arrow_ids: Vec<ElementId>,
        /// True when the drag began on empty space inside the selection's
        /// bounding box: a click with no movement then clears the selection
        /// instead of moving anything.
        started_in_selection: bool,
    },
    Resizing {
        handle: crate::editor::Handle,
        start: Point,
        bounds: Rect,
        originals: Vec<Element>,
    },
    Freehand {
        id: ElementId,
    },
    Line {
        id: ElementId,
        start: Point,
    },
    Connecting {
        id: ElementId,
        start: Point,
    },
    /// Dragging one endpoint of a selected connector to rebind or detach it.
    ArrowEndpoint {
        id: ElementId,
        end: EndpointEnd,
    },
    /// Dragging a bend on a selected connector's route.
    ArrowWaypoint {
        id: ElementId,
        drag: WaypointDrag,
    },
    /// Dragging a fresh note off a sticky stack; released away from the pile
    /// it becomes a real note, released on the pile it is discarded.
    PullingSticky {
        id: ElementId,
        origin: Point,
    },
    Erasing {
        erased: BTreeSet<ElementId>,
    },
    FrameNameEditing {
        id: ElementId,
    },
    TextEditing {
        id: ElementId,
        original: Box<Option<Element>>,
    },
}

/// The gesture state machine and its preview caches.
pub(crate) struct InteractionController {
    pub(crate) interaction: Interaction,
    pub(crate) tx: Option<Transaction>,
    /// Total snapped drag delta already previewed (see `Interaction::Moving`):
    /// preview moves translate incrementally, clone-free. Reset whenever a
    /// move gesture starts.
    pub(crate) drag_applied: Point,
    /// Scene-layer slots of the dragged selection, resolved once when the
    /// gesture starts (no structural edits mid-drag, so they stay valid).
    /// Lets every preview move shift caches without per-id map lookups.
    pub(crate) drag_slots: Vec<u32>,
    /// Contiguous preview truth for the in-flight drag: translated in place
    /// every move (no clones, no map lookups), written back to the document
    /// once on release, dropped on cancel. The document is never touched
    /// mid-gesture, so cancel needs no restore.
    pub(crate) drag_working: Vec<Element>,
    /// Selected-arrow endpoints frozen to static anchors:
    /// (working index, start freeze, end freeze). Ends bound to selected
    /// targets ride the drag rigidly and need no entry.
    pub(crate) drag_frozen: Vec<(usize, Option<Point>, Option<Point>)>,
    /// Attached-arrow preview state for the in-flight drag.
    pub(crate) drag_attached: Vec<AttachedDrag>,
    pub(crate) marquee_current: Option<Point>,
    pub(crate) last_pressure: f64,
    pub(crate) hover_anchors: Option<(ElementId, Vec<Point>)>,
    /// The sticky stack under the pointer, if its pull zone is hovered.
    pub(crate) hover_stack: Option<ElementId>,
}

impl InteractionController {
    pub(crate) fn new() -> Self {
        Self {
            interaction: Interaction::Idle,
            tx: None,
            drag_applied: Point::ZERO,
            drag_slots: Vec::new(),
            drag_working: Vec::new(),
            drag_frozen: Vec::new(),
            drag_attached: Vec::new(),
            marquee_current: None,
            last_pressure: 0.5,
            hover_anchors: None,
            hover_stack: None,
        }
    }

    pub(crate) fn is_interacting(&self) -> bool {
        self.interaction != Interaction::Idle
    }

    /// Abandon every in-flight gesture. The document is untouched because
    /// previews never write it; callers still unshift the projection through
    /// the editor's cancel path.
    pub(crate) fn reset(&mut self) {
        self.interaction = Interaction::Idle;
        self.tx = None;
        self.marquee_current = None;
        self.hover_anchors = None;
        self.hover_stack = None;
        self.reset_drag_caches();
    }

    fn reset_drag_caches(&mut self) {
        self.drag_applied = Point::ZERO;
        self.drag_slots.clear();
        self.drag_working.clear();
        self.drag_frozen.clear();
        self.drag_attached.clear();
    }

    pub(crate) fn editing_text_id(&self) -> Option<&str> {
        match &self.interaction {
            Interaction::TextEditing { id, .. } => Some(id.as_str()),
            _ => None,
        }
    }

    /// The in-progress marquee rectangle, if any.
    pub(crate) fn marquee_rect(&self) -> Option<Rect> {
        match self.interaction {
            Interaction::Marquee { start } => self
                .marquee_current
                .map(|end| Rect::from_points(start, end)),
            _ => None,
        }
    }

    pub(crate) fn set_pressure(&mut self, pressure: f64) {
        self.last_pressure = pressure.clamp(0.0, 1.0);
    }

    pub(crate) fn take_interaction(&mut self) -> Interaction {
        std::mem::replace(&mut self.interaction, Interaction::Idle)
    }

    /// Install the caches for a selection drag. `working` is the contiguous
    /// preview truth (the caller's pre-gesture copies); the scene slots were
    /// resolved against the same ids. One setup pass, zero work per move.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn begin_move(
        &mut self,
        start: Point,
        originals: Vec<Element>,
        arrow_ids: Vec<ElementId>,
        started_in_selection: bool,
        slots: Vec<u32>,
        frozen: Vec<(usize, Option<Point>, Option<Point>)>,
        attached: Vec<AttachedDrag>,
    ) {
        self.tx = Some(Transaction::begin());
        self.drag_applied = Point::ZERO;
        self.drag_slots = slots;
        self.drag_working = originals.clone();
        self.drag_frozen = frozen;
        self.drag_attached = attached;
        self.interaction = Interaction::Moving {
            start,
            originals,
            arrow_ids,
            started_in_selection,
        };
    }

    /// Preview one move of the in-flight selection drag. Returns false when
    /// the pointer has not moved the snapped total (no repaint due).
    pub(crate) fn preview_move(
        &mut self,
        current: Point,
        view: &mut ViewState,
        projection: &mut Projection,
    ) -> bool {
        let Interaction::Moving {
            start, originals, ..
        } = &self.interaction
        else {
            return false;
        };
        let (dx, dy) = (current.x - start.x, current.y - start.y);
        if dx.abs() < 1e-9 && dy.abs() < 1e-9 {
            return false;
        }
        let (dx, dy) = view.snap_delta(originals, dx, dy);
        // Incremental since the last preview: the document already holds
        // everything up to `applied`, so this move shifts by the difference
        // only — no per-move clones, no rebuilds.
        let (inc_x, inc_y) = (dx - self.drag_applied.x, dy - self.drag_applied.y);
        if inc_x.abs() < 1e-9 && inc_y.abs() < 1e-9 {
            return false;
        }
        self.drag_applied = Point::new(dx, dy);
        // Preview truth lives in `drag_working` (contiguous, cache-hot):
        // translate it in place, snap frozen ends back to their static
        // anchors. No clones, no map lookups, and no document writes until
        // release.
        for element in self.drag_working.iter_mut() {
            crate::editor::translate_element(element, inc_x, inc_y);
        }
        for (idx, freeze_start, freeze_end) in self.drag_frozen.iter() {
            let Some(element) = self.drag_working.get_mut(*idx) else {
                continue;
            };
            let crate::model::ElementKind::Arrow { start, end, .. } = &mut element.kind else {
                continue;
            };
            if let Some(point) = freeze_start {
                start.x = point.x;
                start.y = point.y;
            }
            if let Some(point) = freeze_end {
                end.x = point.x;
                end.y = point.y;
            }
        }
        // Attached arrows: following ends advance arithmetically (uniform
        // translation never flips the anchor side), frozen ends stay. The
        // first rubber band per arrow installs fresh ops; later moves refresh
        // the layer's points in place: one small head-polygon allocation per
        // arrow, nothing else.
        for meta in self.drag_attached.iter_mut() {
            if meta.a_follows {
                meta.a.x += inc_x;
                meta.a.y += inc_y;
            }
            if meta.b_follows {
                meta.b.x += inc_x;
                meta.b.y += inc_y;
            }
            let width = meta.style.stroke_width;
            let bounds = crate::rough::rubber_band_bounds(meta.a, meta.b, width);
            if meta.built {
                projection.refresh_preview_ops(&meta.id, meta.a, meta.b, width, bounds);
            } else {
                let style = meta.style.clone();
                let (ops, _) = crate::rough::rubber_band_ops(meta.a, meta.b, &style);
                projection.set_preview_ops(&meta.id, ops, bounds);
                meta.built = true;
            }
        }
        // Scene: rigid-shift the selection by gesture-stable slots. The
        // changed set stays empty: preview paints from the drained dirty
        // rects, and release recomputes the full set for settle.
        projection.shift_slots(&self.drag_slots, inc_x, inc_y);
        view.shift_selection_bounds(inc_x, inc_y);
        true
    }

    /// Whether the drag actually moved (and so previewed) at all.
    pub(crate) fn previewed(&self) -> bool {
        self.drag_applied.x.abs() > 1e-9 || self.drag_applied.y.abs() > 1e-9
    }

    /// Take the contiguous preview truth for the release write-back.
    pub(crate) fn take_working(&mut self) -> Vec<Element> {
        std::mem::take(&mut self.drag_working)
    }

    pub(crate) fn take_attached(&mut self) -> Vec<AttachedDrag> {
        std::mem::take(&mut self.drag_attached)
    }

    /// Drop the gesture caches after a committed move release.
    pub(crate) fn settle_move(&mut self) {
        self.drag_applied = Point::ZERO;
        self.drag_slots.clear();
        self.drag_frozen.clear();
    }

    /// Undo the scene preview of a cancelled move and return the arrows whose
    /// rubber bands must be rebuilt exactly. The document was never touched.
    pub(crate) fn cancel_move(&mut self, projection: &mut Projection) -> BTreeSet<ElementId> {
        let total = self.drag_applied;
        let attached: BTreeSet<ElementId> = self
            .drag_attached
            .iter()
            .map(|meta| meta.id.clone())
            .collect();
        self.drag_working.clear();
        self.drag_frozen.clear();
        self.drag_attached.clear();
        let _ = self.tx.take();
        self.drag_applied = Point::ZERO;
        if total.x.abs() > 1e-9 || total.y.abs() > 1e-9 {
            let slots = std::mem::take(&mut self.drag_slots);
            projection.shift_slots(&slots, -total.x, -total.y);
        } else {
            self.drag_slots.clear();
        }
        attached
    }
}
