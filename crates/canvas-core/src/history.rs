//! Transactions and history: gesture-sized undo/redo built on deltas.
//!
//! A transaction applies deltas immediately (so drags preview live) and is
//! only pushed onto the history when committed. Consecutive updates to the
//! same element inside one transaction coalesce, so a drag is one undo step.

use crate::delta::{Delta, EditError};
use crate::id::new_version_nonce;
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

    /// Stamp a freshly written element: strictly newer than both the copy
    /// currently in the document (if any) and the historical copy being
    /// replayed. An undo or redo is a new edit, not a rewind of the clock —
    /// without this the merge rules read the replayed copy as stale and a
    /// synced peer "corrects" it back to the pre-undo state.
    fn freshen(current: Option<&Element>, mut target: Element, now_ms: u64) -> Element {
        let base = current
            .map(|element| element.version)
            .unwrap_or(0)
            .max(target.version);
        target.version = base + 1;
        target.version_nonce = new_version_nonce();
        target.updated = now_ms;
        target
    }

    /// Undo the most recent transaction; returns the ids it touched so the
    /// caller can repaint exactly those layers. An empty vec means the stack
    /// was empty.
    pub fn undo(&mut self, doc: &mut Document, now_ms: u64) -> Result<Vec<ElementId>, EditError> {
        let Some(mut tx) = self.undo_stack.pop() else {
            return Ok(Vec::new());
        };
        let ids: Vec<ElementId> = tx
            .deltas()
            .iter()
            .map(|delta| delta.element_id().clone())
            .collect();
        for index in (0..tx.deltas.len()).rev() {
            match tx.deltas[index].clone() {
                Delta::Add { .. } => {
                    let id = tx.deltas[index].element_id().clone();
                    let current = doc
                        .element(&id)
                        .cloned()
                        .ok_or(EditError::UnknownElement(id))?;
                    // Purge the live copy; the wire projects the purge as a
                    // freshly touched tombstone, so peers accept the deletion.
                    let purge = Delta::Purge {
                        element: current.clone(),
                    };
                    doc.apply(&purge)?;
                    self.outbox.push(purge);
                    // Peers now hold a tombstone one version newer than the
                    // purged copy; record that lineage on the stored delta so
                    // a redo lands one newer still.
                    let mut lineage = current;
                    lineage.touch(now_ms);
                    tx.deltas[index] = Delta::Add { element: lineage };
                }
                Delta::Update { before, .. } | Delta::Remove { before, .. } => {
                    let id = before.id.clone();
                    let current = doc
                        .element(&id)
                        .cloned()
                        .ok_or(EditError::UnknownElement(id))?;
                    let restored = Delta::Update {
                        before: current.clone(),
                        after: Self::freshen(Some(&current), before, now_ms),
                    };
                    doc.apply(&restored)?;
                    self.outbox.push(restored);
                }
                Delta::Purge { .. } => {
                    // A purge is only ever an inverse (undo of a creation); it
                    // never sits on the undo stack as a forward delta.
                    debug_assert!(false, "a purge cannot be undone");
                    let inverse = tx.deltas[index].clone().invert();
                    doc.apply(&inverse)?;
                    self.outbox.push(inverse);
                }
            }
        }
        self.redo_stack.push(tx);
        Ok(ids)
    }

    /// Redo the most recently undone transaction; returns the ids it touched.
    /// Like undo, every replayed write is stamped fresh so it wins the merge.
    pub fn redo(&mut self, doc: &mut Document, now_ms: u64) -> Result<Vec<ElementId>, EditError> {
        let Some(tx) = self.redo_stack.pop() else {
            return Ok(Vec::new());
        };
        let ids: Vec<ElementId> = tx
            .deltas()
            .iter()
            .map(|delta| delta.element_id().clone())
            .collect();
        for delta in tx.deltas() {
            let applied = match delta {
                Delta::Add { element } => Delta::Add {
                    element: Self::freshen(doc.element(&element.id), element.clone(), now_ms),
                },
                Delta::Update { after, .. } => {
                    let current = doc
                        .element(&after.id)
                        .cloned()
                        .ok_or_else(|| EditError::UnknownElement(after.id.clone()))?;
                    Delta::Update {
                        before: current.clone(),
                        after: Self::freshen(Some(&current), after.clone(), now_ms),
                    }
                }
                Delta::Remove { tombstoned, .. } => {
                    let current = doc
                        .element(&tombstoned.id)
                        .cloned()
                        .ok_or_else(|| EditError::UnknownElement(tombstoned.id.clone()))?;
                    Delta::Remove {
                        before: current.clone(),
                        tombstoned: Self::freshen(Some(&current), tombstoned.clone(), now_ms),
                    }
                }
                Delta::Purge { .. } => {
                    // See undo: a purge is never a forward delta.
                    debug_assert!(false, "a purge cannot be redone");
                    delta.clone()
                }
            };
            doc.apply(&applied)?;
            self.outbox.push(applied);
        }
        self.undo_stack.push(tx);
        Ok(ids)
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
        assert!(!history.undo(&mut doc, 9).unwrap().is_empty());
        assert_eq!(doc.ordered_live().len(), 0);
        assert!(!history.redo(&mut doc, 10).unwrap().is_empty());
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

        history.undo(&mut doc, 9).unwrap();
        let undone = history.drain_outbox();
        assert_eq!(undone.len(), 1);
        assert!(
            matches!(undone[0], Delta::Purge { .. }),
            "undoing an add purges the element"
        );

        history.redo(&mut doc, 10).unwrap();
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

        assert!(!history.undo(&mut doc, 9).unwrap().is_empty());
        let restored = doc.element(&id).expect("undo brings the element back");
        assert!(restored.content_eq(&before), "undo restores the content");
        assert!(
            restored.version > before.version,
            "undo is a new edit: the version advances"
        );
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

        assert!(!history.undo(&mut doc, 9).unwrap().is_empty());
        let restored = doc.element(&id).expect("undo brings the element back");
        assert!(restored.content_eq(&before), "undo restores the content");
        assert!(
            restored.version > before.version,
            "undo is a new edit: the version advances"
        );
        assert!(!history.redo(&mut doc, 10).unwrap().is_empty());
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
        history.undo(&mut doc, 9).unwrap();
        assert!(history.can_redo());

        let mut tx = Transaction::begin();
        tx.add(&mut doc, Element::new(shape(20.0, 0.0), "a1".into(), 2))
            .unwrap();
        tx.commit(&mut history);
        assert!(!history.can_redo());
        assert!(!history.undo(&mut doc, 9).unwrap().is_empty());
    }

    /// Undo and redo restamp merge metadata, so snapshots compare equal only
    /// once the version stamps are stripped — content is what must round-trip.
    fn normalized(doc: &Document) -> String {
        let mut doc = doc.clone();
        for element in doc.elements.values_mut() {
            element.version = 0;
            element.version_nonce = 0;
            element.updated = 0;
        }
        to_json(&doc).unwrap()
    }

    #[test]
    fn randomized_undo_all_redo_all_restores_every_state() {
        let mut doc = Document::new();
        let mut history = History::new();
        for i in 0..5 {
            doc.add(shape(i as f64 * 20.0, 0.0), 1);
        }

        let mut rng = Lcg(42);
        let mut snapshots = vec![normalized(&doc)];

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
                snapshots.push(normalized(&doc));
            }
        }

        for (k, expected) in snapshots.iter().enumerate().rev().skip(1) {
            assert!(!history.undo(&mut doc, 1000 + k as u64).unwrap().is_empty());
            assert_eq!(&normalized(&doc), expected, "undo to step {k}");
        }
        assert!(history.undo(&mut doc, 2000).unwrap().is_empty());

        for expected in snapshots.iter().skip(1) {
            assert!(!history.redo(&mut doc, 3000).unwrap().is_empty());
            assert_eq!(&normalized(&doc), expected);
        }
        assert!(history.redo(&mut doc, 4000).unwrap().is_empty());
    }

    /// The sync regression: an undone move must win the merge on a peer that
    /// already holds the moved copy, or the peer "corrects" the page back.
    #[test]
    fn undo_of_update_wins_the_merge_on_a_synced_peer() {
        use crate::patch::Patch;

        let mut doc = Document::new();
        let mut history = History::new();
        let id = doc.add(shape(0.0, 0.0), 1);
        let base = doc.element(&id).unwrap().clone();

        let mut tx = Transaction::begin();
        tx.update(&mut doc, &id, 2, |e| {
            if let ElementKind::Shape { rect, .. } = &mut e.kind {
                rect.x += 10.0;
            }
        })
        .unwrap();
        tx.commit(&mut history);
        let moved_outbox = history.drain_outbox();
        let moved = doc.element(&id).unwrap().clone();
        assert!(moved.version > base.version);

        history.undo(&mut doc, 3).unwrap();
        let restored = doc.element(&id).unwrap().clone();
        assert!(restored.content_eq(&base), "undo restores the content");
        assert!(
            restored.version > moved.version,
            "undo is a new edit: the version advances past the move"
        );
        let undone_outbox = history.drain_outbox();

        // The peer absorbed the move before the undo arrived.
        let mut mirror = Document::new();
        mirror.insert(base.clone());
        Patch::from_deltas(&moved_outbox, 4).apply_to(&mut mirror);
        assert!(mirror.element(&id).unwrap().content_eq(&moved));

        Patch::from_deltas(&undone_outbox, 5).apply_to(&mut mirror);
        assert!(
            mirror.element(&id).unwrap().content_eq(&base),
            "the undo patch wins the merge; the move does not resurrect"
        );
    }

    /// Undoing a creation purges locally; the redo must beat the tombstone
    /// the peer synthesized from the purge.
    #[test]
    fn redo_of_creation_wins_over_the_undo_tombstone() {
        use crate::patch::Patch;

        let mut doc = Document::new();
        let mut history = History::new();
        let mut tx = Transaction::begin();
        tx.add(&mut doc, Element::new(shape(0.0, 0.0), "a0".into(), 1))
            .unwrap();
        tx.commit(&mut history);
        let id = doc.ordered_live()[0].id.clone();
        let created = history.drain_outbox();

        history.undo(&mut doc, 2).unwrap();
        assert!(doc.element(&id).is_none(), "undo purges the creation");
        let undone = history.drain_outbox();

        let mut mirror = Document::new();
        Patch::from_deltas(&created, 3).apply_to(&mut mirror);
        Patch::from_deltas(&undone, 4).apply_to(&mut mirror);
        assert!(mirror.element(&id).unwrap().is_deleted);

        history.redo(&mut doc, 5).unwrap();
        let live = doc.element(&id).expect("redo brings the element back");
        assert!(!live.is_deleted);
        let redone = history.drain_outbox();

        Patch::from_deltas(&redone, 6).apply_to(&mut mirror);
        let back = mirror.element(&id).unwrap();
        assert!(!back.is_deleted, "redo beats the undo tombstone");
        assert!(back.content_eq(live));
    }

    /// Repeated undo/redo of one move: content alternates, versions rise
    /// monotonically, so no cycle can tie and lose a merge.
    #[test]
    fn undo_redo_cycles_advance_versions_monotonically() {
        let mut doc = Document::new();
        let mut history = History::new();
        let id = doc.add(shape(0.0, 0.0), 1);
        let base = doc.element(&id).unwrap().clone();

        let mut tx = Transaction::begin();
        tx.update(&mut doc, &id, 2, |e| {
            if let ElementKind::Shape { rect, .. } = &mut e.kind {
                rect.x += 10.0;
            }
        })
        .unwrap();
        tx.commit(&mut history);
        let moved = doc.element(&id).unwrap().clone();

        let mut last_version = moved.version;
        for round in 0..3u64 {
            history.undo(&mut doc, 10 + round * 2).unwrap();
            let restored = doc.element(&id).unwrap().clone();
            assert!(restored.content_eq(&base), "round {round}: undo restores");
            assert!(
                restored.version > last_version,
                "round {round}: version rises"
            );
            last_version = restored.version;

            history.redo(&mut doc, 11 + round * 2).unwrap();
            let again = doc.element(&id).unwrap().clone();
            assert!(again.content_eq(&moved), "round {round}: redo re-applies");
            assert!(again.version > last_version, "round {round}: version rises");
            last_version = again.version;
        }
    }
}
