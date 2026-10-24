//! Page/server sync semantics: explicit page frames and the page purge ledger.
//!
//! A page snapshot is a complete document *plus* the server revision it was
//! built from. It is not an authority on absence: an element missing from a
//! snapshot is only deleted when the page also sends an explicit tombstone.
//! Pages materialize those tombstones into the snapshot from their purge
//! ledger, so undo of a creation still propagates without guessing.
//!
//! The same module exposes the frame decoder both wire encodings converge on,
//! and the ledger that keeps purged elements expressible across reconnects.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::delta::Delta;
use crate::model::{Document, Element, ElementId};
use crate::patch::Patch;
use crate::serialization;
use crate::wire::{self, FrameKind, WireError};

/// A document frame uploaded by a page.
#[derive(Clone, Debug, PartialEq)]
pub enum PageFrame {
    /// A full document and the server revision it was based on. The base is a
    /// diagnostic and catch-up hint; element-level LWW still decides merges.
    Snapshot { base_revision: u64, doc: Document },
    /// A set of full elements, tombstones included.
    Patch { put: Vec<Element> },
}

impl PageFrame {
    pub fn is_snapshot(&self) -> bool {
        matches!(self, PageFrame::Snapshot { .. })
    }

    /// Decode a binary frame from the page. Header revision is the page's
    /// base revision for snapshots; patches ignore it.
    pub fn from_binary(bytes: &[u8]) -> Result<Self, WireError> {
        match wire::kind(bytes)? {
            FrameKind::Snapshot => {
                let (base_revision, doc) = wire::decode_snapshot(bytes)?;
                Ok(PageFrame::Snapshot { base_revision, doc })
            }
            FrameKind::Patch => {
                let (_, put) = wire::decode_patch(bytes)?;
                Ok(PageFrame::Patch { put })
            }
        }
    }

    /// Decode a JSON document frame. Legacy pages omit `rev`; they are treated
    /// as base zero and every merge still follows LWW. Control frames return
    /// `None` so the caller can keep handling them separately.
    pub fn from_json(frame: &Value) -> Option<Self> {
        match frame.get("type").and_then(Value::as_str) {
            Some("snapshot") => {
                let doc = serialization::from_value(frame.get("doc")?.clone()).ok()?;
                let base_revision = frame.get("rev").and_then(Value::as_u64).unwrap_or(0);
                Some(PageFrame::Snapshot { base_revision, doc })
            }
            Some("patch") => {
                let put = serde_json::from_value(frame.get("put")?.clone()).ok()?;
                Some(PageFrame::Patch { put })
            }
            _ => None,
        }
    }
}

/// A page's explicit deletion ledger for elements whose document entry was
/// purged locally (undo of a creation). The live document has no entry left,
/// so without this ledger a reconnect snapshot would silently forget the
/// deletion and the mirror would resurrect the element.
///
/// Entries are complete tombstones, not bare IDs: their version/nonce make
/// them merge like any other element, and they can be folded into autosaves
/// and snapshots without changing the live document.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PurgeLedger {
    tombstones: BTreeMap<ElementId, Element>,
}

impl PurgeLedger {
    pub fn is_empty(&self) -> bool {
        self.tombstones.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tombstones.len()
    }

    pub fn clear(&mut self) {
        self.tombstones.clear();
    }

    /// Record the purges in one drained outbox, using the exact tombstones the
    /// wire patch projects (`projected`). A later add of the same id in the
    /// same batch forgets the ledger entry again.
    pub fn record(&mut self, deltas: &[Delta], projected: &Patch) {
        let projected: BTreeMap<&str, &Element> = projected
            .put
            .iter()
            .map(|element| (element.id.as_str(), element))
            .collect();
        for delta in deltas {
            match delta {
                Delta::Purge { element } => match projected.get(element.id.as_str()) {
                    Some(tombstone) if tombstone.is_deleted => {
                        self.tombstones
                            .insert(element.id.clone(), (*tombstone).clone());
                    }
                    _ => {
                        self.tombstones.remove(&element.id);
                    }
                },
                Delta::Add { element } => {
                    self.tombstones.remove(&element.id);
                }
                _ => {}
            }
        }
    }

