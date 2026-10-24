//! The editor: the interaction state machine over a Document.
//!
//! Owns the document, camera, scene cache, history, tool, style defaults and
//! selection. Everything here is browser-free; the wasm façade only forwards
//! input and paints the effects.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::camera::Camera;
use crate::connector;
use crate::delta::Delta;
use crate::geometry::{Point, Rect};
use crate::history::{History, Transaction};
use crate::hit::{elements_in_rect_indexed, hit_test_indexed, hit_test_raw_indexed};
use crate::id::{new_element_id, new_version_nonce};
use crate::model::{
    Binding, Document, Element, ElementId, ElementKind, Endpoint, Routing, ShapeKind, Style,
    StylePatch, TextAlign,
};
use crate::nav::NavigationState;
use crate::scene::Scene;
use crate::spatial::{HitIndex, ObstacleIndex};
use crate::text::{layout_text, ApproxMeasurer, TextMeasurer};
use crate::tool::Tool;

/// A message from the core to the browser shell (via the wasm façade).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ShellAction {
    OpenTextEditor {
        id: ElementId,
        /// Screen-space rectangle in CSS pixels.
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        font_size: f64,
        text: String,
        align: TextAlign,
        /// True for one-line fields (frame names): Enter commits instead of
        /// inserting a newline.
        single_line: bool,
    },
    /// The document changed: the shell should debounce and call `flush_save`.
    DocumentChanged,
}

/// What the shell should repaint after an input.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Effects {
    /// Elements whose cached layers must be rebuilt.
    pub changed: BTreeSet<ElementId>,
    /// The whole frame changed (camera moves, history jumps).
    pub repaint_all: bool,
    /// Selection or in-progress decoration changed (overlay repaint).
    pub overlay_changed: bool,
    /// The document itself changed, as opposed to only the camera: this is
    /// what makes an autosave due. Camera pans repaint without saving.
    pub document_changed: bool,
    /// Mid-gesture preview (selection drags): caches already shifted, so the
    /// shell must repaint dirty regions WITHOUT rebuilding layers, and must
    /// skip obstacle/hit maintenance until release. Never persisted.
    pub preview: bool,
    /// Shell actions to dispatch (text editor, later: dialogs).
    pub actions: Vec<ShellAction>,
}

impl Effects {
    fn changed(id: impl Into<ElementId>) -> Self {
        let mut effects = Self {
            document_changed: true,
            ..Self::default()
        };
        effects.changed.insert(id.into());
        effects
    }

    fn merge(&mut self, other: Effects) {
        self.changed.extend(other.changed);
        self.repaint_all |= other.repaint_all;
        self.overlay_changed |= other.overlay_changed;
        self.document_changed |= other.document_changed;
        self.preview |= other.preview;
        self.actions.extend(other.actions);
    }
}

/// Modifier keys as the shell reports them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
    pub meta: bool,
}

impl Modifiers {
    /// Cmd on macOS, Ctrl elsewhere.
    pub fn command(&self) -> bool {
        self.ctrl || self.meta
    }
}

const SELECT_TOLERANCE_PX: f64 = 4.0;
const MIN_DRAW_SIZE_PX: f64 = 3.0;
const HANDLE_HIT_PX: f64 = 7.0;
const MIN_RESIZE_PX: f64 = 4.0;
/// Default edge length of a sticky note (and of a sticky stack's pile).
const STICKY_SIZE: f64 = 140.0;
const STICKY_DEFAULT_COLOR: &str = "#ffec99";
/// Sticky text is inked in the note's own outline colour.
const STICKY_INK: &str = "#3b2f1c";
/// How far outside an element's edge its connection nodes float, in screen px.
const CONNECTION_NODE_PX: f64 = 16.0;
/// The border ring of a sticky stack that drags the pile rather than pulling
/// a note, in screen px.
const STACK_EDGE_PX: f64 = 16.0;
/// Default world-space spacing of the optional grid, and the snap step.
pub const GRID_SIZE: f64 = 20.0;

/// A resize handle on the selection bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    Nw,
    N,
    Ne,
    E,
    Se,
    S,
    Sw,
    W,
}

impl Handle {
    pub const ALL: [Handle; 8] = [
        Handle::Nw,
        Handle::N,
        Handle::Ne,
        Handle::E,
        Handle::Se,
        Handle::S,
        Handle::Sw,
        Handle::W,
    ];

    /// The handle's position on a rectangle, in the rectangle's space.
    pub fn position(self, r: Rect) -> Point {
        let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
        match self {
            Handle::Nw => Point::new(r.x, r.y),
            Handle::N => Point::new(cx, r.y),
            Handle::Ne => Point::new(r.max_x(), r.y),
            Handle::E => Point::new(r.max_x(), cy),
            Handle::Se => Point::new(r.max_x(), r.max_y()),
            Handle::S => Point::new(cx, r.max_y()),
            Handle::Sw => Point::new(r.x, r.max_y()),
            Handle::W => Point::new(r.x, cy),
        }
    }

    fn moves_x(self) -> bool {
        matches!(
            self,
            Handle::Nw | Handle::Ne | Handle::Se | Handle::Sw | Handle::E | Handle::W
        )
    }

    fn moves_y(self) -> bool {
        matches!(
            self,
            Handle::Nw | Handle::Ne | Handle::Se | Handle::Sw | Handle::N | Handle::S
        )
    }

    fn affects_left(self) -> bool {
        matches!(self, Handle::Nw | Handle::Sw | Handle::W)
    }

    fn affects_top(self) -> bool {
        matches!(self, Handle::Nw | Handle::Ne | Handle::N)
    }

    fn affects_right(self) -> bool {
        matches!(self, Handle::Ne | Handle::Se | Handle::E)
    }

    fn affects_bottom(self) -> bool {
        matches!(self, Handle::Se | Handle::Sw | Handle::S)
    }
}

/// Which end of a connector an `ArrowEndpoint` drag is moving.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EndpointEnd {
    Start,
    End,
}

/// A draggable bend on a selected connector's route.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ArrowHandle {
    /// An existing waypoint, by list index.
    Waypoint { index: usize, point: Point },
    /// The midpoint of a route segment; dragging it bows the segment out by
    /// inserting two waypoints.
    Insert {
        point: Point,
        start: Point,
        end: Point,
        insert_at: usize,
    },
}

impl ArrowHandle {
    pub fn point(self) -> Point {
        match self {
            ArrowHandle::Waypoint { point, .. } | ArrowHandle::Insert { point, .. } => point,
        }
    }
}

/// What an `ArrowWaypoint` drag is moving.
#[derive(Clone, Copy, Debug, PartialEq)]
enum WaypointDrag {
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
struct AttachedDrag {
    id: ElementId,
    a: Point,
    b: Point,
    a_follows: bool,
    b_follows: bool,
    style: Style,
    /// Pre-gesture element for the release undo step.
    pre: Element,
    /// Whether this arrow's rubber band is installed (later moves refresh
    /// the layer's points in place instead of rebuilding).
    built: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
enum Interaction {
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
        handle: Handle,
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

/// The editable session.
pub struct Editor {
    pub doc: Document,
    pub camera: Camera,
    pub scene: Scene,
    /// Spatial index of routing obstacles, kept in sync with `doc`.
    pub obstacles: ObstacleIndex,
    /// Packed hit-test index over all live elements: hover, click and marquee
    /// must never scan the document per pointer event.
    pub hit: HitIndex,
    pub history: History,
    pub nav: NavigationState,
    pub tool: Tool,
    pub tool_locked: bool,
    pub style: Style,
    /// Whether the optional grid is drawn (view state; the shell persists it).
    pub grid_visible: bool,
    /// Whether pointer-driven edits snap to the grid.
    pub snap_to_grid: bool,
    /// World-space grid spacing (the snap step when snapping is enabled).
    pub grid_size: f64,
    pub selection: BTreeSet<ElementId>,
    /// Cached selection bounds: shifted (not recomputed) on preview moves,
    /// cleared on any selection or structural change. Overlay reads this per
    /// frame during drags; recomputing over thousands of elements per move
    /// is what made big drags stutter the marching ants.
    selection_bounds_cache: Option<Rect>,
    /// Total snapped drag delta already previewed (see `Interaction::Moving`):
    /// preview moves translate incrementally, clone-free. Reset whenever a
    /// move gesture starts.
    drag_applied: Point,
    /// Scene-layer slots of the dragged selection, resolved once when the
    /// gesture starts (no structural edits mid-drag, so they stay valid).
    /// Lets every preview move shift caches without per-id map lookups.
    drag_slots: Vec<u32>,
    /// Contiguous preview truth for the in-flight drag: translated in place
    /// every move (no clones, no map lookups), written back to the document
    /// once on release, dropped on cancel. The document is never touched
    /// mid-gesture, so cancel needs no restore.
    drag_working: Vec<Element>,
    /// Selected-arrow endpoints frozen to static anchors:
    /// (working index, start freeze, end freeze). Ends bound to selected
    /// targets ride the drag rigidly and need no entry.
    drag_frozen: Vec<(usize, Option<Point>, Option<Point>)>,
    /// Attached-arrow preview state for the in-flight drag.
    drag_attached: Vec<AttachedDrag>,
    interaction: Interaction,
    tx: Option<Transaction>,
    marquee_current: Option<Point>,
    clipboard: Vec<Element>,
    /// How many pastes the current clipboard has produced; each paste lands
    /// further from the source so copies never stack invisibly.
    paste_serial: u32,
    last_pressure: f64,
    measurer: Box<dyn TextMeasurer>,
    hover_anchors: Option<(ElementId, Vec<Point>)>,
    /// The sticky stack under the pointer, if its pull zone is hovered.
    hover_stack: Option<ElementId>,
    viewport_size: (f64, f64),
}

impl Editor {
    pub fn new(doc: Document) -> Self {
        let mut editor = Self {
            doc,
            camera: Camera::default(),
            scene: Scene::new(),
            obstacles: ObstacleIndex::new(),
            hit: HitIndex::new(),
            history: History::new(),
            nav: NavigationState::new(),
            tool: Tool::default(),
            tool_locked: false,
            style: Style::default(),
            grid_visible: false,
            snap_to_grid: false,
            grid_size: GRID_SIZE,
            selection: BTreeSet::new(),
            selection_bounds_cache: None,
            drag_applied: Point::ZERO,
            drag_slots: Vec::new(),
            drag_working: Vec::new(),
            drag_frozen: Vec::new(),
            drag_attached: Vec::new(),
            interaction: Interaction::Idle,
            tx: None,
            marquee_current: None,
            clipboard: Vec::new(),
            paste_serial: 0,
            last_pressure: 0.5,
            measurer: Box::new(ApproxMeasurer),
            hover_anchors: None,
            hover_stack: None,
            viewport_size: (800.0, 600.0),
        };
        editor.refresh_scene();
        editor
    }

    /// The shell reports its viewport size (used for insert-at-centre).
    pub fn set_viewport(&mut self, width: f64, height: f64) {
        self.viewport_size = (width.max(1.0), height.max(1.0));
    }

    /// The last viewport size the shell reported, in CSS pixels.
    pub fn viewport_size(&self) -> (f64, f64) {
        self.viewport_size
    }

    /// A camera that fits an element — frames include their descendants — or
    /// the whole document when `id` is `None`, with `padding` screen px.
    pub fn focus_camera(&self, id: Option<&str>, padding: f64) -> Option<Camera> {
        let bounds = match id {
            Some(id) => self.doc.framing_bounds(id)?,
            None => self.doc.bounds()?,
        };
        let mut camera = self.camera;
        camera.zoom_to_fit(bounds, self.viewport_size, padding);
        Some(camera)
    }

    /// Select a tool (single-shot: reverts to Select after one use). The
    /// freehand brush is the exception: it stays armed for repeated strokes.
    ///
    /// Switching tools abandons an in-flight gesture: without this a mid-drag
    /// tool switch would commit a half-drawn element on pointer-up.
    pub fn set_tool(&mut self, tool: Tool) -> Effects {
        let mut effects = if self.is_interacting() {
            self.cancel()
        } else {
            Effects::default()
        };
        self.tool = tool;
        self.tool_locked = tool == Tool::Freehand;
        effects.overlay_changed = true;
        effects
    }

    /// After a finished gesture, single-shot tools hand back to Select.
    fn finish_tool_use(&mut self) {
        if !self.tool_locked {
            self.tool = Tool::Select;
        }
    }

    /// Whether a gesture is in progress (no committed save point yet).
    pub fn is_interacting(&self) -> bool {
        self.interaction != Interaction::Idle
    }

    /// Replace the whole document (used when opening a file or restoring).
    pub fn replace_document(&mut self, doc: Document) -> Effects {
        self.doc = doc;
        self.history = History::new();
        self.selection.clear();
        self.interaction = Interaction::Idle;
        self.tx = None;
        self.marquee_current = None;
        self.hover_anchors = None;
        self.hover_stack = None;
        self.refresh_scene();
        Effects {
            repaint_all: true,
            overlay_changed: true,
            document_changed: true,
            ..Effects::default()
        }
    }

    /// Install a real text measurer (the wasm façade does this).
    pub fn set_measurer(&mut self, measurer: Box<dyn TextMeasurer>) {
        self.measurer = measurer;
        self.refresh_scene();
    }

    /// Rebuild the whole scene cache.
    pub fn refresh_scene(&mut self) -> Vec<Rect> {
        // Loading skips the obstacle build (tens of ms plus megabytes on huge
        // documents); the first edit or route that needs it builds once.
        self.obstacles.invalidate();
        self.hit.rebuild(&self.doc);
        self.selection_bounds_cache = None;
        self.scene
            .rebuild_all(&self.doc, self.measurer.as_ref(), &self.obstacles)
    }

    /// Rebuild the changed elements' layers, returning dirty world rects.
    pub fn rebuild_changed(&mut self, changed: &BTreeSet<ElementId>) -> Vec<Rect> {
        self.obstacles.apply(&self.doc, changed);
        self.hit.apply(&self.doc, changed);
        self.selection_bounds_cache = None;
        self.scene
            .update(&self.doc, changed, self.measurer.as_ref(), &self.obstacles)
    }

    /// Dirty regions for a preview move: caches already shifted in place, so
    /// this repaints without rebuilding layers or touching indexes.
    pub fn preview_dirty(&mut self) -> Vec<Rect> {
        self.scene.preview_dirty()
    }

    /// Build draw ops for unbuilt layers touching `area` (and obstacles on
    /// first use). Full-detail paint, repaint and export call this before
    /// drawing ops; the LOD overview never does, so huge boards open without
    /// ever paying the rough-geometry pass for off-screen detail.
    pub fn ensure_area(&mut self, area: Rect) {
        if self.scene.unbuilt_count() == 0 {
            return;
        }
        self.obstacles.ensure(&self.doc);
        self.scene
            .ensure_area(&self.doc, area, self.measurer.as_ref(), &self.obstacles);
    }

    /// Scene-layer slots for a set of ids, resolved once per gesture.
    fn scene_slot_subset(&self, ids: &BTreeSet<ElementId>) -> Vec<u32> {
        ids.iter()
            .filter_map(|id| self.scene.slot_for(id))
            .collect()
    }

    /// Rigid bulk translation for the all-elements-moving benchmark: shift
    /// every document element and every cached scene point by `(dx, dy)` in
    /// place, with no layer rebuilds, no history entries, no obstacle
    /// rebucketing (deferred to the next routing query). Exact for uniform
    /// drift: deterministic rough geometry translates identically, and bound
    /// connectors shift with their targets so bindings stay valid. Returns
    /// nothing; the caller repaints.
    pub fn benchmark_tick(&mut self, dx: f64, dy: f64) {
        if !dx.is_finite() || !dy.is_finite() || (dx == 0.0 && dy == 0.0) {
            return;
        }
        for element in self.doc.elements.values_mut() {
            if element.is_deleted {
                continue;
            }
            translate_element(element, dx, dy);
        }
        self.scene.shift_all(dx, dy);
        self.obstacles.shift_all(dx, dy);
        self.hit.shift_all(dx, dy);
    }

    /// The text measurer in use (for SVG export).
    pub fn measurer(&self) -> &dyn TextMeasurer {
        self.measurer.as_ref()
    }

    /// The shell reports pointer pressure for freehand capture.
    pub fn set_pressure(&mut self, pressure: f64) {
        self.last_pressure = pressure.clamp(0.0, 1.0);
    }

    fn tolerance(&self) -> f64 {
        SELECT_TOLERANCE_PX / self.camera.zoom
    }

    fn min_draw_size(&self) -> f64 {
        MIN_DRAW_SIZE_PX / self.camera.zoom
    }

