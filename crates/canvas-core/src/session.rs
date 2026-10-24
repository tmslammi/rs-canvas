//! The committed document session.
//!
//! This is the only owner of committed document content, its transaction
//! journal and undo/redo history, and the internal clipboard. Gesture previews
//! belong to [`crate::interaction::InteractionController`]; the editor writes
//! preview truth back through [`DocumentSession::element_mut`] on release and
//! records the gesture as one transaction.
//!
//! Invariants:
//!
//! - Every committed content change is a reversible [`Delta`] applied to `doc`
//!   here (ADR-0002's one seam). Add/update/remove helpers pair with the
//!   editor's open [`Transaction`], so the journal and the document cannot
//!   drift.
//! - `history` is the only undo/redo state, and its outbox is what the page
//!   bridge drains into wire patches. Replacement (file open, new document)
//!   resets it; remote merges and patches deliberately do not.
//! - The clipboard copies elements, never ids: paste remaps references and
//!   assigns fresh ids. `paste_serial` makes repeated pastes land apart.
//! - Nothing here knows about cameras, selection, scene caches or shell
//!   effects. Those are derived or view state.

use std::collections::BTreeSet;

use crate::delta::{Delta, EditError};
use crate::history::{History, Transaction};
use crate::model::{Document, Element, ElementId};
use crate::patch::Patch;

pub(crate) struct DocumentSession {
    doc: Document,
    history: History,
    clipboard: Vec<Element>,
    /// How many pastes the current clipboard has produced; each paste lands
    /// further from the source so copies never stack invisibly.
    paste_serial: u32,
}

impl DocumentSession {
    pub(crate) fn new(doc: Document) -> Self {
        Self {
            doc,
            history: History::new(),
            clipboard: Vec::new(),
            paste_serial: 0,
        }
    }

    pub(crate) fn document(&self) -> &Document {
        &self.doc
    }

    /// Direct document access for preview release and test setup. Command
    /// paths use the transaction helpers instead.
    pub(crate) fn element_mut(&mut self, id: &str) -> Option<&mut Element> {
        self.doc.element_mut(id)
    }

    /// Replace the whole document and reset the journal (file open, restore).
    pub(crate) fn replace(&mut self, doc: Document) {
        self.doc = doc;
        self.history = History::new();
    }

    /// Replace the document without touching history (remote merge).
    pub(crate) fn replace_merged(&mut self, doc: Document) {
        self.doc = doc;
    }

    // -- transaction journal ----------------------------------------------

    pub(crate) fn add(&mut self, tx: &mut Transaction, element: Element) -> Result<(), EditError> {
        tx.add(&mut self.doc, element)
    }

    pub(crate) fn update<F>(
        &mut self,
        tx: &mut Transaction,
        id: &str,
        now_ms: u64,
        mutate: F,
    ) -> Result<(), EditError>
    where
        F: FnOnce(&mut Element),
    {
        tx.update(&mut self.doc, id, now_ms, mutate)
    }

    pub(crate) fn remove(
        &mut self,
        tx: &mut Transaction,
        id: &str,
        now_ms: u64,
    ) -> Result<(), EditError> {
        tx.remove(&mut self.doc, id, now_ms)
    }

    /// Apply a delta directly (defensive cleanup and cursor undo paths).
    pub(crate) fn apply(&mut self, delta: &Delta) -> Result<(), EditError> {
        self.doc.apply(delta)
    }

