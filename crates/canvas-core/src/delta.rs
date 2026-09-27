//! Deltas: the only way a Document changes.
//!
//! Every mutation is expressed as a reversible `Delta`. Applying the inverse
//! restores the exact prior state — that is what undo/redo is built on, and
//! it is the seam a future CRDT plugs into. Applying a delta is idempotent:
//! `Add` upserts, so redos and remote replays cannot conflict.

use std::fmt;

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

/// A single reversible change.
#[derive(Clone, Debug, PartialEq)]
pub enum Delta {
    /// The element is inserted (or re-inserted over a tombstone).
    Add { element: Element },
    /// The element entry is removed entirely. This is the inverse of `Add`
    /// (undoing a creation un-creates it); user deletes use `Remove`, whose
    /// tombstone is what propagates through merge.
    Purge { element: Element },
    /// The element is tombstoned; both sides of the change are kept so the
    /// inverse is a pure function.
    Remove {
        before: Element,
        tombstoned: Element,
    },
    /// The element changed; whole-element last-writer-wins keeps merge simple.
    Update { before: Element, after: Element },
}

impl Delta {
    pub fn element_id(&self) -> &ElementId {
        match self {
            Delta::Add { element } | Delta::Purge { element } => &element.id,
            Delta::Remove { tombstoned, .. } => &tombstoned.id,
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
            Delta::Remove { before, tombstoned } => Delta::Update {
                before: tombstoned.clone(),
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
            Delta::Remove { tombstoned, .. } => match self.elements.get_mut(&tombstoned.id) {
                Some(slot) => *slot = tombstoned.clone(),
                None => return Err(EditError::UnknownElement(tombstoned.id.clone())),
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
        let mut tombstoned = before.clone();
        tombstoned.is_deleted = true;
        tombstoned.touch(99);
        let delta = Delta::Remove {
            before: before.clone(),
            tombstoned,
        };
        doc.apply(&delta).unwrap();
        assert!(doc.element(&before.id).unwrap().is_deleted);
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