    /// The nearest grid point when snapping is enabled.
    fn snap(&self, point: Point) -> Point {
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
    fn snap_delta(&self, originals: &[Element], dx: f64, dy: f64) -> (f64, f64) {
        if !self.snap_to_grid || originals.is_empty() {
            return (dx, dy);
        }
        let Some(bounds) = originals
            .iter()
            .map(|element| element.bounds())
            .reduce(|a, b| a.union(&b))
        else {
            return (dx, dy);
        };
        let target = self.snap(Point::new(bounds.x + dx, bounds.y + dy));
        (target.x - bounds.x, target.y - bounds.y)
    }

    // -- selection ---------------------------------------------------------

    pub fn select(&mut self, ids: impl IntoIterator<Item = ElementId>) -> Effects {
        self.selection = ids.into_iter().collect();
        self.selection_bounds_cache = self
            .selection
            .iter()
            .filter_map(|id| self.doc.element(id))
            .map(|e| e.bounds())
            .reduce(|a, b| a.union(&b));
        Effects {
            overlay_changed: true,
            ..Effects::default()
        }
    }

    pub fn clear_selection(&mut self) -> Effects {
        if self.selection.is_empty() {
            return Effects::default();
        }
        self.selection.clear();
        self.selection_bounds_cache = None;
        Effects {
            overlay_changed: true,
            ..Effects::default()
        }
    }

    pub fn select_all(&mut self) -> Effects {
        let live = self.doc.ordered_live();
        let mut ids: Vec<ElementId> = live
            .iter()
            .filter(|e| !e.is_frame())
            .map(|e| e.id.clone())
            .collect();
        if ids.is_empty() {
            ids = live.iter().map(|e| e.id.clone()).collect();
        }
        self.select(ids)
    }

    /// Bounds of the current selection, in world space.
    pub fn selection_bounds(&self) -> Option<Rect> {
        if let Some(cached) = self.selection_bounds_cache {
            return Some(cached);
        }
        self.selection
            .iter()
            .filter_map(|id| self.doc.element(id))
            .map(|e| e.bounds())
            .reduce(|a, b| a.union(&b))
    }

    /// The in-progress marquee rectangle, if any.
    pub fn marquee_rect(&self) -> Option<Rect> {
        match self.interaction {
            Interaction::Marquee { start } => self
                .marquee_current
                .map(|end| Rect::from_points(start, end)),
            _ => None,
        }
    }

    // -- pointer -----------------------------------------------------------

    pub fn pointer_down(
        &mut self,
        screen: Point,
        button: u8,
        mods: Modifiers,
        now: u64,
    ) -> Effects {
        if let Interaction::TextEditing { id, .. } = &self.interaction {
            let id = id.clone();
            let current = self
                .doc
                .element(&id)
                .and_then(|element| match &element.kind {
                    ElementKind::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            let _ = self.commit_text(&id, &current, now);
        }
        if self.nav.pointer_down(screen, button) {
            return Effects::default();
        }
        if button != 0 {
            return Effects::default();
        }
        let world = self.camera.screen_to_world(screen);
        // Geometry-creating tools snap their anchor; hit-testing tools must
        // aim at what the user actually pointed at.
        let spot = self.snap(world);
        match self.tool {
            Tool::Select => self.begin_select(screen, world, mods, now),
            Tool::Rectangle | Tool::Diamond | Tool::Ellipse => self.begin_drawing(spot, now),
            Tool::Sticky => self.begin_sticky(spot, now),
            Tool::StickyStack => self.begin_sticky_stack(spot, now),
            Tool::Frame => self.begin_frame(spot, now),
            Tool::Freehand => self.begin_freehand(world, now),
            Tool::Line => self.begin_line(spot, now),
            Tool::Arrow => self.begin_connector(spot, now),
            Tool::Eraser => self.begin_erase(world, now),
            Tool::Text => self.begin_text_tool(spot, now),
            Tool::Hand => {
                self.nav.start_pan(screen);
                Effects::default()
            }
        }
    }

    pub fn pointer_move(&mut self, screen: Point, mods: Modifiers, now: u64) -> Effects {
        if self.nav.pointer_move(&mut self.camera, screen) {
            return Effects {
                repaint_all: true,
                ..Effects::default()
            };
        }
        // The eraser mutates through `&mut self`, so handle it before the
        // read-only match below (this also keeps the match clone-free).
        if let Interaction::Erasing { erased } = &self.interaction {
            let mut erased = erased.clone();
            let world = self.camera.screen_to_world(screen);
            let mut effects = Effects::default();
            self.erase_at(world, &mut erased, &mut effects, now);
            self.interaction = Interaction::Erasing { erased };
            return effects;
        }
        // Waypoint drags mutate through `&mut self` too: a segment bow is
        // inserted on the first move, then dragged.
        if let Interaction::ArrowWaypoint { id, drag } = &self.interaction {
            let id = id.clone();
            let mut drag = *drag;
            let current = self.snap(self.camera.screen_to_world(screen));
            let effects = Effects {
                changed: [id.clone()].into_iter().collect(),
                overlay_changed: true,
                ..Effects::default()
            };
            if let Some(tx) = self.tx.as_mut() {
                match &mut drag {
                    WaypointDrag::Existing { index } => {
                        let target = *index;
                        let _ = tx.update(&mut self.doc, &id, now, |element| {
                            if let ElementKind::Arrow { waypoints, .. } = &mut element.kind {
                                if let Some(slot) = waypoints.get_mut(target) {
                                    *slot = current;
                                }
                            }
                        });
                    }
                    WaypointDrag::Bow {
                        start,
                        end,
                        grab,
                        insert_at,
                        inserted,
                    } => {
                        // A click that never moved (pointer_up replays a
                        // zero-distance move) must leave the route alone.
                        if inserted.is_none() {
                            if current.distance(*grab) <= 2.0 / self.camera.zoom {
                                self.interaction = Interaction::ArrowWaypoint { id, drag };
                                return effects;
                            }
                            let horizontal = (end.y - start.y).abs() <= (end.x - start.x).abs();
                            // Inset inward from each end along the segment, so
                            // the bow never lands outside the grabbed segment.
                            let inset = (start.distance(*end) / 4.0).clamp(8.0, 32.0);
                            let (first, second) = if horizontal {
                                let dir = if end.x >= start.x { 1.0 } else { -1.0 };
                                (
                                    Point::new(start.x + dir * inset, start.y),
                                    Point::new(end.x - dir * inset, end.y),
                                )
                            } else {
                                let dir = if end.y >= start.y { 1.0 } else { -1.0 };
                                (
                                    Point::new(start.x, start.y + dir * inset),
                                    Point::new(end.x, end.y - dir * inset),
                                )
                            };
                            let existing = self
                                .doc
                                .element(&id)
                                .and_then(|element| match &element.kind {
                                    ElementKind::Arrow { waypoints, .. } => Some(waypoints.len()),
                                    _ => None,
                                })
                                .unwrap_or(0);
                            let at = (*insert_at).min(existing);
                            let _ = tx.update(&mut self.doc, &id, now, |element| {
                                if let ElementKind::Arrow { waypoints, .. } = &mut element.kind {
                                    let at = at.min(waypoints.len());
                                    waypoints.insert(at, first);
                                    waypoints.insert(at + 1, second);
                                }
                            });
                            *inserted = Some((at, horizontal));
                        }
                        let (first, horizontal) = inserted.unwrap_or((0, true));
                        let _ = tx.update(&mut self.doc, &id, now, |element| {
                            if let ElementKind::Arrow { waypoints, .. } = &mut element.kind {
                                for offset in 0..2 {
                                    if let Some(slot) = waypoints.get_mut(first + offset) {
                                        if horizontal {
                                            slot.y = current.y;
                                        } else {
                                            slot.x = current.x;
                                        }
                                    }
                                }
                            }
                        });
                    }
                }
            }
            self.interaction = Interaction::ArrowWaypoint { id, drag };
            return effects;
        }
        // Hover dots belong to the Select tool alone; while any other
        // interaction is in flight the overlay is owned by that interaction.
        if !matches!(
            self.interaction,
            Interaction::Idle | Interaction::Connecting { .. } | Interaction::ArrowEndpoint { .. }
        ) {
            self.hover_anchors = None;
        }
        match &self.interaction {
            Interaction::Idle => {
                let world = self.camera.screen_to_world(screen);
                self.update_hover(world)
            }
            Interaction::FrameNameEditing { .. } => Effects::default(),
            Interaction::Drawing { id, start } => {
                let current = self.snap(self.camera.screen_to_world(screen));
                // A sticky note starts at its default size: a click keeps it,
                // and only a real drag resizes it.
                if self.doc.element(id).is_some_and(|e| e.is_sticky())
                    && current.distance(*start) < self.min_draw_size()
                {
                    return Effects::default();
                }
                let rect = self.drawn_rect(*start, current, mods);
                if let Some(tx) = self.tx.as_mut() {
                    if let Err(err) =
                        tx.update(&mut self.doc, id, now, |element| match &mut element.kind {
                            ElementKind::Shape { rect: r, .. }
                            | ElementKind::Frame { rect: r, .. } => *r = rect,
                            _ => {}
                        })
                    {
                        debug_assert!(false, "drawing update failed: {err}");
                    }
                }
                Effects::changed(id.clone())
            }
            Interaction::Marquee { start: _ } => {
                self.marquee_current = Some(self.camera.screen_to_world(screen));
                let _ = mods;
                Effects {
                    overlay_changed: true,
                    ..Effects::default()
                }
            }
            Interaction::Moving {
                start, originals, ..
            } => {
                let current = self.camera.screen_to_world(screen);
                let (dx, dy) = (current.x - start.x, current.y - start.y);
                if dx.abs() < 1e-9 && dy.abs() < 1e-9 {
                    return Effects::default();
                }
                let (dx, dy) = self.snap_delta(originals, dx, dy);
                // Incremental since the last preview: the document already
                // holds everything up to `applied`, so this move shifts by
                // the difference only — no per-move clones, no rebuilds.
                let (inc_x, inc_y) = (dx - self.drag_applied.x, dy - self.drag_applied.y);
                if inc_x.abs() < 1e-9 && inc_y.abs() < 1e-9 {
                    return Effects::default();
                }
                self.drag_applied = Point::new(dx, dy);
                // Preview truth lives in `drag_working` (contiguous,
                // cache-hot): translate it in place, snap frozen ends back
                // to their static anchors. No clones, no map lookups, and
                // no doc writes until release.
                for element in self.drag_working.iter_mut() {
                    translate_element(element, inc_x, inc_y);
                }
                for (idx, freeze_start, freeze_end) in self.drag_frozen.iter() {
                    let Some(element) = self.drag_working.get_mut(*idx) else {
                        continue;
                    };
                    let ElementKind::Arrow { start, end, .. } = &mut element.kind else {
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
                // Attached arrows: following ends advance arithmetically
                // (uniform translation never flips the anchor side), frozen
                // ends stay. The first rubber band per arrow installs fresh
                // ops; later moves refresh the layer's points in place: one
                // small head-polygon allocation per arrow, nothing else.
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
                        self.scene
                            .refresh_preview_ops(&meta.id, meta.a, meta.b, width, bounds);
                    } else {
                        let style = meta.style.clone();
                        let (ops, _) = crate::rough::rubber_band_ops(meta.a, meta.b, &style);
                        self.scene.set_preview_ops(&meta.id, ops, bounds);
                        meta.built = true;
                    }
                }
                // Scene: rigid-shift the selection by gesture-stable slots.
                // The changed set stays empty: preview paints from the drained
                // dirty rects, and release recomputes the full set for settle.
                self.scene.shift_slots(&self.drag_slots, inc_x, inc_y);
                if let Some(cached) = self.selection_bounds_cache.as_mut() {
                    cached.x += inc_x;
                    cached.y += inc_y;
                }
                Effects {
                    preview: true,
                    overlay_changed: true,
                    ..Effects::default()
                }
            }
            Interaction::Resizing {
                handle,
                start,
                bounds,
                originals,
            } => {
                let current = self.snap(self.camera.screen_to_world(screen));
                let target = resized_bounds(*bounds, *handle, *start, current, mods.shift);
                let mut effects = Effects::default();
                for original in originals {
                    let mut scaled = original.clone();
                    scale_element(&mut scaled, *bounds, target);
                    let kind = scaled.kind;
                    if let Some(tx) = self.tx.as_mut() {
                        if let Err(err) = tx.update(&mut self.doc, &original.id, now, |element| {
                            element.kind = kind;
                        }) {
                            debug_assert!(false, "resize update failed: {err}");
                        }
                    }
                    effects.changed.insert(original.id.clone());
                }
                effects
            }
            Interaction::Erasing { .. } => unreachable!("handled above"),
            Interaction::ArrowWaypoint { .. } => unreachable!("handled above"),
            Interaction::TextEditing { .. } => Effects::default(),
            Interaction::Freehand { id } => {
                let current = self.camera.screen_to_world(screen);
                let last = self.doc.element(id).and_then(|e| match &e.kind {
                    ElementKind::Freehand { points, .. } => points.last().copied(),
                    _ => None,
                });
                let mut effects = Effects::default();
                let far_enough = last
                    .map(|last| last.distance(current) >= 1.5 / self.camera.zoom)
                    .unwrap_or(false);
                if far_enough {
                    let pressure = self.last_pressure;
                    if let Some(tx) = self.tx.as_mut() {
                        let _ = tx.update(&mut self.doc, id, now, |element| {
                            if let ElementKind::Freehand {
                                points, pressures, ..
                            } = &mut element.kind
                            {
                                points.push(current);
                                pressures.push(pressure);
                            }
                        });
                    }
                    effects.changed.insert(id.clone());
                }
                effects
            }
            Interaction::Line { id, start } => {
                let current = self.camera.screen_to_world(screen);
                let end = constrain_angle(*start, current, mods.shift);
                // An angle-constrained end is already intentional; snapping it
                // would shear the angle.
                let end = if mods.shift { end } else { self.snap(end) };
                if let Some(tx) = self.tx.as_mut() {
                    let _ = tx.update(&mut self.doc, id, now, |element| {
                        if let ElementKind::Arrow { end: e, .. } = &mut element.kind {
                            e.x = end.x;
                            e.y = end.y;
                        }
                    });
                }
                Effects::changed(id.clone())
            }
            Interaction::Connecting { id, start } => {
                let current = self.camera.screen_to_world(screen);
                let tolerance = self.tolerance().max(4.0);
                let approach = self
                    .doc
                    .element(id)
                    .and_then(|element| match &element.kind {
                        ElementKind::Arrow { start, .. } => Some(start.point()),
                        _ => None,
                    });
                let node = self.node_hit(current, Some(id.as_str()));
                let explicit = node.is_some();
                let candidate = node.or_else(|| {
                    connector::binding_candidate(
                        &self.doc,
                        current,
                        tolerance,
                        approach,
                        Some(id.as_str()),
                    )
                });
                let (end, binding, hover) = match candidate {
                    Some((target_id, side, point)) => {
                        let anchors = self
                            .doc
                            .element(&target_id)
                            .map(|element| self.node_points_for(element))
                            .unwrap_or_default();
                        (
                            point,
                            Some(Binding {
                                element_id: target_id.clone(),
                                // A node grab picks the side explicitly; a
                                // body drop keeps re-anchoring toward the
                                // other endpoint so the route never cuts back
                                // through the shape it binds to.
                                focus: explicit.then(|| side.focus()),
                            }),
                            Some((target_id, anchors)),
                        )
                    }
                    None => (self.snap(current), None, None),
                };
                self.hover_anchors = hover;
                let _ = start;
                // Store the resolved anchor, not the heuristic one: endpoint
                // coordinates must describe the drawn geometry so bounds(),
                // endpoint handles and selection math stay honest.
                let stored_end = {
                    let other = self
                        .doc
                        .element(id)
                        .and_then(|element| match &element.kind {
                            ElementKind::Arrow { start, .. } => Some(start.point()),
                            _ => None,
                        });
                    match (&binding, other) {
                        (Some(binding), Some(other)) => {
                            let ep = crate::model::Endpoint {
                                x: end.x,
                                y: end.y,
                                binding: Some(binding.clone()),
                            };
                            connector::resolve_endpoint(&ep, other, &self.doc).0
                        }
                        _ => end,
                    }
                };
                // Now that the end has a real position, the bound start's
                // stored point re-resolves against it — the anchor it picked
                // while the pointer still stood inside the source element is
                // not the side that faces the destination.
                let stored_start = self
                    .doc
                    .element(id)
                    .and_then(|element| match &element.kind {
                        ElementKind::Arrow { start, .. } if start.binding.is_some() => {
                            Some(connector::resolve_endpoint(start, stored_end, &self.doc).0)
                        }
                        _ => None,
                    });
                if let Some(tx) = self.tx.as_mut() {
                    let _ = tx.update(&mut self.doc, id, now, |element| {
                        if let ElementKind::Arrow { start, end: e, .. } = &mut element.kind {
                            e.x = stored_end.x;
                            e.y = stored_end.y;
                            e.binding = binding.clone();
                            if let Some(p) = stored_start {
                                start.x = p.x;
                                start.y = p.y;
                            }
                        }
                    });
                }
                Effects {
                    changed: [id.clone()].into_iter().collect(),
                    overlay_changed: true,
                    ..Effects::default()
                }
            }
            Interaction::ArrowEndpoint { id, end } => {
                let current = self.camera.screen_to_world(screen);
                let tolerance = self.tolerance().max(4.0);
                let id = id.clone();
                let end = *end;
                let approach = self
                    .doc
                    .element(&id)
                    .and_then(|element| match &element.kind {
                        ElementKind::Arrow {
                            start, end: tail, ..
                        } => Some(match end {
                            EndpointEnd::Start => tail.point(),
                            EndpointEnd::End => start.point(),
                        }),
                        _ => None,
                    });
                let node = self.node_hit(current, Some(&id));
                let explicit = node.is_some();
                let candidate = node.or_else(|| {
                    connector::binding_candidate(&self.doc, current, tolerance, approach, Some(&id))
                });
                let (point, binding, hover) = match candidate {
                    Some((target_id, side, point)) => {
                        let anchors = self
                            .doc
                            .element(&target_id)
                            .map(|element| self.node_points_for(element))
                            .unwrap_or_default();
                        (
                            point,
                            Some(Binding {
                                element_id: target_id.clone(),
                                focus: explicit.then(|| side.focus()),
                            }),
                            Some((target_id, anchors)),
                        )
                    }
                    None => (self.snap(current), None, None),
                };
                self.hover_anchors = hover;
                // Store the resolved anchor so endpoint coordinates describe
                // the drawn geometry (see the Connecting arm).
                let stored_point = match (&binding, approach) {
                    (Some(binding), Some(other)) => {
                        let ep = crate::model::Endpoint {
                            x: point.x,
                            y: point.y,
                            binding: Some(binding.clone()),
                        };
                        connector::resolve_endpoint(&ep, other, &self.doc).0
                    }
                    _ => point,
                };
                // A rebind can re-anchor the untouched bound end too — its
                // stored point must track the new opposite anchor.
                let stored_other = self
                    .doc
                    .element(&id)
                    .and_then(|element| match &element.kind {
                        ElementKind::Arrow {
                            start, end: tail, ..
                        } => {
                            let other_ep = match end {
                                EndpointEnd::Start => tail,
                                EndpointEnd::End => start,
                            };
                            other_ep.binding.as_ref().map(|_| {
                                connector::resolve_endpoint(other_ep, stored_point, &self.doc).0
                            })
                        }
                        _ => None,
                    });
                if let Some(tx) = self.tx.as_mut() {
                    let _ = tx.update(&mut self.doc, &id, now, |element| {
                        if let ElementKind::Arrow {
                            start, end: tail, ..
                        } = &mut element.kind
                        {
                            let (endpoint, other_ep) = match end {
                                EndpointEnd::Start => (start, tail),
                                EndpointEnd::End => (tail, start),
                            };
                            endpoint.x = stored_point.x;
                            endpoint.y = stored_point.y;
                            endpoint.binding = binding.clone();
                            if let Some(p) = stored_other {
                                other_ep.x = p.x;
                                other_ep.y = p.y;
                            }
                        }
                    });
                }
                Effects {
                    changed: [id].into_iter().collect(),
                    overlay_changed: true,
                    ..Effects::default()
                }
            }
            Interaction::PullingSticky { id, .. } => {
                let current = self.snap(self.camera.screen_to_world(screen));
                let size = self
                    .doc
                    .element(id)
                    .map(|element| element.bounds())
                    .map(|bounds| (bounds.w, bounds.h))
                    .unwrap_or((STICKY_SIZE, STICKY_SIZE));
                if let Some(tx) = self.tx.as_mut() {
                    let _ = tx.update(&mut self.doc, id, now, |element| {
                        if let ElementKind::Shape { rect, .. } = &mut element.kind {
                            *rect = Rect::new(
                                current.x - size.0 / 2.0,
                                current.y - size.1 / 2.0,
                                size.0,
                                size.1,
                            );
                        }
                    });
                }
                Effects::changed(id.clone())
            }
        }
    }

    pub fn pointer_up(&mut self, screen: Point, mods: Modifiers, now: u64) -> Effects {
        if self.nav.pointer_up(0) || self.nav.pointer_up(1) {
            return Effects::default();
        }
        // Take the interaction out rather than cloning it: `Moving`/`Resizing`
        // carry the original elements, which must not be copied per event.
        let interaction = std::mem::replace(&mut self.interaction, Interaction::Idle);
        let mut effects = match interaction {
            Interaction::Drawing { .. }
            | Interaction::Moving { .. }
            | Interaction::Resizing { .. }
            | Interaction::Connecting { .. }
            | Interaction::ArrowEndpoint { .. }
            | Interaction::ArrowWaypoint { .. }
            | Interaction::PullingSticky { .. }
            | Interaction::Line { .. } => {
                self.interaction = interaction;
                self.pointer_move(screen, mods, now)
            }
            other => {
                self.interaction = other;
                Effects::default()
            }
        };
        let interaction = std::mem::replace(&mut self.interaction, Interaction::Idle);
        match interaction {
            Interaction::Idle => {}
            Interaction::FrameNameEditing { id } => {
                // The shell owns the frame-name editor; stay in the state.
                self.interaction = Interaction::FrameNameEditing { id };
            }
            Interaction::Drawing { id, .. } => {
                let tiny = self
                    .doc
                    .element(&id)
                    .map(|e| {
                        let b = e.bounds();
                        b.w < self.min_draw_size() || b.h < self.min_draw_size()
                    })
                    .unwrap_or(true);
                self.marquee_current = None;
                if tiny {
                    self.tx = None;
                    let purged = self.doc.element(&id).cloned();
                    if let Some(element) = purged {
                        let _ = self.doc.apply(&Delta::Purge { element });
                    }
                    effects.merge(Effects {
                        changed: [id].into_iter().collect(),
                        overlay_changed: true,
                        ..Effects::default()
                    });
                } else {
                    self.sync_frame_membership(now);
                    if let Some(tx) = self.tx.take() {
                        tx.commit(&mut self.history);
                    }
                    self.finish_tool_use();
                    let selection_effects = self.select([id.clone()]);
                    effects.changed.insert(id);
                    effects.merge(selection_effects);
                }
            }
            Interaction::Marquee { start } => {
                let end = self.camera.screen_to_world(screen);
                let rect = Rect::from_points(start, end);
                self.marquee_current = None;
                // Dropping the marquee is an overlay change even when the
                // selection outcome is a no-op (empty rect, or only frames
                // that the marquee failed to enclose). Without this the
                // orange rectangle stays painted until some later event
                // happens to invalidate the overlay.
                effects.overlay_changed = true;
                let hits: Vec<ElementId> = elements_in_rect_indexed(&self.doc, &self.hit, rect)
                    .into_iter()
                    .map(|id| crate::hit::resolve_container(&self.doc, &id))
                    // A marquee inside a frame selects its contents; enclosing
                    // the whole frame selects the container too.
                    .filter(|id| {
                        self.doc.element(id).is_none_or(|element| {
                            if !element.is_frame() {
                                return true;
                            }
                            let bounds = element.bounds();
                            rect.contains(Point::new(bounds.min_x(), bounds.min_y()))
                                && rect.contains(Point::new(bounds.max_x(), bounds.max_y()))
                        })
                    })
                    .collect();
                let mut hits = hits;
                hits.sort();
                hits.dedup();
                if mods.shift {
                    hits.retain(|id| !self.selection.contains(id));
                    self.selection.extend(hits);
                    effects.merge(Effects {
                        overlay_changed: true,
                        ..Effects::default()
                    });
                } else if hits.is_empty() {
                    effects.merge(self.clear_selection());
                } else {
                    hits.sort();
                    effects.merge(self.select(hits));
                }
            }
            Interaction::Moving {
                start,
                originals,
                arrow_ids,
                started_in_selection,
                ..
            } => {
                let end = self.camera.screen_to_world(screen);
                let moved = end.distance(start) > 0.5 / self.camera.zoom;
                // Preview ran iff the pointer actually moved the drag total.
                let previewed =
                    self.drag_applied.x.abs() > 1e-9 || self.drag_applied.y.abs() > 1e-9;
                // Commit the preview truth: write working + attached endpoints
                // back (single pass each), unless this was a click.
                let mut attached_pres: Vec<(ElementId, Element)> = Vec::new();
                if previewed {
                    for element in std::mem::take(&mut self.drag_working) {
                        if let Some(slot) = self.doc.element_mut(&element.id) {
                            *slot = element;
                            slot.touch(now);
                        }
                    }
                    for meta in std::mem::take(&mut self.drag_attached) {
                        attached_pres.push((meta.id.clone(), meta.pre.clone()));
                        if let Some(element) = self.doc.element_mut(&meta.id) {
                            if let ElementKind::Arrow { start, end, .. } = &mut element.kind {
                                start.x = meta.a.x;
                                start.y = meta.a.y;
                                end.x = meta.b.x;
                                end.y = meta.b.y;
                            }
                            element.touch(now);
                        }
                    }
                }
                self.sync_frame_membership_for(
                    &originals
                        .iter()
                        .map(|o| o.id.clone())
                        .chain(arrow_ids.iter().cloned())
                        .collect(),
                    now,
                );
                // Exact settle for rubber-banded arrows: reflow their labels
                // now so the staged undo step covers routes + labels.
                // Rigid-shifted shapes and selected arrows are already exact.
                let attached: BTreeSet<ElementId> = arrow_ids.iter().cloned().collect();
                if previewed && !attached.is_empty() {
                    let reflows = self.bound_text_reflows(&attached);
                    if let Some(tx) = self.tx.as_mut() {
                        for (text_id, rect) in &reflows {
                            let _ = tx.update(&mut self.doc, text_id, now, |element| {
                                if let ElementKind::Text { rect: r, .. } = &mut element.kind {
                                    *r = *rect;
                                }
                            });
                        }
                    }
                    // Reflowed labels changed geometry after the rigid shift:
                    // rebuild them exactly instead of skipping as previewed.
                    for (text_id, _) in &reflows {
                        self.scene.unmark_preview(text_id);
                    }
                }
                if let Some(mut tx) = self.tx.take() {
                    // Stage the whole gesture as one undo step: pre-gesture
                    // befores plus afters from the just-written document.
                    // (Preview moves bypass per-move staging.)
                    if previewed {
                        // The document already holds the touched preview
                        // truth. Stage exactly that: touching a second copy
                        // would give the outbox delta a newer version than
                        // the local document, so a later undo could tie with
                        // it and lose the merge to its own move.
                        for original in &originals {
                            if let Some(current) = self.doc.element(&original.id).cloned() {
                                tx.push(Delta::Update {
                                    before: original.clone(),
                                    after: current,
                                });
                            }
                        }
                        for (id, pre) in attached_pres {
                            if let Some(current) = self.doc.element(&id).cloned() {
                                tx.push(Delta::Update {
                                    before: pre,
                                    after: current,
                                });
                            }
                        }
                    }
                    let mut changed: BTreeSet<ElementId> =
                        originals.iter().map(|o| o.id.clone()).collect();
                    changed.extend(arrow_ids.iter().cloned());
                    for delta in tx.deltas().iter() {
                        changed.insert(delta.element_id().clone());
                    }
                    tx.commit(&mut self.history);
                    effects.merge(Effects {
                        changed,
                        overlay_changed: true,
                        ..Effects::default()
                    });
                }
                // Clicking empty space inside the selection's bounding box
                // deselects; only an actual drag moves the selection.
                if started_in_selection && !moved {
                    effects.merge(self.clear_selection());
                }
                self.drag_applied = Point::ZERO;
                self.drag_slots.clear();
                self.drag_frozen.clear();
            }
            Interaction::Resizing { .. } => {
                let changed: BTreeSet<ElementId> = self
                    .tx
                    .as_ref()
                    .map(|tx| tx.deltas().iter().map(|d| d.element_id().clone()).collect())
                    .unwrap_or_default();
                self.sync_frame_membership(now);
                // Bound arrows re-resolve against the resized elements first —
                // their labels reflow only after the new geometry is in place.
                let arrow_updates = self.connected_arrow_updates(&changed);
                let mut reflow_targets = changed.clone();
                if let Some(tx) = self.tx.as_mut() {
                    for (arrow_id, start_point, end_point) in arrow_updates {
                        reflow_targets.insert(arrow_id.clone());
                        let _ = tx.update(&mut self.doc, &arrow_id, now, |element| {
                            if let ElementKind::Arrow { start, end, .. } = &mut element.kind {
                                start.x = start_point.x;
                                start.y = start_point.y;
                                end.x = end_point.x;
                                end.y = end_point.y;
                            }
                        });
                    }
                }
                let reflows = self.bound_text_reflows(&reflow_targets);
                if let Some(tx) = self.tx.as_mut() {
                    for (text_id, rect) in &reflows {
                        let _ = tx.update(&mut self.doc, text_id, now, |element| {
                            if let ElementKind::Text { rect: r, .. } = &mut element.kind {
                                *r = *rect;
                            }
                        });
                    }
                }
                if let Some(tx) = self.tx.take() {
                    let mut changed_now: BTreeSet<ElementId> =
                        tx.deltas().iter().map(|d| d.element_id().clone()).collect();
                    changed_now.extend(changed);
                    tx.commit(&mut self.history);
                    effects.merge(Effects {
                        changed: changed_now,
                        overlay_changed: true,
                        ..Effects::default()
                    });
                }
            }
            Interaction::Erasing { erased } => {
                if let Some(tx) = self.tx.take() {
                    tx.commit(&mut self.history);
                }
                effects.changed.extend(erased);
                effects.overlay_changed = true;
            }
            Interaction::TextEditing { id, original } => {
                // The shell owns the text editor; stay in the state.
                self.interaction = Interaction::TextEditing { id, original };
            }
            Interaction::Freehand { id } => {
                if let Some(tx) = self.tx.as_mut() {
                    let _ = tx.update(&mut self.doc, &id, now, |element| {
                        if let ElementKind::Freehand {
                            points, pressures, ..
                        } = &mut element.kind
                        {
                            let simplified = simplify_freehand(std::mem::take(points));
                            *points = simplified;
                            let n = points.len();
                            pressures.truncate(n);
                            while pressures.len() < n {
                                pressures.push(0.5);
                            }
                        }
                    });
                }
                let tiny = self
                    .doc
                    .element(&id)
                    .and_then(|e| match &e.kind {
                        ElementKind::Freehand { points, .. } => Some(points.len() < 2),
                        _ => None,
                    })
                    .unwrap_or(true);
                if tiny {
                    self.tx = None;
                    let purged = self.doc.element(&id).cloned();
                    if let Some(element) = purged {
                        let _ = self.doc.apply(&Delta::Purge { element });
                    }
                    effects.merge(Effects {
                        changed: [id].into_iter().collect(),
                        overlay_changed: true,
                        ..Effects::default()
                    });
                } else {
                    self.sync_frame_membership(now);
                    if let Some(tx) = self.tx.take() {
                        tx.commit(&mut self.history);
                    }
                    self.finish_tool_use();
                    let selection_effects = self.select([id.clone()]);
                    effects.changed.insert(id);
                    effects.merge(selection_effects);
                }
            }
            Interaction::Line { id, .. } => {
                let tiny = self
                    .doc
                    .element(&id)
                    .map(|e| {
                        let b = e.bounds();
                        b.w < self.min_draw_size() && b.h < self.min_draw_size()
                    })
                    .unwrap_or(true);
                if tiny {
                    self.tx = None;
                    let purged = self.doc.element(&id).cloned();
                    if let Some(element) = purged {
                        let _ = self.doc.apply(&Delta::Purge { element });
                    }
                    effects.merge(Effects {
                        changed: [id].into_iter().collect(),
                        overlay_changed: true,
                        ..Effects::default()
                    });
                } else {
                    self.sync_frame_membership(now);
                    if let Some(tx) = self.tx.take() {
                        tx.commit(&mut self.history);
                    }
                    self.finish_tool_use();
                    let selection_effects = self.select([id.clone()]);
                    effects.changed.insert(id);
                    effects.merge(selection_effects);
                }
            }
            Interaction::PullingSticky { id, origin } => {
                let end = self.camera.screen_to_world(screen);
                let pulled = end.distance(origin) > 3.0 / self.camera.zoom;
                if pulled {
                    self.sync_frame_membership(now);
                    if let Some(tx) = self.tx.take() {
                        tx.commit(&mut self.history);
                    }
                    self.finish_tool_use();
                    let selection_effects = self.select([id.clone()]);
                    effects.changed.insert(id);
                    effects.merge(selection_effects);
                } else {
                    // A click on the pile leaves it as it was.
                    self.tx = None;
                    let purged = self.doc.element(&id).cloned();
                    if let Some(element) = purged {
                        let _ = self.doc.apply(&Delta::Purge { element });
                    }
                    self.finish_tool_use();
                    effects.merge(Effects {
                        changed: [id].into_iter().collect(),
                        overlay_changed: true,
                        ..Effects::default()
                    });
                }
            }
            Interaction::Connecting { id, .. } => {
                self.hover_anchors = None;
                let tiny = self
                    .doc
                    .element(&id)
                    .map(|e| {
                        let b = e.bounds();
                        b.w < self.min_draw_size() && b.h < self.min_draw_size()
                    })
                    .unwrap_or(true);
                if tiny {
                    self.tx = None;
                    let purged = self.doc.element(&id).cloned();
                    if let Some(element) = purged {
                        let _ = self.doc.apply(&Delta::Purge { element });
                    }
                    effects.merge(Effects {
                        changed: [id].into_iter().collect(),
                        overlay_changed: true,
                        ..Effects::default()
                    });
                } else {
                    self.sync_frame_membership(now);
                    if let Some(tx) = self.tx.take() {
                        tx.commit(&mut self.history);
                    }
                    self.finish_tool_use();
                    let selection_effects = self.select([id.clone()]);
                    effects.changed.insert(id);
                    effects.merge(selection_effects);
                }
            }
            Interaction::ArrowEndpoint { id, .. } | Interaction::ArrowWaypoint { id, .. } => {
                self.hover_anchors = None;
                // The connector's label rides along its new geometry.
                let reflows = self.bound_text_reflows(&[id.clone()].into_iter().collect());
                if let Some(tx) = self.tx.as_mut() {
                    for (text_id, rect) in &reflows {
                        let _ = tx.update(&mut self.doc, text_id, now, |element| {
                            if let ElementKind::Text { rect: r, .. } = &mut element.kind {
                                *r = *rect;
                            }
                        });
                    }
                }
                self.sync_frame_membership(now);
                if let Some(tx) = self.tx.take() {
                    let changed: BTreeSet<ElementId> =
                        tx.deltas().iter().map(|d| d.element_id().clone()).collect();
                    tx.commit(&mut self.history);
                    effects.merge(Effects {
                        changed,
                        overlay_changed: true,
                        ..Effects::default()
                    });
                }
                let selection_effects = self.select([id.clone()]);
                effects.changed.insert(id);
                effects.merge(selection_effects);
            }
        }
        effects
    }

    /// Double-click: edit text, or create bound text inside a shape/glyph.
    pub fn double_click(&mut self, screen: Point, now: u64) -> Effects {
        if self.interaction != Interaction::Idle {
            return Effects::default();
        }
        let world = self.camera.screen_to_world(screen);
        // Double-clicking a bend removes it.
        if let Some((id, ArrowHandle::Waypoint { index, .. })) = self.arrow_handle_at(world) {
            let mut tx = Transaction::begin();
            if let Err(err) = tx.update(&mut self.doc, &id, now, |element| {
                if let ElementKind::Arrow { waypoints, .. } = &mut element.kind {
                    if index < waypoints.len() {
                        waypoints.remove(index);
                    }
                }
            }) {
                debug_assert!(false, "waypoint removal failed: {err}");
            }
            let mut changed: BTreeSet<ElementId> = [id.clone()].into_iter().collect();
            for (text_id, rect) in self.bound_text_reflows(&changed) {
                let _ = tx.update(&mut self.doc, &text_id, now, |element| {
                    if let ElementKind::Text { rect: r, .. } = &mut element.kind {
                        *r = rect;
                    }
                });
                changed.insert(text_id);
            }
            tx.commit(&mut self.history);
            return Effects {
                changed,
                overlay_changed: true,
                ..Effects::default()
            };
        }
        let tolerance = self.tolerance();
        let Some(id) = hit_test_raw_indexed(&self.doc, &self.hit, world, tolerance) else {
            return Effects::default();
        };
        enum Target {
            Existing(ElementId),
            Bound {
                container: ElementId,
                rect: Rect,
                existing: Option<ElementId>,
            },
            Frame {
                id: ElementId,
                rect: Rect,
                name: String,
            },
        }
        let arrow_bounds = self
            .doc
            .element(&id)
            .map(|e| e.bounds())
            .unwrap_or(Rect::ZERO);
        let target = match self.doc.element(&id).map(|e| &e.kind) {
            Some(ElementKind::Text { .. }) => Target::Existing(id.clone()),
            Some(ElementKind::Shape {
                bound_text_id,
                rect,
                ..
            }) => Target::Bound {
                container: id.clone(),
                rect: *rect,
                existing: bound_text_id.clone(),
            },
            Some(ElementKind::Glyph {
                caption_id, rect, ..
            }) => Target::Bound {
                container: id.clone(),
                rect: *rect,
                existing: caption_id.clone(),
            },
            Some(ElementKind::Arrow { label_id, .. }) => Target::Bound {
                container: id.clone(),
                rect: arrow_bounds,
                existing: label_id.clone(),
            },
            Some(ElementKind::Frame { rect, name, .. }) => Target::Frame {
                id: id.clone(),
                rect: *rect,
                name: name.clone(),
            },
            _ => return Effects::default(),
        };
        match target {
            Target::Existing(id) => self.begin_text_edit(self.doc.element(&id).cloned(), now),
            Target::Bound {
                container,
                rect,
                existing,
            } => {
                // A dangling link (corrupted document, abandoned draft) heals
                // by creating a fresh bound text instead of editing nothing.
                match existing
                    .and_then(|id| self.doc.element(&id).filter(|e| !e.is_deleted).map(|_| id))
                {
                    Some(id) => self.begin_text_edit(self.doc.element(&id).cloned(), now),
                    None => self.create_bound_text(container, rect, now),
                }
            }
            Target::Frame { id, rect, name } => {
                self.interaction = Interaction::FrameNameEditing { id: id.clone() };
                let top_left = self
                    .camera
                    .world_to_screen(Point::new(rect.x, rect.y - 18.0));
                let action = ShellAction::OpenTextEditor {
                    id,
                    x: top_left.x,
                    y: top_left.y,
                    width: (rect.w * self.camera.zoom).max(120.0),
                    height: 20.0 * self.camera.zoom.max(0.5),
                    font_size: 14.0 * self.camera.zoom,
                    text: name,
                    align: TextAlign::Left,
                    single_line: true,
                };
                Effects {
                    overlay_changed: true,
                    actions: vec![action],
                    ..Effects::default()
                }
            }
        }
    }

    /// The text tool: clicking existing text edits it; clicking empty canvas
    /// starts a new point-text element.
    fn begin_text_tool(&mut self, world: Point, now: u64) -> Effects {
        let tolerance = self.tolerance();
        if let Some(id) = hit_test_raw_indexed(&self.doc, &self.hit, world, tolerance) {
            match self.doc.element(&id).map(|e| &e.kind) {
                Some(ElementKind::Text { .. }) => {
                    return self.begin_text_edit(self.doc.element(&id).cloned(), now);
                }
                // Clicking a container with the text tool labels it: the text
                // is bound and centred, so it lands in the middle of the
                // element instead of wherever the click was.
                Some(ElementKind::Shape { .. })
                | Some(ElementKind::Glyph { .. })
                | Some(ElementKind::Arrow { .. }) => {
                    return self.begin_bound_text(id, now);
                }
                _ => {}
            }
        }
        self.start_text(world, now)
    }

    /// Edit (or create) the text bound to a container, centred in it.
    fn begin_bound_text(&mut self, container_id: ElementId, now: u64) -> Effects {
        let Some(element) = self.doc.element(&container_id).cloned() else {
            return Effects::default();
        };
        let (rect, existing) = match &element.kind {
            ElementKind::Shape {
                rect,
                bound_text_id,
                ..
            } => (*rect, bound_text_id.clone()),
            ElementKind::Glyph {
                rect, caption_id, ..
            } => (*rect, caption_id.clone()),
            ElementKind::Arrow { label_id, .. } => (element.bounds(), label_id.clone()),
            _ => return Effects::default(),
        };
        match existing {
            Some(text_id) => {
                // A dangling link (corrupted document, abandoned draft) heals
                // here: treat it like no link and create a fresh bound text.
                match self
                    .doc
                    .element(&text_id)
                    .filter(|element| !element.is_deleted)
                    .cloned()
                {
                    Some(element) => self.begin_text_edit(Some(element), now),
                    None => self.create_bound_text(container_id, rect, now),
                }
            }
            None => self.create_bound_text(container_id, rect, now),
        }
    }

    /// Create a point-text element and open the editor.
    pub fn start_text(&mut self, world: Point, now: u64) -> Effects {
        if self.interaction != Interaction::Idle {
            return Effects::default();
        }
        let mut tx = Transaction::begin();
        let line_height = self.measurer.line_height(self.style.font_size);
        let element = Element::new(
            ElementKind::Text {
                rect: Rect::new(
                    world.x,
                    // Centre the first line on the click so the text appears
                    // where the pointer is, not below it.
                    world.y - line_height / 2.0,
                    0.0,
                    line_height,
                ),
                text: String::new(),
                align: TextAlign::Left,
                fixed_width: false,
                container_id: None,
                style: self.style.clone(),
            },
            self.next_index(),
            now,
        );
        let id = element.id.clone();
        if let Err(err) = tx.add(&mut self.doc, element) {
            debug_assert!(false, "text add failed: {err}");
        }
        self.tx = Some(tx);
        self.interaction = Interaction::TextEditing {
            id: id.clone(),
            original: Box::new(None),
        };
        let action = self.text_editor_action(&id, String::new());
        Effects {
            changed: [id].into_iter().collect(),
            overlay_changed: true,
            actions: action.into_iter().collect(),
            ..Effects::default()
        }
    }

    fn begin_text_edit(&mut self, existing: Option<Element>, now: u64) -> Effects {
        let _ = now;
        let Some(element) = existing else {
            return Effects::default();
        };
        let id = element.id.clone();
        let content = match &element.kind {
            ElementKind::Text { text, .. } => text.clone(),
            _ => return Effects::default(),
        };
        self.tx = Some(Transaction::begin());
        self.interaction = Interaction::TextEditing {
            id: id.clone(),
            original: Box::new(Some(element)),
        };
        let action = self.text_editor_action(&id, content);
        Effects {
            changed: [id].into_iter().collect(),
            overlay_changed: true,
            actions: action.into_iter().collect(),
            ..Effects::default()
        }
    }

    fn create_bound_text(
        &mut self,
        container_id: ElementId,
        container_rect: Rect,
        now: u64,
    ) -> Effects {
        let is_glyph = matches!(
            self.doc.element(&container_id).map(|e| &e.kind),
            Some(ElementKind::Glyph { .. })
        );
        let is_sticky = matches!(
            self.doc.element(&container_id).map(|e| &e.kind),
            Some(ElementKind::Shape {
                shape: ShapeKind::Sticky,
                ..
            })
        );
        let mut tx = Transaction::begin();
        let font_size = self.style.font_size;
        let line_height = self.measurer.line_height(font_size);
        // Sticky text sits inside the note, inked in the note's own (dark)
        // outline colour so it reads on the pastel fill.
        let padding = if is_sticky { 14.0 } else { 10.0 };
        let mut text_style = self.style.clone();
        if is_sticky {
            if let Some(ElementKind::Shape { style, .. }) =
                self.doc.element(&container_id).map(|e| &e.kind)
            {
                text_style.stroke_color = style.stroke_color.clone();
            }
        }
        let element = Element::new(
            ElementKind::Text {
                rect: Rect::new(
                    container_rect.x + padding,
                    if is_glyph {
                        container_rect.max_y() + 4.0
                    } else if is_sticky {
                        container_rect.y + (container_rect.h - line_height) / 2.0
                    } else {
                        container_rect.center().y
                    },
                    (container_rect.w - padding * 2.0).max(20.0),
                    line_height,
                ),
                text: String::new(),
                align: TextAlign::Center,
                fixed_width: true,
                container_id: Some(container_id.clone()),
                style: text_style,
            },
            self.next_index(),
            now,
        );
        let text_id = element.id.clone();
        if let Err(err) = tx.add(&mut self.doc, element) {
            debug_assert!(false, "bound text add failed: {err}");
        }
        let link_id = text_id.clone();
        if let Err(err) = tx.update(
            &mut self.doc,
            &container_id,
            now,
            |element| match &mut element.kind {
                ElementKind::Shape { bound_text_id, .. } => *bound_text_id = Some(link_id),
                ElementKind::Glyph { caption_id, .. } => *caption_id = Some(link_id),
                ElementKind::Arrow { label_id, .. } => *label_id = Some(link_id),
                _ => {}
            },
        ) {
            debug_assert!(false, "container link failed: {err}");
        }
        self.tx = Some(tx);
        self.interaction = Interaction::TextEditing {
            id: text_id.clone(),
            original: Box::new(None),
        };
        let action = self.text_editor_action(&text_id, String::new());
        Effects {
            changed: [container_id, text_id].into_iter().collect(),
            overlay_changed: true,
            actions: action.into_iter().collect(),
            ..Effects::default()
        }
    }

    fn text_editor_action(&self, id: &str, content: String) -> Option<ShellAction> {
        let element = self.doc.element(id)?;
        let (rect, font_size, align) = match &element.kind {
            ElementKind::Text {
                rect, align, style, ..
            } => (*rect, style.font_size, *align),
            _ => return None,
        };
        let top_left = self.camera.world_to_screen(Point::new(rect.x, rect.y));
        let bottom_right = self
            .camera
            .world_to_screen(Point::new(rect.max_x(), rect.max_y()));
        let screen = Rect::from_points(top_left, bottom_right);
        Some(ShellAction::OpenTextEditor {
            id: id.into(),
            x: screen.x,
            y: screen.y,
            width: screen.w.max(60.0),
            height: screen.h.max(font_size * self.camera.zoom * 1.6),
            font_size: font_size * self.camera.zoom,
            text: content,
            align,
            single_line: false,
        })
    }

    /// Commit the open text edit with the shell's textarea contents.
    pub fn commit_text(&mut self, id: &str, text: &str, now: u64) -> Effects {
        if let Interaction::FrameNameEditing { id: editing_id } = &self.interaction {
            if editing_id != id {
                return Effects::default();
            }
            self.interaction = Interaction::Idle;
            let effects = Effects {
                changed: [id.to_string()].into_iter().collect(),
                overlay_changed: true,
                ..Effects::default()
            };
            if !text.trim().is_empty() {
                let mut tx = Transaction::begin();
                let name = text.to_string();
                let _ = tx.update(&mut self.doc, id, now, |element| {
                    if let ElementKind::Frame { name: current, .. } = &mut element.kind {
                        *current = name.clone();
                    }
                });
                tx.commit(&mut self.history);
            }
            return effects;
        }
        if !matches!(self.interaction, Interaction::TextEditing { .. }) {
            return Effects::default();
        }
        let interaction = std::mem::replace(&mut self.interaction, Interaction::Idle);
        let Interaction::TextEditing {
            id: editing_id,
            original,
        } = interaction
        else {
            unreachable!("checked above");
        };
        let original = *original;
        if editing_id != id {
            return Effects::default();
        }
        let tx = self.tx.take();
        self.interaction = Interaction::Idle;
        let mut effects = Effects {
            changed: [id.to_string()].into_iter().collect(),
            overlay_changed: true,
            ..Effects::default()
        };

        if text.trim().is_empty() {
            match original {
                Some(orig) => {
                    if let Some(mut tx) = tx {
                        let _ = tx.update(&mut self.doc, id, now, |element| {
                            if let (
                                ElementKind::Text {
                                    text: current,
                                    rect,
                                    ..
                                },
                                ElementKind::Text {
                                    text: orig_text,
                                    rect: orig_rect,
                                    ..
                                },
                            ) = (&mut element.kind, &orig.kind)
                            {
                                *current = orig_text.clone();
                                *rect = *orig_rect;
                            }
                        });
                        tx.commit(&mut self.history);
                    }
                }
                None => {
                    // Abandon the whole draft transaction: reverting drops the
                    // fresh text AND the container link it created (bound
                    // text/caption/label), so no dangling bound_text_id is
                    // left and nothing lands in history.
                    match tx {
                        Some(tx) => effects.changed.extend(self.revert_transaction(&tx)),
                        None => {
                            if let Some(element) = self.doc.element(id).cloned() {
                                let _ = self.doc.apply(&Delta::Purge { element });
                            }
                            self.unlink_dead_reference(id);
                        }
                    }
                    self.selection.remove(id);
                }
            }
            return effects;
        }

        let new_rect = self.text_rect_for(id, text);
        let was_new_text = original.is_none();
        if let (Some(mut tx), Some(new_rect)) = (tx, new_rect) {
            let content = text.to_string();
            let _ = tx.update(&mut self.doc, id, now, |element| {
                if let ElementKind::Text { text, rect, .. } = &mut element.kind {
                    *text = content.clone();
                    *rect = new_rect;
                }
            });
            // Text committed inside a frame becomes its child, like any other
            // element, so the frame carries it when it moves.
            self.tx = Some(tx);
            self.sync_frame_membership(now);
            if let Some(tx) = self.tx.take() {
                tx.commit(&mut self.history);
            }
            self.selection = [id.to_string()].into_iter().collect();
            effects.changed.insert(id.to_string());
            if was_new_text {
                self.finish_tool_use();
            }
        }
        effects
    }

    fn text_rect_for(&self, id: &str, text: &str) -> Option<Rect> {
        let element = self.doc.element(id)?;
        let (rect, fixed_width, container_id, font_size) = match &element.kind {
            ElementKind::Text {
                rect,
                fixed_width,
                container_id,
                style,
                ..
            } => (*rect, *fixed_width, container_id.clone(), style.font_size),
            _ => return None,
        };
        let measurer = self.measurer.as_ref();
        let Some(container_id) = container_id else {
            let layout = if fixed_width {
                layout_text(text, font_size, Some(rect.w), measurer)
            } else {
                layout_text(text, font_size, None, measurer)
            };
            let width = if fixed_width {
                rect.w
            } else {
                layout.width.max(1.0)
            };
            return Some(Rect::new(rect.x, rect.y, width, layout.height));
        };
        let container = self.doc.element(&container_id)?;
        let container_rect = container.bounds();
        let padding = 10.0;
        let wrap = (container_rect.w - padding * 2.0).max(20.0);
        let layout = layout_text(text, font_size, Some(wrap), measurer);
        match &container.kind {
            ElementKind::Glyph { .. } => Some(Rect::new(
                container_rect.x,
                container_rect.max_y() + 4.0,
                container_rect.w,
                layout.height,
            )),
            ElementKind::Arrow {
                start,
                end,
                routing,
                waypoints,
                ..
            } => {
                let points = connector::route_avoiding(
                    start,
                    end,
                    *routing,
                    waypoints,
                    &self.doc,
                    &self.obstacles,
                );
                let mid = connector::point_at(&points, 0.5);
                let width = if rect.w > 1.0 { rect.w } else { 100.0 };
                Some(Rect::new(
                    mid.x - width / 2.0,
                    mid.y - layout.height / 2.0,
                    width,
                    layout.height,
                ))
            }
            _ => Some(Rect::new(
                container_rect.x + padding,
                container_rect.y + (container_rect.h - layout.height) / 2.0,
                container_rect.w - padding * 2.0,
                layout.height,
            )),
        }
    }

    /// Revert an abandoned transaction against the live document, returning
    /// the ids it touched. Drafts are thrown away this way so every element
    /// they created or modified is restored together.
    fn revert_transaction(&mut self, tx: &Transaction) -> BTreeSet<ElementId> {
        let changed: BTreeSet<ElementId> = tx
            .deltas()
            .iter()
            .map(|delta| delta.element_id().clone())
            .collect();
        for delta in tx.deltas().iter().rev() {
            let _ = self.doc.apply(&delta.invert());
        }
        changed
    }

    /// Clear any container's bound-text link that points at `id`. The
    /// transactional paths can't leave these dangling; this is a defensive
    /// fallback for edits that bypass history.
    fn unlink_dead_reference(&mut self, id: &str) {
        let owners: Vec<ElementId> = self
            .doc
            .elements
            .values()
            .filter(|element| match &element.kind {
                ElementKind::Shape { bound_text_id, .. } => bound_text_id.as_deref() == Some(id),
                ElementKind::Glyph { caption_id, .. } => caption_id.as_deref() == Some(id),
                ElementKind::Arrow { label_id, .. } => label_id.as_deref() == Some(id),
                _ => false,
            })
            .map(|element| element.id.clone())
            .collect();
        for owner_id in owners {
            let Some(before) = self.doc.element(&owner_id).cloned() else {
                continue;
            };
            let mut after = before.clone();
            match &mut after.kind {
                ElementKind::Shape { bound_text_id, .. } => *bound_text_id = None,
                ElementKind::Glyph { caption_id, .. } => *caption_id = None,
                ElementKind::Arrow { label_id, .. } => *label_id = None,
                _ => {}
            }
            let _ = self.doc.apply(&Delta::Update { before, after });
        }
    }

    // -- keyboard ----------------------------------------------------------

    pub fn key_down(&mut self, key: &str, mods: Modifiers, now: u64) -> Effects {
        match key {
            "Escape" => self.cancel(),
            "Delete" | "Backspace" => self.delete_selection(now),
            "ArrowUp" => self.nudge(0.0, -self.nudge_step(mods), now),
            "ArrowDown" => self.nudge(0.0, self.nudge_step(mods), now),
            "ArrowLeft" => self.nudge(-self.nudge_step(mods), 0.0, now),
            "ArrowRight" => self.nudge(self.nudge_step(mods), 0.0, now),
            "a" | "A" if mods.command() => self.select_all(),
            "z" | "Z" if mods.command() && mods.shift => self.redo(now),
            "z" | "Z" if mods.command() => self.undo(now),
            "d" | "D" if mods.command() => self.duplicate(now),
            "c" | "C" if mods.command() => self.copy(),
            "x" | "X" if mods.command() => self.cut(now),
            "v" | "V" if mods.command() => self.paste(now),
            "]" | "}" if mods.command() => self.reorder(1, now),
            "[" | "{" if mods.command() => self.reorder(-1, now),
            " " => {
                self.nav.set_space(true);
                Effects::default()
            }
            _ => {
                if mods.command() || mods.alt {
                    return Effects::default();
                }
                if let Some(tool) = Tool::from_shortcut(key) {
                    if tool == self.tool {
                        self.tool_locked = !self.tool_locked;
                        return Effects {
                            overlay_changed: true,
                            ..Effects::default()
                        };
                    }
                    return self.set_tool(tool);
                }
                Effects::default()
            }
        }
    }

    fn nudge_step(&self, mods: Modifiers) -> f64 {
        if self.snap_to_grid {
            return self.grid_size;
        }
        if mods.shift {
            10.0 / self.camera.zoom
        } else {
            1.0 / self.camera.zoom
        }
    }

    pub fn key_up(&mut self, key: &str) -> Effects {
        if key == " " {
            self.nav.set_space(false);
        }
        Effects::default()
    }

    pub fn cancel(&mut self) -> Effects {
        let interaction = std::mem::take(&mut self.interaction);
        let current = self.marquee_current.take();
        let _ = current;
        match interaction {
            Interaction::Idle => self.clear_selection(),
            Interaction::Drawing { id, .. } | Interaction::PullingSticky { id, .. } => {
                self.tx = None;
                let purged = self.doc.element(&id).cloned();
                if let Some(element) = purged {
                    let _ = self.doc.apply(&Delta::Purge { element });
                }
                Effects {
                    changed: [id].into_iter().collect(),
                    overlay_changed: true,
                    ..Effects::default()
                }
            }
            Interaction::Marquee { .. } => Effects {
                overlay_changed: true,
                ..Effects::default()
            },
            Interaction::Moving { .. } => {
                self.hover_anchors = None;
                // Preview never touched the document: unshift the rigid scene
                // preview and rebuild the rubber-banded arrows exactly. The
                // unshift restores bounds; ops points were deferred all
                // along, so nothing else needs restoring.
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
                    self.scene.shift_slots(&slots, -total.x, -total.y);
                } else {
                    self.drag_slots.clear();
                }
                Effects {
                    changed: attached,
                    overlay_changed: true,
                    ..Effects::default()
                }
            }
            Interaction::Resizing { .. }
            | Interaction::ArrowEndpoint { .. }
            | Interaction::ArrowWaypoint { .. } => {
                self.hover_anchors = None;
                // Drop gesture-preview caches (rigid shifts, rubber bands):
                // the changed set below rebuilds everything exactly.
                self.scene.discard_preview();
                if let Some(tx) = self.tx.take() {
                    for delta in tx.deltas().iter().rev() {
                        let _ = self.doc.apply(&delta.invert());
                    }
                    let changed: BTreeSet<ElementId> =
                        tx.deltas().iter().map(|d| d.element_id().clone()).collect();
                    return Effects {
                        changed,
                        overlay_changed: true,
                        ..Effects::default()
                    };
                }
                Effects {
                    overlay_changed: true,
                    ..Effects::default()
                }
            }
            Interaction::Erasing { erased } => {
                if let Some(tx) = self.tx.take() {
                    for delta in tx.deltas().iter().rev() {
                        let _ = self.doc.apply(&delta.invert());
                    }
                }
                Effects {
                    changed: erased,
                    overlay_changed: true,
                    ..Effects::default()
                }
            }
            Interaction::Freehand { id } | Interaction::Line { id, .. } => {
                self.tx = None;
                let purged = self.doc.element(&id).cloned();
                if let Some(element) = purged {
                    let _ = self.doc.apply(&Delta::Purge { element });
                }
                Effects {
                    changed: [id].into_iter().collect(),
                    overlay_changed: true,
                    ..Effects::default()
                }
            }
            Interaction::Connecting { id, .. } => {
                self.tx = None;
                self.hover_anchors = None;
                let purged = self.doc.element(&id).cloned();
                if let Some(element) = purged {
                    let _ = self.doc.apply(&Delta::Purge { element });
                }
                Effects {
                    changed: [id].into_iter().collect(),
                    overlay_changed: true,
                    ..Effects::default()
                }
            }
            Interaction::TextEditing { id, original } => {
                let mut changed: BTreeSet<ElementId> = [id.clone()].into_iter().collect();
                match *original {
                    Some(orig) => {
                        self.tx = None;
                        if let Some(current) = self.doc.element(&id).cloned() {
                            let _ = self.doc.apply(&Delta::Update {
                                before: current,
                                after: orig,
                            });
                        }
                    }
                    None => match self.tx.take() {
                        Some(tx) => changed.extend(self.revert_transaction(&tx)),
                        None => {
                            if let Some(element) = self.doc.element(&id).cloned() {
                                let _ = self.doc.apply(&Delta::Purge { element });
                            }
                            self.unlink_dead_reference(&id);
                        }
                    },
                }
                Effects {
                    changed,
                    overlay_changed: true,
                    ..Effects::default()
                }
            }
            Interaction::FrameNameEditing { id } => {
                self.interaction = Interaction::Idle;
                Effects {
                    changed: [id].into_iter().collect(),
                    overlay_changed: true,
                    ..Effects::default()
                }
            }
        }
    }

    // -- commands ----------------------------------------------------------

    pub fn delete_selection(&mut self, now: u64) -> Effects {
        if self.interaction != Interaction::Idle {
            return self.cancel();
        }
        if self.selection.is_empty() {
            return Effects::default();
        }
        let mut tx = Transaction::begin();
        let ids: Vec<ElementId> = self
            .selection_with_followers_recursive()
            .into_iter()
            .collect();
        for id in &ids {
            if let Err(err) = tx.remove(&mut self.doc, id, now) {
                debug_assert!(false, "delete failed: {err}");
            }
        }
        tx.commit(&mut self.history);
        self.selection.clear();
        Effects {
            changed: ids.into_iter().collect(),
            overlay_changed: true,
            ..Effects::default()
        }
    }

    pub fn undo(&mut self, now: u64) -> Effects {
        let changed: BTreeSet<ElementId> = match self.history.undo(&mut self.doc, now) {
            Ok(ids) => ids.into_iter().collect(),
            Err(err) => {
                debug_assert!(false, "undo failed: {err}");
                BTreeSet::new()
            }
        };
        self.selection.clear();
        Effects {
            changed,
            overlay_changed: true,
            document_changed: true,
            ..Effects::default()
        }
    }

    pub fn redo(&mut self, now: u64) -> Effects {
        let changed: BTreeSet<ElementId> = match self.history.redo(&mut self.doc, now) {
            Ok(ids) => ids.into_iter().collect(),
            Err(err) => {
                debug_assert!(false, "redo failed: {err}");
                BTreeSet::new()
            }
        };
        self.selection.clear();
        Effects {
            changed,
            overlay_changed: true,
            document_changed: true,
            ..Effects::default()
        }
    }

    // -- tool helpers ------------------------------------------------------

    /// Begin a selection drag: snapshot contiguous preview truth, precompute
    /// frozen ends and attached-arrow state, resolve scene slots. One setup
    /// pass so every move stays allocation-free over contiguous memory.
    fn begin_move(
        &mut self,
        world: Point,
        originals: Vec<Element>,
        arrow_ids: Vec<ElementId>,
        started_in_selection: bool,
    ) -> Effects {
        let moved: HashSet<&str> = originals.iter().map(|e| e.id.as_str()).collect();
        // Selected arrows bound to static targets freeze those ends to the
        // anchor resolved once here; everything else rides rigidly.
        let mut frozen: Vec<(usize, Option<Point>, Option<Point>)> = Vec::new();
        for (idx, original) in originals.iter().enumerate() {
            let ElementKind::Arrow { start, end, .. } = &original.kind else {
                continue;
            };
            let freeze = |endpoint: &Endpoint, other: Point| -> Option<Point> {
                match &endpoint.binding {
                    Some(binding) if !moved.contains(binding.element_id.as_str()) => {
                        let (point, _) = connector::resolve_endpoint(endpoint, other, &self.doc);
                        Some(point)
                    }
                    _ => None,
                }
            };
            let start_point = start.point();
            let frozen_start = freeze(start, end.point());
            let frozen_end = freeze(end, start_point);
            if frozen_start.is_some() || frozen_end.is_some() {
                frozen.push((idx, frozen_start, frozen_end));
            }
        }
        // Attached arrows: snapshot ends, style and pre-gesture element.
        let mut attached: Vec<AttachedDrag> = Vec::new();
        for arrow_id in &arrow_ids {
            let Some(element) = self.doc.element(arrow_id) else {
                continue;
            };
            let ElementKind::Arrow {
                start, end, style, ..
            } = &element.kind
            else {
                continue;
            };
            let follows = |endpoint: &Endpoint| {
                endpoint
                    .binding
                    .as_ref()
                    .is_some_and(|b| moved.contains(b.element_id.as_str()))
            };
            attached.push(AttachedDrag {
                id: arrow_id.clone(),
                a: start.point(),
                b: end.point(),
                a_follows: follows(start),
                b_follows: follows(end),
                style: style.clone(),
                pre: element.clone(),
                built: false,
            });
        }
        let working = originals.clone();
        self.tx = Some(Transaction::begin());
        self.drag_applied = Point::ZERO;
        self.drag_slots = self.scene_slot_subset(
            &originals
                .iter()
                .map(|e| e.id.clone())
                .collect::<BTreeSet<ElementId>>(),
        );
        self.drag_working = working;
        self.drag_frozen = frozen;
        self.drag_attached = attached;
        self.interaction = Interaction::Moving {
            start: world,
            originals,
            arrow_ids,
            started_in_selection,
        };
        Effects {
            overlay_changed: true,
            ..Effects::default()
        }
    }

    fn begin_select(&mut self, screen: Point, world: Point, mods: Modifiers, now: u64) -> Effects {
        if mods.command() {
            return Effects::default();
        }
        // Dragging a connector's endpoint rewires or detaches it; the grab
        // radius matches the resize handles so the affordance is the same.
        if !mods.shift {
            if let Some((id, end)) = self.arrow_endpoint_at(world) {
                self.tx = Some(Transaction::begin());
                self.interaction = Interaction::ArrowEndpoint { id, end };
                return Effects {
                    overlay_changed: true,
                    ..Effects::default()
                };
            }
            // A bend on a selected connector: drag it to reshape the path.
            if let Some((id, handle)) = self.arrow_handle_at(world) {
                self.tx = Some(Transaction::begin());
                let drag = match handle {
                    ArrowHandle::Waypoint { index, .. } => WaypointDrag::Existing { index },
                    ArrowHandle::Insert {
                        start,
                        end,
                        insert_at,
                        ..
                    } => WaypointDrag::Bow {
                        start,
                        end,
                        grab: world,
                        insert_at,
                        inserted: None,
                    },
                };
                self.interaction = Interaction::ArrowWaypoint { id, drag };
                return Effects {
                    overlay_changed: true,
                    ..Effects::default()
                };
            }
            // A lone connector's shaft: begin Moving directly. Otherwise the
            // click lands on the degenerate bounding box's edge handles —
            // and resizing a zero-height or zero-width box is a no-op, so the
            // arrow would seem glued in place.
            if let Some(originals) = self.lone_arrow_shaft_hit(world) {
                let moved: BTreeSet<ElementId> = originals.iter().map(|e| e.id.clone()).collect();
                let arrow_ids = self.attached_arrows(&moved);
                return self.begin_move(world, originals, arrow_ids, false);
            }
        }
        // Resize handles win over connection nodes: the nodes float outside
        // the element, so a near-miss grab at the edge must still resize.
        if let Some(bounds) = self.selection_bounds() {
            if let Some(handle) = self.handle_at(screen, bounds) {
                let originals: Vec<Element> = self
                    .selection
                    .iter()
                    .filter_map(|sid| self.doc.element(sid).cloned())
                    .collect();
                self.tx = Some(Transaction::begin());
                self.interaction = Interaction::Resizing {
                    handle,
                    start: world,
                    bounds,
                    originals,
                };
                return Effects {
                    overlay_changed: true,
                    ..Effects::default()
                };
            }
        }
        if !mods.shift {
            // A connection node floating outside a hovered element starts a
            // bound arrow.
            if let Some((id, side)) = self.connection_node_at(world) {
                return self.begin_connector_from(id, side, now);
            }
            // Dragging the middle of a sticky stack pulls a note off it; its
            // border ring falls through and moves the pile.
            if let Some(stack_id) = self.stack_pull_at(world) {
                return self.begin_pull_from_stack(&stack_id, world, now);
            }
        }
        let tolerance = self.tolerance();
        match hit_test_indexed(&self.doc, &self.hit, world, tolerance) {
            Some(id) => {
                if mods.shift {
                    if self.selection.contains(&id) {
                        self.selection.remove(&id);
                    } else {
                        self.selection.insert(id);
                    }
                    self.selection_bounds_cache = None;
                    return Effects {
                        overlay_changed: true,
                        ..Effects::default()
                    };
                }
                if !self.selection.contains(&id) {
                    self.selection = [id].into_iter().collect();
                    self.selection_bounds_cache = None;
                }
                let originals: Vec<Element> = self.selection_with_followers();
                let moved: BTreeSet<ElementId> = originals.iter().map(|e| e.id.clone()).collect();
                let arrow_ids = self.attached_arrows(&moved);
                self.begin_move(world, originals, arrow_ids, false)
            }
            None => {
                // Dragging from empty space inside the selection's bounding
                // box moves the whole selection; only a click clears it.
                if !mods.shift && !self.selection.is_empty() {
                    if let Some(bounds) = self.selection_bounds() {
                        if bounds.contains(world) {
                            let originals = self.selection_with_followers();
                            let moved: BTreeSet<ElementId> =
                                originals.iter().map(|e| e.id.clone()).collect();
                            let arrow_ids = self.attached_arrows(&moved);
                            return self.begin_move(world, originals, arrow_ids, true);
                        }
                    }
                }
                self.marquee_current = Some(world);
                self.interaction = Interaction::Marquee { start: world };
                Effects {
                    overlay_changed: true,
                    ..Effects::default()
                }
            }
        }
    }

    fn begin_drawing(&mut self, world: Point, now: u64) -> Effects {
        let mut tx = Transaction::begin();
        let kind = match self.tool {
            Tool::Rectangle => ShapeKind::Rectangle,
            Tool::Diamond => ShapeKind::Diamond,
            _ => ShapeKind::Ellipse,
        };
        let element = Element::new(
            ElementKind::Shape {
                shape: kind,
                rect: Rect::new(world.x, world.y, 0.0, 0.0),
                style: self.style.clone(),
                bound_text_id: None,
            },
            self.next_index(),
            now,
        );
        let id = element.id.clone();
        if let Err(err) = tx.add(&mut self.doc, element) {
            debug_assert!(false, "drawing add failed: {err}");
        }
        self.tx = Some(tx);
        self.interaction = Interaction::Drawing {
            id: id.clone(),
            start: world,
        };
        Effects {
            changed: [id].into_iter().collect(),
            overlay_changed: true,
            ..Effects::default()
        }
    }

    fn next_index(&self) -> String {
        match self.doc.top_index() {
            Some(top) => crate::index::key_between(Some(&top), None).expect("index space"),
            None => crate::index::key_between(None, None).expect("first index"),
        }
    }

    /// Resize handle positions in screen space, if something is selected.
    pub fn resize_handles(&self) -> Option<[(Handle, Point); 8]> {
        self.selection_bounds()
            .map(|r| Handle::ALL.map(|h| (h, self.camera.world_to_screen(h.position(r)))))
    }

    fn handle_at(&self, screen: Point, _bounds: Rect) -> Option<Handle> {
        self.resize_handles().and_then(|handles| {
            handles
                .into_iter()
                .find(|(_, p)| p.distance(screen) <= HANDLE_HIT_PX)
                .map(|(h, _)| h)
        })
    }

    // -- connection nodes --------------------------------------------------

    fn node_grab(&self) -> f64 {
        HANDLE_HIT_PX / self.camera.zoom
    }

    /// Where a side's connection node floats: just outside the edge, in
    /// screen-constant distance, so it never fights the resize handles.
    fn node_position(&self, side: connector::AnchorSide, anchor: Point) -> Point {
        anchor + side.focus().scale(CONNECTION_NODE_PX / self.camera.zoom)
    }

    fn node_points_for(&self, element: &Element) -> Vec<Point> {
        connector::anchor_points(element.bounds())
            .into_iter()
            .map(|(side, anchor)| self.node_position(side, anchor))
            .collect()
    }

    /// Elements that accept connection nodes: everything an arrow can bind to
    /// except connectors themselves and the stacks (which are sources, not
    /// targets).
    fn is_connectable(element: &Element) -> bool {
        element.is_bind_target()
    }

    /// The connection node of a bindable element under `world`, if any.
    /// Returns the element, its side and the edge anchor the node belongs to.
    fn node_hit(
        &self,
        world: Point,
        exclude: Option<&str>,
    ) -> Option<(ElementId, connector::AnchorSide, Point)> {
        let reach = (CONNECTION_NODE_PX + HANDLE_HIT_PX) / self.camera.zoom;
        let grab = self.node_grab();
        // Grid candidates instead of a full scan: same model-bounds reject.
        let pad = reach + crate::spatial::hit_pad();
        let area = crate::geometry::Rect::new(world.x - pad, world.y - pad, pad * 2.0, pad * 2.0);
        let mut best: Option<ElementId> = None;
        for row in self.hit.query(area) {
            let Some((id, _, _)) = self.hit.row(row) else {
                continue;
            };
            if exclude == Some(id) {
                continue;
            }
            let Some(element) = self.doc.element(id).filter(|e| !e.is_deleted) else {
                continue;
            };
            // Bound text is not a node source, but it still occludes the
            // nodes that float over its container — a click on a label
            // selects the container instead of pulling a connector.
            let is_bound_text = matches!(
                element.kind,
                ElementKind::Text {
                    container_id: Some(_),
                    ..
                }
            );
            if !Self::is_connectable(element) && !is_bound_text {
                continue;
            }
            if !element.bounds().expand(reach).contains(world) {
                continue;
            }
            let topmost = best.as_ref().is_none_or(|b| {
                let (ka, kb) = (
                    element.index.as_str(),
                    self.doc.element(b).map(|e| e.index.as_str()).unwrap_or(""),
                );
                (ka, element.id.as_str()) > (kb, b.as_str())
            });
            if topmost {
                best = Some(element.id.clone());
            }
        }
        let element = self.doc.element(best.as_deref()?)?;
        if !Self::is_connectable(element) {
            return None;
        }
        let (side, anchor) = connector::anchor_points(element.bounds())
            .into_iter()
            .find(|(side, anchor)| self.node_position(*side, *anchor).distance(world) <= grab)?;
        Some((element.id.clone(), side, anchor))
    }

    /// The connection node under `world`, if the hovered element has one.
    fn connection_node_at(&self, world: Point) -> Option<(ElementId, connector::AnchorSide)> {
        self.node_hit(world, None).map(|(id, side, _)| (id, side))
    }

    /// Refresh the hovered element's connection nodes for the Select tool.
    fn update_hover(&mut self, world: Point) -> Effects {
        if self.tool != Tool::Select {
            return self.clear_hover();
        }
        let mut effects = Effects::default();
        // Hovering a sticky stack's pull zone offers to drag a note off it.
        let next_stack = self.stack_pull_at(world);
        if self.hover_stack != next_stack {
            self.hover_stack = next_stack;
            effects.overlay_changed = true;
        }
        // Keep the current nodes while the pointer is out on one of them:
        // they float outside the element, so a fresh hit test would miss.
        if let Some((_, nodes)) = &self.hover_anchors {
            if nodes.iter().any(|p| p.distance(world) <= self.node_grab()) {
                return effects;
            }
        }
        let next = hit_test_indexed(&self.doc, &self.hit, world, self.tolerance())
            .and_then(|id| self.doc.element(&id).cloned())
            .filter(Self::is_connectable)
            .map(|element| {
                let nodes = self.node_points_for(&element);
                (element.id.clone(), nodes)
            })
            .or_else(|| {
                self.node_hit(world, None).and_then(|(id, _, _)| {
                    let element = self.doc.element(&id)?;
                    Some((id, self.node_points_for(element)))
                })
            });
        if self.hover_anchors != next {
            self.hover_anchors = next;
            effects.overlay_changed = true;
        }
        effects
    }

    /// Drop the hover state (pointer left the canvas, tool changed, …).
    pub fn clear_hover(&mut self) -> Effects {
        let changed = self.hover_anchors.take().is_some() | self.hover_stack.take().is_some();
        if changed {
            Effects {
                overlay_changed: true,
                ..Effects::default()
            }
        } else {
            Effects::default()
        }
    }

    /// The sticky stack whose pull zone (everything but the border ring) is
    /// under `world`, if any.
    fn stack_pull_at(&self, world: Point) -> Option<ElementId> {
        let id = hit_test_indexed(&self.doc, &self.hit, world, self.tolerance())?;
        let element = self.doc.element(&id)?;
        if !element.is_sticky_stack() {
            return None;
        }
        let rect = element.bounds();
        let inset = STACK_EDGE_PX / self.camera.zoom;
        let inner = Rect::new(
            rect.x + inset,
            rect.y + inset,
            (rect.w - inset * 2.0).max(1.0),
            (rect.h - inset * 2.0).max(1.0),
        );
        inner.contains(world).then_some(id)
    }

    /// Whether the pointer is offering to pull a note off a stack.
    pub fn stack_pull_hover(&self) -> bool {
        self.hover_stack.is_some()
    }

    /// Draggable bends on the selected lone connector: one handle per
    /// existing waypoint, plus an insert handle on every segment long enough
    /// to aim at.
    pub fn arrow_path_handles(&self) -> Vec<ArrowHandle> {
        let Some(element) = self.lone_arrow() else {
            return Vec::new();
        };
        let ElementKind::Arrow {
            start,
            end,
            routing,
            waypoints,
            ..
        } = &element.kind
        else {
            return Vec::new();
        };
        let route =
            connector::route_avoiding(start, end, *routing, waypoints, &self.doc, &self.obstacles);
        let mut handles: Vec<ArrowHandle> = waypoints
            .iter()
            .enumerate()
            .map(|(index, point)| ArrowHandle::Waypoint {
                index,
                point: *point,
            })
            .collect();
        let min_segment = 24.0 / self.camera.zoom;
        // On a multi-segment elbow the first and last segments leave the
        // anchors themselves; bowing them can only double the route back on
        // itself. Only interior runs offer insert handles.
        let interior_only = route.len() > 2;
        for (index, segment) in route.windows(2).enumerate() {
            if segment[0].distance(segment[1]) < min_segment {
                continue;
            }
            if interior_only && (index == 0 || index + 2 == route.len()) {
                continue;
            }
            let point = segment[0].lerp(segment[1], 0.5);
            handles.push(ArrowHandle::Insert {
                point,
                start: segment[0],
                end: segment[1],
                insert_at: connector::waypoint_insertion_index(&route, waypoints, point),
            });
        }
        handles
    }

    fn lone_arrow(&self) -> Option<&Element> {
        if self.selection.len() != 1 {
            return None;
        }
        let element = self.doc.element(self.selection.iter().next()?)?;
        matches!(element.kind, ElementKind::Arrow { .. }).then_some(element)
    }

    fn arrow_handle_at(&self, world: Point) -> Option<(ElementId, ArrowHandle)> {
        let element = self.lone_arrow()?;
        let grab = HANDLE_HIT_PX / self.camera.zoom;
        let handle = self
            .arrow_path_handles()
            .into_iter()
            .find(|handle| handle.point().distance(world) <= grab)?;
        Some((element.id.clone(), handle))
    }

    /// When the selection is a lone connector, which endpoint `world` grabs.
    /// Returns the element id and the grabbed end.
    fn arrow_endpoint_at(&self, world: Point) -> Option<(ElementId, EndpointEnd)> {
        if self.selection.len() != 1 {
            return None;
        }
        let id = self.selection.iter().next()?;
        let element = self.doc.element(id)?;
        let ElementKind::Arrow { start, end, .. } = &element.kind else {
            return None;
        };
        let grab = HANDLE_HIT_PX / self.camera.zoom;
        let start_point = connector::endpoint_point(start, end.point(), &self.doc);
        if world.distance(start_point) <= grab {
            return Some((id.clone(), EndpointEnd::Start));
        }
        let end_point = connector::endpoint_point(end, start.point(), &self.doc);
        if world.distance(end_point) <= grab {
            return Some((id.clone(), EndpointEnd::End));
        }
        None
    }

    /// When the selection is a lone connector and `world` is on its routed
    /// shaft (but not an endpoint — `arrow_endpoint_at` covers those), the
    /// originals for a body drag: the connector plus its followers.
    fn lone_arrow_shaft_hit(&self, world: Point) -> Option<Vec<Element>> {
        if self.selection.len() != 1 {
            return None;
        }
        let id = self.selection.iter().next()?;
        let element = self.doc.element(id)?;
        if !matches!(element.kind, ElementKind::Arrow { .. }) {
            return None;
        }
        if !crate::hit::hits(&self.doc, element, world, self.tolerance().max(4.0)) {
            return None;
        }
        Some(self.selection_with_followers())
    }

    /// Apply a style patch to the selection (undoably) and to the defaults
    /// for new elements.
    pub fn update_style(&mut self, patch: &StylePatch, now: u64) -> Effects {
        if patch.is_empty() {
            return Effects::default();
        }
        patch.apply_to(&mut self.style);
        if self.selection.is_empty() || self.interaction != Interaction::Idle {
            return Effects::default();
        }
        let ids: Vec<ElementId> = self.selection.iter().cloned().collect();
        let mut tx = Transaction::begin();
        let mut effects = Effects::default();
        for id in &ids {
            let patched = patch.clone();
            if let Err(err) = tx.update(&mut self.doc, id, now, |element| {
                if let Some(style) = element_style_mut(element) {
                    patched.apply_to(style);
                }
            }) {
                debug_assert!(false, "style update failed: {err}");
            }
            effects.changed.insert(id.clone());
            // Text reflows when its font size changes.
            if let Some(content) = self
                .doc
                .element(id)
                .and_then(|element| match &element.kind {
                    ElementKind::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
            {
                if let Some(rect) = self.text_rect_for(id, &content) {
                    let _ = tx.update(&mut self.doc, id, now, |element| {
                        if let ElementKind::Text { rect: r, .. } = &mut element.kind {
                            *r = rect;
                        }
                    });
                }
            }
        }
        tx.commit(&mut self.history);
        effects.overlay_changed = true;
        effects
    }

    /// The style shown in the panel: the selection's style, else defaults.
    pub fn current_style(&self) -> Style {
        self.selection
            .iter()
            .next()
            .and_then(|id| self.doc.element(id))
            .and_then(|element| element_style(element))
            .cloned()
            .unwrap_or_else(|| self.style.clone())
    }

    /// Insert a library glyph at the viewport centre with a caption beneath.
    pub fn insert_glyph(&mut self, glyph: &str, now: u64) -> Effects {
        let Some(definition) = crate::glyphs::lookup(glyph) else {
            return Effects::default();
        };
        if self.interaction != Interaction::Idle {
            return Effects::default();
        }
        let center = self.camera.screen_to_world(Point::new(
            self.viewport_size.0 / 2.0,
            self.viewport_size.1 / 2.0,
        ));
        let size = 96.0;
        let rect = Rect::new(center.x - size / 2.0, center.y - size / 2.0, size, size);
        let mut tx = Transaction::begin();

        let glyph_element = Element::new(
            ElementKind::Glyph {
                glyph: glyph.to_string(),
                rect,
                style: self.style.clone(),
                caption_id: None,
            },
            self.next_index(),
            now,
        );
        let glyph_id = glyph_element.id.clone();
        if let Err(err) = tx.add(&mut self.doc, glyph_element) {
            debug_assert!(false, "glyph insert failed: {err}");
        }

        let wrap = rect.w * 2.0;
        let layout = layout_text(
            definition.name,
            self.style.font_size,
            Some(wrap),
            self.measurer.as_ref(),
        );
        let caption_rect = Rect::new(
            center.x - wrap / 2.0,
            rect.max_y() + 4.0,
            wrap,
            layout.height,
        );
        let caption = Element::new(
            ElementKind::Text {
                rect: caption_rect,
                text: definition.name.to_string(),
                align: TextAlign::Center,
                fixed_width: true,
                container_id: Some(glyph_id.clone()),
                style: self.style.clone(),
            },
            self.next_index(),
            now,
        );
        let caption_id = caption.id.clone();
        if let Err(err) = tx.add(&mut self.doc, caption) {
            debug_assert!(false, "caption insert failed: {err}");
        }
        let link = caption_id.clone();
        if let Err(err) = tx.update(&mut self.doc, &glyph_id, now, |element| {
            if let ElementKind::Glyph { caption_id, .. } = &mut element.kind {
                *caption_id = Some(link);
            }
        }) {
            debug_assert!(false, "caption link failed: {err}");
        }
        tx.commit(&mut self.history);
        self.selection = [glyph_id.clone()].into_iter().collect();
        Effects {
            changed: [glyph_id, caption_id].into_iter().collect(),
            overlay_changed: true,
            ..Effects::default()
        }
    }

    /// The style new sticky notes and stacks inherit: a solid pastel fill and
    /// dark ink, so text reads on the note.
    fn sticky_style(&self) -> Style {
        let mut style = self.style.clone();
        style.fill_style = crate::model::FillPattern::Solid;
        if style.fill_color == "transparent" {
            style.fill_color = STICKY_DEFAULT_COLOR.into();
        }
        style.stroke_color = STICKY_INK.into();
        style
    }

    /// The sticky tool: pull a note off a stack under the pointer, otherwise
    /// start a new note (click for the default size, drag to size it).
    fn begin_sticky(&mut self, world: Point, now: u64) -> Effects {
        let tolerance = self.tolerance();
        if let Some(id) = hit_test_indexed(&self.doc, &self.hit, world, tolerance) {
            if self.doc.element(&id).is_some_and(|e| e.is_sticky_stack()) {
                return self.begin_pull_from_stack(&id, world, now);
            }
        }
        let mut tx = Transaction::begin();
        let size = STICKY_SIZE;
        let element = Element::new(
            ElementKind::Shape {
                shape: ShapeKind::Sticky,
                rect: Rect::new(world.x - size / 2.0, world.y - size / 2.0, size, size),
                style: self.sticky_style(),
                bound_text_id: None,
            },
            self.next_index(),
            now,
        );
        let id = element.id.clone();
        if let Err(err) = tx.add(&mut self.doc, element) {
            debug_assert!(false, "sticky add failed: {err}");
        }
        self.tx = Some(tx);
        self.interaction = Interaction::Drawing {
            id: id.clone(),
            start: world,
        };
        Effects {
            changed: [id].into_iter().collect(),
            overlay_changed: true,
            ..Effects::default()
        }
    }

    /// Place a new infinite sticky stack.
    fn begin_sticky_stack(&mut self, world: Point, now: u64) -> Effects {
        if self.interaction != Interaction::Idle {
            return Effects::default();
        }
        let size = STICKY_SIZE;
        let mut tx = Transaction::begin();
        let element = Element::new(
            ElementKind::StickyStack {
                rect: Rect::new(world.x - size / 2.0, world.y - size / 2.0, size, size),
                style: self.sticky_style(),
            },
            self.next_index(),
            now,
        );
        let id = element.id.clone();
        if let Err(err) = tx.add(&mut self.doc, element) {
            debug_assert!(false, "sticky stack add failed: {err}");
        }
        self.tx = Some(tx);
        // A stack placed inside a frame belongs to it like any note.
        self.sync_frame_membership(now);
        if let Some(tx) = self.tx.take() {
            tx.commit(&mut self.history);
        }
        self.selection = [id.clone()].into_iter().collect();
        self.finish_tool_use();
        Effects {
            changed: [id].into_iter().collect(),
            overlay_changed: true,
            ..Effects::default()
        }
    }

    /// Pull a note from the stack with the given id.
    fn begin_pull_from_stack(&mut self, stack_id: &str, world: Point, now: u64) -> Effects {
        let Some(ElementKind::StickyStack { rect, style }) = self
            .doc
            .element(stack_id)
            .map(|element| element.kind.clone())
        else {
            return Effects::default();
        };
        self.begin_pull_sticky(rect, style, world, now)
    }

    /// Create the note that will follow the pointer until release. It only
    /// becomes a real note if it is dragged clear of the pile.
    fn begin_pull_sticky(
        &mut self,
        stack_rect: Rect,
        mut style: Style,
        world: Point,
        now: u64,
    ) -> Effects {
        style.fill_style = crate::model::FillPattern::Solid;
        if style.fill_color == "transparent" {
            style.fill_color = STICKY_DEFAULT_COLOR.into();
        }
        let mut tx = Transaction::begin();
        let element = Element::new(
            ElementKind::Shape {
                shape: ShapeKind::Sticky,
                rect: Rect::new(
                    world.x - stack_rect.w / 2.0,
                    world.y - stack_rect.h / 2.0,
                    stack_rect.w,
                    stack_rect.h,
                ),
                style,
                bound_text_id: None,
            },
            self.next_index(),
            now,
        );
        let id = element.id.clone();
        if let Err(err) = tx.add(&mut self.doc, element) {
            debug_assert!(false, "pulled sticky add failed: {err}");
        }
        self.tx = Some(tx);
        self.interaction = Interaction::PullingSticky {
            id: id.clone(),
            origin: world,
        };
        Effects {
            changed: [id].into_iter().collect(),
            overlay_changed: true,
            ..Effects::default()
        }
    }

    fn begin_erase(&mut self, world: Point, now: u64) -> Effects {
        self.tx = Some(Transaction::begin());
        let mut erased = BTreeSet::new();
        let mut effects = Effects::default();
        self.erase_at(world, &mut erased, &mut effects, now);
        self.interaction = Interaction::Erasing { erased };
        effects.overlay_changed = true;
        effects
    }

    fn begin_frame(&mut self, world: Point, now: u64) -> Effects {
        let mut tx = Transaction::begin();
        let count = self
            .doc
            .elements
            .values()
            .filter(|element| element.is_frame() && !element.is_deleted)
            .count();
        let name = format!("Frame {}", count + 1);
        let element = Element::new(
            ElementKind::Frame {
                rect: Rect::new(world.x, world.y, 0.0, 0.0),
                name,
            },
            self.next_index(),
            now,
        );
        let id = element.id.clone();
        if let Err(err) = tx.add(&mut self.doc, element) {
            debug_assert!(false, "frame add failed: {err}");
        }
        self.tx = Some(tx);
        self.interaction = Interaction::Drawing {
            id: id.clone(),
            start: world,
        };
        Effects {
            changed: [id].into_iter().collect(),
            overlay_changed: true,
            ..Effects::default()
        }
    }

    fn begin_freehand(&mut self, world: Point, now: u64) -> Effects {
        let mut tx = Transaction::begin();
        let element = Element::new(
            ElementKind::Freehand {
                points: vec![world],
                pressures: vec![self.last_pressure],
                style: self.style.clone(),
            },
            self.next_index(),
            now,
        );
        let id = element.id.clone();
        if let Err(err) = tx.add(&mut self.doc, element) {
            debug_assert!(false, "freehand add failed: {err}");
        }
        self.tx = Some(tx);
        self.interaction = Interaction::Freehand { id: id.clone() };
        Effects {
            changed: [id].into_iter().collect(),
            overlay_changed: true,
            ..Effects::default()
        }
    }

    /// Anchor points to highlight during a connector drag.
    pub fn hover_anchor_points(&self) -> &[Point] {
        self.hover_anchors
            .as_ref()
            .map(|(_, points)| points.as_slice())
            .unwrap_or_default()
    }

    fn begin_connector(&mut self, world: Point, now: u64) -> Effects {
        let tolerance = self.tolerance().max(4.0);
        let node = self.node_hit(world, None);
        let explicit = node.is_some();
        let candidate =
            node.or_else(|| connector::binding_candidate(&self.doc, world, tolerance, None, None));
        let (start_point, start_binding) = match candidate {
            Some((target_id, side, point)) => {
                let binding = Binding {
                    element_id: target_id,
                    focus: explicit.then(|| side.focus()),
                };
                // Store the resolved anchor so endpoint coordinates describe
                // the drawn geometry (see the Connecting arm).
                let ep = crate::model::Endpoint {
                    x: point.x,
                    y: point.y,
                    binding: Some(binding.clone()),
                };
                let (resolved, _) = connector::resolve_endpoint(&ep, world, &self.doc);
                (resolved, Some(binding))
            }
            None => (world, None),
        };
        self.begin_connector_bound(start_point, start_binding, world, now)
    }

    /// Start a connector from a specific element side (the connection nodes).
    fn begin_connector_from(
        &mut self,
        id: ElementId,
        side: connector::AnchorSide,
        now: u64,
    ) -> Effects {
        let Some(element) = self.doc.element(&id) else {
            return Effects::default();
        };
        let anchor = side.point(element.bounds());
        let binding = Binding {
            element_id: id,
            focus: Some(side.focus()),
        };
        self.begin_connector_bound(anchor, Some(binding), anchor, now)
    }

    fn begin_connector_bound(
        &mut self,
        start_point: Point,
        start_binding: Option<Binding>,
        world: Point,
        now: u64,
    ) -> Effects {
        let mut tx = Transaction::begin();
        let element = Element::new(
            ElementKind::Arrow {
                start: Endpoint {
                    x: start_point.x,
                    y: start_point.y,
                    binding: start_binding,
                },
                end: Endpoint::free(world.x, world.y),
                routing: Routing::Elbow,
                start_arrowhead: false,
                end_arrowhead: true,
                style: self.style.clone(),
                waypoints: Vec::new(),
                label_id: None,
            },
            self.next_index(),
            now,
        );
        let id = element.id.clone();
        if let Err(err) = tx.add(&mut self.doc, element) {
            debug_assert!(false, "connector add failed: {err}");
        }
        self.tx = Some(tx);
        self.interaction = Interaction::Connecting {
            id: id.clone(),
            start: world,
        };
        Effects {
            changed: [id].into_iter().collect(),
            overlay_changed: true,
            ..Effects::default()
        }
    }

    /// Arrows bound to this element.
    pub fn bound_arrows(&self, element_id: &str) -> Vec<ElementId> {
        self.doc
            .elements
            .values()
            .filter(|element| {
                if element.is_deleted {
                    return false;
                }
                if let ElementKind::Arrow { start, end, .. } = &element.kind {
                    let binds = |endpoint: &Endpoint| {
                        endpoint
                            .binding
                            .as_ref()
                            .is_some_and(|b| b.element_id == element_id)
                    };
                    binds(start) || binds(end)
                } else {
                    false
                }
            })
            .map(|element| element.id.clone())
            .collect()
    }

    /// Arrows whose endpoints re-resolve when the given elements move: arrows
    /// bound to them, and moved arrows that carry bindings of their own.
    ///
    /// One pass over the document — call this once per gesture, never per
    /// pointer move (on a large board the pass is the expensive part).
    fn arrows_touching(&self, moved: &BTreeSet<ElementId>) -> Vec<ElementId> {
        let mut arrows = Vec::new();
        for element in self.doc.elements.values() {
            let ElementKind::Arrow { start, end, .. } = &element.kind else {
                continue;
            };
            if element.is_deleted {
                continue;
            }
            let binds_moved = |endpoint: &Endpoint| {
                endpoint
                    .binding
                    .as_ref()
                    .is_some_and(|b| moved.contains(&b.element_id))
            };
            // A moved arrow with bindings needs a post-translate resolve too:
            // its bound endpoints were glued mid-transaction, possibly against
            // elements that had not moved yet.
            let moved_bound =
                moved.contains(&element.id) && (start.binding.is_some() || end.binding.is_some());
            if binds_moved(start) || binds_moved(end) || moved_bound {
                arrows.push(element.id.clone());
            }
        }
        arrows
    }

    /// Arrows bound to moved elements but not moved themselves: the attached
    /// set a selection drag stretches. Selected arrows ride the drag rigidly
    /// (exact under translation), so only these need rubber-band previews
    /// and an exact re-route on release.
    fn attached_arrows(&self, moved: &BTreeSet<ElementId>) -> Vec<ElementId> {
        let mut arrows = Vec::new();
        for element in self.doc.elements.values() {
            let ElementKind::Arrow { start, end, .. } = &element.kind else {
                continue;
            };
            if element.is_deleted || moved.contains(&element.id) {
                continue;
            }
            let binds_moved = |endpoint: &Endpoint| {
                endpoint
                    .binding
                    .as_ref()
                    .is_some_and(|b| moved.contains(&b.element_id))
            };
            if binds_moved(start) || binds_moved(end) {
                arrows.push(element.id.clone());
            }
        }
        arrows
    }

    /// Fresh endpoint positions for the given arrows.
    fn arrow_updates_for(&self, arrow_ids: &[ElementId]) -> Vec<(ElementId, Point, Point)> {
        let mut updates = Vec::with_capacity(arrow_ids.len());
        for id in arrow_ids {
            let Some(element) = self.doc.element(id) else {
                continue;
            };
            let ElementKind::Arrow { start, end, .. } = &element.kind else {
                continue;
            };
            let (s, _) = connector::resolve_endpoint(start, end.point(), &self.doc);
            let (e, _) = connector::resolve_endpoint(end, start.point(), &self.doc);
            updates.push((id.clone(), s, e));
        }
        updates
    }

    /// New endpoint positions for arrows connected to the given elements.
    fn connected_arrow_updates(
        &self,
        moved: &BTreeSet<ElementId>,
    ) -> Vec<(ElementId, Point, Point)> {
        self.arrow_updates_for(&self.arrows_touching(moved))
    }

    /// Frame membership updates needed after the given elements moved.
    fn frame_membership_updates(
        &self,
        ids: &BTreeSet<ElementId>,
    ) -> Vec<(ElementId, Option<ElementId>)> {
        let frames: Vec<(&str, Rect)> = self
            .doc
            .elements
            .values()
            .filter(|e| !e.is_deleted)
            .filter_map(|e| match &e.kind {
                ElementKind::Frame { rect, .. } => Some((e.id.as_str(), *rect)),
                _ => None,
            })
            .collect();
        let mut out = Vec::new();
        for id in ids {
            let Some(element) = self.doc.element(id) else {
                continue;
            };
            if element.is_deleted {
                continue;
            }
            if let ElementKind::Text {
                container_id: Some(_),
                ..
            } = &element.kind
            {
                continue;
            }
            let target = if element.is_frame() {
                // A nested frame is owned by the smallest frame that strictly
                // contains it; strict containment keeps the hierarchy acyclic.
                smallest_frame_strictly_containing(&frames, element)
            } else {
                smallest_frame_containing(&frames, element.bounds().center())
            };
            if element.frame_id != target {
                out.push((id.clone(), target));
            }
        }
        out
    }

    /// Reconcile frame membership inside the open transaction.
    fn sync_frame_membership(&mut self, now: u64) {
        let Some(tx) = self.tx.as_ref() else {
            return;
        };
        let ids: BTreeSet<ElementId> = tx.deltas().iter().map(|d| d.element_id().clone()).collect();
        self.sync_frame_membership_for(&ids, now);
    }

    /// Reconcile frame membership for an explicit id set (used when the
    /// transaction does not yet hold the staged ids, e.g. drag release after
    /// preview moves bypassed per-move staging).
    fn sync_frame_membership_for(&mut self, ids: &BTreeSet<ElementId>, now: u64) {
        if self.tx.is_none() {
            return;
        }
        let updates = self.frame_membership_updates(ids);
        if let Some(tx) = self.tx.as_mut() {
            for (id, frame_id) in updates {
                let _ = tx.update(&mut self.doc, &id, now, |element| {
                    element.frame_id = frame_id.clone();
                });
            }
        }
    }

    fn begin_line(&mut self, world: Point, now: u64) -> Effects {
        let mut tx = Transaction::begin();
        let arrowhead = self.tool == Tool::Arrow;
        let element = Element::new(
            ElementKind::Arrow {
                start: crate::model::Endpoint::free(world.x, world.y),
                end: crate::model::Endpoint::free(world.x, world.y),
                routing: crate::model::Routing::Straight,
                start_arrowhead: false,
                end_arrowhead: arrowhead,
                style: self.style.clone(),
                waypoints: Vec::new(),
                label_id: None,
            },
            self.next_index(),
            now,
        );
        let id = element.id.clone();
        if let Err(err) = tx.add(&mut self.doc, element) {
            debug_assert!(false, "line add failed: {err}");
        }
        self.tx = Some(tx);
        self.interaction = Interaction::Line {
            id: id.clone(),
            start: world,
        };
        Effects {
            changed: [id].into_iter().collect(),
            overlay_changed: true,
            ..Effects::default()
        }
    }

    fn erase_at(
        &mut self,
        world: Point,
        erased: &mut BTreeSet<ElementId>,
        effects: &mut Effects,
        now: u64,
    ) {
        let tolerance = self.tolerance();
        if let Some(id) = hit_test_indexed(&self.doc, &self.hit, world, tolerance) {
            let targets: Vec<ElementId> = std::iter::once(id.clone())
                .chain(self.followers(&id))
                .collect();
            for target in targets {
                if erased.contains(&target) {
                    continue;
                }
                if let Some(tx) = self.tx.as_mut() {
                    if tx.remove(&mut self.doc, &target, now).is_ok() {
                        effects.changed.insert(target.clone());
                        erased.insert(target);
                    }
                }
            }
        }
    }

    // -- commands ----------------------------------------------------------

    /// Elements that always travel with this element: bound text, frame
    /// children.
    pub fn followers(&self, id: &str) -> Vec<ElementId> {
        match self.doc.element(id).map(|e| &e.kind) {
            Some(ElementKind::Shape {
                bound_text_id: Some(text_id),
                ..
            })
            | Some(ElementKind::Glyph {
                caption_id: Some(text_id),
                ..
            })
            | Some(ElementKind::Arrow {
                label_id: Some(text_id),
                ..
            }) => vec![text_id.clone()],
            Some(ElementKind::Frame { .. }) => self
                .doc
                .elements
                .values()
                .filter(|element| !element.is_deleted && element.frame_id.as_deref() == Some(id))
                .map(|element| element.id.clone())
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The selection plus every follower, transitively.
    pub fn selection_with_followers_recursive(&self) -> BTreeSet<ElementId> {
        let mut out = self.selection.clone();
        let mut frontier: Vec<ElementId> = out.iter().cloned().collect();
        while let Some(id) = frontier.pop() {
            for follower in self.followers(&id) {
                if out.insert(follower.clone()) {
                    frontier.push(follower);
                }
            }
        }
        out
    }

    fn selection_with_followers(&self) -> Vec<Element> {
        self.selection_with_followers_recursive()
            .iter()
            .filter_map(|id| self.doc.element(id).cloned())
            .collect()
    }

    /// Bound-text rectangles that must be recomputed after containers change.
    fn bound_text_reflows(&self, containers: &BTreeSet<ElementId>) -> Vec<(ElementId, Rect)> {
        let mut reflows = Vec::new();
        for container_id in containers {
            let Some(text_id) = self.followers(container_id).into_iter().next() else {
                continue;
            };
            let Some(content) = self.doc.element(&text_id).and_then(|e| match &e.kind {
                ElementKind::Text { text, .. } => Some(text.clone()),
                _ => None,
            }) else {
                continue;
            };
            if let Some(rect) = self.text_rect_for(&text_id, &content) {
                reflows.push((text_id, rect));
            }
        }
        reflows
    }

    /// Move the selection by a world-space delta, one undo step.
    pub fn nudge(&mut self, dx: f64, dy: f64, now: u64) -> Effects {
        if self.selection.is_empty() || self.interaction != Interaction::Idle {
            return Effects::default();
        }
        let originals = self.selection_with_followers();
        let mut tx = Transaction::begin();
        let mut effects = Effects::default();
        for original in &originals {
            let mut translated = original.clone();
            translate_element_keeping_bindings(&self.doc, &mut translated, dx, dy);
            let kind = translated.kind;
            if let Err(err) = tx.update(&mut self.doc, &original.id, now, |element| {
                element.kind = kind;
            }) {
                debug_assert!(false, "nudge failed: {err}");
            }
            effects.changed.insert(original.id.clone());
        }
        let moved_ids: BTreeSet<ElementId> = originals.iter().map(|o| o.id.clone()).collect();
        let mut reflow_targets: BTreeSet<ElementId> = BTreeSet::new();
        for (arrow_id, start_point, end_point) in self.connected_arrow_updates(&moved_ids) {
            reflow_targets.insert(arrow_id.clone());
            let _ = tx.update(&mut self.doc, &arrow_id, now, |element| {
                if let ElementKind::Arrow { start, end, .. } = &mut element.kind {
                    start.x = start_point.x;
                    start.y = start_point.y;
                    end.x = end_point.x;
                    end.y = end_point.y;
                }
            });
            effects.changed.insert(arrow_id);
        }
        // Arrows in the nudged set re-resolve bound endpoints, so their labels
        // need a midpoint reflow rather than the rigid translate they got.
        for id in &moved_ids {
            if self
                .doc
                .element(id)
                .is_some_and(|e| matches!(e.kind, ElementKind::Arrow { .. }))
            {
                reflow_targets.insert(id.clone());
            }
        }
        for (text_id, rect) in self.bound_text_reflows(&reflow_targets) {
            let _ = tx.update(&mut self.doc, &text_id, now, |element| {
                if let ElementKind::Text { rect: r, .. } = &mut element.kind {
                    *r = rect;
                }
            });
            effects.changed.insert(text_id);
        }
        tx.commit(&mut self.history);
        effects
    }

    /// Duplicate the selection with a small offset.
    pub fn duplicate(&mut self, now: u64) -> Effects {
        if self.interaction != Interaction::Idle {
            return Effects::default();
        }
        let source = self.selection_with_followers();
        self.insert_copies(source, 12.0, 12.0, now)
    }

    /// Copy the selection (with its followers) into the internal clipboard.
    pub fn copy(&mut self) -> Effects {
        self.clipboard = self.selection_with_followers();
        self.paste_serial = 0;
        Effects::default()
    }

    /// Copy and delete the selection.
    pub fn cut(&mut self, now: u64) -> Effects {
        self.clipboard = self.selection_with_followers();
        self.paste_serial = 0;
        self.delete_selection(now)
    }

    /// Paste the clipboard with a cumulative offset.
    pub fn paste(&mut self, now: u64) -> Effects {
        if self.clipboard.is_empty() {
            return Effects::default();
        }
        self.paste_serial += 1;
        let offset = 12.0 * self.paste_serial as f64;
        let source = self.clipboard.clone();
        self.insert_copies(source, offset, offset, now)
    }

    fn insert_copies(&mut self, source: Vec<Element>, dx: f64, dy: f64, now: u64) -> Effects {
        if source.is_empty() {
            return Effects::default();
        }
        let mut tx = Transaction::begin();
        let mut map: HashMap<ElementId, ElementId> = HashMap::new();
        let mut last_index = self.doc.top_index();
        let mut copies = Vec::new();
        for original in &source {
            let mut copy = original.clone();
            copy.id = new_element_id();
            copy.version = 1;
            copy.version_nonce = new_version_nonce();
            copy.updated = now;
            copy.is_deleted = false;
            copy.index = match &last_index {
                Some(top) => crate::index::key_between(Some(top), None).expect("index space"),
                None => crate::index::key_between(None, None).expect("first index"),
            };
            last_index = Some(copy.index.clone());
            translate_element(&mut copy, dx, dy);
            map.insert(original.id.clone(), copy.id.clone());
            copies.push(copy);
        }
        for copy in copies.iter_mut() {
            remap_references(copy, &map);
        }
        let mut effects = Effects::default();
        for copy in copies {
            let id = copy.id.clone();
            if let Err(err) = tx.add(&mut self.doc, copy) {
                debug_assert!(false, "paste failed: {err}");
            }
            effects.changed.insert(id);
        }
        tx.commit(&mut self.history);
        self.selection = effects.changed.clone();
        effects.overlay_changed = true;
        effects
    }

    /// Move the selection one step forward (`1`) or backward (`-1`) in z-order.
    pub fn reorder(&mut self, direction: i32, now: u64) -> Effects {
        if self.selection.is_empty() || self.interaction != Interaction::Idle {
            return Effects::default();
        }
        let mut seq: Vec<ElementId> = self
            .doc
            .ordered_live()
            .iter()
            .map(|e| e.id.clone())
            .collect();
        let selected = self.selection.clone();
        match direction {
            1 => {
                for i in (0..seq.len().saturating_sub(1)).rev() {
                    if selected.contains(&seq[i]) && !selected.contains(&seq[i + 1]) {
                        seq.swap(i, i + 1);
                    }
                }
            }
            -1 => {
                for i in 1..seq.len() {
                    if selected.contains(&seq[i]) && !selected.contains(&seq[i - 1]) {
                        seq.swap(i, i - 1);
                    }
                }
            }
            _ => return Effects::default(),
        }

        let mut indices: Vec<String> = seq
            .iter()
            .map(|id| {
                self.doc
                    .element(id)
                    .map(|e| e.index.clone())
                    .unwrap_or_default()
            })
            .collect();
        let mut tx = Transaction::begin();
        let mut effects = Effects::default();
        for i in 0..seq.len() {
            if !selected.contains(&seq[i]) {
                continue;
            }
            let prev = if i > 0 {
                Some(indices[i - 1].as_str())
            } else {
                None
            };
            let next = indices.get(i + 1).map(|s| s.as_str());
            let new_index = crate::index::key_between(prev, next).expect("index space");
            indices[i] = new_index.clone();
            let id = seq[i].clone();
            if let Err(err) = tx.update(&mut self.doc, &id, now, |element| {
                element.index = new_index.clone();
            }) {
                debug_assert!(false, "reorder failed: {err}");
            }
            effects.changed.insert(id);
        }
        tx.commit(&mut self.history);
        effects
    }

    fn drawn_rect(&self, start: Point, current: Point, mods: Modifiers) -> Rect {
        let mut current = current;
        if mods.shift {
            let size = (current.x - start.x).abs().max((current.y - start.y).abs());
            current = Point::new(
                start.x + size.copysign(current.x - start.x),
                start.y + size.copysign(current.y - start.y),
            );
        }
        Rect::from_points(start, current)
    }
}

/// Light smoothing for freehand capture: drop near-duplicate points, then a
/// gentle moving average that keeps both endpoints.
pub fn simplify_freehand(points: Vec<Point>) -> Vec<Point> {
    let mut dedup: Vec<Point> = Vec::with_capacity(points.len());
    for p in points {
        if dedup
            .last()
            .map(|last| last.distance(p) >= 1.0)
            .unwrap_or(true)
        {
            dedup.push(p);
        }
    }
    if dedup.len() < 3 {
        return dedup;
    }
    let mut out = Vec::with_capacity(dedup.len());
    out.push(dedup[0]);
    for window in dedup.windows(3) {
        out.push(Point::new(
            window[0].x * 0.25 + window[1].x * 0.5 + window[2].x * 0.25,
            window[0].y * 0.25 + window[1].y * 0.5 + window[2].y * 0.25,
        ));
    }
    out.push(dedup[dedup.len() - 1]);
    out
}

/// Snap a line end to 15-degree steps when shift is held.
fn constrain_angle(start: Point, current: Point, constrain: bool) -> Point {
    if !constrain {
        return current;
    }
    let d = current - start;
    let length = d.length();
    if length < 1e-6 {
        return current;
    }
    let step = std::f64::consts::PI / 12.0;
    let angle = (d.y.atan2(d.x) / step).round() * step;
    Point::new(
        start.x + angle.cos() * length,
        start.y + angle.sin() * length,
    )
}

/// The mutable style of any element that draws.
pub fn element_style_mut(element: &mut Element) -> Option<&mut Style> {
    match &mut element.kind {
        ElementKind::Shape { style, .. }
        | ElementKind::Freehand { style, .. }
        | ElementKind::Glyph { style, .. }
        | ElementKind::Arrow { style, .. }
        | ElementKind::StickyStack { style, .. }
        | ElementKind::Text { style, .. } => Some(style),
        ElementKind::Frame { .. } => None,
    }
}

/// The style of any element that draws.
pub fn element_style(element: &Element) -> Option<&Style> {
    match &element.kind {
        ElementKind::Shape { style, .. }
        | ElementKind::Freehand { style, .. }
        | ElementKind::Glyph { style, .. }
        | ElementKind::Arrow { style, .. }
        | ElementKind::StickyStack { style, .. }
        | ElementKind::Text { style, .. } => Some(style),
        ElementKind::Frame { .. } => None,
    }
}

/// Translate an element by a world-space delta while a bound connector keeps
/// its bindings: free endpoints move with the drag, bound endpoints stay glued
/// to their element and only refresh their stored coordinates. Dragging a
/// fully bound connector therefore holds it still — drag one of its endpoints
/// to rewire or detach it.
fn translate_element_keeping_bindings(doc: &Document, element: &mut Element, dx: f64, dy: f64) {
    if !matches!(element.kind, ElementKind::Arrow { .. }) {
        translate_element(element, dx, dy);
        return;
    }
    let ElementKind::Arrow {
        start,
        end,
        waypoints,
        ..
    } = &mut element.kind
    else {
        return;
    };
    if start.binding.is_none() {
        start.x += dx;
        start.y += dy;
    }
    if end.binding.is_none() {
        end.x += dx;
        end.y += dy;
    }
    // Waypoints are authored bends: they ride the drag whichever ends are
    // bound, so a bound connector's path can still be repositioned.
    for waypoint in waypoints.iter_mut() {
        waypoint.x += dx;
        waypoint.y += dy;
    }
    if start.binding.is_some() {
        let other = Point::new(end.x, end.y);
        let (resolved, _) = connector::resolve_endpoint(start, other, doc);
        start.x = resolved.x;
        start.y = resolved.y;
    }
    if end.binding.is_some() {
        let other = Point::new(start.x, start.y);
        let (resolved, _) = connector::resolve_endpoint(end, other, doc);
        end.x = resolved.x;
        end.y = resolved.y;
    }
}

/// Translate every kind of element in place.
pub fn translate_element(element: &mut Element, dx: f64, dy: f64) {
    match &mut element.kind {
        ElementKind::Shape { rect, .. }
        | ElementKind::Glyph { rect, .. }
        | ElementKind::Text { rect, .. }
        | ElementKind::StickyStack { rect, .. }
        | ElementKind::Frame { rect, .. } => {
            rect.x += dx;
            rect.y += dy;
        }
        ElementKind::Freehand { points, .. } => {
            for p in points.iter_mut() {
                p.x += dx;
                p.y += dy;
            }
        }
        ElementKind::Arrow {
            start,
            end,
            waypoints,
            ..
        } => {
            start.x += dx;
            start.y += dy;
            end.x += dx;
            end.y += dy;
            for waypoint in waypoints.iter_mut() {
                waypoint.x += dx;
                waypoint.y += dy;
            }
        }
    }
}

/// Map every kind of element's geometry from one bounding rectangle to another.
pub fn scale_element(element: &mut Element, from: Rect, to: Rect) {
    if from.w <= 0.001 || from.h <= 0.001 {
        return;
    }
    let sx = to.w / from.w;
    let sy = to.h / from.h;
    let map = |p: Point| Point::new(to.x + (p.x - from.x) * sx, to.y + (p.y - from.y) * sy);
    match &mut element.kind {
        ElementKind::Shape { rect, .. }
        | ElementKind::Glyph { rect, .. }
        | ElementKind::Text { rect, .. }
        | ElementKind::StickyStack { rect, .. }
        | ElementKind::Frame { rect, .. } => {
            *rect = Rect::new(
                to.x + (rect.x - from.x) * sx,
                to.y + (rect.y - from.y) * sy,
                rect.w * sx,
                rect.h * sy,
            );
        }
        ElementKind::Freehand { points, .. } => {
            for p in points.iter_mut() {
                *p = map(*p);
            }
        }
        ElementKind::Arrow {
            start,
            end,
            waypoints,
            ..
        } => {
            let s = map(Point::new(start.x, start.y));
            let e = map(Point::new(end.x, end.y));
            *start = crate::model::Endpoint {
                x: s.x,
                y: s.y,
                binding: start.binding.clone(),
            };
            *end = crate::model::Endpoint {
                x: e.x,
                y: e.y,
                binding: end.binding.clone(),
            };
            for waypoint in waypoints.iter_mut() {
                *waypoint = map(*waypoint);
            }
        }
    }
}

/// The smallest frame whose rectangle contains a point.
fn smallest_frame_containing(frames: &[(&str, Rect)], point: Point) -> Option<ElementId> {
    let mut best: Option<(f64, &str)> = None;
    for (id, rect) in frames {
        if rect.contains(point) {
            let area = rect.w * rect.h;
            if best.as_ref().map(|(a, _)| area < *a).unwrap_or(true) {
                best = Some((area, id));
            }
        }
    }
    best.map(|(_, id)| id.to_string())
}

/// The smallest frame that strictly contains `element`'s bounds — the parent
/// of a nested frame. Strict containment (equal rects don't count) keeps
/// frame parentage acyclic: two frames can never own each other.
fn smallest_frame_strictly_containing(
    frames: &[(&str, Rect)],
    element: &Element,
) -> Option<ElementId> {
    let bounds = element.bounds();
    let min = Point::new(bounds.min_x(), bounds.min_y());
    let max = Point::new(bounds.max_x(), bounds.max_y());
    let mut best: Option<(f64, &str)> = None;
    for (id, rect) in frames {
        if *id == element.id || *rect == bounds {
            continue;
        }
        if rect.contains(min) && rect.contains(max) {
            let area = rect.w * rect.h;
            if best.as_ref().map(|(a, _)| area < *a).unwrap_or(true) {
                best = Some((area, id));
            }
        }
    }
    best.map(|(_, id)| id.to_string())
}

/// Compute resized bounds from a handle drag. Dragging an edge or corner
/// past the opposite side flips the rectangle (the anchor point stays put).
fn resized_bounds(
    bounds: Rect,
    handle: Handle,
    start: Point,
    current: Point,
    keep_aspect: bool,
) -> Rect {
    let dx = if handle.moves_x() {
        current.x - start.x
    } else {
        0.0
    };
    let dy = if handle.moves_y() {
        current.y - start.y
    } else {
        0.0
    };

    // The fixed point: opposite corner for corner handles, opposite edge for
    // edge handles.
    let anchor_x = if handle.affects_left() {
        bounds.max_x()
    } else {
        bounds.x
    };
    let anchor_y = if handle.affects_top() {
        bounds.max_y()
    } else {
        bounds.y
    };

    let (mut left, mut right) = (bounds.x, bounds.max_x());
    let (mut top, mut bottom) = (bounds.y, bounds.max_y());
    if handle.affects_left() {
        left += dx;
    }
    if handle.affects_right() {
        right += dx;
    }
    if handle.affects_top() {
        top += dy;
    }
    if handle.affects_bottom() {
        bottom += dy;
    }

    if keep_aspect && handle.moves_x() && handle.moves_y() && bounds.w > 0.001 && bounds.h > 0.001 {
        // Scale about the anchor, preserving each axis's drag direction.
        let dir_x = if right < left { -1.0 } else { 1.0 };
        let dir_y = if bottom < top { -1.0 } else { 1.0 };
        let width = (right - left).abs();
        let height = (bottom - top).abs();
        let scale = (width / bounds.w).max(height / bounds.h).max(0.05);
        let new_w = bounds.w * scale;
        let new_h = bounds.h * scale;
        if handle.affects_left() {
            left = anchor_x - dir_x * new_w;
            right = anchor_x;
        } else {
            left = anchor_x;
            right = anchor_x + dir_x * new_w;
        }
        if handle.affects_top() {
            top = anchor_y - dir_y * new_h;
            bottom = anchor_y;
        } else {
            top = anchor_y;
            bottom = anchor_y + dir_y * new_h;
        }
    }

    // Minimum size measured from the anchor; flips are preserved.
    let min = MIN_RESIZE_PX;
    if handle.moves_x() {
        let sign = if right < left { -1.0 } else { 1.0 };
        let width = (right - left).abs().max(min);
        if handle.affects_left() {
            left = anchor_x - sign * width;
            right = anchor_x;
        } else {
            left = anchor_x;
            right = anchor_x + sign * width;
        }
    }
    if handle.moves_y() {
        let sign = if bottom < top { -1.0 } else { 1.0 };
        let height = (bottom - top).abs().max(min);
        if handle.affects_top() {
            top = anchor_y - sign * height;
            bottom = anchor_y;
        } else {
            top = anchor_y;
            bottom = anchor_y + sign * height;
        }
    }

    Rect::from_points(Point::new(left, top), Point::new(right, bottom))
}

/// Point copied elements' references at copied elements where they can.
fn remap_references(element: &mut Element, map: &HashMap<ElementId, ElementId>) {
    if let Some(new_id) = element
        .frame_id
        .as_ref()
        .and_then(|old| map.get(old).cloned())
    {
        element.frame_id = Some(new_id);
    }
    let remap = |slot: &mut Option<ElementId>| {
        if let Some(new_id) = slot.as_ref().and_then(|old| map.get(old).cloned()) {
            *slot = Some(new_id);
        }
    };
    match &mut element.kind {
        ElementKind::Shape { bound_text_id, .. } => remap(bound_text_id),
        ElementKind::Glyph { caption_id, .. } => remap(caption_id),
        ElementKind::Text { container_id, .. } => remap(container_id),
        ElementKind::Arrow {
            start,
            end,
            label_id,
            ..
        } => {
            remap(label_id);
            for endpoint in [start, end] {
                if let Some(binding) = &mut endpoint.binding {
                    if let Some(new_id) = map.get(&binding.element_id) {
                        binding.element_id = new_id.clone();
                    }
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor() -> Editor {
        Editor::new(Document::new())
    }

    fn press(editor: &mut Editor, key: &str, mods: Modifiers, now: u64) -> Effects {
        let fx = editor.key_down(key, mods, now);
        pump(editor, &fx);
        fx
    }

    /// Mirror the shell: effects must be pumped into the scene/hit caches.
    /// Without this the interaction tests read stale caches — exactly as a
    /// page that ignored its repaint effects would.
    fn pump(editor: &mut Editor, fx: &Effects) {
        if fx.repaint_all {
            editor.refresh_scene();
        } else if !fx.changed.is_empty() {
            editor.rebuild_changed(&fx.changed);
        }
    }

    fn drag_shape(editor: &mut Editor, from: Point, to: Point, now: u64) {
        let mods = Modifiers::default();
        press(editor, "r", mods, now);
        let fx = editor.pointer_down(from, 0, mods, now);
        pump(editor, &fx);
        let fx = editor.pointer_move(to, mods, now + 1);
        pump(editor, &fx);
        let fx = editor.pointer_up(to, mods, now + 2);
        pump(editor, &fx);
    }

    #[test]
    fn focus_camera_frames_an_element_and_requires_content() {
        let mut e = editor();
        assert!(e.focus_camera(None, 40.0).is_none());
        assert!(e.focus_camera(Some("missing"), 40.0).is_none());

        e.set_viewport(800.0, 600.0);
        drag_shape(
            &mut e,
            Point::new(100.0, 100.0),
            Point::new(300.0, 200.0),
            1,
        );
        let id = e.doc.ordered_live()[0].id.clone();
        let camera = e.focus_camera(Some(&id), 40.0).expect("focus target");
        let bounds = e.doc.framing_bounds(&id).unwrap();
        let screen = camera.world_to_screen(bounds.center());
        assert!((screen.x - 400.0).abs() < 1e-9);
        assert!((screen.y - 300.0).abs() < 1e-9);
        assert!(camera.zoom > 1.0);

        let fit_all = e.focus_camera(None, 40.0).expect("document fits");
        assert!((fit_all.zoom - camera.zoom).abs() < 1e-9);
    }

    #[test]
    fn focus_camera_of_a_frame_includes_its_children() {
        let mut e = editor();
        e.set_viewport(800.0, 600.0);
        let frame = e.doc.add(
            ElementKind::Frame {
                rect: Rect::new(0.0, 0.0, 200.0, 200.0),
                name: "VPC".into(),
            },
            1,
        );
        let child = e.doc.add(
            ElementKind::Shape {
                shape: ShapeKind::Rectangle,
                rect: Rect::new(400.0, 100.0, 100.0, 60.0),
                style: Style::default(),
                bound_text_id: None,
            },
            2,
        );
        e.doc.element_mut(&child).unwrap().frame_id = Some(frame.clone());
        let camera = e.focus_camera(Some(&frame), 40.0).unwrap();
        let screen = camera.world_to_screen(Point::new(250.0, 100.0));
        assert!(
            (screen.x - 400.0).abs() < 1e-9,
            "frame focus missed the child"
        );
    }

    #[test]
    fn dragging_with_rectangle_tool_creates_a_shape() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(10.0, 10.0), Point::new(110.0, 60.0), 1);
        assert_eq!(e.doc.ordered_live().len(), 1);
        let shape = e.doc.ordered_live()[0];
        assert_eq!(shape.bounds(), Rect::new(10.0, 10.0, 100.0, 50.0));
        assert_eq!(e.selection.len(), 1);
    }

    #[test]
    fn snapping_rounds_creation_to_the_grid() {
        let mut e = editor();
        assert!(!e.snap_to_grid, "snapping is off by default");
        assert!(!e.grid_visible, "the grid is off by default");
        e.snap_to_grid = true;
        drag_shape(&mut e, Point::new(13.0, 27.0), Point::new(103.0, 217.0), 1);
        assert_eq!(
            e.doc.ordered_live()[0].bounds(),
            Rect::new(20.0, 20.0, 80.0, 200.0)
        );
    }

    #[test]
    fn snapping_respects_a_custom_grid_size() {
        let mut e = editor();
        e.snap_to_grid = true;
        e.grid_size = 40.0;
        drag_shape(&mut e, Point::new(13.0, 27.0), Point::new(103.0, 217.0), 1);
        assert_eq!(
            e.doc.ordered_live()[0].bounds(),
            Rect::new(0.0, 40.0, 120.0, 160.0)
        );
    }

    #[test]
    fn snapping_moves_shapes_in_grid_steps() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 50.0), 1);
        press(&mut e, "v", Modifiers::default(), 2);
        e.snap_to_grid = true;
        let mods = Modifiers::default();
        {
            let fx = e.pointer_down(Point::new(50.0, 25.0), 0, mods, 3);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(63.0, 25.0), mods, 4);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(63.0, 25.0), mods, 5);
            pump(&mut e, &fx);
        }
        assert_eq!(e.doc.ordered_live()[0].bounds().x, 20.0);
    }

    #[test]
    fn snapping_rounds_resize_edges() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 100.0), 1);
        press(&mut e, "v", Modifiers::default(), 2);
        e.snap_to_grid = true;
        let mods = Modifiers::default();
        {
            let fx = e.pointer_down(Point::new(100.0, 100.0), 0, mods, 3);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(153.0, 147.0), mods, 4);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(153.0, 147.0), mods, 5);
            pump(&mut e, &fx);
        }
        let bounds = e.doc.ordered_live()[0].bounds();
        assert_eq!(bounds.w, 160.0);
        assert_eq!(bounds.h, 140.0);
    }

