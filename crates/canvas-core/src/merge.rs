//! Snapshot reconciliation: merge two documents deterministically.
//!
//! Implements ADR-0002's merge-ready model: a pure `reconcile(local, remote)`
//! with documented last-writer-wins rules that a CRDT can later replace
//! without touching the document model. Snapshots and patches share the same
//! element policy ([`merge_elements`]); a snapshot is only a complete set of
//! elements, not an authoritative statement that absent elements were deleted.
//!
//! Rules:
//! - The result is the union of element IDs; absence is *not* deletion —
//!   only a tombstone (`isDeleted`) deletes.
//! - Both sides present: higher `version` wins; equal version, lower
//!   `versionNonce` wins; still tied, the lexicographically smaller JSON wins
//!   (a symmetric tiebreak so every peer converges on the same element).
//! - Collided order keys are regenerated deterministically from the sorted
//!   (index, id) order, so peers converge on the same z-order. Patches run
//!   the same pass, so a collision resolves identically whichever path
//!   carried the elements.

use std::collections::BTreeMap;

use crate::index;
use crate::model::{Document, Element, ElementId};

/// One element changed by a merge, with the copy it replaced on the receiver.
#[derive(Clone, Debug, PartialEq)]
pub struct ElementChange {
    pub before: Option<Element>,
    pub after: Element,
}

/// The result of merging incoming elements into a document.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MergeOutcome {
    /// Stored elements that changed, with the receiver's previous copy.
    pub changed: Vec<ElementChange>,
    /// Incoming copies that lost to a stored copy, as the stored winner.
    /// A sender applies these to catch up instead of overwriting the winner.
    pub corrections: Vec<Element>,
    /// Incoming IDs whose copy is the stored winner.
    pub accepted: Vec<ElementId>,
}

/// The result of reconciling a complete remote snapshot into a local document.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReconcileOutcome {
    pub document: Document,
    /// Stored elements that changed, with the local previous copy.
    pub changed: Vec<ElementChange>,
    /// Elements the snapshot uploader must apply to match the merged document:
    /// stored live elements it omitted, or stored winners where its copy lost.
    pub corrections: Vec<Element>,
    /// Incoming IDs whose copy is the stored winner.
    pub accepted: Vec<ElementId>,
}

/// Merge two documents into one deterministic result.
pub fn reconcile(local: &Document, remote: &Document) -> Document {
    reconcile_document(local, remote).document
}

/// Merge a complete remote snapshot and report what the receiver changed and
/// what the remote side still needs. Absence in `remote` never deletes.
pub fn reconcile_document(local: &Document, remote: &Document) -> ReconcileOutcome {
    let mut document = local.clone();
    let merged = merge_elements(&mut document, remote.elements.values());
    let mut corrections = Vec::new();
    for (id, stored) in &document.elements {
        match remote.elements.get(id) {
            Some(uploaded) if uploaded == stored => {}
            Some(_) => corrections.push(stored.clone()),
            // A page does not need a tombstone it never had; a live element
            // the page omitted is a re-download, not a deletion.
            None if !stored.is_deleted => corrections.push(stored.clone()),
            None => {}
        }
    }
    ReconcileOutcome {
        document,
        changed: merged.changed,
        corrections,
        accepted: merged.accepted,
    }
}

