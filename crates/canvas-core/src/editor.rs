//! The editor: the interaction state machine over a Document.
//!
//! Owns the document, camera, scene cache, history, tool, style defaults and
//! selection. Everything here is browser-free; the wasm façade only forwards
//! input and paints the effects.

use std::collections::{BTreeSet, HashMap};

use crate::camera::Camera;
use crate::connector;
use crate::delta::Delta;
use crate::geometry::{Point, Rect};
use crate::history::{History, Transaction};
use crate::hit::{elements_in_rect, hit_test, hit_test_raw};
use crate::id::{new_element_id, new_version_nonce};
use crate::model::{
    Binding, Document, Element, ElementId, ElementKind, Endpoint, Routing, ShapeKind, Style,
    StylePatch, TextAlign,
};
use crate::nav::NavigationState;
use crate::scene::Scene;
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
    },
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
    /// Shell actions to dispatch (text editor, later: dialogs).
    pub actions: Vec<ShellAction>,
}

impl Effects {
    fn changed(id: impl Into<ElementId>) -> Self {
        let mut effects = Self::default();
        effects.changed.insert(id.into());
        effects
    }

    fn merge(&mut self, other: Effects) {
        self.changed.extend(other.changed);
        self.repaint_all |= other.repaint_all;
        self.overlay_changed |= other.overlay_changed;
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
    pub history: History,
    pub nav: NavigationState,
    pub tool: Tool,
    pub tool_locked: bool,
    pub style: Style,
    pub selection: BTreeSet<ElementId>,
    interaction: Interaction,
    tx: Option<Transaction>,
    marquee_current: Option<Point>,
    clipboard: Vec<Element>,
    last_pressure: f64,
    measurer: Box<dyn TextMeasurer>,
    hover_anchors: Option<(ElementId, Vec<Point>)>,
    viewport_size: (f64, f64),
}

impl Editor {
    pub fn new(doc: Document) -> Self {
        let mut editor = Self {
            doc,
            camera: Camera::default(),
            scene: Scene::new(),
            history: History::new(),
            nav: NavigationState::new(),
            tool: Tool::default(),
            tool_locked: false,
            style: Style::default(),
            selection: BTreeSet::new(),
            interaction: Interaction::Idle,
            tx: None,
            marquee_current: None,
            clipboard: Vec::new(),
            last_pressure: 0.5,
            measurer: Box::new(ApproxMeasurer),
            hover_anchors: None,
            viewport_size: (800.0, 600.0),
        };
        editor.refresh_scene();
        editor
    }

    /// The shell reports its viewport size (used for insert-at-centre).
    pub fn set_viewport(&mut self, width: f64, height: f64) {
        self.viewport_size = (width.max(1.0), height.max(1.0));
    }

    /// Select a tool (single-shot: reverts to Select after one use).
    pub fn set_tool(&mut self, tool: Tool) {
        self.tool = tool;
        self.tool_locked = false;
    }

