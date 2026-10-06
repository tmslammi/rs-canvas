//! Stable short handles for element ids.
//!
//! The agent reads `e12` instead of a 36-char UUID, and every tool accepts
//! either. Handles are assigned on first sight, in read order, and kept for
//! the server's lifetime; deleted elements keep theirs so an old reference
//! still resolves (and fails meaningfully if the element is gone).

use std::collections::HashMap;

use canvas_core::describe::IdMap;

#[derive(Default)]
pub struct Aliases {
    next: u64,
    by_id: HashMap<String, String>,
    by_handle: HashMap<String, String>,
}

impl Aliases {
    /// The handle for `id`, assigning one on first sight.
    pub fn handle(&mut self, id: &str) -> String {
        if let Some(handle) = self.by_id.get(id) {
            return handle.clone();
        }
        self.next += 1;
        let handle = format!("e{}", self.next);
        self.by_id.insert(id.to_string(), handle.clone());
        self.by_handle.insert(handle.clone(), id.to_string());
        handle
    }

    /// Resolve a handle; anything unknown passes through as-is, so raw ids
    /// (and clear errors about them) still work.
    pub fn resolve(&self, name: &str) -> String {
        self.by_handle
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }
}

impl IdMap for Aliases {
    fn map(&mut self, id: &str) -> String {
        self.handle(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_are_assigned_once_and_stay_stable() {
        let mut aliases = Aliases::default();
        assert_eq!(aliases.handle("aaa"), "e1");
        assert_eq!(aliases.handle("bbb"), "e2");
        assert_eq!(aliases.handle("aaa"), "e1", "same id, same handle");
    }

    #[test]
    fn resolve_maps_back_and_passes_unknown_names_through() {
        let mut aliases = Aliases::default();
        let handle = aliases.handle("aaa");
        assert_eq!(aliases.resolve(&handle), "aaa");
        assert_eq!(aliases.resolve("raw-uuid"), "raw-uuid");
        assert_eq!(aliases.resolve("e99"), "e99");
    }
}
