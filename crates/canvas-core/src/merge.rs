//! Snapshot reconciliation: merge two documents deterministically.
//!
//! This is the insurance ADR-0002 bought: a pure `reconcile(local, remote)`
//! with documented last-writer-wins rules that a CRDT can later replace
//! without touching the document model.
//!
//! Rules:
//! - The result is the union of element IDs; absence is *not* deletion —
//!   only a tombstone (`isDeleted`) deletes.
//! - Both sides present: higher `version` wins; equal version, lower
//!   `versionNonce` wins; still tied, the lexicographically smaller JSON wins
//!   (a symmetric tiebreak so every peer converges on the same element).
//! - Collided order keys are regenerated deterministically from the sorted
//!   (index, id) order, so peers converge on the same z-order.

use std::collections::BTreeSet;

use crate::index;
use crate::model::{Document, Element};

/// Merge two documents into one deterministic result.
pub fn reconcile(local: &Document, remote: &Document) -> Document {
    let mut out = Document::new();
    let ids: BTreeSet<&String> = local
        .elements
        .keys()
        .chain(remote.elements.keys())
        .collect();
    for id in ids {
        let element = match (local.elements.get(id), remote.elements.get(id)) {
            (Some(l), Some(r)) => pick_winner(l, r),
            (Some(l), None) => l.clone(),
            (None, Some(r)) => r.clone(),
            (None, None) => unreachable!("id collected from one of the two documents"),
        };
        out.elements.insert(id.clone(), element);
    }
    dedupe_indices(&mut out);
    out
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
/// (index, id) order.
fn dedupe_indices(doc: &mut Document) {
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
}