    /// Select a tool and keep it armed for repeated use.
    pub fn set_tool_locked(&mut self, tool: Tool) {
        self.tool = tool;
        self.tool_locked = true;
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
        self.refresh_scene();
        Effects {
            repaint_all: true,
            overlay_changed: true,
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
        self.scene.rebuild_all(&self.doc, self.measurer.as_ref())
    }

    /// Rebuild the changed elements' layers, returning dirty world rects.
    pub fn rebuild_changed(&mut self, changed: &BTreeSet<ElementId>) -> Vec<Rect> {
        self.scene
            .update(&self.doc, changed, self.measurer.as_ref())
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

    // -- selection ---------------------------------------------------------

    pub fn select(&mut self, ids: impl IntoIterator<Item = ElementId>) -> Effects {
        self.selection = ids.into_iter().collect();
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
        Effects {
            overlay_changed: true,
            ..Effects::default()
        }
    }

    pub fn select_all(&mut self) -> Effects {
        let ids = self
            .doc
            .ordered_live()
            .iter()
            .filter(|e| !e.is_frame())
            .map(|e| e.id.clone())
            .collect::<Vec<_>>();
        let ids = if ids.is_empty() {
            self.doc
                .ordered_live()
                .iter()
                .map(|e| e.id.clone())
                .collect()
        } else {
            ids
        };
        self.select(ids)
    }

    /// Bounds of the current selection, in world space.
    pub fn selection_bounds(&self) -> Option<Rect> {
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
        if let Interaction::TextEditing { id, .. } = self.interaction.clone() {
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
        match self.tool {
            Tool::Select => self.begin_select(screen, world, mods),
            Tool::Rectangle | Tool::Diamond | Tool::Ellipse => self.begin_drawing(world, now),
            Tool::Frame => self.begin_frame(world, now),
            Tool::Freehand => self.begin_freehand(world, now),
            Tool::Line => self.begin_line(world, now),
            Tool::Arrow => self.begin_connector(world, now),
            Tool::Eraser => self.begin_erase(world, now),
            Tool::Text => self.begin_text_tool(world, now),
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
        match self.interaction.clone() {
            Interaction::Idle => Effects::default(),
            Interaction::FrameNameEditing { .. } => Effects::default(),
            Interaction::Drawing { id, start } => {
                let current = self.camera.screen_to_world(screen);
                let rect = self.drawn_rect(start, current, mods);
                if let Some(tx) = self.tx.as_mut() {
                    if let Err(err) =
                        tx.update(&mut self.doc, &id, now, |element| match &mut element.kind {
                            ElementKind::Shape { rect: r, .. }
                            | ElementKind::Frame { rect: r, .. } => *r = rect,
                            _ => {}
                        })
                    {
                        debug_assert!(false, "drawing update failed: {err}");
                    }
                }
                Effects::changed(id)
            }
            Interaction::Marquee { start: _ } => {
                self.marquee_current = Some(self.camera.screen_to_world(screen));
                let _ = mods;
                Effects {
                    overlay_changed: true,
                    ..Effects::default()
                }
            }
            Interaction::Moving { start, originals } => {
                let current = self.camera.screen_to_world(screen);
                let (dx, dy) = (current.x - start.x, current.y - start.y);
                let mut effects = Effects::default();
                for original in &originals {
                    let mut translated = original.clone();
                    translate_element(&mut translated, dx, dy);
                    let kind = translated.kind;
                    if let Some(tx) = self.tx.as_mut() {
                        if let Err(err) = tx.update(&mut self.doc, &original.id, now, |element| {
                            element.kind = kind.clone();
                        }) {
                            debug_assert!(false, "moving update failed: {err}");
                        }
                    }
                    effects.changed.insert(original.id.clone());
                }
                let moved_ids: BTreeSet<ElementId> =
                    originals.iter().map(|o| o.id.clone()).collect();
                let updates = self.connected_arrow_updates(&moved_ids);
                if let Some(tx) = self.tx.as_mut() {
                    for (arrow_id, start_point, end_point) in updates {
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
                }
                effects
            }
            Interaction::Resizing {
                handle,
                start,
                bounds,
                originals,
            } => {
                let current = self.camera.screen_to_world(screen);
                let target = resized_bounds(bounds, handle, start, current, mods.shift);
                let mut effects = Effects::default();
                for original in &originals {
                    let mut scaled = original.clone();
                    scale_element(&mut scaled, bounds, target);
                    let kind = scaled.kind;
                    if let Some(tx) = self.tx.as_mut() {
                        if let Err(err) = tx.update(&mut self.doc, &original.id, now, |element| {
                            element.kind = kind.clone();
                        }) {
                            debug_assert!(false, "resize update failed: {err}");
                        }
                    }
                    effects.changed.insert(original.id.clone());
                }
                effects
            }
            Interaction::Erasing { erased } => {
                let world = self.camera.screen_to_world(screen);
                let mut effects = Effects::default();
                let mut erased = erased;
                self.erase_at(world, &mut erased, &mut effects, now);
                self.interaction = Interaction::Erasing { erased };
                effects
            }
            Interaction::TextEditing { .. } => Effects::default(),
            Interaction::Freehand { id } => {
                let current = self.camera.screen_to_world(screen);
                let last = self.doc.element(&id).and_then(|e| match &e.kind {
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
                        let _ = tx.update(&mut self.doc, &id, now, |element| {
                            if let ElementKind::Freehand {
                                points, pressures, ..
                            } = &mut element.kind
                            {
                                points.push(current);
                                pressures.push(pressure);
                            }
                        });
                    }
                    effects.changed.insert(id);
                }
                effects
            }
            Interaction::Line { id, start } => {
                let current = self.camera.screen_to_world(screen);
                let end = constrain_angle(start, current, mods.shift);
                if let Some(tx) = self.tx.as_mut() {
                    let _ = tx.update(&mut self.doc, &id, now, |element| {
                        if let ElementKind::Arrow { end: e, .. } = &mut element.kind {
                            e.x = end.x;
                            e.y = end.y;
                        }
                    });
                }
                Effects::changed(id)
            }
            Interaction::Connecting { id, start } => {
                let current = self.camera.screen_to_world(screen);
                let tolerance = self.tolerance().max(4.0);
                let approach = self
                    .doc
                    .element(&id)
                    .and_then(|element| match &element.kind {
                        ElementKind::Arrow { start, .. } => Some(start.point()),
                        _ => None,
                    });
                let candidate =
                    connector::binding_candidate(&self.doc, current, tolerance, approach);
                let (end, binding, hover) = match candidate {
                    Some((target_id, side, point)) => {
                        let anchors = self
                            .doc
                            .element(&target_id)
                            .map(|element| {
                                connector::anchor_points(element.bounds())
                                    .into_iter()
                                    .map(|(_, p)| p)
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        (
                            point,
                            Some(Binding {
                                element_id: target_id.clone(),
                                focus: Some(side.focus()),
                            }),
                            Some((target_id, anchors)),
                        )
                    }
                    None => (current, None, None),
                };
                self.hover_anchors = hover;
                let _ = start;
                if let Some(tx) = self.tx.as_mut() {
                    let _ = tx.update(&mut self.doc, &id, now, |element| {
                        if let ElementKind::Arrow { end: e, .. } = &mut element.kind {
                            e.x = end.x;
                            e.y = end.y;
                            e.binding = binding.clone();
                        }
                    });
                }
                Effects {
                    changed: [id].into_iter().collect(),
                    overlay_changed: true,
                    ..Effects::default()
                }
            }
        }
    }

    pub fn pointer_up(&mut self, screen: Point, mods: Modifiers, now: u64) -> Effects {
        if self.nav.pointer_up(0) || self.nav.pointer_up(1) {
            return Effects::default();
        }
        let mut effects = match self.interaction {
            Interaction::Drawing { .. }
            | Interaction::Moving { .. }
            | Interaction::Resizing { .. }
            | Interaction::Connecting { .. }
            | Interaction::Line { .. } => self.pointer_move(screen, mods, now),
            _ => Effects::default(),
        };
        match self.interaction.clone() {
            Interaction::Idle => {}
            Interaction::FrameNameEditing { .. } => {}
            Interaction::Drawing { id, .. } => {
                let tiny = self
                    .doc
                    .element(&id)
                    .map(|e| {
                        let b = e.bounds();
                        b.w < self.min_draw_size() || b.h < self.min_draw_size()
                    })
                    .unwrap_or(true);
                self.interaction = Interaction::Idle;
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
                self.interaction = Interaction::Idle;
                self.marquee_current = None;
                let hits: Vec<ElementId> = elements_in_rect(&self.doc, rect)
                    .into_iter()
                    .map(|id| crate::hit::resolve_container(&self.doc, &id))
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
            Interaction::Moving { .. } => {
                self.interaction = Interaction::Idle;
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
            }
            Interaction::Resizing { .. } => {
                self.interaction = Interaction::Idle;
                let changed: BTreeSet<ElementId> = self
                    .tx
                    .as_ref()
                    .map(|tx| tx.deltas().iter().map(|d| d.element_id().clone()).collect())
                    .unwrap_or_default();
                let reflows = self.bound_text_reflows(&changed);
                let arrow_updates = self.connected_arrow_updates(&changed);
                self.sync_frame_membership(now);
                if let Some(tx) = self.tx.as_mut() {
                    for (text_id, rect) in &reflows {
                        let _ = tx.update(&mut self.doc, text_id, now, |element| {
                            if let ElementKind::Text { rect: r, .. } = &mut element.kind {
                                *r = *rect;
                            }
                        });
                    }
                    for (arrow_id, start_point, end_point) in arrow_updates {
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
                self.interaction = Interaction::Idle;
                if let Some(tx) = self.tx.take() {
                    tx.commit(&mut self.history);
                }
                effects.changed.extend(erased);
                effects.overlay_changed = true;
            }
            Interaction::TextEditing { .. } => {}
            Interaction::Freehand { id } => {
                self.interaction = Interaction::Idle;
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
                    let selection_effects = self.select([id.clone()]);
                    effects.changed.insert(id);
                    effects.merge(selection_effects);
                }
            }
            Interaction::Line { id, .. } => {
                self.interaction = Interaction::Idle;
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
            Interaction::Connecting { id, .. } => {
                self.interaction = Interaction::Idle;
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
        }
        effects
    }

    /// Double-click: edit text, or create bound text inside a shape/glyph.
    pub fn double_click(&mut self, screen: Point, now: u64) -> Effects {
        if self.interaction != Interaction::Idle {
            return Effects::default();
        }
        let world = self.camera.screen_to_world(screen);
        let tolerance = self.tolerance();
        let Some(id) = hit_test_raw(&self.doc, world, tolerance) else {
            return Effects::default();
        };
        match self.doc.element(&id).map(|e| e.kind.clone()) {
            Some(ElementKind::Text { .. }) => {
                self.begin_text_edit(self.doc.element(&id).cloned(), now)
            }
            Some(ElementKind::Shape {
                bound_text_id,
                rect,
                ..
            }) => match bound_text_id {
                Some(text_id) => self.begin_text_edit(self.doc.element(&text_id).cloned(), now),
                None => self.create_bound_text(id, rect, now),
            },
            Some(ElementKind::Glyph {
                caption_id, rect, ..
            }) => match caption_id {
                Some(text_id) => self.begin_text_edit(self.doc.element(&text_id).cloned(), now),
                None => self.create_bound_text(id, rect, now),
            },
            Some(ElementKind::Arrow { label_id, .. }) => match label_id {
                Some(text_id) => self.begin_text_edit(self.doc.element(&text_id).cloned(), now),
                None => {
                    let bounds = self
                        .doc
                        .element(&id)
                        .map(|e| e.bounds())
                        .unwrap_or(Rect::ZERO);
                    self.create_bound_text(id, bounds, now)
                }
            },
            Some(ElementKind::Frame { rect, name, .. }) => {
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
                };
                Effects {
                    overlay_changed: true,
                    actions: vec![action],
                    ..Effects::default()
                }
            }
            _ => Effects::default(),
        }
    }

    /// The text tool: clicking existing text edits it; clicking empty canvas
    /// starts a new point-text element.
    fn begin_text_tool(&mut self, world: Point, now: u64) -> Effects {
        let tolerance = self.tolerance();
        if let Some(id) = hit_test_raw(&self.doc, world, tolerance) {
            let is_text = matches!(
                self.doc.element(&id).map(|e| &e.kind),
                Some(ElementKind::Text { .. })
            );
            if is_text {
                return self.begin_text_edit(self.doc.element(&id).cloned(), now);
            }
        }
        self.start_text(world, now)
    }

    /// Create a point-text element and open the editor.
    pub fn start_text(&mut self, world: Point, now: u64) -> Effects {
        if self.interaction != Interaction::Idle {
            return Effects::default();
        }
        let mut tx = Transaction::begin();
        let element = Element::new(
            ElementKind::Text {
                rect: Rect::new(
                    world.x,
                    world.y,
                    0.0,
                    self.measurer.line_height(self.style.font_size),
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
        let mut tx = Transaction::begin();
        let font_size = self.style.font_size;
        let element = Element::new(
            ElementKind::Text {
                rect: Rect::new(
                    container_rect.x + 10.0,
                    if is_glyph {
                        container_rect.max_y() + 4.0
                    } else {
                        container_rect.center().y
                    },
                    (container_rect.w - 20.0).max(20.0),
                    self.measurer.line_height(font_size),
                ),
                text: String::new(),
                align: TextAlign::Center,
                fixed_width: true,
                container_id: Some(container_id.clone()),
                style: self.style.clone(),
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
        })
    }

    /// Commit the open text edit with the shell's textarea contents.
    pub fn commit_text(&mut self, id: &str, text: &str, now: u64) -> Effects {
        if let Interaction::FrameNameEditing { id: editing_id } = self.interaction.clone() {
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
        let Interaction::TextEditing {
            id: editing_id,
            original,
        } = self.interaction.clone()
        else {
            return Effects::default();
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
                    let purged = self.doc.element(id).cloned();
                    if let Some(element) = purged {
                        let _ = self.doc.apply(&Delta::Purge { element });
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
            tx.commit(&mut self.history);
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
                ..
            } => {
                let points = connector::route(start, end, *routing, &self.doc);
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

    /// Discard the open text edit, restoring the prior state.
    pub fn cancel_text(&mut self) -> Effects {
        let Interaction::TextEditing { id, original } = self.interaction.clone() else {
            return Effects::default();
        };
        let original = *original;
        self.tx = None;
        self.interaction = Interaction::Idle;
        match original {
            Some(orig) => {
                if let Some(current) = self.doc.element(&id).cloned() {
                    let _ = self.doc.apply(&Delta::Update {
                        before: current,
                        after: orig,
                    });
                }
            }
            None => {
                let purged = self.doc.element(&id).cloned();
                if let Some(element) = purged {
                    let _ = self.doc.apply(&Delta::Purge { element });
                }
            }
        }
        Effects {
            changed: [id].into_iter().collect(),
            overlay_changed: true,
            ..Effects::default()
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
            "z" | "Z" if mods.command() && mods.shift => self.redo(),
            "z" | "Z" if mods.command() => self.undo(),
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
                    } else {
                        self.set_tool(tool);
                    }
                    return Effects {
                        overlay_changed: true,
                        ..Effects::default()
                    };
                }
                Effects::default()
            }
        }
    }

    fn nudge_step(&self, mods: Modifiers) -> f64 {
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
            Interaction::Drawing { id, .. } => {
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
            Interaction::Moving { .. } | Interaction::Resizing { .. } => {
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
                self.tx = None;
                match *original {
                    Some(orig) => {
                        if let Some(current) = self.doc.element(&id).cloned() {
                            let _ = self.doc.apply(&Delta::Update {
                                before: current,
                                after: orig,
                            });
                        }
                    }
                    None => {
                        let purged = self.doc.element(&id).cloned();
                        if let Some(element) = purged {
                            let _ = self.doc.apply(&Delta::Purge { element });
                        }
                    }
                }
                Effects {
                    changed: [id].into_iter().collect(),
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

    pub fn undo(&mut self) -> Effects {
        if let Err(err) = self.history.undo(&mut self.doc) {
            debug_assert!(false, "undo failed: {err}");
        }
        self.selection.clear();
        Effects {
            repaint_all: true,
            overlay_changed: true,
            ..Effects::default()
        }
    }

    pub fn redo(&mut self) -> Effects {
        if let Err(err) = self.history.redo(&mut self.doc) {
            debug_assert!(false, "redo failed: {err}");
        }
        self.selection.clear();
        Effects {
            repaint_all: true,
            overlay_changed: true,
            ..Effects::default()
        }
    }

    // -- tool helpers ------------------------------------------------------

    fn begin_select(&mut self, screen: Point, world: Point, mods: Modifiers) -> Effects {
        if mods.command() {
            return Effects::default();
        }
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
        let tolerance = self.tolerance();
        match hit_test(&self.doc, world, tolerance) {
            Some(id) => {
                if mods.shift {
                    if self.selection.contains(&id) {
                        self.selection.remove(&id);
                    } else {
                        self.selection.insert(id);
                    }
                    return Effects {
                        overlay_changed: true,
                        ..Effects::default()
                    };
                }
                if !self.selection.contains(&id) {
                    self.selection = [id].into_iter().collect();
                }
                let originals: Vec<Element> = self.selection_with_followers();
                self.tx = Some(Transaction::begin());
                self.interaction = Interaction::Moving {
                    start: world,
                    originals,
                };
                Effects {
                    overlay_changed: true,
                    ..Effects::default()
                }
            }
            None => {
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
    pub fn hover_anchor_points(&self) -> Vec<Point> {
        self.hover_anchors
            .as_ref()
            .map(|(_, points)| points.clone())
            .unwrap_or_default()
    }

    fn begin_connector(&mut self, world: Point, now: u64) -> Effects {
        let mut tx = Transaction::begin();
        let tolerance = self.tolerance().max(4.0);
        let candidate = connector::binding_candidate(&self.doc, world, tolerance, None);
        let (start_point, start_binding) = match candidate {
            Some((target_id, side, point)) => (
                point,
                Some(Binding {
                    element_id: target_id,
                    focus: Some(side.focus()),
                }),
            ),
            None => (world, None),
        };
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

    /// New endpoint positions for arrows connected to the given elements.
    fn connected_arrow_updates(
        &self,
        moved: &BTreeSet<ElementId>,
    ) -> Vec<(ElementId, Point, Point)> {
        let mut updates = Vec::new();
        for moved_id in moved {
            for arrow_id in self.bound_arrows(moved_id) {
                if moved.contains(&arrow_id) {
                    continue;
                }
                let Some(ElementKind::Arrow { start, end, .. }) =
                    self.doc.element(&arrow_id).map(|e| e.kind.clone())
                else {
                    continue;
                };
                let (s, _) = connector::resolve_endpoint(&start, end.point(), &self.doc);
                let (e, _) = connector::resolve_endpoint(&end, start.point(), &self.doc);
                updates.push((arrow_id, s, e));
            }
        }
        updates
    }

    /// Frame membership updates needed after the given elements moved.
    fn frame_membership_updates(
        &self,
        ids: &BTreeSet<ElementId>,
    ) -> Vec<(ElementId, Option<ElementId>)> {
        let mut out = Vec::new();
        for id in ids {
            let Some(element) = self.doc.element(id) else {
                continue;
            };
            if element.is_deleted || element.is_frame() {
                continue;
            }
            if let ElementKind::Text {
                container_id: Some(_),
                ..
            } = &element.kind
            {
                continue;
            }
            let target = self.frame_containing(element.bounds().center());
            if element.frame_id != target {
                out.push((id.clone(), target));
            }
        }
        out
    }

    /// The smallest frame whose rectangle contains a point.
    fn frame_containing(&self, point: Point) -> Option<ElementId> {
        let mut best: Option<(f64, ElementId)> = None;
        for element in self.doc.elements.values() {
            if element.is_deleted {
                continue;
            }
            if let ElementKind::Frame { rect, .. } = &element.kind {
                if rect.contains(point) {
                    let area = rect.w * rect.h;
                    if best.as_ref().map(|(a, _)| area < *a).unwrap_or(true) {
                        best = Some((area, element.id.clone()));
                    }
                }
            }
        }
        best.map(|(_, id)| id)
    }

    /// Reconcile frame membership inside the open transaction.
    fn sync_frame_membership(&mut self, now: u64) {
        let Some(tx) = self.tx.as_ref() else {
            return;
        };
        let ids: BTreeSet<ElementId> = tx.deltas().iter().map(|d| d.element_id().clone()).collect();
        let updates = self.frame_membership_updates(&ids);
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
        if let Some(id) = hit_test(&self.doc, world, tolerance) {
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

    fn selected_elements(&self) -> Vec<Element> {
        self.selection
            .iter()
            .filter_map(|id| self.doc.element(id).cloned())
            .collect()
    }

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
        let originals = self.selected_elements();
        let mut tx = Transaction::begin();
        let mut effects = Effects::default();
        for original in &originals {
            let mut translated = original.clone();
            translate_element(&mut translated, dx, dy);
            let kind = translated.kind;
            if let Err(err) = tx.update(&mut self.doc, &original.id, now, |element| {
                element.kind = kind.clone();
            }) {
                debug_assert!(false, "nudge failed: {err}");
            }
            effects.changed.insert(original.id.clone());
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
        Effects::default()
    }

    /// Copy and delete the selection.
    pub fn cut(&mut self, now: u64) -> Effects {
        self.clipboard = self.selection_with_followers();
        self.delete_selection(now)
    }

    /// Paste the clipboard with a cumulative offset.
    pub fn paste(&mut self, now: u64) -> Effects {
        if self.clipboard.is_empty() {
            return Effects::default();
        }
        let source = self.clipboard.clone();
        self.insert_copies(source, 12.0, 12.0, now)
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
        | ElementKind::Text { style, .. } => Some(style),
        ElementKind::Frame { .. } => None,
    }
}

/// Translate every kind of element in place.
pub fn translate_element(element: &mut Element, dx: f64, dy: f64) {
    match &mut element.kind {
        ElementKind::Shape { rect, .. }
        | ElementKind::Glyph { rect, .. }
        | ElementKind::Text { rect, .. }
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
        ElementKind::Arrow { start, end, .. } => {
            start.x += dx;
            start.y += dy;
            end.x += dx;
            end.y += dy;
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
        ElementKind::Arrow { start, end, .. } => {
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
        }
    }
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
        editor.key_down(key, mods, now)
    }

    fn drag_shape(editor: &mut Editor, from: Point, to: Point, now: u64) {
        let mods = Modifiers::default();
        press(editor, "r", mods, now);
        editor.pointer_down(from, 0, mods, now);
        editor.pointer_move(to, mods, now + 1);
        editor.pointer_up(to, mods, now + 2);
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
        e.pointer_down(Point::new(50.0, 25.0), 0, mods, 3);
        e.pointer_move(Point::new(80.0, 45.0), mods, 4);
        e.pointer_up(Point::new(80.0, 45.0), mods, 5);

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

        e.pointer_down(Point::new(50.0, 25.0), 0, mods, 3);
        for step in 0..10 {
            e.pointer_move(Point::new(50.0 + step as f64, 25.0), mods, 4 + step);
        }
        e.pointer_up(Point::new(60.0, 25.0), mods, 20);
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
        e.pointer_down(Point::new(-20.0, -20.0), 0, mods, 4);
        e.pointer_move(Point::new(300.0, 100.0), mods, 5);
        e.pointer_up(Point::new(300.0, 100.0), mods, 6);
        assert_eq!(e.selection.len(), 2);
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
        e.pointer_down(Point::new(25.0, 25.0), 0, mods, 4);
        e.pointer_up(Point::new(25.0, 25.0), mods, 5);

        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        e.pointer_down(Point::new(125.0, 25.0), 0, shift, 6);
        e.pointer_up(Point::new(125.0, 25.0), shift, 7);
        assert_eq!(e.selection.len(), 2);

        e.pointer_down(Point::new(125.0, 25.0), 0, shift, 8);
        e.pointer_up(Point::new(125.0, 25.0), shift, 9);
        assert_eq!(e.selection.len(), 1);
    }

    #[test]
    fn escape_cancels_an_in_progress_draw() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "r", mods, 1);
        e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
        e.pointer_move(Point::new(100.0, 100.0), mods, 3);
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

        e.pointer_down(Point::new(100.0, 50.0), 0, mods, 3);
        e.pointer_move(Point::new(150.0, 100.0), mods, 4);
        e.pointer_up(Point::new(150.0, 100.0), mods, 5);
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
        e.pointer_down(Point::new(400.0, 200.0), 0, mods, 3);
        e.pointer_move(Point::new(150.0, 350.0), mods, 4);
        e.pointer_up(Point::new(150.0, 350.0), mods, 5);

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
        e.pointer_down(Point::new(100.0, 50.0), 0, mods, 3);
        e.pointer_move(Point::new(1.0, 1.0), mods, 4);
        e.pointer_up(Point::new(1.0, 1.0), mods, 5);
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

        e.pointer_down(Point::new(100.0, 50.0), 0, shift, 3);
        e.pointer_move(Point::new(140.0, 50.0), shift, 4);
        e.pointer_up(Point::new(140.0, 50.0), shift, 5);
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
        e.pointer_down(Point::new(25.0, 25.0), 0, mods, 4);
        e.pointer_move(Point::new(125.0, 25.0), mods, 5);
        e.pointer_up(Point::new(125.0, 25.0), mods, 6);
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
        e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
        for i in 1..=20 {
            e.pointer_move(
                Point::new(i as f64 * 3.0, (i as f64 * 0.7).sin() * 4.0),
                mods,
                2 + i,
            );
        }
        e.pointer_up(Point::new(60.0, 0.0), mods, 30);

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
        e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
        e.pointer_move(Point::new(50.0, 10.0), mods, 3);
        e.key_down("Escape", mods, 4);
        assert!(e.doc.is_empty());
    }

    #[test]
    fn line_tool_creates_an_arrow_element() {
        let mut e = editor();
        press(&mut e, "l", Modifiers::default(), 1);
        let mods = Modifiers::default();
        e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
        e.pointer_move(Point::new(100.0, 50.0), mods, 3);
        e.pointer_up(Point::new(100.0, 50.0), mods, 4);

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
        e.pointer_down(Point::new(200.0, 0.0), 0, mods, 6);
        e.pointer_move(Point::new(300.0, 0.0), mods, 7);
        e.pointer_up(Point::new(300.0, 0.0), mods, 8);
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
        e.pointer_down(Point::new(0.0, 0.0), 0, shift, 2);
        e.pointer_move(Point::new(100.0, 10.0), shift, 3);
        e.pointer_up(Point::new(100.0, 10.0), shift, 4);
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
        e.pointer_down(Point::new(50.0, 50.0), 0, mods, 2);
        let id = e.doc.ordered_live()[0].id.clone();
        e.commit_text(&id, "Hello world", 3);

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
        e.pointer_down(Point::new(50.0, 50.0), 0, mods, 2);
        let id = e.doc.ordered_live()[0].id.clone();
        e.commit_text(&id, "   ", 3);
        assert!(e.doc.is_empty());
        assert!(!e.history.can_undo());
    }

    #[test]
    fn double_click_shapes_creates_bound_text_that_follows_them() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(200.0, 100.0), 1);
        let shape_id = e.doc.ordered_live()[0].id.clone();
        press(&mut e, "v", Modifiers::default(), 2);

        e.double_click(Point::new(100.0, 50.0), 3);
        let text_id = e
            .doc
            .ordered_live()
            .iter()
            .find(|element| element.id != shape_id)
            .expect("bound text created")
            .id
            .clone();
        e.commit_text(&text_id, "node", 4);

        match e.doc.element(&shape_id).unwrap().kind.clone() {
            ElementKind::Shape { bound_text_id, .. } => {
                assert_eq!(bound_text_id, Some(text_id.clone()));
            }
            other => panic!("expected shape, got {other:?}"),
        }
        let before = e.doc.element(&text_id).unwrap().bounds();

        let mods = Modifiers::default();
        e.pointer_down(Point::new(100.0, 50.0), 0, mods, 5);
        e.pointer_move(Point::new(150.0, 70.0), mods, 6);
        e.pointer_up(Point::new(150.0, 70.0), mods, 7);
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
        e.double_click(Point::new(100.0, 50.0), 3);
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
        e.pointer_down(Point::new(5.0, 5.0), 0, mods, 5);
        e.pointer_up(Point::new(5.0, 5.0), mods, 5);
        assert!(e.selection.contains(&shape_id));

        // Shrink the shape by dragging its SE handle.
        e.pointer_down(Point::new(200.0, 100.0), 0, mods, 6);
        e.pointer_move(Point::new(70.0, 100.0), mods, 7);
        e.pointer_up(Point::new(70.0, 100.0), mods, 8);
        let height_after = e.doc.element(&text_id).unwrap().bounds().h;
        assert!(
            height_after > height_before,
            "text wrapped taller after shrinking: {height_before} -> {height_after}"
        );
    }

    #[test]
    fn escape_cancels_text_editing() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "t", mods, 1);
        e.pointer_down(Point::new(50.0, 50.0), 0, mods, 2);
        e.cancel_text();
        assert!(e.doc.is_empty());
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
        e.update_style(&patch, 2);

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
        e.update_style(&patch, 1);
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
        e.pointer_down(Point::new(50.0, 50.0), 0, mods, 4);
        e.pointer_move(Point::new(200.0, 50.0), mods, 5);
        e.pointer_move(Point::new(350.0, 50.0), mods, 6);
        assert_eq!(e.hover_anchor_points().len(), 4);
        e.pointer_up(Point::new(350.0, 50.0), mods, 7);

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

    #[test]
    fn moving_a_bound_shape_updates_the_arrow() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 100.0), 1);
        drag_shape(&mut e, Point::new(300.0, 0.0), Point::new(400.0, 100.0), 2);
        let b = e.doc.ordered_live()[1].id.clone();

        let mods = Modifiers::default();
        press(&mut e, "a", mods, 3);
        e.pointer_down(Point::new(50.0, 50.0), 0, mods, 4);
        e.pointer_move(Point::new(350.0, 50.0), mods, 5);
        e.pointer_up(Point::new(350.0, 50.0), mods, 6);

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
        e.pointer_down(Point::new(350.0, 50.0), 0, mods, 8);
        e.pointer_move(Point::new(450.0, 150.0), mods, 9);
        e.pointer_up(Point::new(450.0, 150.0), mods, 10);

        let after = e.doc.element(&arrow_id).unwrap().bounds();
        assert!(
            after != before,
            "arrow endpoints followed the moved shape: {before:?} -> {after:?}"
        );
    }

    #[test]
    fn double_click_a_connector_creates_a_label() {
        let mut e = editor();
        drag_shape(&mut e, Point::new(0.0, 0.0), Point::new(100.0, 100.0), 1);
        drag_shape(&mut e, Point::new(300.0, 0.0), Point::new(400.0, 100.0), 2);
        let mods = Modifiers::default();
        press(&mut e, "a", mods, 3);
        e.pointer_down(Point::new(50.0, 50.0), 0, mods, 4);
        e.pointer_move(Point::new(350.0, 50.0), mods, 5);
        e.pointer_up(Point::new(350.0, 50.0), mods, 6);

        press(&mut e, "v", mods, 7);
        e.double_click(Point::new(200.0, 50.0), 8);
        let text_id = e
            .doc
            .ordered_live()
            .iter()
            .find(|element| element.is_text())
            .expect("label text created")
            .id
            .clone();
        e.commit_text(&text_id, "https", 9);

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
        e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
        e.pointer_move(Point::new(400.0, 300.0), mods, 3);
        e.pointer_up(Point::new(400.0, 300.0), mods, 4);
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
        e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
        e.pointer_move(Point::new(400.0, 300.0), mods, 3);
        e.pointer_up(Point::new(400.0, 300.0), mods, 4);
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
        e.pointer_down(Point::new(100.0, 100.0), 0, mods, 7);
        e.pointer_move(Point::new(600.0, 100.0), mods, 8);
        e.pointer_up(Point::new(600.0, 100.0), mods, 9);
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
        e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
        e.pointer_move(Point::new(400.0, 300.0), mods, 3);
        e.pointer_up(Point::new(400.0, 300.0), mods, 4);
        let frame_id = e.doc.ordered_live()[0].id.clone();

        drag_shape(&mut e, Point::new(50.0, 50.0), Point::new(150.0, 150.0), 5);
        let shape_id = e.doc.ordered_live()[1].id.clone();
        let before = e.doc.element(&shape_id).unwrap().bounds();

        press(&mut e, "v", mods, 6);
        e.select([frame_id.clone()]);
        // Grab the frame by its border, away from the resize handles.
        e.pointer_down(Point::new(300.0, 300.0), 0, mods, 7);
        e.pointer_move(Point::new(340.0, 330.0), mods, 8);
        e.pointer_up(Point::new(340.0, 330.0), mods, 9);

        let after = e.doc.element(&shape_id).unwrap().bounds();
        assert!((after.x - before.x - 40.0).abs() < 0.01);
        assert!((after.y - before.y - 30.0).abs() < 0.01);
    }

    #[test]
    fn deleting_a_frame_deletes_children_in_one_undoable_step() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "f", mods, 1);
        e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
        e.pointer_move(Point::new(400.0, 300.0), mods, 3);
        e.pointer_up(Point::new(400.0, 300.0), mods, 4);
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
    fn duplicating_a_frame_duplicates_its_children() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "f", mods, 1);
        e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
        e.pointer_move(Point::new(400.0, 300.0), mods, 3);
        e.pointer_up(Point::new(400.0, 300.0), mods, 4);
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
        e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
        e.pointer_move(Point::new(400.0, 300.0), mods, 3);
        e.pointer_up(Point::new(400.0, 300.0), mods, 4);
        let frame_id = e.doc.ordered_live()[0].id.clone();

        press(&mut e, "v", mods, 5);
        e.double_click(Point::new(100.0, -8.0), 6);
        e.commit_text(&frame_id, "us-central1", 7);
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
            e.pointer_down(Point::new(0.0, 0.0), 0, mods, 2);
            e.pointer_move(Point::new(100.0, 60.0), mods, 3);
            e.pointer_up(Point::new(100.0, 60.0), mods, 4);
            let live = e.doc.ordered_live();
            assert_eq!(live.len(), 1);
            live[0].id.clone()
        };
        assert_eq!(e.tool, Tool::Select, "the draw tool is single-shot");
        assert_eq!(e.selection.iter().next(), Some(&id));

        // The placed shape is selectable after deselecting.
        e.clear_selection();
        e.pointer_down(Point::new(50.0, 30.0), 0, mods, 5);
        e.pointer_up(Point::new(50.0, 30.0), mods, 6);
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
            e.pointer_down(Point::new(*start, 0.0), 0, mods, now);
            e.pointer_move(Point::new(start + 80.0, 60.0), mods, now + 1);
            e.pointer_up(Point::new(start + 80.0, 60.0), mods, now + 2);
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
        e.pointer_down(Point::new(50.0, 50.0), 0, mods, 2);
        let id = e.doc.ordered_live()[0].id.clone();
        e.commit_text(&id, "hello", 3);
        assert_eq!(e.tool, Tool::Select, "new text is single-shot");

        press(&mut e, "t", mods, 4);
        let effects = e.pointer_down(Point::new(55.0, 55.0), 0, mods, 5);
        assert!(!effects.actions.is_empty(), "editing opens the overlay");
        e.commit_text(&id, "hello again", 6);
        assert_eq!(e.tool, Tool::Text, "editing existing text keeps the tool");
    }

    #[test]
    fn clicking_a_caption_selects_and_moves_the_component() {
        let mut e = editor();
        e.set_viewport(800.0, 600.0);
        e.insert_glyph("cloud_storage", 1);
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
        e.pointer_down(caption_centre, 0, mods, 3);
        e.pointer_up(caption_centre, mods, 4);
        assert_eq!(
            e.selection.iter().next().map(String::as_str),
            Some(glyph.id.as_str()),
            "clicking the caption selects the glyph"
        );

        let before = glyph.bounds();
        e.pointer_down(caption_centre, 0, mods, 5);
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
        e.insert_glyph("cloud_storage", 1);
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
        e.pointer_down(caption_centre, 0, mods, 3);
        e.pointer_up(caption_centre, mods, 4);
        assert_eq!(e.doc.ordered_live().len(), 0, "glyph and caption both go");
    }

    #[test]
    fn text_tool_click_on_existing_text_edits_instead_of_stacking() {
        let mut e = editor();
        let mods = Modifiers::default();
        press(&mut e, "t", mods, 1);
        e.pointer_down(Point::new(50.0, 50.0), 0, mods, 2);
        let id = e.doc.ordered_live()[0].id.clone();
        e.commit_text(&id, "hello", 3);
        assert_eq!(e.doc.ordered_live().len(), 1);
        assert_eq!(e.tool, Tool::Select, "placing text is single-shot");

        press(&mut e, "t", mods, 4);
        e.pointer_down(Point::new(55.0, 55.0), 0, mods, 5);
        assert_eq!(
            e.doc.ordered_live().len(),
            1,
            "clicking existing text does not stack a new element"
        );
        e.commit_text(&id, "hello world", 5);
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
        e.pointer_down(center, 0, mods, 2);
        e.pointer_move(Point::new(center.x + 60.0, center.y), mods, 3);
        e.pointer_up(Point::new(center.x + 60.0, center.y), mods, 4);
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
            let ops = crate::rough::build_element_ops(&element, &doc, &ApproxMeasurer);
            assert!(!ops.is_empty(), "{} renders nothing", info.id);
            doc.insert(element);
        }
        let mut scene = Scene::new();
        scene.rebuild_all(&doc, &ApproxMeasurer);
        assert_eq!(scene.layers.len(), crate::glyphs::catalog().len());
    }
}
