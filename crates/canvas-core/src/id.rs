//! Client-generated identifiers.
//!
//! Element IDs are immutable, globally unique and never reused. Versions
//! carry a random nonce so equal versions from concurrent editors resolve
//! deterministically.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;

use uuid::Uuid;

/// A fresh element ID.
pub fn new_element_id() -> String {
    Uuid::new_v4().to_string()
}

static NONCE_SEED: OnceLock<u32> = OnceLock::new();
static NONCE_COUNTER: AtomicU32 = AtomicU32::new(0);

/// A fresh random nonce for a version bump.
///
/// Drawn once from a random per-process seed and advanced by a counter: `touch`
/// runs on every pointer event, so this must not cross into the browser for
/// entropy the way `Uuid::new_v4` does.
pub fn new_version_nonce() -> u32 {
    let seed = *NONCE_SEED.get_or_init(|| {
        let bytes = Uuid::new_v4();
        let b = bytes.as_bytes();
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    });
    seed.wrapping_add(
        NONCE_COUNTER
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(0x9E37_79B9),
    )
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