/// Merge incoming full elements (patch or snapshot members) into `doc` by the
/// same last-writer-wins and index-collision rules. This is the one policy
/// seam both wire paths use.
pub fn merge_elements<'a>(
    doc: &mut Document,
    incoming: impl IntoIterator<Item = &'a Element>,
) -> MergeOutcome {
    let incoming: Vec<&Element> = incoming.into_iter().collect();
    // Capture the receiver's copy before any change so the outcome can carry
    // before/after pairs for change reporting.
    let mut before: BTreeMap<ElementId, Option<Element>> = BTreeMap::new();
    for element in &incoming {
        before
            .entry(element.id.clone())
            .or_insert_with(|| doc.elements.get(&element.id).cloned());
    }
    for element in &incoming {
        match doc.elements.get(&element.id) {
            Some(current) => {
                let winner = pick_winner(current, element);
                if winner != *current {
                    doc.elements.insert(element.id.clone(), winner);
                }
            }
            None => {
                doc.elements.insert(element.id.clone(), (*element).clone());
            }
        }
    }
    dedupe_indices(doc, &mut before);

    let mut changed = Vec::new();
    for (id, previous) in &before {
        if let Some(after) = doc.elements.get(id) {
            if Some(after) != previous.as_ref() {
                changed.push(ElementChange {
                    before: previous.clone(),
                    after: after.clone(),
                });
            }
        }
    }

    // A repeated id is decided by its last incoming copy, matching
    // `Patch::from_deltas` and wire decode order.
    let mut final_incoming: BTreeMap<&str, &Element> = BTreeMap::new();
    for element in &incoming {
        final_incoming.insert(element.id.as_str(), element);
    }
    let mut accepted = Vec::new();
    let mut corrections = Vec::new();
    for (id, element) in final_incoming {
        match doc.elements.get(id) {
            Some(stored) if stored == element => accepted.push(id.to_string()),
            Some(stored) => corrections.push(stored.clone()),
            None => {}
        }
    }
    MergeOutcome {
        changed,
        corrections,
        accepted,
    }
}

/// Pick the winning side for an element present in both documents.
pub fn pick_winner(a: &Element, b: &Element) -> Element {
    if a.version != b.version {
        return if a.version > b.version {
            a.clone()
        } else {
            b.clone()
        };
    }
    if a.version_nonce != b.version_nonce {
        return if a.version_nonce < b.version_nonce {
            a.clone()
        } else {
            b.clone()
        };
    }
    let ja = serde_json::to_string(a).unwrap_or_default();
    let jb = serde_json::to_string(b).unwrap_or_default();
    if ja <= jb {
        a.clone()
    } else {
        b.clone()
    }
}

