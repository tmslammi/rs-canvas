//! Client-generated identifiers.
//!
//! Element IDs are immutable, globally unique and never reused. Versions
//! carry a random nonce so equal versions from concurrent editors resolve
//! deterministically.
//!
//! Both IDs and version nonces draw their per-process seed from the platform
//! RNG exactly once. Everything after that comes from an in-process PRNG:
//! on wasm every `Uuid::new_v4` would otherwise cross into JS
//! (`crypto.getRandomValues`) once per element, which dominates large fixture
//! and bulk builds.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;

use uuid::Uuid;

/// The one platform-entropy draw per process: two 64-bit seed words.
static PROCESS_SEED: OnceLock<(u64, u64)> = OnceLock::new();

/// Monotonic per-process id sequence. Thread-safe, so server callers share it.
static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

static NONCE_SEED: OnceLock<u32> = OnceLock::new();
static NONCE_COUNTER: AtomicU32 = AtomicU32::new(0);

#[cfg(test)]
static PLATFORM_SEED_INITIALIZATIONS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
thread_local! {
    static ELEMENT_ID_GENERATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Draw the process seed from the platform RNG, at most once.
fn process_seed() -> (u64, u64) {
    *PROCESS_SEED.get_or_init(|| {
        #[cfg(test)]
        PLATFORM_SEED_INITIALIZATIONS.fetch_add(1, Ordering::Relaxed);
        // The only platform-entropy crossing this process makes.
        let uuid = Uuid::new_v4();
        let bytes = uuid.as_bytes();
        (seed_word(bytes, 0), seed_word(bytes, 8))
    })
}

/// Initialize the process RNG ahead of first use.
///
/// The wasm shell calls this at startup so the one platform-entropy crossing
/// happens during boot rather than inside a fixture load or the first edit.
pub fn seed_process_rng() {
    let _ = process_seed();
}

fn seed_word(bytes: &[u8; 16], offset: usize) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(word)
}

/// SplitMix64's finalizer: a bijective 64-bit mix.
fn mix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

/// A fresh element ID.
///
/// Canonical 36-character UUID v4 format with 122 random-ish bits, produced
/// from the seeded per-process PRNG rather than a platform RNG crossing per
/// call. The counter drives a non-repeating 128-bit sequence, the 128-bit
/// seed keeps separate processes (and the server) apart, and the version /
/// variant mask leaves the usual 122 random bits.
pub fn new_element_id() -> String {
    #[cfg(test)]
    ELEMENT_ID_GENERATIONS.with(|count| count.set(count.get() + 1));

    let (seed_a, seed_b) = process_seed();
    let counter = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&mix64(seed_a ^ mix64(counter)).to_le_bytes());
    bytes[8..]
        .copy_from_slice(&mix64(seed_b ^ mix64(counter ^ 0xD1B5_4A32_D192_ED03)).to_le_bytes());
    // RFC 4122 version 4 and variant bits: consumers still see a UUID v4.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes).to_string()
}

/// A fresh random nonce for a version bump.
///
/// Drawn once from the random per-process seed and advanced by a counter:
/// `touch` runs on every pointer event, so this must not cross into the
/// browser for entropy the way `Uuid::new_v4` does.
pub fn new_version_nonce() -> u32 {
    let (seed_a, _) = process_seed();
    let seed = *NONCE_SEED.get_or_init(|| {
        let word = mix64(seed_a);
        (word ^ (word >> 32)) as u32
    });
    seed.wrapping_add(
        NONCE_COUNTER
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(0x9E37_79B9),
    )
}

/// Number of element IDs generated on this thread (test-only).
#[cfg(test)]
pub(crate) fn element_id_generations() -> usize {
    ELEMENT_ID_GENERATIONS.with(std::cell::Cell::get)
}

/// Number of platform RNG draws made by this process (test-only).
#[cfg(test)]
pub(crate) fn platform_seed_initializations() -> usize {
    PLATFORM_SEED_INITIALIZATIONS.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn ids_are_unique() {
        let a = new_element_id();
        let b = new_element_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
    }

    #[test]
    fn ids_are_canonical_uuid_v4() {
        let id = new_element_id();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[8..9], "-");
        assert_eq!(&id[13..14], "-");
        assert_eq!(&id[18..19], "-");
        assert_eq!(&id[23..24], "-");
        let parsed = Uuid::parse_str(&id).expect("canonical UUID");
        assert_eq!(parsed.get_version_num(), 4);
        assert_eq!(parsed.get_variant(), uuid::Variant::RFC4122);
    }

    #[test]
    fn a_large_batch_stays_unique() {
        let ids: HashSet<String> = (0..100_000).map(|_| new_element_id()).collect();
        assert_eq!(ids.len(), 100_000);
        assert!(ids.iter().all(|id| id.len() == 36));
    }

    #[test]
    fn the_platform_rng_is_seeded_once_per_process() {
        for _ in 0..1_000 {
            let _ = new_element_id();
        }
        // Other tests may have seeded first; the OnceLock closure still ran
        // exactly once for the whole process.
        assert_eq!(platform_seed_initializations(), 1);
    }

    #[test]
    fn explicit_seeding_is_one_shot_and_draws_no_id() {
        let before = element_id_generations();
        seed_process_rng();
        seed_process_rng();
        assert_eq!(platform_seed_initializations(), 1);
        assert_eq!(element_id_generations(), before);
    }
}
