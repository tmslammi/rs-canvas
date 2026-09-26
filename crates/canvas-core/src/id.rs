//! Client-generated identifiers.
//!
//! Element IDs are immutable, globally unique and never reused. Versions
//! carry a random nonce so equal versions from concurrent editors resolve
//! deterministically (see the merge ticket).

use uuid::Uuid;

/// A fresh element ID.
pub fn new_element_id() -> String {
    Uuid::new_v4().to_string()
}

/// A fresh random nonce for a version bump.
pub fn new_version_nonce() -> u32 {
    let bytes = Uuid::new_v4();
    let b = bytes.as_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique() {
        let a = new_element_id();
        let b = new_element_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
    }
}
