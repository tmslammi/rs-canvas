//! Transactions and history: gesture-sized undo/redo built on deltas.
//!
//! A transaction applies deltas immediately (so drags preview live) and is
//! only pushed onto the history when committed. Consecutive updates to the
//! same element inside one transaction coalesce, so a drag is one undo step.

use crate::delta::{Delta, EditError};
use crate::model::{Document, Element, ElementId};

/// A recorded, closed group of deltas: one user gesture.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Transaction {
    deltas: Vec<Delta>,
}

impl Transaction {
    pub fn begin() -> Self {
        Self::default()
    }

    pub fn deltas(&self) -> &[Delta] {
        &self.deltas
    }

    pub fn is_empty(&self) -> bool {
        self.deltas.is_empty()
    }

    pub fn len(&self) -> usize {
        self.deltas.len()
    }

    /// Apply a delta and record it.
    pub fn push_applied(&mut self, doc: &mut Document, delta: Delta) -> Result<(), EditError> {
        doc.apply(&delta)?;
        self.push(delta);
        Ok(())
    }

    /// Record an already-applied delta (used for coalescing-friendly flows).
    pub fn push(&mut self, delta: Delta) {
        if let Some(last) = self.deltas.last_mut() {
            let coalesces = match (&mut *last, &delta) {
                (
                    Delta::Update { after, .. },
                    Delta::Update {
                        after: new_after, ..
                    },
                ) => after.id == new_after.id,
                (
                    Delta::Add { element },
                    Delta::Update {
                        after: new_after, ..
                    },
                ) => element.id == new_after.id,
                _ => false,
            };
            if coalesces {
                match delta {
                    Delta::Update {
                        after: new_after, ..
                    } => match last {
                        Delta::Update { after, .. } => *after = new_after,
                        Delta::Add { element } => *element = new_after,
                        _ => unreachable!("coalesce implies a recorded change"),
                    },
                    _ => unreachable!("coalesce implies an update delta"),
                }
                return;
            }
        }
        self.deltas.push(delta);
    }

    /// Insert a new element (top of the stack unless the caller set an index).
    pub fn add(&mut self, doc: &mut Document, element: Element) -> Result<(), EditError> {
        self.push_applied(doc, Delta::Add { element })
    }

    /// Tombstone an element.
    pub fn remove(&mut self, doc: &mut Document, id: &str, now_ms: u64) -> Result<(), EditError> {
        let before = doc
            .element(id)
            .ok_or_else(|| EditError::UnknownElement(id.to_string()))?
            .clone();
        let mut tombstoned = before.clone();
        tombstoned.is_deleted = true;
        tombstoned.touch(now_ms);
        self.push_applied(doc, Delta::Remove { before, tombstoned })
    }

    /// Mutate an element and bump its version.
    pub fn update<F>(
        &mut self,
        doc: &mut Document,
        id: &str,
        now_ms: u64,
        mutate: F,
    ) -> Result<(), EditError>
    where
        F: FnOnce(&mut Element),
    {
        let before = doc
            .element(id)
            .ok_or_else(|| EditError::UnknownElement(id.to_string()))?
            .clone();
        let mut after = before.clone();
        mutate(&mut after);
        if after.content_eq(&before) {
            return Ok(());
        }
        after.touch(now_ms);
        self.push_applied(doc, Delta::Update { before, after })
    }

    /// Close the transaction and push it onto the history.
    pub fn commit(self, history: &mut History) {
        if !self.deltas.is_empty() {
            history.outbox.extend(self.deltas.iter().cloned());
            history.undo_stack.push(self);
            history.redo_stack.clear();
        }
    }
}

