//! The boundary between the core and the browser shell.
//!
//! `canvas-core` never touches the DOM. Inputs arrive as plain values, and the
//! results of an input are an [`Effects`] value: which cached layers changed,
//! whether the frame or overlay needs a repaint, whether the document is now
//! dirty for autosave, and any [`ShellAction`] the shell must perform (open the
//! text editor, schedule a save).
//!
//! This module owns only those messages. It deliberately has no dependency on
//! the editor state: the editor decides *what* changed, the shell decides *how*
//! to paint or persist it.

use std::collections::BTreeSet;

use crate::model::{ElementId, TextAlign};

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
        /// The text's own ink, so the overlay reads exactly like the element
        /// (a sticky's dark ink, not the canvas' light default).
        color: String,
        /// True when the canvas wraps this text to its rect width. Auto-width
        /// point text must not wrap in the overlay either.
        wrap: bool,
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
    pub(crate) fn changed(id: impl Into<ElementId>) -> Self {
        let mut effects = Self {
            document_changed: true,
            ..Self::default()
        };
        effects.changed.insert(id.into());
        effects
    }

    pub(crate) fn merge(&mut self, other: Effects) {
        self.changed.extend(other.changed);
        self.repaint_all |= other.repaint_all;
        self.overlay_changed |= other.overlay_changed;
        self.document_changed |= other.document_changed;
        self.preview |= other.preview;
        self.actions.extend(other.actions);
    }
}