    /// Revert an abandoned draft transaction against the live document and
    /// return the ids it touched.
    pub(crate) fn revert(&mut self, tx: &Transaction) -> BTreeSet<ElementId> {
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

    pub(crate) fn commit(&mut self, tx: Transaction) {
        tx.commit(&mut self.history);
    }

    pub(crate) fn commit_opt(&mut self, tx: Option<Transaction>) {
        if let Some(tx) = tx {
            self.commit(tx);
        }
    }

    pub(crate) fn undo(&mut self, now_ms: u64) -> BTreeSet<ElementId> {
        match self.history.undo(&mut self.doc, now_ms) {
            Ok(ids) => ids.into_iter().collect(),
            Err(err) => {
                debug_assert!(false, "undo failed: {err}");
                BTreeSet::new()
            }
        }
    }

    pub(crate) fn redo(&mut self, now_ms: u64) -> BTreeSet<ElementId> {
        match self.history.redo(&mut self.doc, now_ms) {
            Ok(ids) => ids.into_iter().collect(),
            Err(err) => {
                debug_assert!(false, "redo failed: {err}");
                BTreeSet::new()
            }
        }
    }

    pub(crate) fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    pub(crate) fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    pub(crate) fn drain_outbox(&mut self) -> Vec<Delta> {
        self.history.drain_outbox()
    }

    // -- clipboard ---------------------------------------------------------

    /// Copy elements into the clipboard, restarting the paste offset.
    pub(crate) fn copy_to_clipboard(&mut self, elements: Vec<Element>) {
        self.clipboard = elements;
        self.paste_serial = 0;
    }

    /// Take a clipboard copy and its cumulative paste offset, if non-empty.
    pub(crate) fn take_paste(&mut self) -> Option<(Vec<Element>, f64)> {
        if self.clipboard.is_empty() {
            return None;
        }
        self.paste_serial += 1;
        let offset = 12.0 * self.paste_serial as f64;
        Some((self.clipboard.clone(), offset))
    }

    // -- bulk / merge ------------------------------------------------------

    /// Rigid uniform translation of every live element for the benchmark
    /// driver. Not an undoable edit: the caller also shifts the projection.
    pub(crate) fn translate_all(&mut self, dx: f64, dy: f64) {
        for element in self.doc.elements.values_mut() {
            if element.is_deleted {
                continue;
            }
            crate::editor::translate_element(element, dx, dy);
        }
    }

    /// Apply a remote element patch with per-element last-writer-wins rules.
    pub(crate) fn apply_patch(&mut self, patch: &Patch) -> Vec<ElementId> {
        patch.apply_to(&mut self.doc)
    }

    /// Purge a live draft (undo of a creation). Returns the removed copy.
    pub(crate) fn purge(&mut self, id: &str) -> Option<Element> {
        let element = self.doc.element(id)?.clone();
        let _ = self.doc.apply(&Delta::Purge {
            element: element.clone(),
        });
        Some(element)
    }

    #[cfg(test)]
    pub(crate) fn document_mut(&mut self) -> &mut Document {
        &mut self.doc
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;
    use crate::model::{ElementKind, ShapeKind, Style};

    fn shape(x: f64) -> ElementKind {
        ElementKind::Shape {
            shape: ShapeKind::Rectangle,
            rect: Rect::new(x, 0.0, 10.0, 10.0),
            style: Style::default(),
            bound_text_id: None,
        }
    }

    #[test]
    fn replace_resets_history_but_merge_keeps_it() {
        let mut session = DocumentSession::new(Document::new());
        let mut tx = Transaction::begin();
        session
            .add(&mut tx, Element::new(shape(0.0), "a0".into(), 1))
            .unwrap();
        session.commit(tx);
        assert!(session.can_undo());

        // A remote snapshot merge must not throw away local undo.
        let merged = session.document().clone();
        session.replace_merged(merged);
        assert!(session.can_undo(), "merge keeps the local journal");

        session.replace(Document::new());
        assert!(!session.can_undo(), "opening a file starts a fresh journal");
        assert!(!session.can_redo());
    }

    #[test]
    fn repeated_pastes_move_away_from_the_source() {
        let mut session = DocumentSession::new(Document::new());
        let mut tx = Transaction::begin();
        session
            .add(&mut tx, Element::new(shape(0.0), "a0".into(), 1))
            .unwrap();
        session.commit(tx);
        let source = session.document().ordered_live()[0].clone();
        session.copy_to_clipboard(vec![source]);

        let (_, first) = session.take_paste().expect("clipboard has a copy");
        let (_, second) = session.take_paste().expect("clipboard has a copy");
        assert_eq!(first, 12.0);
        assert_eq!(second, 24.0);
    }
}
