//! Element-granular document updates for the wire.
//!
//! A patch is a set of full elements — tombstones included — carrying their
//! merge metadata. Applying one is idempotent and order-independent: each
//! element is merged with the stored copy by the same last-writer-wins rules
//! as `merge::reconcile`. Patches are the steady-state replacement for full
//! snapshots; snapshots remain for connect and resync.
//!
//! Patches are version-locked to the bridge: page and server ship together,
//! so elements on the wire carry no schema gate. Snapshots remain the
//! schema-checked, migrating entry point (`serialization::from_value`).

use serde::{Deserialize, Serialize};

use crate::delta::Delta;
use crate::merge::pick_winner;
use crate::model::{Document, Element, ElementId};

/// A set of element upserts (and tombstones) to merge into a document.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Patch {
    pub put: Vec<Element>,
}

impl Patch {
    /// Project a transaction's deltas onto the wire. `Purge` (undo of a
    /// creation) becomes a synthesized tombstone so peers can express the
    /// deletion; when an id appears more than once, the later entry wins.
    pub fn from_deltas(deltas: &[Delta], now_ms: u64) -> Patch {
        let mut put: Vec<Element> = Vec::new();
        for delta in deltas {
            let element = match delta {
                Delta::Add { element } => element.clone(),
                Delta::Update { after, .. } => after.clone(),
                Delta::Remove { before, tombstone } => tombstone.materialize(before),
                Delta::Purge { element } => {
                    let mut tombstone = element.clone();
                    tombstone.is_deleted = true;
                    tombstone.touch(now_ms);
                    tombstone
                }
            };
            match put.iter_mut().find(|existing| existing.id == element.id) {
                Some(slot) => *slot = element,
                None => put.push(element),
            }
        }
        Patch { put }
    }

    /// Merge every element into `doc` through the shared reconcile policy;
    /// returns the ids that changed (empty when the patch was fully stale).
    /// Index collisions are resolved exactly as a snapshot merge would.
    pub fn apply_to(&self, doc: &mut Document) -> Vec<ElementId> {
        crate::merge::merge_elements(doc, &self.put)
            .changed
            .into_iter()
            .map(|change| change.after.id)
            .collect()
    }
}

/// Merge one element into a document by the merge-ready LWW rules. Returns
/// whether the stored document changed.
pub fn merge_element(doc: &mut Document, incoming: &Element) -> bool {
    match doc.elements.get(&incoming.id) {
        Some(current) => {
            let winner = pick_winner(current, incoming);
            if winner == *current {
                return false;
            }
            doc.elements.insert(incoming.id.clone(), winner);
            true
        }
        None => {
            doc.elements.insert(incoming.id.clone(), incoming.clone());
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::Tombstone;
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

    fn element(id: &str, x: f64) -> Element {
        Element::new(shape(x), "a0".into(), 1).with_id(id)
    }

    #[test]
    fn from_deltas_projects_updates_and_tombstones() {
        let live = element("one", 0.0);
        let mut updated = live.clone();
        updated.version += 1;
        updated.touch(2);
        let mut tombstoned = updated.clone();
        tombstoned.is_deleted = true;
        tombstoned.touch(3);
        let deltas = vec![
            Delta::Update {
                before: live.clone(),
                after: updated.clone(),
            },
            Delta::Remove {
                before: updated.clone(),
                tombstone: Tombstone {
                    version: tombstoned.version,
                    version_nonce: tombstoned.version_nonce,
                    updated: tombstoned.updated,
                },
            },
        ];

        let patch = Patch::from_deltas(&deltas, 9);
        assert_eq!(patch.put.len(), 1, "the later delta for an id wins");
        assert_eq!(patch.put[0], tombstoned);
    }

    #[test]
    fn remove_projects_the_derived_tombstone() {
        let before = element("one", 0.0);
        let tombstone = Tombstone {
            version: before.version + 3,
            version_nonce: 77,
            updated: 42,
        };
        let patch = Patch::from_deltas(
            &[Delta::Remove {
                before: before.clone(),
                tombstone,
            }],
            9,
        );
        let mut expected = before;
        expected.is_deleted = true;
        expected.version = tombstone.version;
        expected.version_nonce = tombstone.version_nonce;
        expected.updated = tombstone.updated;
        assert_eq!(patch.put, vec![expected]);
    }

    #[test]
    fn purge_becomes_a_synthesized_tombstone() {
        let original = element("one", 0.0);
        let patch = Patch::from_deltas(
            &[Delta::Purge {
                element: original.clone(),
            }],
            42,
        );
        assert_eq!(patch.put.len(), 1);
        assert!(patch.put[0].is_deleted);
        assert_eq!(patch.put[0].id, original.id);
        assert!(patch.put[0].version > original.version);
        assert_eq!(patch.put[0].updated, 42);
    }

    #[test]
    fn apply_is_idempotent() {
        let mut doc = Document::new();
        let patch = Patch {
            put: vec![element("one", 0.0)],
        };
        assert_eq!(patch.apply_to(&mut doc), vec!["one".to_string()]);
        assert!(
            patch.apply_to(&mut doc).is_empty(),
            "replaying a patch is a no-op"
        );
        assert_eq!(doc.live_count(), 1);
    }

    #[test]
    fn apply_order_does_not_matter() {
        let mut first = element("one", 0.0);
        first.version = 5;
        first.version_nonce = 10;
        let mut second = element("one", 99.0);
        second.version = 5;
        second.version_nonce = 3;

        let mut a = Document::new();
        let mut b = Document::new();
        Patch {
            put: vec![first.clone(), second.clone()],
        }
        .apply_to(&mut a);
        Patch {
            put: vec![second, first],
        }
        .apply_to(&mut b);
        assert_eq!(a, b, "patches commute per element");
        assert_eq!(a.element("one").unwrap().version_nonce, 3);
    }

    #[test]
    fn an_older_patch_cannot_clobber_a_newer_copy() {
        let mut doc = Document::new();
        let mut newer = element("one", 5.0);
        newer.version = 4;
        merge_element(&mut doc, &newer);

        let older = element("one", 0.0);
        assert!(!merge_element(&mut doc, &older));
        assert_eq!(doc.element("one").unwrap().kind, shape(5.0));
    }

    #[test]
    fn a_tombstone_wins_over_an_older_live_copy() {
        let mut doc = Document::new();
        merge_element(&mut doc, &element("one", 0.0));

        let mut tombstone = element("one", 0.0);
        tombstone.is_deleted = true;
        tombstone.touch(2);
        assert!(merge_element(&mut doc, &tombstone));
        assert!(doc.element("one").unwrap().is_deleted);
    }
}