/// Undo/redo stacks of committed transactions.
#[derive(Clone, Debug, Default)]
pub struct History {
    undo_stack: Vec<Transaction>,
    redo_stack: Vec<Transaction>,
    /// Deltas applied since the last drain, in application order. Commits,
    /// undos and redos all land here; the page bridge projects them into wire
    /// patches so peers see every change, not just the document snapshots.
    outbox: Vec<Delta>,
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    /// Commit a transaction without going through `Transaction::commit`.
    pub fn commit(&mut self, tx: Transaction) {
        tx.commit(self);
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    pub fn undo_depth(&self) -> usize {
        self.undo_stack.len()
    }

    /// Undo the most recent transaction; returns the ids it touched so the
    /// caller can repaint exactly those layers. An empty vec means the stack
    /// was empty.
    pub fn undo(&mut self, doc: &mut Document) -> Result<Vec<ElementId>, EditError> {
        match self.undo_stack.pop() {
            None => Ok(Vec::new()),
            Some(tx) => {
                let ids: Vec<ElementId> = tx
                    .deltas()
                    .iter()
                    .map(|delta| delta.element_id().clone())
                    .collect();
                for delta in tx.deltas().iter().rev() {
                    let inverse = delta.invert();
                    doc.apply(&inverse)?;
                    self.outbox.push(inverse);
                }
                self.redo_stack.push(tx);
                Ok(ids)
            }
        }
    }

    /// Redo the most recently undone transaction; returns the ids it touched.
    pub fn redo(&mut self, doc: &mut Document) -> Result<Vec<ElementId>, EditError> {
        match self.redo_stack.pop() {
            None => Ok(Vec::new()),
            Some(tx) => {
                let ids: Vec<ElementId> = tx
                    .deltas()
                    .iter()
                    .map(|delta| delta.element_id().clone())
                    .collect();
                for delta in tx.deltas() {
                    doc.apply(delta)?;
                    self.outbox.push(delta.clone());
                }
                self.undo_stack.push(tx);
                Ok(ids)
            }
        }
    }

    /// Take every delta applied since the last drain.
    pub fn drain_outbox(&mut self) -> Vec<Delta> {
        std::mem::take(&mut self.outbox)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;
    use crate::model::{ElementKind, ShapeKind};
    use crate::serialization::to_json;

    fn shape(x: f64, y: f64) -> ElementKind {
        ElementKind::Shape {
            shape: ShapeKind::Rectangle,
            rect: Rect::new(x, y, 10.0, 10.0),
            style: Default::default(),
            bound_text_id: None,
        }
    }

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() as usize) % n
        }
    }

    #[test]
    fn add_commit_undo_redo() {
        let mut doc = Document::new();
        let mut history = History::new();

        let mut tx = Transaction::begin();
        let element = Element::new(shape(0.0, 0.0), "a0".into(), 1);
        let id = element.id.clone();
        tx.add(&mut doc, element).unwrap();
        tx.commit(&mut history);

        assert_eq!(doc.ordered_live().len(), 1);
        assert!(!history.undo(&mut doc).unwrap().is_empty());
        assert_eq!(doc.ordered_live().len(), 0);
        assert!(!history.redo(&mut doc).unwrap().is_empty());
        assert_eq!(doc.ordered_live().len(), 1);
        assert_eq!(doc.ordered_live()[0].id, id);
    }

    #[test]
    fn outbox_records_commits_undos_and_redos() {
        let mut doc = Document::new();
        let mut history = History::new();

        let mut tx = Transaction::begin();
        let element = Element::new(shape(0.0, 0.0), "a0".into(), 1);
        tx.add(&mut doc, element).unwrap();
        tx.commit(&mut history);

        let committed = history.drain_outbox();
        assert_eq!(committed.len(), 1);
        assert!(matches!(committed[0], Delta::Add { .. }));
        assert!(history.drain_outbox().is_empty(), "a drain empties the box");

        history.undo(&mut doc).unwrap();
        let undone = history.drain_outbox();
        assert_eq!(undone.len(), 1);
        assert!(
            matches!(undone[0], Delta::Purge { .. }),
            "undoing an add purges the element"
        );

        history.redo(&mut doc).unwrap();
        let redone = history.drain_outbox();
        assert_eq!(redone.len(), 1);
        assert!(matches!(redone[0], Delta::Add { .. }));
    }

    #[test]
    fn remove_undo_restores_exactly() {
        let mut doc = Document::new();
        let mut history = History::new();
        let id = doc.add(shape(1.0, 2.0), 1);
        let before = doc.element(&id).unwrap().clone();

        let mut tx = Transaction::begin();
        tx.remove(&mut doc, &id, 10).unwrap();
        tx.commit(&mut history);
        assert!(doc.element(&id).unwrap().is_deleted);

        assert!(!history.undo(&mut doc).unwrap().is_empty());
        assert_eq!(doc.element(&id), Some(&before));
    }

    #[test]
    fn noop_updates_are_not_recorded() {
        let mut doc = Document::new();
        let mut history = History::new();
        let id = doc.add(shape(0.0, 0.0), 1);
        let before = doc.element(&id).unwrap().clone();

        let mut tx = Transaction::begin();
        tx.update(&mut doc, &id, 2, |_| {}).unwrap();
        assert!(tx.is_empty(), "a no-op update records nothing");
        assert_eq!(doc.element(&id), Some(&before), "metadata did not change");
        tx.commit(&mut history);
        assert!(!history.can_undo());
    }

    #[test]
    fn drag_updates_coalesce_into_one_undo_step() {
        let mut doc = Document::new();
        let mut history = History::new();
        let id = doc.add(shape(0.0, 0.0), 1);
        let before = doc.element(&id).unwrap().clone();

        let mut tx = Transaction::begin();
        for step in 0..25 {
            tx.update(&mut doc, &id, step as u64, |e| {
                if let ElementKind::Shape { rect, .. } = &mut e.kind {
                    rect.x += 1.0;
                }
            })
            .unwrap();
        }
        assert_eq!(tx.len(), 1, "updates on one element coalesce");
        tx.commit(&mut history);

        assert!(!history.undo(&mut doc).unwrap().is_empty());
        assert_eq!(doc.element(&id), Some(&before));
        assert!(!history.redo(&mut doc).unwrap().is_empty());
        if let ElementKind::Shape { rect, .. } = &doc.element(&id).unwrap().kind {
            assert_eq!(rect.x, 25.0);
        } else {
            panic!("expected shape");
        }
    }

    #[test]
    fn empty_transaction_is_not_recorded() {
        let mut history = History::new();
        let tx = Transaction::begin();
        tx.commit(&mut history);
        assert!(!history.can_undo());
    }

    #[test]
    fn new_commit_clears_redo() {
        let mut doc = Document::new();
        let mut history = History::new();

        let mut tx = Transaction::begin();
        tx.add(&mut doc, Element::new(shape(0.0, 0.0), "a0".into(), 1))
            .unwrap();
        tx.commit(&mut history);
        history.undo(&mut doc).unwrap();
        assert!(history.can_redo());

        let mut tx = Transaction::begin();
        tx.add(&mut doc, Element::new(shape(20.0, 0.0), "a1".into(), 2))
            .unwrap();
        tx.commit(&mut history);
        assert!(!history.can_redo());
        assert!(!history.undo(&mut doc).unwrap().is_empty());
    }

    #[test]
    fn randomized_undo_all_redo_all_restores_every_state() {
        let mut doc = Document::new();
        let mut history = History::new();
        for i in 0..5 {
            doc.add(shape(i as f64 * 20.0, 0.0), 1);
        }

        let mut rng = Lcg(42);
        let mut snapshots = vec![to_json(&doc).unwrap()];

        for step in 0..60u64 {
            let mut tx = Transaction::begin();
            match rng.below(3) {
                0 => {
                    let index = match doc.top_index() {
                        Some(top) => crate::index::key_between(Some(&top), None).unwrap(),
                        None => crate::index::key_between(None, None).unwrap(),
                    };
                    tx.add(
                        &mut doc,
                        Element::new(
                            shape(rng.below(500) as f64, rng.below(400) as f64),
                            index,
                            step,
                        ),
                    )
                    .unwrap();
                }
                1 => {
                    let ids: Vec<String> =
                        doc.ordered_live().iter().map(|e| e.id.clone()).collect();
                    if !ids.is_empty() {
                        let id = ids[rng.below(ids.len())].clone();
                        tx.update(&mut doc, &id, step, |e| {
                            if let ElementKind::Shape { rect, .. } = &mut e.kind {
                                rect.x += 1.0;
                            }
                        })
                        .unwrap();
                    }
                }
                _ => {
                    let ids: Vec<String> =
                        doc.ordered_live().iter().map(|e| e.id.clone()).collect();
                    if !ids.is_empty() {
                        let id = ids[rng.below(ids.len())].clone();
                        tx.remove(&mut doc, &id, step).unwrap();
                    }
                }
            }
            let recorded = !tx.is_empty();
            tx.commit(&mut history);
            if recorded {
                snapshots.push(to_json(&doc).unwrap());
            }
        }

        for (k, expected) in snapshots.iter().enumerate().rev().skip(1) {
            assert!(!history.undo(&mut doc).unwrap().is_empty());
            assert_eq!(&to_json(&doc).unwrap(), expected, "undo to step {k}");
        }
        assert!(history.undo(&mut doc).unwrap().is_empty());

        for expected in snapshots.iter().skip(1) {
            assert!(!history.redo(&mut doc).unwrap().is_empty());
            assert_eq!(&to_json(&doc).unwrap(), expected);
        }
        assert!(history.redo(&mut doc).unwrap().is_empty());
    }
}
