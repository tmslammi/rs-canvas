//! The editor: the interaction state machine over a Document.
//!
//! Owns the document, camera, scene cache, history, tool, style defaults and
//! selection. Everything here is browser-free; the wasm façade only forwards
//! input and paints the effects.

use std::collections::BTreeSet;

use crate::camera::Camera;
use crate::delta::Delta;
use crate::geometry::{Point, Rect};
use crate::history::{History, Transaction};
use crate::hit::{elements_in_rect, hit_test};
use crate::model::{Document, Element, ElementId, ElementKind, ShapeKind, Style};
use crate::nav::NavigationState;
use crate::scene::Scene;
use crate::tool::Tool;

/// What the shell should repaint after an input.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Effects {
    /// Elements whose cached layers must be rebuilt.
    pub changed: BTreeSet<ElementId>,
    /// The whole frame changed (camera moves, history jumps).
    pub repaint_all: bool,
    /// Selection or in-progress decoration changed (overlay repaint).
    pub overlay_changed: bool,
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
}

/// The editable session.
pub struct Editor {
    pub doc: Document,
    pub camera: Camera,
    pub scene: Scene,
    pub history: History,
    pub nav: NavigationState,
    pub tool: Tool,
    pub style: Style,
    pub selection: BTreeSet<ElementId>,
    interaction: Interaction,
    tx: Option<Transaction>,
    marquee_current: Option<Point>,
}

impl Editor {
    pub fn new(doc: Document) -> Self {
        let mut scene = Scene::new();
        scene.rebuild_all(&doc);
        Self {
            doc,
            camera: Camera::default(),
            scene,
            history: History::new(),
            nav: NavigationState::new(),
            tool: Tool::default(),
            style: Style::default(),
            selection: BTreeSet::new(),
            interaction: Interaction::Idle,
            tx: None,
            marquee_current: None,
        }
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
        if self.nav.pointer_down(screen, button) {
            return Effects::default();
        }
        if button != 0 {
            return Effects::default();
        }
        let world = self.camera.screen_to_world(screen);
        match self.tool {
            Tool::Select => self.begin_select(world, mods),
            Tool::Rectangle | Tool::Diamond | Tool::Ellipse => self.begin_drawing(world, now),
            Tool::Hand => {
                self.nav.start_pan(screen);
                Effects::default()
            }
            _ => Effects::default(),
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
            Interaction::Drawing { id, start } => {
                let current = self.camera.screen_to_world(screen);
                let rect = self.drawn_rect(start, current, mods);
                if let Some(tx) = self.tx.as_mut() {
                    if let Err(err) = tx.update(&mut self.doc, &id, now, |element| {
                        if let ElementKind::Shape { rect: r, .. } = &mut element.kind {
                            *r = rect;
                        }
                    }) {
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
                effects
            }
        }
    }

    pub fn pointer_up(&mut self, screen: Point, mods: Modifiers, now: u64) -> Effects {
        if self.nav.pointer_up(0) || self.nav.pointer_up(1) {
            return Effects::default();
        }
        let mut effects = match self.interaction {
            Interaction::Drawing { .. } | Interaction::Moving { .. } => {
                self.pointer_move(screen, mods, now)
            }
            _ => Effects::default(),
        };
        match self.interaction.clone() {
            Interaction::Idle => {}
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
                    if let Some(tx) = self.tx.take() {
                        tx.commit(&mut self.history);
                    }
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
                let mut hits = elements_in_rect(&self.doc, rect);
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
        }
        effects
    }

    /// Double-click (for bound text later).
    pub fn double_click(&mut self, screen: Point, now: u64) -> Effects {
        let _ = (screen, now);
        Effects::default()
    }

    // -- keyboard ----------------------------------------------------------

    pub fn key_down(&mut self, key: &str, mods: Modifiers, now: u64) -> Effects {
        match key {
            "Escape" => self.cancel(),
            "Delete" | "Backspace" => self.delete_selection(now),
            "a" | "A" if mods.command() => self.select_all(),
            "z" | "Z" if mods.command() && mods.shift => self.redo(),
            "z" | "Z" if mods.command() => self.undo(),
            " " => {
                self.nav.set_space(true);
                Effects::default()
            }
            _ => {
                if mods.command() || mods.alt {
                    return Effects::default();
                }
                if let Some(tool) = Tool::from_shortcut(key) {
                    self.tool = tool;
                    return Effects {
                        overlay_changed: true,
                        ..Effects::default()
                    };
                }
                Effects::default()
            }
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
            Interaction::Moving { .. } => {
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
                self.nav.cancel();
                Effects {
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
        let ids: Vec<ElementId> = self.selection.iter().cloned().collect();
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

    fn begin_select(&mut self, world: Point, mods: Modifiers) -> Effects {
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
                let originals: Vec<Element> = self
                    .selection
                    .iter()
                    .filter_map(|sid| self.doc.element(sid).cloned())
                    .collect();
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
}