    /// A copy of `doc` with ledger tombstones added where the live document
    /// has no entry. A live or already-tombstoned local copy wins: the ledger
    /// only restores an explicit deletion the live document forgot.
    pub fn materialize(&self, doc: &Document) -> Document {
        if self.tombstones.is_empty() {
            return doc.clone();
        }
        let mut synced = doc.clone();
        for (id, tombstone) in &self.tombstones {
            if !synced.elements.contains_key(id) {
                synced.elements.insert(id.clone(), tombstone.clone());
            }
        }
        synced
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;
    use crate::model::{ElementKind, ShapeKind, Style};

    fn element(id: &str, version: u64) -> Element {
        let mut element = Element::new(
            ElementKind::Shape {
                shape: ShapeKind::Rectangle,
                rect: Rect::new(0.0, 0.0, 10.0, 10.0),
                style: Style::default(),
                bound_text_id: None,
            },
            "a0".into(),
            1,
        )
        .with_id(id);
        element.version = version;
        element
    }

    #[test]
    fn binary_and_json_snapshots_carry_the_page_base_revision() {
        let mut doc = Document::new();
        doc.insert(element("one", 1));

        let bytes = wire::encode_snapshot(9, &doc).unwrap();
        assert_eq!(
            PageFrame::from_binary(&bytes).unwrap(),
            PageFrame::Snapshot {
                base_revision: 9,
                doc: doc.clone()
            }
        );

        let json = serde_json::json!({
            "type": "snapshot",
            "rev": 9,
            "doc": serde_json::to_value(&doc).unwrap(),
        });
        assert_eq!(
            PageFrame::from_json(&json).unwrap(),
            PageFrame::Snapshot {
                base_revision: 9,
                doc
            }
        );
    }

    #[test]
    fn legacy_json_snapshots_default_to_base_zero() {
        let mut doc = Document::new();
        doc.insert(element("one", 1));
        let json = serde_json::json!({
            "type": "snapshot",
            "doc": serde_json::to_value(&doc).unwrap(),
        });
        assert_eq!(
            PageFrame::from_json(&json).unwrap(),
            PageFrame::Snapshot {
                base_revision: 0,
                doc
            }
        );
    }

    #[test]
    fn json_and_binary_patches_decode_to_the_same_elements() {
        let element = element("one", 1);
        let json = serde_json::json!({
            "type": "patch",
            "rev": 4,
            "put": [serde_json::to_value(&element).unwrap()],
        });
        let from_json = PageFrame::from_json(&json).unwrap();
        let bytes = wire::encode_patch(4, std::slice::from_ref(&element)).unwrap();
        let from_binary = PageFrame::from_binary(&bytes).unwrap();
        assert_eq!(from_json, from_binary);
        assert_eq!(from_json, PageFrame::Patch { put: vec![element] });
    }

    #[test]
    fn control_frames_are_not_page_document_frames() {
        assert!(PageFrame::from_json(&serde_json::json!({
            "type": "hello", "sync": true
        }))
        .is_none());
        assert!(PageFrame::from_json(&serde_json::json!({
            "type": "synced", "rev": 3
        }))
        .is_none());
    }

    #[test]
    fn purge_ledger_materializes_only_explicit_deletions() {
        let element = element("one", 3);
        let mut doc = Document::new();
        doc.insert(element.clone());

        let deltas = vec![Delta::Purge {
            element: element.clone(),
        }];
        let projected = Patch::from_deltas(&deltas, 9);
        let mut ledger = PurgeLedger::default();
        ledger.record(&deltas, &projected);
        assert_eq!(ledger.len(), 1);

        let synced = ledger.materialize(&Document::new());
        let tombstone = synced.element("one").expect("ledger tombstone");
        assert!(tombstone.is_deleted);
        assert_eq!(tombstone.version, 4);

        // The live document's own copy wins; the ledger is not a second doc.
        let unchanged = ledger.materialize(&doc);
        assert_eq!(unchanged.element("one"), Some(&element));
    }

    #[test]
    fn re_adding_a_purged_id_forgets_the_ledger_entry() {
        let element = element("one", 3);
        let deltas = vec![Delta::Purge {
            element: element.clone(),
        }];
        let projected = Patch::from_deltas(&deltas, 9);
        let mut ledger = PurgeLedger::default();
        ledger.record(&deltas, &projected);

        let mut revived = element;
        revived.version = 5;
        let deltas = vec![Delta::Add {
            element: revived.clone(),
        }];
        ledger.record(&deltas, &Patch::from_deltas(&deltas, 10));
        assert!(ledger.is_empty());
        assert!(!ledger
            .materialize(&Document::new())
            .elements
            .contains_key("one"));
    }
}
