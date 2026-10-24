//! Deltas: the only way a Document changes.
//!
//! Every mutation is expressed as a reversible `Delta`. Applying the inverse
//! restores the exact prior state — that is what undo/redo is built on, and
//! it is the seam a future CRDT plugs into. Applying a delta is idempotent:
//! `Add` upserts, so redos and remote replays cannot conflict.

use std::fmt;

use crate::id::new_version_nonce;
use crate::model::{Document, Element, ElementId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditError {
    UnknownElement(ElementId),
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EditError::UnknownElement(id) => write!(f, "unknown element {id}"),
        }
    }
}

impl std::error::Error for EditError {}

/// Merge metadata for a removal's tombstone.
///
/// A removal records the element as it was (`before`) plus this compact
/// marker instead of a second full element: the tombstone is derived from
/// the pair. That keeps one full `Element` clone per removal in history while
/// the wire projection stays identical to a materialized tombstone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tombstone {
    pub version: u64,
    pub version_nonce: u32,
    pub updated: u64,
}

impl Tombstone {
    /// Stamp a fresh tombstone for a just-removed element.
    pub fn fresh(before: &Element, now_ms: u64) -> Self {
        Self {
            version: before.version + 1,
            version_nonce: new_version_nonce(),
            updated: now_ms,
        }
    }

    /// Re-stamp a replayed tombstone as a new edit that beats `current`.
    pub fn freshened(self, current: Option<&Element>, now_ms: u64) -> Self {
        let base = current
            .map(|element| element.version)
            .unwrap_or(0)
            .max(self.version);
        Self {
            version: base + 1,
            version_nonce: new_version_nonce(),
            updated: now_ms,
        }
    }

    /// Materialize the tombstoned element for apply and wire patches.
    pub fn materialize(self, before: &Element) -> Element {
        let mut element = before.clone();
        element.is_deleted = true;
        element.version = self.version;
        element.version_nonce = self.version_nonce;
        element.updated = self.updated;
        element
    }
}

/// A single reversible change.
///
/// `Update` is the widest variant because it carries both sides of an edit;
/// boxing them would allocate on every change for no size win, since the
/// enum was already this large before removals shrank their variant.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum Delta {
    /// The element is inserted (or re-inserted over a tombstone).
    Add { element: Element },
    /// The element entry is removed entirely. This is the inverse of `Add`
    /// (undoing a creation un-creates it); user deletes use `Remove`, whose
    /// tombstone is what propagates through merge.
    Purge { element: Element },
    /// The element is tombstoned; the deleted copy is derived from `before`
    /// and the compact marker, so the inverse is a pure function.
    Remove {
        before: Element,
        tombstone: Tombstone,
    },
    /// The element changed; whole-element last-writer-wins keeps merge simple.
    Update { before: Element, after: Element },
}

impl Delta {
    pub fn element_id(&self) -> &ElementId {
        match self {
            Delta::Add { element } | Delta::Purge { element } => &element.id,
            Delta::Remove { before, .. } => &before.id,
            Delta::Update { after, .. } => &after.id,
        }
    }

    /// The delta that exactly undoes this one.
    pub fn invert(&self) -> Delta {
        match self {
            Delta::Add { element } => Delta::Purge {
                element: element.clone(),
            },
            Delta::Purge { element } => Delta::Add {
                element: element.clone(),
            },
            Delta::Remove { before, tombstone } => Delta::Update {
                before: tombstone.materialize(before),
                after: before.clone(),
            },
            Delta::Update { before, after } => Delta::Update {
                before: after.clone(),
                after: before.clone(),
            },
        }
    }
}

impl Document {
    /// Apply a delta to the document.
    pub fn apply(&mut self, delta: &Delta) -> Result<(), EditError> {
        match delta {
            Delta::Add { element } => {
                self.elements.insert(element.id.clone(), element.clone());
            }
            Delta::Purge { element } => {
                self.elements.remove(&element.id);
            }
            Delta::Remove { before, tombstone } => match self.elements.get_mut(&before.id) {
                Some(slot) => *slot = tombstone.materialize(before),
                None => return Err(EditError::UnknownElement(before.id.clone())),
            },
            Delta::Update { after, .. } => match self.elements.get_mut(&after.id) {
                Some(slot) => *slot = after.clone(),
                None => return Err(EditError::UnknownElement(after.id.clone())),
            },
        }
        Ok(())
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

    fn element_at(doc: &mut Document, x: f64) -> Element {
        let id = doc.add(shape(x), 1);
        doc.element(&id).unwrap().clone()
    }

    #[test]
    fn add_inverse_purges_then_redo_restores() {
        let mut doc = Document::new();
        let element = element_at(&mut doc, 0.0);
        let delta = Delta::Add { element };
        doc.apply(&delta.invert()).unwrap();
        assert!(doc.is_empty(), "undoing an add removes the element entry");
        doc.apply(&delta).unwrap();
        assert_eq!(doc.ordered_live().len(), 1);
    }

    #[test]
    fn remove_inverse_restores_exact_element() {
        let mut doc = Document::new();
        let before = element_at(&mut doc, 5.0);
        let mut expected = before.clone();
        expected.is_deleted = true;
        expected.version += 1;
        expected.version_nonce = 99;
        expected.updated = 42;
        let delta = Delta::Remove {
            before: before.clone(),
            tombstone: Tombstone {
                version: expected.version,
                version_nonce: 99,
                updated: 42,
            },
        };
        doc.apply(&delta).unwrap();
        assert_eq!(doc.element(&before.id), Some(&expected));
        doc.apply(&delta.invert()).unwrap();
        assert_eq!(doc.element(&before.id), Some(&before));
    }

    #[test]
    fn update_inverse_restores_exact_element() {
        let mut doc = Document::new();
        let before = element_at(&mut doc, 0.0);
        let mut after = before.clone();
        after.touch(42);
        let delta = Delta::Update {
            before: before.clone(),
            after: after.clone(),
        };
        doc.apply(&delta).unwrap();
        assert_eq!(doc.element(&before.id), Some(&after));
        doc.apply(&delta.invert()).unwrap();
        assert_eq!(doc.element(&before.id), Some(&before));
    }

    #[test]
    fn update_of_unknown_element_errors() {
        let mut doc = Document::new();
        let mut element = element_at(&mut doc, 0.0);
        element.id = "missing".into();
        let delta = Delta::Update {
            before: element.clone(),
            after: element,
        };
        assert_eq!(
            doc.apply(&delta),
            Err(EditError::UnknownElement("missing".into()))
        );
    }
}