    #[test]
    fn tiny_draws_are_discarded() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(10.0, 10.0), Point::new(11.0, 11.0), 1);
        assert!(e.doc.is_empty());
    }

    #[test]
    fn click_selects_and_drag_moves() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 50.0), 1);
        press(&mut e, "v", Modifiers::default(), 2);
        e.clear_selection();

        let mods = Modifiers::default();
        {
            let fx = e.pointer_down(Point::new(50.0, 25.0), 0, mods, 3);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(80.0, 45.0), mods, 4);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(80.0, 45.0), mods, 5);
            pump(&mut e, &fx);
        }

        let moved = e.doc.ordered_live()[0];
        assert_eq!(moved.bounds(), Rect::new(30.0, 20.0, 100.0, 50.0));
        assert_eq!(e.selection.len(), 1);
    }

    #[test]
    fn move_is_one_undo_step_and_undo_restores() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 50.0), 1);
        press(&mut e, "v", Modifiers::default(), 2);
        let mods = Modifiers::default();

        {
            let fx = e.pointer_down(Point::new(50.0, 25.0), 0, mods, 3);
            pump(&mut e, &fx);
        }
        for step in 0..10 {
            {
                let fx = e.pointer_move(Point::new(50.0 + step as f64, 25.0), mods, 4 + step);
                pump(&mut e, &fx);
            }
        }
        {
            let fx = e.pointer_up(Point::new(60.0, 25.0), mods, 20);
            pump(&mut e, &fx);
        }
        assert_eq!(e.doc.ordered_live()[0].bounds().x, 10.0);

        let mods_cmd = Modifiers {
            meta: true,
            ..Modifiers::default()
        };
        press(&mut e, "z", mods_cmd, 21);
        assert_eq!(e.doc.ordered_live()[0].bounds().x, 0.0);
        press(&mut e, "z", mods_cmd, 22);
        assert!(e.doc.is_empty(), "second undo removes the drawn shape");
    }

    #[test]
    fn marquee_selects_everything_it_touches() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(50.0, 50.0), 1);
        drag_shape(&mut e, Point::new(200.0, 0.0), Point::new(250.0, 50.0), 2);
        press(&mut e, "v", Modifiers::default(), 3);
        e.clear_selection();

        let mods = Modifiers::default();
        {
            let fx = e.pointer_down(Point::new(-20.0, -20.0), 0, mods, 4);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(300.0, 100.0), mods, 5);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(300.0, 100.0), mods, 6);
            pump(&mut e, &fx);
        }
        assert_eq!(e.selection.len(), 2);
    }

    #[test]
    fn dragging_empty_space_inside_the_selection_moves_everything() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 50.0), 1);
        drag_shape(&mut e, Point::new(200.0, 0.0), Point::new(300.0, 50.0), 4);
        let ids: Vec<ElementId> = e
            .doc
            .ordered_live()
            .iter()
            .map(|element| element.id.clone())
            .collect();
        press(&mut e, "v", Modifiers::default(), 7);
        e.select(ids);

        let mods = Modifiers::default();
        // Empty space between the two shapes, inside their common bounds.
        {
            let fx = e.pointer_down(Point::new(150.0, 25.0), 0, mods, 8);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(160.0, 45.0), mods, 9);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(160.0, 45.0), mods, 10);
            pump(&mut e, &fx);
        }

        assert_eq!(
            e.doc.ordered_live()[0].bounds(),
            Rect::new(10.0, 20.0, 100.0, 50.0)
        );
        assert_eq!(
            e.doc.ordered_live()[1].bounds(),
            Rect::new(210.0, 20.0, 100.0, 50.0)
        );
        assert_eq!(e.selection.len(), 2, "the whole group stays selected");

        let mods_cmd = Modifiers {
            meta: true,
            ..Modifiers::default()
        };
        press(&mut e, "z", mods_cmd, 11);
        assert_eq!(
            e.doc.ordered_live()[0].bounds(),
            Rect::new(0.0, 0.0, 100.0, 50.0),
            "the group move is one undo step"
        );
    }

    #[test]
    fn clicking_empty_space_inside_the_selection_clears_it() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 50.0), 1);
        drag_shape(&mut e, Point::new(200.0, 0.0), Point::new(300.0, 50.0), 4);
        let ids: Vec<ElementId> = e
            .doc
            .ordered_live()
            .iter()
            .map(|element| element.id.clone())
            .collect();
        press(&mut e, "v", Modifiers::default(), 7);
        e.select(ids);

        let mods = Modifiers::default();
        {
            let fx = e.pointer_down(Point::new(150.0, 25.0), 0, mods, 8);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(150.0, 25.0), mods, 9);
            pump(&mut e, &fx);
        }
        assert!(e.selection.is_empty(), "a click inside the box deselects");
        assert_eq!(
            e.doc.ordered_live()[0].bounds(),
            Rect::new(0.0, 0.0, 100.0, 50.0),
            "nothing moved"
        );
    }

    #[test]
    fn a_drag_outbox_delta_carries_the_committed_version() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 50.0), 1);
        let id = e.doc.ordered_live()[0].id.clone();
        // The creation patch is irrelevant; inspect only the move.
        e.history.drain_outbox();

        press(&mut e, "v", Modifiers::default(), 10);
        e.select([id.clone()]);
        let mods = Modifiers::default();
        let fx = e.pointer_down(Point::new(50.0, 25.0), 0, mods, 20);
        pump(&mut e, &fx);
        let fx = e.pointer_move(Point::new(80.0, 45.0), mods, 21);
        pump(&mut e, &fx);
        let fx = e.pointer_up(Point::new(80.0, 45.0), mods, 22);
        pump(&mut e, &fx);

        let patch = crate::patch::Patch::from_deltas(&e.history.drain_outbox(), 23);
        let committed = e.doc.element(&id).unwrap();
        let wire = patch
            .put
            .iter()
            .find(|element| element.id == id)
            .expect("the move is on the wire");
        assert_eq!(
            (wire.version, wire.version_nonce),
            (committed.version, committed.version_nonce),
            "an outbox delta must match the committed document, or an undo can lose the merge"
        );
        assert!(wire.content_eq(committed));
    }

    #[test]
    fn dragging_an_element_in_the_selection_still_moves_the_group() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 50.0), 1);
        drag_shape(&mut e, Point::new(200.0, 0.0), Point::new(300.0, 50.0), 4);
        let ids: Vec<ElementId> = e
            .doc
            .ordered_live()
            .iter()
            .map(|element| element.id.clone())
            .collect();
        press(&mut e, "v", Modifiers::default(), 7);
        e.select(ids);

        let mods = Modifiers::default();
        {
            let fx = e.pointer_down(Point::new(50.0, 25.0), 0, mods, 8);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(70.0, 35.0), mods, 9);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(70.0, 35.0), mods, 10);
            pump(&mut e, &fx);
        }

        assert_eq!(
            e.doc.ordered_live()[0].bounds(),
            Rect::new(20.0, 10.0, 100.0, 50.0)
        );
        assert_eq!(
            e.doc.ordered_live()[1].bounds(),
            Rect::new(220.0, 10.0, 100.0, 50.0)
        );
    }

    #[test]
    fn delete_removes_selection_and_undo_restores_it() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 50.0), 1);
        let removed = e.doc.ordered_live()[0].id.clone();
        press(&mut e, "v", Modifiers::default(), 2);
        e.select([removed.clone()]);
        press(&mut e, "Delete", Modifiers::default(), 3);
        assert!(e.doc.ordered_live().is_empty());
        assert!(e.doc.element(&removed).unwrap().is_deleted);

        let mods_cmd = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        press(&mut e, "z", mods_cmd, 4);
        assert_eq!(e.doc.ordered_live().len(), 1);
    }

    #[test]
    fn shift_click_toggles_selection() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(50.0, 50.0), 1);
        drag_shape(&mut e, Point::new(100.0, 0.0), Point::new(150.0, 50.0), 2);
        press(&mut e, "v", Modifiers::default(), 3);

        let mods = Modifiers::default();
        {
            let fx = e.pointer_down(Point::new(25.0, 25.0), 0, mods, 4);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(25.0, 25.0), mods, 5);
            pump(&mut e, &fx);
        }

        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        {
            let fx = e.pointer_down(Point::new(125.0, 25.0), 0, shift, 6);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(125.0, 25.0), shift, 7);
            pump(&mut e, &fx);
        }
        assert_eq!(e.selection.len(), 2);

        {
            let fx = e.pointer_down(Point::new(125.0, 25.0), 0, shift, 8);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(125.0, 25.0), shift, 9);
            pump(&mut e, &fx);
        }
        assert_eq!(e.selection.len(), 1);
    }

    #[test]
    fn escape_cancels_an_in_progress_draw() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "r", mods, 1);
        {
            let fx = e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(100.0, 100.0), mods, 3);
            pump(&mut e, &fx);
        }
        e.key_down("Escape", mods, 4);
        assert!(e.doc.is_empty());
        assert_eq!(e.interaction, Interaction::Idle);
    }

    #[test]
    fn tool_shortcuts_switch_tools() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "d", mods, 1);
        assert_eq!(e.tool, Tool::Diamond);
        press(&mut e, "o", mods, 2);
        assert_eq!(e.tool, Tool::Ellipse);
        press(&mut e, "v", mods, 3);
        assert_eq!(e.tool, Tool::Select);
    }

    #[test]
    fn resize_handle_drag_scales_the_selection() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 50.0), 1);
        press(&mut e, "v", Modifiers::default(), 2);
        let mods = Modifiers::default();

        let handle = e.resize_handles().unwrap()[4];
        assert_eq!(handle.0, Handle::Se);
        assert_eq!(handle.1, Point::new(100.0, 50.0));

        {
            let fx = e.pointer_down(Point::new(100.0, 50.0), 0, mods, 3);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(150.0, 100.0), mods, 4);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(150.0, 100.0), mods, 5);
            pump(&mut e, &fx);
        }
        assert_eq!(
            e.doc.ordered_live()[0].bounds(),
            Rect::new(0.0, 0.0, 150.0, 100.0)
        );

        let cmd = Modifiers {
            meta: true,
            ..Modifiers::default()
        };
        press(&mut e, "z", cmd, 6);
        assert_eq!(
            e.doc.ordered_live()[0].bounds(),
            Rect::new(0.0, 0.0, 100.0, 50.0)
        );
    }

    #[test]
    fn dragging_a_handle_past_the_opposite_corner_flips_the_shape() {
        let mut e = editor();
        drag_shape(
            &mut e,
            Point::new(200.0, 200.0),
            Point::new(400.0, 300.0),
            1,
        );
        press(&mut e, "v", Modifiers::default(), 2);
        let mods = Modifiers::default();

        // NE handle sits at (400, 200); drag it past the SW corner (200, 300).
        {
            let fx = e.pointer_down(Point::new(400.0, 200.0), 0, mods, 3);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(150.0, 350.0), mods, 4);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(150.0, 350.0), mods, 5);
            pump(&mut e, &fx);
        }

        let flipped = e.doc.ordered_live()[0].bounds();
        assert_eq!(
            flipped,
            Rect::new(150.0, 300.0, 50.0, 50.0),
            "the rectangle flipped to the far side of its SW anchor"
        );

        let cmd = Modifiers {
            meta: true,
            ..Modifiers::default()
        };
        press(&mut e, "z", cmd, 6);
        assert_eq!(
            e.doc.ordered_live()[0].bounds(),
            Rect::new(200.0, 200.0, 200.0, 100.0)
        );
    }

    #[test]
    fn resizing_stops_at_a_minimum_against_the_anchor() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 50.0), 1);
        press(&mut e, "v", Modifiers::default(), 2);
        let mods = Modifiers::default();

        // Drag the SE handle almost onto the NW anchor.
        {
            let fx = e.pointer_down(Point::new(100.0, 50.0), 0, mods, 3);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(1.0, 1.0), mods, 4);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(1.0, 1.0), mods, 5);
            pump(&mut e, &fx);
        }
        let bounds = e.doc.ordered_live()[0].bounds();
        assert_eq!(bounds.x, 0.0);
        assert_eq!(bounds.y, 0.0);
        assert!(
            bounds.w >= 4.0 && bounds.h >= 4.0,
            "min size kept: {bounds:?}"
        );
    }

    #[test]
    fn aspect_preserving_resize_keeps_ratio() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 50.0), 1);
        press(&mut e, "v", Modifiers::default(), 2);
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };

        {
            let fx = e.pointer_down(Point::new(100.0, 50.0), 0, shift, 3);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(140.0, 50.0), shift, 4);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(140.0, 50.0), shift, 5);
            pump(&mut e, &fx);
        }
        assert_eq!(
            e.doc.ordered_live()[0].bounds(),
            Rect::new(0.0, 0.0, 140.0, 70.0)
        );
    }

    #[test]
    fn nudge_moves_by_one_pixel_or_ten_with_shift() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 50.0), 1);
        let mods = Modifiers::default();
        press(&mut e, "ArrowRight", mods, 2);
        assert_eq!(e.doc.ordered_live()[0].bounds().x, 1.0);
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        press(&mut e, "ArrowDown", shift, 3);
        assert_eq!(e.doc.ordered_live()[0].bounds().y, 10.0);

        let cmd = Modifiers {
            meta: true,
            ..Modifiers::default()
        };
        press(&mut e, "z", cmd, 4);
        assert_eq!(e.doc.ordered_live()[0].bounds().y, 0.0);
    }

    #[test]
    fn duplicate_offsets_and_selects_the_copies() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 50.0), 1);
        let original_id = e.doc.ordered_live()[0].id.clone();

        let cmd = Modifiers {
            meta: true,
            ..Modifiers::default()
        };
        press(&mut e, "d", cmd, 2);
        assert_eq!(e.doc.ordered_live().len(), 2);
        assert_eq!(e.selection.len(), 1);
        assert!(!e.selection.contains(&original_id));
        let copy = e.doc.element(e.selection.iter().next().unwrap()).unwrap();
        assert_eq!(copy.bounds(), Rect::new(12.0, 12.0, 100.0, 50.0));
    }

    #[test]
    fn copy_paste_and_cut() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 50.0), 1);
        let cmd = Modifiers {
            meta: true,
            ..Modifiers::default()
        };
        press(&mut e, "c", cmd, 2);
        press(&mut e, "v", cmd, 3);
        press(&mut e, "v", cmd, 4);
        assert_eq!(e.doc.ordered_live().len(), 3);

        press(&mut e, "a", cmd, 5);
        assert_eq!(e.selection.len(), 3);
        press(&mut e, "x", cmd, 6);
        assert_eq!(e.doc.ordered_live().len(), 0);

        press(&mut e, "z", cmd, 7);
        assert_eq!(e.doc.ordered_live().len(), 3);
    }

    #[test]
    fn bring_forward_and_send_backward_change_z_order() {
        let mut e = editor();
        let first = e.doc.add(
            ElementKind::Shape {
                shape: ShapeKind::Rectangle,
                rect: Rect::new(0.0, 0.0, 50.0, 50.0),
                style: Style::default(),
                bound_text_id: None,
            },
            1,
        );
        let second = e.doc.add(
            ElementKind::Shape {
                shape: ShapeKind::Rectangle,
                rect: Rect::new(0.0, 0.0, 50.0, 50.0),
                style: Style::default(),
                bound_text_id: None,
            },
            1,
        );
        e.refresh_scene();
        e.select([first.clone()]);

        let cmd = Modifiers {
            meta: true,
            shift: true,
            ..Modifiers::default()
        };
        press(&mut e, "]", cmd, 2);
        assert_eq!(e.doc.ordered_ids(), vec![second.clone(), first.clone()]);
        press(&mut e, "[", cmd, 3);
        assert_eq!(e.doc.ordered_ids(), vec![first.clone(), second]);
    }

    #[test]
    fn eraser_drag_deletes_touched_elements_as_one_step() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(50.0, 50.0), 1);
        drag_shape(&mut e, Point::new(100.0, 0.0), Point::new(150.0, 50.0), 2);
        press(&mut e, "e", Modifiers::default(), 3);

        let mods = Modifiers::default();
        {
            let fx = e.pointer_down(Point::new(25.0, 25.0), 0, mods, 4);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(125.0, 25.0), mods, 5);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(125.0, 25.0), mods, 6);
            pump(&mut e, &fx);
        }
        assert!(e.doc.ordered_live().is_empty());

        let cmd = Modifiers {
            meta: true,
            ..Modifiers::default()
        };
        press(&mut e, "z", cmd, 7);
        assert_eq!(e.doc.ordered_live().len(), 2, "one undo restores both");
    }

    #[test]
    fn freehand_captures_points_and_smooths() {
        let mut e = editor();
        press(&mut e, "p", Modifiers::default(), 1);
        let mods = Modifiers::default();
        e.set_pressure(0.7);
        {
            let fx = e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
            pump(&mut e, &fx);
        }
        for i in 1..=20 {
            e.pointer_move(
                Point::new(i as f64 * 3.0, (i as f64 * 0.7).sin() * 4.0),
                mods,
                2 + i,
            );
        }
        {
            let fx = e.pointer_up(Point::new(60.0, 0.0), mods, 30);
            pump(&mut e, &fx);
        }

        assert_eq!(e.doc.ordered_live().len(), 1);
        let points = match &e.doc.ordered_live()[0].kind {
            ElementKind::Freehand {
                points, pressures, ..
            } => {
                assert_eq!(pressures.len(), points.len());
                assert!(pressures.iter().all(|p| (*p - 0.7).abs() < 1e-9));
                points.clone()
            }
            other => panic!("expected freehand, got {other:?}"),
        };
        assert!(points.len() >= 3, "kept a stroke of points");
        assert_eq!(points[0], Point::new(0.0, 0.0), "endpoints are preserved");
    }

    #[test]
    fn escape_cancels_freehand() {
        let mut e = editor();
        press(&mut e, "p", Modifiers::default(), 1);
        let mods = Modifiers::default();
        {
            let fx = e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(50.0, 10.0), mods, 3);
            pump(&mut e, &fx);
        }
        e.key_down("Escape", mods, 4);
        assert!(e.doc.is_empty());
    }

    #[test]
    fn freehand_stays_armed_and_the_stroke_stays_selectable() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "p", mods, 1);
        assert!(e.tool_locked, "the brush arms locked");
        {
            let fx = e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
            pump(&mut e, &fx);
        }
        for i in 1..=20 {
            e.pointer_move(
                Point::new(i as f64 * 3.0, (i as f64 * 0.7).sin() * 4.0),
                mods,
                2 + i,
            );
        }
        {
            let fx = e.pointer_up(Point::new(57.0, 2.6), mods, 30);
            pump(&mut e, &fx);
        }

        assert_eq!(e.tool, Tool::Freehand, "the pencil stays armed");
        assert_eq!(e.doc.ordered_live().len(), 1);

        // A second stroke needs no re-arming.
        {
            let fx = e.pointer_down(Point::new(100.0, 100.0), 0, mods, 31);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(140.0, 120.0), mods, 32);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(140.0, 120.0), mods, 33);
            pump(&mut e, &fx);
        }
        assert_eq!(e.doc.ordered_live().len(), 2, "the brush keeps drawing");

        let id = e.doc.ordered_live()[0].id.clone();
        e.clear_selection();
        press(&mut e, "v", mods, 34);
        {
            let fx = e.pointer_down(Point::new(0.0, 0.0), 0, mods, 35);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(0.0, 0.0), mods, 36);
            pump(&mut e, &fx);
        }
        assert_eq!(
            e.selection.iter().next(),
            Some(&id),
            "the stroke is selectable once Select is armed"
        );
    }

    #[test]
    fn pressing_the_armed_brush_shortcut_unlocks_it() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "p", mods, 1);
        assert!(e.tool_locked);
        press(&mut e, "p", mods, 2);
        assert!(!e.tool_locked, "second press makes the brush single-shot");

        {
            let fx = e.pointer_down(Point::new(0.0, 0.0), 0, mods, 3);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(60.0, 20.0), mods, 4);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(60.0, 20.0), mods, 5);
            pump(&mut e, &fx);
        }
        assert_eq!(
            e.tool,
            Tool::Select,
            "unlocked, it hands back after one use"
        );
    }

    #[test]
    fn line_tool_creates_an_arrow_element() {
        let mut e = editor();
        press(&mut e, "l", Modifiers::default(), 1);
        let mods = Modifiers::default();
        {
            let fx = e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(100.0, 50.0), mods, 3);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(100.0, 50.0), mods, 4);
            pump(&mut e, &fx);
        }

        let element = e.doc.ordered_live()[0];
        match &element.kind {
            ElementKind::Arrow {
                start,
                end,
                end_arrowhead,
                ..
            } => {
                assert_eq!(start.point(), Point::new(0.0, 0.0));
                assert_eq!(end.point(), Point::new(100.0, 50.0));
                assert!(!end_arrowhead);
            }
            other => panic!("expected arrow, got {other:?}"),
        }

        press(&mut e, "a", Modifiers::default(), 5);
        {
            let fx = e.pointer_down(Point::new(200.0, 0.0), 0, mods, 6);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(300.0, 0.0), mods, 7);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(300.0, 0.0), mods, 8);
            pump(&mut e, &fx);
        }
        let arrow = e.doc.ordered_live()[1];
        match &arrow.kind {
            ElementKind::Arrow { end_arrowhead, .. } => assert!(end_arrowhead),
            other => panic!("expected arrow, got {other:?}"),
        }
    }

    #[test]
    fn shift_constrains_line_to_angles() {
        let mut e = editor();
        press(&mut e, "l", Modifiers::default(), 1);
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        {
            let fx = e.pointer_down(Point::new(0.0, 0.0), 0, shift, 2);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(100.0, 10.0), shift, 3);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(100.0, 10.0), shift, 4);
            pump(&mut e, &fx);
        }
        let element = e.doc.ordered_live()[0];
        if let ElementKind::Arrow { end, .. } = &element.kind {
            assert!(end.y.abs() < 1e-6, "snapped to horizontal: {end:?}");
            assert!((end.x - 100.4).abs() < 1.0);
        } else {
            panic!("expected arrow");
        }
    }

    #[test]
    fn text_tool_commit_sets_content_and_autosizes() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "t", mods, 1);
        {
            let fx = e.pointer_down(Point::new(50.0, 50.0), 0, mods, 2);
            pump(&mut e, &fx);
        }
        let id = e.doc.ordered_live()[0].id.clone();
        {
            let fx = e.commit_text(&id, "Hello world", 3);
            pump(&mut e, &fx);
        }

        match &e.doc.element(&id).unwrap().kind {
            ElementKind::Text { text, rect, .. } => {
                assert_eq!(text, "Hello world");
                assert!(rect.w > 100.0, "auto width grew: {rect:?}");
            }
            other => panic!("expected text, got {other:?}"),
        }
        assert_eq!(e.selection.len(), 1);
    }

    #[test]
    fn empty_text_is_discarded_without_history() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "t", mods, 1);
        {
            let fx = e.pointer_down(Point::new(50.0, 50.0), 0, mods, 2);
            pump(&mut e, &fx);
        }
        let id = e.doc.ordered_live()[0].id.clone();
        {
            let fx = e.commit_text(&id, "   ", 3);
            pump(&mut e, &fx);
        }
        assert!(e.doc.is_empty());
        assert!(!e.history.can_undo());
    }

    #[test]
    fn double_click_shapes_creates_bound_text_that_follows_them() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(200.0, 100.0), 1);
        let shape_id = e.doc.ordered_live()[0].id.clone();
        press(&mut e, "v", Modifiers::default(), 2);

        {
            let fx = e.double_click(Point::new(100.0, 50.0), 3);
            pump(&mut e, &fx);
        }
        let text_id = e
            .doc
            .ordered_live()
            .iter()
            .find(|element| element.id != shape_id)
            .expect("bound text created")
            .id
            .clone();
        {
            let fx = e.commit_text(&text_id, "node", 4);
            pump(&mut e, &fx);
        }

        match e.doc.element(&shape_id).unwrap().kind.clone() {
            ElementKind::Shape { bound_text_id, .. } => {
                assert_eq!(bound_text_id, Some(text_id.clone()));
            }
            other => panic!("expected shape, got {other:?}"),
        }
        let before = e.doc.element(&text_id).unwrap().bounds();

        let mods = Modifiers::default();
        {
            let fx = e.pointer_down(Point::new(100.0, 50.0), 0, mods, 5);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(150.0, 70.0), mods, 6);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(150.0, 70.0), mods, 7);
            pump(&mut e, &fx);
        }
        let after = e.doc.element(&text_id).unwrap().bounds();
        assert!(
            (after.x - before.x - 50.0).abs() < 0.01,
            "bound text follows its shape: {before:?} -> {after:?}"
        );
    }

    #[test]
    fn resizing_a_shape_reflows_bound_text() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(200.0, 100.0), 1);
        let shape_id = e.doc.ordered_live()[0].id.clone();
        press(&mut e, "v", Modifiers::default(), 2);
        {
            let fx = e.double_click(Point::new(100.0, 50.0), 3);
            pump(&mut e, &fx);
        }
        let text_id = e
            .doc
            .ordered_live()
            .iter()
            .find(|element| element.id != shape_id)
            .unwrap()
            .id
            .clone();
        e.commit_text(
            &text_id,
            "a somewhat long caption that wraps onto several lines at small widths",
            4,
        );

        let height_before = e.doc.element(&text_id).unwrap().bounds().h;
        let mods = Modifiers::default();
        // Select the shape (clicking its edge; the text is selected after commit).
        {
            let fx = e.pointer_down(Point::new(5.0, 5.0), 0, mods, 5);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(5.0, 5.0), mods, 5);
            pump(&mut e, &fx);
        }
        assert!(e.selection.contains(&shape_id));

        // Shrink the shape by dragging its SE handle.
        {
            let fx = e.pointer_down(Point::new(200.0, 100.0), 0, mods, 6);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(70.0, 100.0), mods, 7);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(70.0, 100.0), mods, 8);
            pump(&mut e, &fx);
        }
        let height_after = e.doc.element(&text_id).unwrap().bounds().h;
        assert!(
            height_after > height_before,
            "text wrapped taller after shrinking: {height_before} -> {height_after}"
        );
    }

    #[test]
    fn style_patch_applies_to_selection_and_undoes() {
        use crate::model::{FillPattern, StylePatch};
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 50.0), 1);
        let id = e.doc.ordered_live()[0].id.clone();

        let patch = StylePatch {
            stroke_color: Some("#e03131".into()),
            fill_style: Some(FillPattern::Solid),
            fill_color: Some("#ffc9c9".into()),
            ..StylePatch::default()
        };
        {
            let fx = e.update_style(&patch, 2);
            pump(&mut e, &fx);
        }

        match &e.doc.element(&id).unwrap().kind {
            ElementKind::Shape { style, .. } => {
                assert_eq!(style.stroke_color, "#e03131");
                assert_eq!(style.fill_style, FillPattern::Solid);
                assert_eq!(style.fill_color, "#ffc9c9");
            }
            other => panic!("expected shape, got {other:?}"),
        }

        let cmd = Modifiers {
            meta: true,
            ..Modifiers::default()
        };
        press(&mut e, "z", cmd, 3);
        match &e.doc.element(&id).unwrap().kind {
            ElementKind::Shape { style, .. } => assert_eq!(style.stroke_color, "#ece4d6"),
            other => panic!("expected shape, got {other:?}"),
        }
    }

    #[test]
    fn style_patch_without_selection_updates_defaults() {
        use crate::model::StylePatch;
        let mut e = editor();
        let patch = StylePatch {
            stroke_width: Some(4.0),
            ..StylePatch::default()
        };
        {
            let fx = e.update_style(&patch, 1);
            pump(&mut e, &fx);
        }
        assert_eq!(e.style.stroke_width, 4.0);

        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(50.0, 50.0), 2);
        match &e.doc.ordered_live()[0].kind {
            ElementKind::Shape { style, .. } => assert_eq!(style.stroke_width, 4.0),
            other => panic!("expected shape, got {other:?}"),
        }
    }

    #[test]
    fn current_style_reflects_the_selection() {
        use crate::model::StylePatch;
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(50.0, 50.0), 1);
        assert_eq!(e.current_style().stroke_color, "#ece4d6");
        e.update_style(
            &StylePatch {
                stroke_color: Some("#2f9e44".into()),
                ..StylePatch::default()
            },
            2,
        );
        assert_eq!(e.current_style().stroke_color, "#2f9e44");
        e.clear_selection();
        assert_eq!(
            e.current_style().stroke_color,
            "#2f9e44",
            "defaults carry over"
        );
    }

    #[test]
    fn connecting_two_shapes_binds_both_ends() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 100.0), 1);
        drag_shape(&mut e, Point::new(300.0, 0.0), Point::new(400.0, 100.0), 2);
        let a = e.doc.ordered_live()[0].id.clone();
        let b = e.doc.ordered_live()[1].id.clone();
        e.selection.clear();

        let mods = Modifiers::default();
        press(&mut e, "a", mods, 3);
        {
            let fx = e.pointer_down(Point::new(50.0, 50.0), 0, mods, 4);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(200.0, 50.0), mods, 5);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(350.0, 50.0), mods, 6);
            pump(&mut e, &fx);
        }
        assert_eq!(e.hover_anchor_points().len(), 4);
        {
            let fx = e.pointer_up(Point::new(350.0, 50.0), mods, 7);
            pump(&mut e, &fx);
        }

        assert!(e.hover_anchor_points().is_empty());
        assert_eq!(e.doc.ordered_live().len(), 3);
        let arrow = e
            .doc
            .ordered_live()
            .into_iter()
            .find(|element| matches!(element.kind, ElementKind::Arrow { .. }))
            .expect("arrow created")
            .clone();
        match &arrow.kind {
            ElementKind::Arrow { start, end, .. } => {
                assert_eq!(
                    start.binding.as_ref().map(|b| b.element_id.clone()),
                    Some(a.clone())
                );
                assert_eq!(
                    end.binding.as_ref().map(|b| b.element_id.clone()),
                    Some(b.clone())
                );
            }
            other => panic!("expected arrow, got {other:?}"),
        }
    }

    /// Two shapes and a bound elbow connector between them, selected.
    fn bound_arrow_board(e: &mut Editor) {
        drag_shape(e, Point::new(0.0, 0.0), Point::new(100.0, 100.0), 1);
        drag_shape(e, Point::new(300.0, 0.0), Point::new(400.0, 100.0), 2);
        let mods = Modifiers::default();
        press(e, "a", mods, 3);
        {
            let fx = e.pointer_down(Point::new(50.0, 50.0), 0, mods, 4);
            pump(&mut *e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(350.0, 50.0), mods, 5);
            pump(&mut *e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(350.0, 50.0), mods, 6);
            pump(&mut *e, &fx);
        }
    }

    fn arrow_waypoints(e: &Editor) -> Vec<Point> {
        e.doc
            .ordered_live()
            .into_iter()
            .find_map(|element| match &element.kind {
                ElementKind::Arrow { waypoints, .. } => Some(waypoints.clone()),
                _ => None,
            })
            .expect("arrow exists")
    }

    #[test]
    fn dragging_a_segment_midpoint_inserts_a_waypoint() {
        let mut e = editor();
        bound_arrow_board(&mut e);
        assert!(arrow_waypoints(&e).is_empty());
        let handles = e.arrow_path_handles();
        assert!(
            handles.iter().any(|h| h.point() == Point::new(200.0, 50.0)),
            "an insert handle sits on the plain route: {handles:?}"
        );

        let mods = Modifiers::default();
        {
            let fx = e.pointer_down(Point::new(200.0, 50.0), 0, mods, 7);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(200.0, 150.0), mods, 8);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(200.0, 150.0), mods, 9);
            pump(&mut e, &fx);
        }

        // The segment bows down: two waypoints, one at each end of the bow.
        assert_eq!(
            arrow_waypoints(&e),
            vec![Point::new(132.0, 150.0), Point::new(268.0, 150.0)]
        );
        let element = e
            .doc
            .ordered_live()
            .into_iter()
            .find(|el| matches!(el.kind, ElementKind::Arrow { .. }))
            .unwrap()
            .clone();
        let ElementKind::Arrow {
            start,
            end,
            routing,
            waypoints,
            ..
        } = &element.kind
        else {
            unreachable!()
        };
        let route = connector::route(start, end, *routing, waypoints, &e.doc);
        for waypoint in waypoints {
            assert!(
                route.contains(waypoint),
                "the drawn route passes through {waypoint:?}: {route:?}"
            );
        }
    }

    #[test]
    fn clicking_an_insert_handle_leaves_the_route_alone() {
        let mut e = editor();
        bound_arrow_board(&mut e);
        let mods = Modifiers::default();
        // Down and up on the insert handle: no movement, no waypoints.
        {
            let fx = e.pointer_down(Point::new(200.0, 50.0), 0, mods, 7);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(200.0, 50.0), mods, 8);
            pump(&mut e, &fx);
        }
        assert!(arrow_waypoints(&e).is_empty());
    }

    #[test]
    fn double_clicking_the_midpoint_does_not_bend_the_route() {
        let mut e = editor();
        bound_arrow_board(&mut e);
        let mods = Modifiers::default();
        {
            let fx = e.pointer_down(Point::new(200.0, 50.0), 0, mods, 7);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(200.0, 50.0), mods, 8);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_down(Point::new(200.0, 50.0), 0, mods, 9);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(200.0, 50.0), mods, 10);
            pump(&mut e, &fx);
        }
        {
            let fx = e.double_click(Point::new(200.0, 50.0), 11);
            pump(&mut e, &fx);
        }
        assert!(arrow_waypoints(&e).is_empty());
    }

    #[test]
    fn dragging_a_waypoint_moves_it() {
        let mut e = editor();
        bound_arrow_board(&mut e);
        let mods = Modifiers::default();
        {
            let fx = e.pointer_down(Point::new(200.0, 50.0), 0, mods, 7);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(200.0, 150.0), mods, 8);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(200.0, 150.0), mods, 9);
            pump(&mut e, &fx);
        }

        {
            let fx = e.pointer_down(Point::new(132.0, 150.0), 0, mods, 10);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(190.0, 180.0), mods, 11);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(190.0, 180.0), mods, 12);
            pump(&mut e, &fx);
        }
        assert_eq!(
            arrow_waypoints(&e),
            vec![Point::new(190.0, 180.0), Point::new(268.0, 150.0)]
        );
    }

    #[test]
    fn double_clicking_a_waypoint_removes_it() {
        let mut e = editor();
        bound_arrow_board(&mut e);
        let mods = Modifiers::default();
        {
            let fx = e.pointer_down(Point::new(200.0, 50.0), 0, mods, 7);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(200.0, 150.0), mods, 8);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(200.0, 150.0), mods, 9);
            pump(&mut e, &fx);
        }
        assert_eq!(arrow_waypoints(&e).len(), 2);

        {
            let fx = e.double_click(Point::new(132.0, 150.0), 10);
            pump(&mut e, &fx);
        }
        assert_eq!(arrow_waypoints(&e), vec![Point::new(268.0, 150.0)]);
    }

    #[test]
    fn moving_a_bound_shape_updates_the_arrow() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 100.0), 1);
        drag_shape(&mut e, Point::new(300.0, 0.0), Point::new(400.0, 100.0), 2);
        let b = e.doc.ordered_live()[1].id.clone();

        let mods = Modifiers::default();
        press(&mut e, "a", mods, 3);
        {
            let fx = e.pointer_down(Point::new(50.0, 50.0), 0, mods, 4);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(350.0, 50.0), mods, 5);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(350.0, 50.0), mods, 6);
            pump(&mut e, &fx);
        }

        let arrow_id = e
            .doc
            .ordered_live()
            .iter()
            .find(|element| matches!(element.kind, ElementKind::Arrow { .. }))
            .unwrap()
            .id
            .clone();
        let before = e.doc.element(&arrow_id).unwrap().bounds();

        press(&mut e, "v", mods, 7);
        e.select([b.clone()]);
        {
            let fx = e.pointer_down(Point::new(350.0, 50.0), 0, mods, 8);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(450.0, 150.0), mods, 9);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(450.0, 150.0), mods, 10);
            pump(&mut e, &fx);
        }

        let after = e.doc.element(&arrow_id).unwrap().bounds();
        assert!(
            after != before,
            "arrow endpoints followed the moved shape: {before:?} -> {after:?}"
        );
    }

    #[test]
    fn dragging_a_bound_arrow_keeps_it_connected() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 100.0), 1);
        drag_shape(&mut e, Point::new(300.0, 0.0), Point::new(400.0, 100.0), 2);
        let a = e.doc.ordered_live()[0].id.clone();
        let b = e.doc.ordered_live()[1].id.clone();

        let mods = Modifiers::default();
        press(&mut e, "a", mods, 3);
        {
            let fx = e.pointer_down(Point::new(50.0, 50.0), 0, mods, 4);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(350.0, 50.0), mods, 5);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(350.0, 50.0), mods, 6);
            pump(&mut e, &fx);
        }
        let arrow_id = e
            .doc
            .ordered_live()
            .iter()
            .find(|element| matches!(element.kind, ElementKind::Arrow { .. }))
            .unwrap()
            .id
            .clone();

        let bindings = |element: &Element| match &element.kind {
            ElementKind::Arrow { start, end, .. } => (
                start.binding.as_ref().map(|b| b.element_id.clone()),
                end.binding.as_ref().map(|b| b.element_id.clone()),
            ),
            other => panic!("expected an arrow, got {other:?}"),
        };
        let connected = (Some(a.clone()), Some(b.clone()));

        // Dragging the shaft must not silently detach the connector.
        {
            let fx = e.pointer_down(Point::new(200.0, 50.0), 0, mods, 7);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(260.0, 110.0), mods, 8);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(260.0, 110.0), mods, 9);
            pump(&mut e, &fx);
        }
        assert_eq!(
            bindings(e.doc.element(&arrow_id).unwrap()),
            connected,
            "a body drag keeps both bindings"
        );

        // Nudging is the same gesture, one pixel at a time.
        e.select([arrow_id.clone()]);
        press(&mut e, "ArrowRight", mods, 10);
        assert_eq!(
            bindings(e.doc.element(&arrow_id).unwrap()),
            connected,
            "nudging keeps both bindings"
        );

        // The link is still live: moving the target drags the endpoint along.
        e.select([b.clone()]);
        {
            let fx = e.pointer_down(Point::new(350.0, 50.0), 0, mods, 11);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(350.0, 150.0), mods, 12);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(350.0, 150.0), mods, 13);
            pump(&mut e, &fx);
        }

        let arrow = e.doc.element(&arrow_id).unwrap();
        let ElementKind::Arrow { end, .. } = &arrow.kind else {
            panic!("expected an arrow");
        };
        let (point, side) = connector::resolve_endpoint(end, Point::new(100.0, 50.0), &e.doc);
        assert_eq!(side, Some(connector::AnchorSide::W));
        assert_eq!(point, Point::new(300.0, 150.0), "the endpoint follows b");
    }

    #[test]
    fn double_click_a_connector_creates_a_label() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 100.0), 1);
        drag_shape(&mut e, Point::new(300.0, 0.0), Point::new(400.0, 100.0), 2);
        let mods = Modifiers::default();
        press(&mut e, "a", mods, 3);
        {
            let fx = e.pointer_down(Point::new(50.0, 50.0), 0, mods, 4);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(350.0, 50.0), mods, 5);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(350.0, 50.0), mods, 6);
            pump(&mut e, &fx);
        }

        press(&mut e, "v", mods, 7);
        {
            let fx = e.double_click(Point::new(200.0, 50.0), 8);
            pump(&mut e, &fx);
        }
        let text_id = e
            .doc
            .ordered_live()
            .iter()
            .find(|element| element.is_text())
            .expect("label text created")
            .id
            .clone();
        {
            let fx = e.commit_text(&text_id, "https", 9);
            pump(&mut e, &fx);
        }

        let label = e.doc.element(&text_id).unwrap();
        assert!(label.bounds().w > 50.0);
        assert!(
            (label.bounds().center().x - 200.0).abs() < 60.0,
            "label sits near the route midpoint: {:?}",
            label.bounds()
        );
    }

    #[test]
    fn frame_tool_draws_a_named_frame() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "f", mods, 1);
        {
            let fx = e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(400.0, 300.0), mods, 3);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(400.0, 300.0), mods, 4);
            pump(&mut e, &fx);
        }
        assert_eq!(e.doc.ordered_live().len(), 1);
        match &e.doc.ordered_live()[0].kind {
            ElementKind::Frame { rect, name } => {
                assert_eq!(*rect, Rect::new(0.0, 0.0, 400.0, 300.0));
                assert_eq!(name, "Frame 1");
            }
            other => panic!("expected frame, got {other:?}"),
        }
    }

    #[test]
    fn dropping_shapes_in_and_out_of_a_frame_sets_membership() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "f", mods, 1);
        {
            let fx = e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(400.0, 300.0), mods, 3);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(400.0, 300.0), mods, 4);
            pump(&mut e, &fx);
        }
        let frame_id = e.doc.ordered_live()[0].id.clone();

        drag_shape(&mut e, Point::new(50.0, 50.0), Point::new(150.0, 150.0), 5);
        let shape_id = e.doc.ordered_live()[1].id.clone();
        assert_eq!(
            e.doc.element(&shape_id).unwrap().frame_id.as_ref(),
            Some(&frame_id),
            "shape drawn inside the frame is owned"
        );

        press(&mut e, "v", mods, 6);
        e.select([shape_id.clone()]);
        {
            let fx = e.pointer_down(Point::new(100.0, 100.0), 0, mods, 7);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(600.0, 100.0), mods, 8);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(600.0, 100.0), mods, 9);
            pump(&mut e, &fx);
        }
        assert_eq!(
            e.doc.element(&shape_id).unwrap().frame_id,
            None,
            "dragged out, membership released"
        );
    }

    #[test]
    fn moving_a_frame_moves_its_children() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "f", mods, 1);
        {
            let fx = e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(400.0, 300.0), mods, 3);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(400.0, 300.0), mods, 4);
            pump(&mut e, &fx);
        }
        let frame_id = e.doc.ordered_live()[0].id.clone();

        drag_shape(&mut e, Point::new(50.0, 50.0), Point::new(150.0, 150.0), 5);
        let shape_id = e.doc.ordered_live()[1].id.clone();
        let before = e.doc.element(&shape_id).unwrap().bounds();

        press(&mut e, "v", mods, 6);
        e.select([frame_id.clone()]);
        // Grab the frame by its border, away from the resize handles.
        {
            let fx = e.pointer_down(Point::new(300.0, 300.0), 0, mods, 7);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(340.0, 330.0), mods, 8);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(340.0, 330.0), mods, 9);
            pump(&mut e, &fx);
        }

        let after = e.doc.element(&shape_id).unwrap().bounds();
        assert!((after.x - before.x - 40.0).abs() < 0.01);
        assert!((after.y - before.y - 30.0).abs() < 0.01);
    }

    #[test]
    fn arrows_bind_to_frames_on_their_border() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "f", mods, 1);
        {
            let fx = e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(200.0, 150.0), mods, 3);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(200.0, 150.0), mods, 4);
            pump(&mut e, &fx);
        }
        let frame_a = e.doc.ordered_live()[0].id.clone();

        press(&mut e, "f", mods, 5);
        {
            let fx = e.pointer_down(Point::new(400.0, 0.0), 0, mods, 6);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(600.0, 150.0), mods, 7);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(600.0, 150.0), mods, 8);
            pump(&mut e, &fx);
        }
        let frame_b = e.doc.ordered_live()[1].id.clone();

        // Connect frame A's east border to frame B's west border.
        press(&mut e, "a", mods, 9);
        {
            let fx = e.pointer_down(Point::new(200.0, 75.0), 0, mods, 10);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(400.0, 75.0), mods, 11);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(400.0, 75.0), mods, 12);
            pump(&mut e, &fx);
        }

        let arrow_id = e
            .doc
            .ordered_live()
            .iter()
            .find(|element| matches!(element.kind, ElementKind::Arrow { .. }))
            .unwrap()
            .id
            .clone();
        let bindings = |element: &Element| match &element.kind {
            ElementKind::Arrow { start, end, .. } => (
                start.binding.as_ref().map(|b| b.element_id.clone()),
                end.binding.as_ref().map(|b| b.element_id.clone()),
            ),
            other => panic!("expected an arrow, got {other:?}"),
        };
        assert_eq!(
            bindings(e.doc.element(&arrow_id).unwrap()),
            (Some(frame_a.clone()), Some(frame_b.clone())),
            "both endpoints bind to the frames"
        );

        // Moving a frame drags the bound endpoint along.
        press(&mut e, "v", mods, 13);
        e.select([frame_a.clone()]);
        {
            let fx = e.pointer_down(Point::new(50.0, 150.0), 0, mods, 14);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(50.0, 200.0), mods, 15);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(50.0, 200.0), mods, 16);
            pump(&mut e, &fx);
        }
        let (start_y, end_y) = match &e.doc.element(&arrow_id).unwrap().kind {
            ElementKind::Arrow { start, end, .. } => (start.y, end.y),
            _ => unreachable!(),
        };
        assert!(
            (start_y - 125.0).abs() < 0.01,
            "start followed the frame down: {start_y}"
        );
        assert!((end_y - 75.0).abs() < 0.01, "end stayed put: {end_y}");
    }

    #[test]
    fn deleting_a_frame_deletes_children_in_one_undoable_step() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "f", mods, 1);
        {
            let fx = e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(400.0, 300.0), mods, 3);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(400.0, 300.0), mods, 4);
            pump(&mut e, &fx);
        }
        let frame_id = e.doc.ordered_live()[0].id.clone();
        drag_shape(&mut e, Point::new(50.0, 50.0), Point::new(150.0, 150.0), 5);
        assert_eq!(e.doc.ordered_live().len(), 2);

        press(&mut e, "v", mods, 6);
        e.select([frame_id]);
        press(&mut e, "Delete", mods, 7);
        assert!(e.doc.ordered_live().is_empty(), "children deleted too");

        let cmd = Modifiers {
            meta: true,
            ..Modifiers::default()
        };
        press(&mut e, "z", cmd, 8);
        assert_eq!(e.doc.ordered_live().len(), 2, "one undo restores all");
    }

    #[test]
    fn sticky_tool_creates_a_note_and_double_click_adds_text() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "n", mods, 1);
        {
            let fx = e.pointer_down(Point::new(200.0, 200.0), 0, mods, 2);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(200.0, 200.0), mods, 3);
            pump(&mut e, &fx);
        }

        assert_eq!(e.doc.ordered_live().len(), 1);
        let note = e.doc.ordered_live()[0].clone();
        assert!(note.is_sticky(), "the sticky tool draws a sticky note");
        assert_eq!(note.bounds(), Rect::new(130.0, 130.0, 140.0, 140.0));
        match &note.kind {
            ElementKind::Shape { style, .. } => {
                assert_eq!(style.fill_style, crate::model::FillPattern::Solid);
                assert_ne!(style.fill_color, "transparent");
                assert_eq!(style.stroke_color, STICKY_INK);
            }
            other => panic!("expected a sticky, got {other:?}"),
        }
        assert_eq!(e.tool, Tool::Select, "the sticky tool is single-shot");

        // Double-click creates the note's bound text, inked in the note colour.
        {
            let fx = e.double_click(Point::new(200.0, 200.0), 4);
            pump(&mut e, &fx);
        }
        let text_id = match &e.interaction {
            Interaction::TextEditing { id, .. } => id.clone(),
            other => panic!("expected the text editor, got {other:?}"),
        };
        {
            let fx = e.commit_text(&text_id, "hello", 5);
            pump(&mut e, &fx);
        }
        assert_eq!(e.doc.ordered_live().len(), 2);
        let text = e.doc.element(&text_id).unwrap();
        match &text.kind {
            ElementKind::Text {
                text,
                container_id,
                style,
                ..
            } => {
                assert_eq!(text, "hello");
                assert_eq!(container_id.as_ref(), Some(&note.id));
                assert_eq!(style.stroke_color, STICKY_INK);
            }
            other => panic!("expected bound text, got {other:?}"),
        }
    }

    #[test]
    fn sticky_tool_pulls_notes_from_a_stack_and_select_moves_the_stack() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "k", mods, 1);
        {
            let fx = e.pointer_down(Point::new(300.0, 300.0), 0, mods, 2);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(300.0, 300.0), mods, 3);
            pump(&mut e, &fx);
        }
        let stack = e.doc.ordered_live()[0].clone();
        assert!(stack.is_sticky_stack());
        assert_eq!(e.tool, Tool::Select, "placing a stack is single-shot");
        assert_eq!(stack.bounds(), Rect::new(230.0, 230.0, 140.0, 140.0));

        // N-drag from the pile pulls a note out; the pile stays.
        press(&mut e, "n", mods, 4);
        {
            let fx = e.pointer_down(Point::new(300.0, 300.0), 0, mods, 5);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(500.0, 320.0), mods, 6);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(500.0, 320.0), mods, 7);
            pump(&mut e, &fx);
        }
        assert_eq!(e.doc.ordered_live().len(), 2);
        let note = e
            .doc
            .ordered_live()
            .into_iter()
            .find(|element| element.is_sticky())
            .cloned()
            .expect("a note was pulled");
        assert_eq!(note.bounds().center(), Point::new(500.0, 320.0));
        assert_eq!(
            e.doc.element(&stack.id).unwrap().bounds(),
            stack.bounds(),
            "the pile did not move"
        );
        assert_eq!(e.tool, Tool::Select);

        // A click on the pile pulls nothing.
        press(&mut e, "n", mods, 8);
        {
            let fx = e.pointer_down(Point::new(300.0, 300.0), 0, mods, 9);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(300.0, 300.0), mods, 10);
            pump(&mut e, &fx);
        }
        assert_eq!(
            e.doc.ordered_live().len(),
            2,
            "clicking the pile is a no-op"
        );

        // Select-drag from the middle pulls a note too, the pile staying put.
        press(&mut e, "v", mods, 11);
        {
            let fx = e.pointer_down(Point::new(300.0, 300.0), 0, mods, 12);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(560.0, 300.0), mods, 13);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(560.0, 300.0), mods, 14);
            pump(&mut e, &fx);
        }
        assert_eq!(e.doc.ordered_live().len(), 3, "select pulled a note");
        assert_eq!(
            e.doc.element(&stack.id).unwrap().bounds(),
            stack.bounds(),
            "the pile did not move"
        );
        let second = e
            .doc
            .ordered_live()
            .into_iter()
            .filter(|element| element.is_sticky())
            .nth(1)
            .cloned()
            .expect("second note");
        assert_eq!(second.bounds().center(), Point::new(560.0, 300.0));

        // Dragging the border ring moves the pile, not a note.
        {
            let fx = e.pointer_down(Point::new(235.0, 300.0), 0, mods, 15);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(265.0, 360.0), mods, 16);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(265.0, 360.0), mods, 17);
            pump(&mut e, &fx);
        }
        assert_eq!(
            e.doc.element(&stack.id).unwrap().bounds(),
            Rect::new(260.0, 290.0, 140.0, 140.0)
        );
        assert_eq!(
            e.doc.element(&note.id).unwrap().bounds().center(),
            Point::new(500.0, 320.0),
            "the pulled note stayed put"
        );
        assert_eq!(e.doc.ordered_live().len(), 3, "moving made no new note");
    }

    #[test]
    fn text_tool_on_a_shape_creates_a_centred_label() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(200.0, 100.0), 1);
        let shape = e.doc.ordered_live()[0].id.clone();
        let mods = Modifiers::default();

        // Click well off-centre: the label still lands in the middle.
        press(&mut e, "t", mods, 2);
        {
            let fx = e.pointer_down(Point::new(60.0, 40.0), 0, mods, 3);
            pump(&mut e, &fx);
        }
        let text_id = match &e.interaction {
            Interaction::TextEditing { id, .. } => id.clone(),
            other => panic!("expected the text editor, got {other:?}"),
        };
        {
            let fx = e.commit_text(&text_id, "label", 4);
            pump(&mut e, &fx);
        }

        let text = e.doc.element(&text_id).unwrap();
        match &text.kind {
            ElementKind::Text {
                container_id,
                align,
                ..
            } => {
                assert_eq!(container_id.as_ref(), Some(&shape));
                assert_eq!(*align, TextAlign::Center);
            }
            other => panic!("expected bound text, got {other:?}"),
        }
        let bounds = text.bounds();
        let shape_bounds = e.doc.element(&shape).unwrap().bounds();
        assert!(
            (bounds.center().x - shape_bounds.center().x).abs() < 1.0,
            "the label is centred in the shape"
        );
    }

    #[test]
    fn marquee_enclosing_an_empty_frame_selects_the_frame() {
        for (start, end) in [
            (Point::new(-20.0, -20.0), Point::new(420.0, 320.0)),
            (Point::new(420.0, 320.0), Point::new(-20.0, -20.0)),
        ] {
            let mut e = editor();
            let mods = Modifiers::default();
            press(&mut e, "f", mods, 1);
            {
                let fx = e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
                pump(&mut e, &fx);
            }
            {
                let fx = e.pointer_move(Point::new(400.0, 300.0), mods, 3);
                pump(&mut e, &fx);
            }
            {
                let fx = e.pointer_up(Point::new(400.0, 300.0), mods, 4);
                pump(&mut e, &fx);
            }
            let frame_id = e.doc.ordered_live()[0].id.clone();
            press(&mut e, "v", mods, 5);
            e.clear_selection();
            {
                let fx = e.pointer_down(start, 0, mods, 6);
                pump(&mut e, &fx);
            }
            {
                let fx = e.pointer_move(end, mods, 7);
                pump(&mut e, &fx);
            }
            {
                let fx = e.pointer_up(end, mods, 8);
                pump(&mut e, &fx);
            }
            assert!(
                e.selection.contains(&frame_id),
                "a selection rectangle enclosing a frame must select it"
            );
        }
    }

    #[test]
    fn marquee_inside_a_frame_selects_contents_not_the_frame() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "f", mods, 1);
        {
            let fx = e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(400.0, 300.0), mods, 3);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(400.0, 300.0), mods, 4);
            pump(&mut e, &fx);
        }
        let frame_id = e.doc.ordered_live()[0].id.clone();
        drag_shape(&mut e, Point::new(50.0, 50.0), Point::new(150.0, 150.0), 5);
        let shape_id = e.doc.ordered_live()[1].id.clone();

        press(&mut e, "v", mods, 6);
        e.clear_selection();
        {
            let fx = e.pointer_down(Point::new(20.0, 20.0), 0, mods, 7);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(300.0, 200.0), mods, 8);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(300.0, 200.0), mods, 9);
            pump(&mut e, &fx);
        }

        assert_eq!(e.selection.len(), 1);
        assert!(e.selection.contains(&shape_id));
        assert!(
            !e.selection.contains(&frame_id),
            "a marquee inside the frame does not grab the frame"
        );
    }

    #[test]
    fn text_committed_inside_a_frame_becomes_its_child() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "f", mods, 1);
        {
            let fx = e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(400.0, 300.0), mods, 3);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(400.0, 300.0), mods, 4);
            pump(&mut e, &fx);
        }
        let frame_id = e.doc.ordered_live()[0].id.clone();

        press(&mut e, "t", mods, 5);
        {
            let fx = e.pointer_down(Point::new(200.0, 150.0), 0, mods, 6);
            pump(&mut e, &fx);
        }
        let text_id = match &e.interaction {
            Interaction::TextEditing { id, .. } => id.clone(),
            other => panic!("expected the text editor, got {other:?}"),
        };
        {
            let fx = e.commit_text(&text_id, "inside", 7);
            pump(&mut e, &fx);
        }
        assert_eq!(
            e.doc.element(&text_id).unwrap().frame_id.as_ref(),
            Some(&frame_id),
            "text dropped inside a frame is owned by it"
        );

        let before = e.doc.element(&text_id).unwrap().bounds();
        press(&mut e, "v", mods, 8);
        e.select([frame_id.clone()]);
        {
            let fx = e.pointer_down(Point::new(100.0, 300.0), 0, mods, 9);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(140.0, 340.0), mods, 10);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(140.0, 340.0), mods, 11);
            pump(&mut e, &fx);
        }
        let after = e.doc.element(&text_id).unwrap().bounds();
        assert!(
            (after.x - before.x - 40.0).abs() < 0.01,
            "the frame carried the text"
        );
    }

    #[test]
    fn dragging_from_a_connection_node_draws_a_bound_arrow() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 50.0), 1);
        drag_shape(&mut e, Point::new(300.0, 0.0), Point::new(400.0, 50.0), 2);
        let a = e.doc.ordered_live()[0].id.clone();
        let b = e.doc.ordered_live()[1].id.clone();
        let mods = Modifiers::default();
        press(&mut e, "v", mods, 3);
        e.clear_selection();

        // Approaching the east node of A reveals the nodes.
        {
            let fx = e.pointer_move(Point::new(112.0, 25.0), mods, 4);
            pump(&mut e, &fx);
        }
        assert!(
            !e.hover_anchor_points().is_empty(),
            "connection nodes appear on approach"
        );

        // Dragging from the node starts an arrow bound to that side.
        {
            let fx = e.pointer_down(Point::new(112.0, 25.0), 0, mods, 5);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(288.0, 25.0), mods, 6);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(288.0, 25.0), mods, 7);
            pump(&mut e, &fx);
        }

        let arrow = e
            .doc
            .ordered_live()
            .into_iter()
            .find(|element| matches!(element.kind, ElementKind::Arrow { .. }))
            .cloned()
            .expect("an arrow was drawn");
        let bindings = match &arrow.kind {
            ElementKind::Arrow { start, end, .. } => (
                start.binding.as_ref().map(|b| b.element_id.clone()),
                end.binding.as_ref().map(|b| b.element_id.clone()),
            ),
            other => panic!("expected an arrow, got {other:?}"),
        };
        assert_eq!(bindings, (Some(a.clone()), Some(b.clone())));
        assert_eq!(
            e.selection.iter().next(),
            Some(&arrow.id),
            "the new arrow is selected"
        );
        assert_eq!(e.tool, Tool::Select);

        // The start followed the source shape when it moved.
        e.select([a.clone()]);
        {
            let fx = e.pointer_down(Point::new(50.0, 25.0), 0, mods, 8);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(50.0, 75.0), mods, 9);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(50.0, 75.0), mods, 10);
            pump(&mut e, &fx);
        }
        let start_y = match &e.doc.element(&arrow.id).unwrap().kind {
            ElementKind::Arrow { start, .. } => start.y,
            _ => unreachable!(),
        };
        assert!((start_y - 75.0).abs() < 0.01, "start followed: {start_y}");
    }

    #[test]
    fn duplicating_a_frame_duplicates_its_children() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "f", mods, 1);
        {
            let fx = e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(400.0, 300.0), mods, 3);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(400.0, 300.0), mods, 4);
            pump(&mut e, &fx);
        }
        let frame_id = e.doc.ordered_live()[0].id.clone();
        drag_shape(&mut e, Point::new(50.0, 50.0), Point::new(150.0, 150.0), 5);

        press(&mut e, "v", mods, 6);
        e.select([frame_id.clone()]);
        let cmd = Modifiers {
            meta: true,
            ..Modifiers::default()
        };
        press(&mut e, "d", cmd, 7);
        assert_eq!(e.doc.ordered_live().len(), 4, "frame + child duplicated");

        let copy_frame = e
            .doc
            .ordered_live()
            .into_iter()
            .filter(|element| element.is_frame())
            .map(|element| element.id.clone())
            .find(|id| id != &frame_id)
            .expect("duplicated frame");
        let copy_child = e
            .doc
            .ordered_live()
            .into_iter()
            .find(|element| element.frame_id.as_ref() == Some(&copy_frame));
        assert!(copy_child.is_some(), "copy's child points at the copy");
    }

    #[test]
    fn frame_names_are_edited_by_double_click() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "f", mods, 1);
        {
            let fx = e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(400.0, 300.0), mods, 3);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(400.0, 300.0), mods, 4);
            pump(&mut e, &fx);
        }
        let frame_id = e.doc.ordered_live()[0].id.clone();

        press(&mut e, "v", mods, 5);
        {
            let fx = e.double_click(Point::new(100.0, -8.0), 6);
            pump(&mut e, &fx);
        }
        {
            let fx = e.commit_text(&frame_id, "us-central1", 7);
            pump(&mut e, &fx);
        }
        match &e.doc.element(&frame_id).unwrap().kind {
            ElementKind::Frame { name, .. } => assert_eq!(name, "us-central1"),
            other => panic!("expected frame, got {other:?}"),
        }
    }

    #[test]
    fn drawing_with_a_single_shot_tool_returns_to_select() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "r", mods, 1);
        let id = {
            {
                let fx = e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
                pump(&mut e, &fx);
            }
            {
                let fx = e.pointer_move(Point::new(100.0, 60.0), mods, 3);
                pump(&mut e, &fx);
            }
            {
                let fx = e.pointer_up(Point::new(100.0, 60.0), mods, 4);
                pump(&mut e, &fx);
            }
            let live = e.doc.ordered_live();
            assert_eq!(live.len(), 1);
            live[0].id.clone()
        };
        assert_eq!(e.tool, Tool::Select, "the draw tool is single-shot");
        assert_eq!(e.selection.iter().next(), Some(&id));

        // The placed shape is selectable after deselecting.
        e.clear_selection();
        {
            let fx = e.pointer_down(Point::new(50.0, 30.0), 0, mods, 5);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(50.0, 30.0), mods, 6);
            pump(&mut e, &fx);
        }
        assert_eq!(e.selection.len(), 1, "clicking the drawn shape selects it");
    }

    #[test]
    fn pressing_the_active_tool_shortcut_locks_it() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "r", mods, 1);
        press(&mut e, "r", mods, 2);
        assert!(e.tool_locked, "second press locks the tool");
        assert_eq!(e.tool, Tool::Rectangle);

        for (i, start) in [0.0_f64, 200.0].iter().enumerate() {
            let now = 10 + i as u64 * 10;
            {
                let fx = e.pointer_down(Point::new(*start, 0.0), 0, mods, now);
                pump(&mut e, &fx);
            }
            {
                let fx = e.pointer_move(Point::new(start + 80.0, 60.0), mods, now + 1);
                pump(&mut e, &fx);
            }
            {
                let fx = e.pointer_up(Point::new(start + 80.0, 60.0), mods, now + 2);
                pump(&mut e, &fx);
            }
        }
        assert_eq!(e.doc.ordered_live().len(), 2, "locked tool keeps drawing");
        assert_eq!(e.tool, Tool::Rectangle);

        let cmd = Modifiers {
            meta: true,
            ..Modifiers::default()
        };
        let _ = press(&mut e, "z", cmd, 99);
        let _ = cmd;
        press(&mut e, "v", mods, 100);
        assert_eq!(e.tool, Tool::Select);
        assert!(!e.tool_locked);
    }

    #[test]
    fn placing_text_returns_to_select_but_editing_does_not() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "t", mods, 1);
        {
            let fx = e.pointer_down(Point::new(50.0, 50.0), 0, mods, 2);
            pump(&mut e, &fx);
        }
        let id = e.doc.ordered_live()[0].id.clone();
        {
            let fx = e.commit_text(&id, "hello", 3);
            pump(&mut e, &fx);
        }
        assert_eq!(e.tool, Tool::Select, "new text is single-shot");

        press(&mut e, "t", mods, 4);
        let effects = e.pointer_down(Point::new(55.0, 55.0), 0, mods, 5);
        assert!(!effects.actions.is_empty(), "editing opens the overlay");
        {
            let fx = e.commit_text(&id, "hello again", 6);
            pump(&mut e, &fx);
        }
        assert_eq!(e.tool, Tool::Text, "editing existing text keeps the tool");
    }

    #[test]
    fn clicking_a_caption_selects_and_moves_the_component() {
        let mut e = editor();
        e.set_viewport(800.0, 600.0);
        {
            let fx = e.insert_glyph("cloud_storage", 1);
            pump(&mut e, &fx);
        }
        let glyph = e
            .doc
            .ordered_live()
            .into_iter()
            .find(|element| matches!(element.kind, ElementKind::Glyph { .. }))
            .expect("glyph")
            .clone();
        let caption_centre = e
            .doc
            .ordered_live()
            .into_iter()
            .find(|element| element.is_text())
            .expect("caption")
            .bounds()
            .center();

        press(&mut e, "v", Modifiers::default(), 2);
        let mods = Modifiers::default();
        {
            let fx = e.pointer_down(caption_centre, 0, mods, 3);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(caption_centre, mods, 4);
            pump(&mut e, &fx);
        }
        assert_eq!(
            e.selection.iter().next().map(String::as_str),
            Some(glyph.id.as_str()),
            "clicking the caption selects the glyph"
        );

        let before = glyph.bounds();
        {
            let fx = e.pointer_down(caption_centre, 0, mods, 5);
            pump(&mut e, &fx);
        }
        e.pointer_move(
            Point::new(caption_centre.x + 40.0, caption_centre.y),
            mods,
            6,
        );
        e.pointer_up(
            Point::new(caption_centre.x + 40.0, caption_centre.y),
            mods,
            7,
        );
        let after = e.doc.element(&glyph.id).unwrap().bounds();
        assert!((after.x - before.x - 40.0).abs() < 0.01);
    }

    #[test]
    fn erasing_a_caption_removes_the_component() {
        let mut e = editor();
        e.set_viewport(800.0, 600.0);
        {
            let fx = e.insert_glyph("cloud_storage", 1);
            pump(&mut e, &fx);
        }
        let caption_centre = e
            .doc
            .ordered_live()
            .into_iter()
            .find(|element| element.is_text())
            .expect("caption")
            .bounds()
            .center();

        press(&mut e, "e", Modifiers::default(), 2);
        let mods = Modifiers::default();
        {
            let fx = e.pointer_down(caption_centre, 0, mods, 3);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(caption_centre, mods, 4);
            pump(&mut e, &fx);
        }
        assert_eq!(e.doc.ordered_live().len(), 0, "glyph and caption both go");
    }

    #[test]
    fn text_tool_click_on_existing_text_edits_instead_of_stacking() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "t", mods, 1);
        {
            let fx = e.pointer_down(Point::new(50.0, 50.0), 0, mods, 2);
            pump(&mut e, &fx);
        }
        let id = e.doc.ordered_live()[0].id.clone();
        {
            let fx = e.commit_text(&id, "hello", 3);
            pump(&mut e, &fx);
        }
        assert_eq!(e.doc.ordered_live().len(), 1);
        assert_eq!(e.tool, Tool::Select, "placing text is single-shot");

        press(&mut e, "t", mods, 4);
        {
            let fx = e.pointer_down(Point::new(55.0, 55.0), 0, mods, 5);
            pump(&mut e, &fx);
        }
        assert_eq!(
            e.doc.ordered_live().len(),
            1,
            "clicking existing text does not stack a new element"
        );
        {
            let fx = e.commit_text(&id, "hello world", 5);
            pump(&mut e, &fx);
        }
        assert_eq!(e.doc.ordered_live().len(), 1);
        match &e.doc.element(&id).unwrap().kind {
            ElementKind::Text { text, .. } => assert_eq!(text, "hello world"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn inserting_a_glyph_creates_it_with_a_linked_caption() {
        let mut e = editor();
        e.set_viewport(800.0, 600.0);
        let effects = e.insert_glyph("cloud_run", 1);
        assert_eq!(e.doc.ordered_live().len(), 2, "glyph + caption");
        assert_eq!(effects.changed.len(), 2);

        let glyph = e
            .doc
            .ordered_live()
            .into_iter()
            .find(|element| matches!(element.kind, ElementKind::Glyph { .. }))
            .expect("glyph")
            .clone();
        let caption = e
            .doc
            .ordered_live()
            .into_iter()
            .find(|element| element.is_text())
            .expect("caption")
            .clone();

        match &glyph.kind {
            ElementKind::Glyph {
                glyph, caption_id, ..
            } => {
                assert_eq!(glyph, "cloud_run");
                assert_eq!(caption_id.as_ref(), Some(&caption.id));
            }
            other => panic!("expected glyph, got {other:?}"),
        }
        match &caption.kind {
            ElementKind::Text {
                text,
                container_id,
                align,
                ..
            } => {
                assert_eq!(text, "Cloud Run");
                assert_eq!(container_id.as_ref(), Some(&glyph.id));
                assert_eq!(*align, TextAlign::Center);
            }
            other => panic!("expected text, got {other:?}"),
        }
        assert!(
            caption.bounds().y >= glyph.bounds().max_y(),
            "caption sits beneath the glyph"
        );

        // The caption follows the glyph when it moves.
        let before = caption.bounds();
        e.select([glyph.id.clone()]);
        let mods = Modifiers::default();
        let center = glyph.bounds().center();
        {
            let fx = e.pointer_down(center, 0, mods, 2);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(center.x + 60.0, center.y), mods, 3);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(center.x + 60.0, center.y), mods, 4);
            pump(&mut e, &fx);
        }
        let after = e.doc.element(&caption.id).unwrap().bounds();
        assert!((after.x - before.x - 60.0).abs() < 0.01);
    }

    #[test]
    fn every_library_glyph_renders_ops() {
        use crate::scene::Scene;
        use crate::text::ApproxMeasurer;
        let mut doc = Document::new();
        for info in crate::glyphs::catalog() {
            let element = Element::new(
                ElementKind::Glyph {
                    glyph: info.id.to_string(),
                    rect: Rect::new(0.0, 0.0, 96.0, 96.0),
                    style: Style::default(),
                    caption_id: None,
                },
                "a0".into(),
                0,
            );
            let ops = crate::rough::build_element_ops(
                &element,
                &doc,
                &ApproxMeasurer,
                &crate::spatial::ObstacleIndex::new(),
            );
            assert!(!ops.is_empty(), "{} renders nothing", info.id);
            doc.insert(element);
        }
        let mut scene = Scene::new();
        scene.rebuild_all(&doc, &ApproxMeasurer, &crate::spatial::ObstacleIndex::new());
        assert_eq!(scene.layers.len(), crate::glyphs::catalog().len());
    }

    #[test]
    fn switching_tools_mid_drag_abandons_the_draft() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "r", mods, 1);
        {
            let fx = e.pointer_down(Point::new(10.0, 10.0), 0, mods, 2);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(80.0, 60.0), mods, 3);
            pump(&mut e, &fx);
        }
        press(&mut e, "v", mods, 4);
        {
            let fx = e.pointer_up(Point::new(80.0, 60.0), mods, 5);
            pump(&mut e, &fx);
        }
        assert!(e.doc.is_empty(), "tool switch committed a half-drawn shape");
        assert_eq!(e.tool, Tool::Select);
        // The gesture is really over: the next drag uses the new tool.
        {
            let fx = e.pointer_down(Point::new(300.0, 300.0), 0, mods, 6);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(320.0, 320.0), mods, 7);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(320.0, 320.0), mods, 8);
            pump(&mut e, &fx);
        }
        assert!(e.doc.is_empty());
    }

    #[test]
    fn empty_bound_text_commit_leaves_no_dangling_link() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 50.0), 1);
        e.set_tool(Tool::Select);
        {
            let fx = e.double_click(Point::new(50.0, 25.0), 4);
            pump(&mut e, &fx);
        }
        let text_id = e
            .doc
            .ordered_live()
            .iter()
            .find(|element| element.is_text())
            .map(|element| element.id.clone())
            .expect("double-click did not open a bound text");
        {
            let fx = e.commit_text(&text_id, "   ", 5);
            pump(&mut e, &fx);
        }
        // The draft is gone entirely and the shape carries no link.
        assert!(e.doc.element(&text_id).is_none());
        assert_eq!(e.doc.ordered_live().len(), 1);
        let ElementKind::Shape { bound_text_id, .. } = &e.doc.ordered_live()[0].kind else {
            panic!("expected a shape")
        };
        assert!(
            bound_text_id.is_none(),
            "empty commit left a dangling bound_text_id"
        );
        // Re-labeling after the abandoned edit works.
        {
            let fx = e.double_click(Point::new(50.0, 25.0), 6);
            pump(&mut e, &fx);
        }
        let fresh = e
            .doc
            .ordered_live()
            .iter()
            .find(|element| element.is_text())
            .map(|element| element.id.clone())
            .expect("second edit did not open a bound text");
        {
            let fx = e.commit_text(&fresh, "real", 7);
            pump(&mut e, &fx);
        }
        let ElementKind::Shape { bound_text_id, .. } = &e.doc.ordered_live()[0].kind else {
            panic!("expected a shape")
        };
        assert_eq!(bound_text_id.as_deref(), Some(fresh.as_str()));
        // Undo removes the label, then the shape — history stayed linear.
        let cmd = Modifiers {
            meta: true,
            ..Modifiers::default()
        };
        press(&mut e, "z", cmd, 8);
        assert_eq!(e.doc.ordered_live().len(), 1, "undo ate the shape");
        press(&mut e, "z", cmd, 9);
        assert!(e.doc.is_empty());
    }

    #[test]
    fn a_dangling_bound_text_id_heals_on_edit() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 50.0), 1);
        let shape_id = e.doc.ordered_live()[0].id.clone();
        if let ElementKind::Shape { bound_text_id, .. } =
            &mut e.doc.element_mut(&shape_id).unwrap().kind
        {
            *bound_text_id = Some("ghost".into());
        }
        {
            let fx = e.double_click(Point::new(50.0, 25.0), 4);
            pump(&mut e, &fx);
        }
        let fresh = e
            .doc
            .ordered_live()
            .iter()
            .find(|element| element.is_text())
            .map(|element| element.id.clone())
            .expect("no fresh bound text opened");
        {
            let fx = e.commit_text(&fresh, "healed", 5);
            pump(&mut e, &fx);
        }
        let ElementKind::Shape { bound_text_id, .. } = &e.doc.element(&shape_id).unwrap().kind
        else {
            panic!("expected a shape")
        };
        assert_eq!(bound_text_id.as_deref(), Some(fresh.as_str()));
    }

    #[test]
    fn nested_frames_cascade_through_parentage() {
        let mut e = editor();
        let mods = Modifiers::default();
        e.set_tool(Tool::Frame);
        {
            let fx = e.pointer_down(Point::new(0.0, 0.0), 0, mods, 1);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(400.0, 300.0), mods, 2);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(400.0, 300.0), mods, 3);
            pump(&mut e, &fx);
        }
        e.set_tool(Tool::Frame);
        {
            let fx = e.pointer_down(Point::new(50.0, 50.0), 0, mods, 4);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(200.0, 150.0), mods, 5);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(200.0, 150.0), mods, 6);
            pump(&mut e, &fx);
        }
        e.set_tool(Tool::Rectangle);
        {
            let fx = e.pointer_down(Point::new(80.0, 80.0), 0, mods, 7);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(140.0, 120.0), mods, 8);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(140.0, 120.0), mods, 9);
            pump(&mut e, &fx);
        }

        let outer = e.doc.ordered_live()[0].id.clone();
        let inner = e.doc.ordered_live()[1].id.clone();
        assert_eq!(
            e.doc.element(&inner).unwrap().frame_id.as_deref(),
            Some(outer.as_str()),
            "inner frame was not adopted by the outer frame"
        );
        e.select([outer.clone()]);
        {
            let fx = e.delete_selection(10);
            pump(&mut e, &fx);
        }
        assert_eq!(
            e.doc.live_count(),
            0,
            "nested contents survived the parent delete"
        );
        let cmd = Modifiers {
            meta: true,
            ..Modifiers::default()
        };
        press(&mut e, "z", cmd, 11);
        assert_eq!(e.doc.live_count(), 3);
    }

    #[test]
    fn arrow_endpoints_bind_shapes_not_their_labels() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(160.0, 100.0), 1);
        let shape_id = e.doc.ordered_live()[0].id.clone();
        {
            let fx = e.double_click(Point::new(80.0, 50.0), 2);
            pump(&mut e, &fx);
        }
        let text_id = e
            .doc
            .ordered_live()
            .iter()
            .find(|element| element.is_text())
            .expect("bound text created")
            .id
            .clone();
        {
            let fx = e.commit_text(&text_id, "hub", 3);
            pump(&mut e, &fx);
        }

        drag_shape(&mut e, Point::new(300.0, 0.0), Point::new(400.0, 100.0), 4);
        let mods = Modifiers::default();
        press(&mut e, "a", mods, 5);
        {
            let fx = e.pointer_down(Point::new(350.0, 50.0), 0, mods, 6);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(80.0, 50.0), mods, 7);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(80.0, 50.0), mods, 8);
            pump(&mut e, &fx);
        }

        let arrow = e
            .doc
            .ordered_live()
            .into_iter()
            .find(|element| matches!(element.kind, ElementKind::Arrow { .. }))
            .expect("arrow created");
        match &arrow.kind {
            ElementKind::Arrow { end, .. } => {
                assert_eq!(
                    end.binding.as_ref().map(|b| b.element_id.clone()),
                    Some(shape_id),
                    "dropping on the label bound the text, not its shape"
                );
            }
            other => panic!("expected arrow, got {other:?}"),
        }
    }

    #[test]
    fn body_dropped_arrows_reanchor_toward_the_target() {
        let mut e = editor();
        // Wide source, destination straight up: a frozen centre-drop anchor
        // picks a vertical side and the route slices back through the shape.
        drag_shape(&mut e, Point::new(0.0, 200.0), Point::new(160.0, 300.0), 1);
        drag_shape(&mut e, Point::new(30.0, 0.0), Point::new(130.0, 80.0), 2);
        let mods = Modifiers::default();
        press(&mut e, "a", mods, 3);
        {
            let fx = e.pointer_down(Point::new(80.0, 250.0), 0, mods, 4);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(80.0, 40.0), mods, 5);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(80.0, 40.0), mods, 6);
            pump(&mut e, &fx);
        }

        let arrow = e
            .doc
            .ordered_live()
            .into_iter()
            .find(|element| matches!(element.kind, ElementKind::Arrow { .. }))
            .expect("arrow created");
        let ElementKind::Arrow {
            start,
            end,
            routing,
            waypoints,
            ..
        } = &arrow.kind
        else {
            unreachable!()
        };
        assert_eq!(
            start.binding.as_ref().and_then(|b| b.focus),
            None,
            "body drops keep dynamic anchoring"
        );
        let route = connector::route(start, end, *routing, waypoints, &e.doc);
        let source = Rect::new(0.0, 200.0, 160.0, 100.0);
        let interior = route
            .iter()
            .filter(|p| {
                p.x > source.x + 0.5
                    && p.x < source.max_x() - 0.5
                    && p.y > source.y + 0.5
                    && p.y < source.max_y() - 0.5
            })
            .count();
        assert_eq!(
            interior, 0,
            "route passes through the bound shape's body: {route:?}"
        );
    }

    #[test]
    fn arrow_labels_follow_when_bound_shapes_move() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 100.0), 1);
        drag_shape(&mut e, Point::new(300.0, 0.0), Point::new(400.0, 100.0), 2);
        let b = e.doc.ordered_live()[1].id.clone();
        let mods = Modifiers::default();
        press(&mut e, "a", mods, 3);
        {
            let fx = e.pointer_down(Point::new(50.0, 50.0), 0, mods, 4);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(350.0, 50.0), mods, 5);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(350.0, 50.0), mods, 6);
            pump(&mut e, &fx);
        }
        {
            let fx = e.double_click(Point::new(200.0, 50.0), 7);
            pump(&mut e, &fx);
        }
        let text_id = e
            .doc
            .ordered_live()
            .iter()
            .find(|element| element.is_text())
            .expect("label created")
            .id
            .clone();
        {
            let fx = e.commit_text(&text_id, "sync", 8);
            pump(&mut e, &fx);
        }
        let before = e.doc.element(&text_id).unwrap().bounds();

        press(&mut e, "v", mods, 9);
        e.select([b.clone()]);
        {
            let fx = e.pointer_down(Point::new(350.0, 50.0), 0, mods, 10);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(350.0, 250.0), mods, 11);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(350.0, 250.0), mods, 12);
            pump(&mut e, &fx);
        }

        let after = e.doc.element(&text_id).unwrap().bounds();
        assert!(
            (after.center().y - before.center().y).abs() > 50.0,
            "label did not follow the re-routed connector: {before:?} -> {after:?}"
        );
    }

    #[test]
    fn nudge_moves_bound_text_and_reflows_arrow_labels() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 100.0), 1);
        let shape_id = e.doc.ordered_live()[0].id.clone();
        {
            let fx = e.double_click(Point::new(50.0, 50.0), 2);
            pump(&mut e, &fx);
        }
        let text_id = e
            .doc
            .ordered_live()
            .iter()
            .find(|element| element.is_text())
            .expect("bound text created")
            .id
            .clone();
        {
            let fx = e.commit_text(&text_id, "node", 3);
            pump(&mut e, &fx);
        }
        let label_before = e.doc.element(&text_id).unwrap().bounds();

        let mods = Modifiers::default();
        e.select([shape_id.clone()]);
        press(&mut e, "ArrowRight", mods, 4);
        let label_after = e.doc.element(&text_id).unwrap().bounds();
        assert!(
            label_after.x > label_before.x,
            "bound text stayed behind on nudge: {label_before:?} -> {label_after:?}"
        );
    }

    #[test]
    fn sticky_stack_inside_a_frame_belongs_to_it() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "f", mods, 1);
        {
            let fx = e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_move(Point::new(400.0, 400.0), mods, 3);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(400.0, 400.0), mods, 4);
            pump(&mut e, &fx);
        }
        let frame_id = e.doc.ordered_live()[0].id.clone();

        press(&mut e, "k", mods, 5);
        {
            let fx = e.pointer_down(Point::new(100.0, 100.0), 0, mods, 6);
            pump(&mut e, &fx);
        }
        {
            let fx = e.pointer_up(Point::new(100.0, 100.0), mods, 7);
            pump(&mut e, &fx);
        }
        let stack = e
            .doc
            .ordered_live()
            .into_iter()
            .find(|element| element.is_sticky_stack())
            .expect("stack created");
        assert_eq!(
            stack.frame_id.as_deref(),
            Some(frame_id.as_str()),
            "stack placed in a frame was not adopted"
        );
    }
}