/// Regenerate order keys that collided, deterministically from sorted
/// (index, id) order. Local-only elements touched by the pass are recorded in
/// `before` so the caller reports them as changed too.
fn dedupe_indices(doc: &mut Document, before: &mut BTreeMap<ElementId, Option<Element>>) {
    let mut ordered: Vec<(String, String)> = doc
        .elements
        .values()
        .map(|e| (e.index.clone(), e.id.clone()))
        .collect();
    ordered.sort();

    let mut i = 0;
    while i < ordered.len() {
        let index_value = ordered[i].0.clone();
        let mut j = i + 1;
        while j < ordered.len() && ordered[j].0 == index_value {
            j += 1;
        }
        if j - i > 1 {
            let next = ordered.get(j).map(|(idx, _)| idx.clone());
            let mut last = index_value.clone();
            for (_, id) in ordered.iter().take(j).skip(i + 1) {
                let regenerated = index::key_between(Some(&last), next.as_deref())
                    .expect("index space exhausted while resolving collisions");
                before
                    .entry(id.clone())
                    .or_insert_with(|| doc.elements.get(id).cloned());
                if let Some(element) = doc.elements.get_mut(id) {
                    element.index = regenerated.clone();
                }
                last = regenerated;
            }
        }
        i = j;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    use crate::geometry::Rect;
    use crate::model::{ElementKind, ShapeKind, Style};

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

    fn shape_at(x: f64) -> ElementKind {
        ElementKind::Shape {
            shape: ShapeKind::Rectangle,
            rect: Rect::new(x, 0.0, 10.0, 10.0),
            style: Style::default(),
            bound_text_id: None,
        }
    }

    fn element(id: &str, index: &str, version: u64, nonce: u32, x: f64) -> Element {
        let mut e = Element::new(shape_at(x), index.into(), 0).with_id(id);
        e.version = version;
        e.version_nonce = nonce;
        e
    }

    #[test]
    fn reconcile_of_identical_documents_is_identity() {
        let mut doc = Document::new();
        doc.add(shape_at(0.0), 1);
        doc.add(shape_at(20.0), 2);
        assert_eq!(reconcile(&doc, &doc), doc);
    }

    #[test]
    fn union_keeps_absent_elements() {
        let mut a = Document::new();
        a.insert(element("one", "a0", 1, 1, 0.0));
        let mut b = Document::new();
        b.insert(element("two", "a1", 1, 1, 0.0));
        let merged = reconcile(&a, &b);
        assert!(merged.elements.contains_key("one"));
        assert!(merged.elements.contains_key("two"));
    }

    #[test]
    fn higher_version_wins_regardless_of_side() {
        let mut a = Document::new();
        a.insert(element("e", "a0", 5, 9, 1.0));
        let mut b = Document::new();
        b.insert(element("e", "a0", 6, 1, 2.0));
        let ab = reconcile(&a, &b);
        let ba = reconcile(&b, &a);
        assert_eq!(ab, ba);
        assert_eq!(ab.element("e").unwrap().version, 6);
    }

    #[test]
    fn lower_nonce_breaks_version_ties() {
        let mut a = Document::new();
        a.insert(element("e", "a0", 3, 100, 1.0));
        let mut b = Document::new();
        b.insert(element("e", "a0", 3, 50, 2.0));
        assert_eq!(reconcile(&a, &b), reconcile(&b, &a));
        assert_eq!(reconcile(&a, &b).element("e").unwrap().version_nonce, 50);
    }

    #[test]
    fn tombstone_wins_unless_a_strictly_newer_edit_exists() {
        let live = element("e", "a0", 4, 1, 0.0);
        let mut deleted = live.clone();
        deleted.is_deleted = true;
        deleted.version = 5;

        let mut a = Document::new();
        a.insert(live.clone());
        let mut b = Document::new();
        b.insert(deleted.clone());
        assert!(reconcile(&a, &b).element("e").unwrap().is_deleted);
        assert!(reconcile(&b, &a).element("e").unwrap().is_deleted);

        // A strictly newer live edit resurrects.
        let mut revived = live.clone();
        revived.version = 6;
        let mut c = Document::new();
        c.insert(revived);
        assert!(!reconcile(&b, &c).element("e").unwrap().is_deleted);
        assert!(!reconcile(&c, &b).element("e").unwrap().is_deleted);
    }

    #[test]
    fn collided_indices_are_regenerated_deterministically() {
        let mut a = Document::new();
        a.insert(element("aaa", "a0", 1, 1, 0.0));
        let mut b = Document::new();
        b.insert(element("bbb", "a0", 1, 1, 5.0));

        let ab = reconcile(&a, &b);
        let ba = reconcile(&b, &a);
        assert_eq!(ab, ba, "collision resolution is symmetric");

        let indices: Vec<String> = ab
            .ordered_ids()
            .into_iter()
            .map(|id| ab.element(&id).unwrap().index.clone())
            .collect();
        assert_ne!(indices[0], indices[1], "colliding keys were regenerated");
        assert!(indices[0] < indices[1]);
    }

    #[test]
    fn independent_edits_commute() {
        let mut base = Document::new();
        for i in 0..4 {
            base.insert(element(
                &format!("e{i}"),
                &format!("a{i}"),
                1,
                10 + i as u32,
                i as f64,
            ));
        }

        // Side A updates e0, side B deletes e3 — disjoint edits.
        let mut a = base.clone();
        let mut e0 = a.element("e0").unwrap().clone();
        e0.version = 2;
        e0.kind = shape_at(99.0);
        a.insert(e0);

        let mut b = base.clone();
        let mut e3 = b.element("e3").unwrap().clone();
        e3.version = 2;
        e3.is_deleted = true;
        b.insert(e3);

        assert_eq!(reconcile(&a, &b), reconcile(&b, &a));
    }

    #[test]
    fn randomized_merge_is_idempotent_and_convergent() {
        let mut base = Document::new();
        for i in 0..10 {
            base.insert(element(
                &format!("e{i:02}"),
                &format!("a{i}"),
                1,
                7,
                i as f64,
            ));
        }

        let mut rng = Lcg(7);
        for _ in 0..200 {
            let mut a = base.clone();
            let mut b = base.clone();
            for side in [&mut a, &mut b] {
                for _ in 0..3 {
                    let id = format!("e{:02}", rng.below(10));
                    let mut e = side.element(&id).unwrap().clone();
                    e.version += 1;
                    e.version_nonce = rng.below(1_000_000) as u32;
                    match rng.below(4) {
                        0 => e.is_deleted = true,
                        1 => e.kind = shape_at(rng.below(100) as f64),
                        2 => e.updated += 1,
                        _ => {}
                    }
                    side.insert(e);
                }
            }

            let ab = reconcile(&a, &b);
            let ba = reconcile(&b, &a);
            assert_eq!(ab, ba, "merge is convergent");

            let ab_again = reconcile(&ab, &b);
            assert_eq!(ab_again, ab, "merge is idempotent against the same side");

            let union: BTreeSet<&String> = a.elements.keys().chain(b.elements.keys()).collect();
            let merged: BTreeSet<&String> = ab.elements.keys().collect();
            assert_eq!(union, merged, "merge keeps the id union");
        }
    }

    #[test]
    fn reconcile_reports_missing_live_elements_as_corrections() {
        let mut local = Document::new();
        local.insert(element("kept", "a0", 4, 1, 0.0));
        let mut remote = Document::new();
        remote.insert(element("new", "a1", 1, 1, 5.0));

        let outcome = reconcile_document(&local, &remote);
        assert!(outcome.document.elements.contains_key("kept"));
        assert!(outcome.document.elements.contains_key("new"));
        assert_eq!(
            outcome
                .corrections
                .iter()
                .map(|element| element.id.as_str())
                .collect::<Vec<_>>(),
            vec!["kept"],
            "an omitted live element is re-downloaded, never deleted"
        );
        assert_eq!(outcome.accepted, vec!["new".to_string()]);
    }

    #[test]
    fn reconcile_reports_a_stale_upload_as_a_correction() {
        let mut local = Document::new();
        local.insert(element("one", "a0", 5, 1, 10.0));
        let mut remote = Document::new();
        remote.insert(element("one", "a0", 2, 1, 0.0));

        let outcome = reconcile_document(&local, &remote);
        assert_eq!(outcome.document.element("one").unwrap().version, 5);
        assert_eq!(outcome.corrections.len(), 1);
        assert_eq!(outcome.corrections[0].version, 5);
        assert_eq!(outcome.accepted, Vec::<String>::new());
        assert!(outcome.changed.is_empty(), "the receiver was not changed");
    }

    #[test]
    fn snapshot_and_patch_paths_resolve_index_collisions_identically() {
        let first = element("aaa", "a0", 1, 1, 0.0);
        let second = element("bbb", "a0", 1, 1, 5.0);

        let mut remote = Document::new();
        remote.insert(first.clone());
        remote.insert(second.clone());
        let snapshot = reconcile(&Document::new(), &remote);

        let mut patched = Document::new();
        merge_elements(&mut patched, [&first]);
        merge_elements(&mut patched, [&second]);

        assert_eq!(patched, snapshot);
        assert_ne!(
            patched.element("aaa").unwrap().index,
            patched.element("bbb").unwrap().index
        );
    }

    #[test]
    fn merge_reports_accepted_lost_and_reindexed_incoming_elements() {
        let mut doc = Document::new();
        doc.insert(element("one", "a0", 5, 1, 10.0));
        doc.insert(element("two", "a1", 1, 1, 20.0));

        let mut stale = element("one", "a0", 2, 1, 0.0);
        stale.version = 2;
        let mut colliding = element("three", "a1", 1, 1, 30.0);
        colliding.version = 1;

        let outcome = merge_elements(&mut doc, [&stale, &colliding]);
        assert_eq!(outcome.accepted, vec!["three".to_string()]);
        assert_eq!(outcome.corrections.len(), 1);
        assert_eq!(outcome.corrections[0].id, "one");
        assert_eq!(outcome.corrections[0].version, 5);
        // The new element collided with `two`; both paths keep distinct keys.
        assert_ne!(
            doc.element("two").unwrap().index,
            doc.element("three").unwrap().index
        );
        assert!(outcome
            .changed
            .iter()
            .any(|change| change.after.id == "three"));
    }
}
