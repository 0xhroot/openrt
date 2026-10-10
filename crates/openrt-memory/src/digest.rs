//! **Digest / canonical discipline for `openrt-memory`** — thin wrappers over
//! the ONE shared implementation in `openrt-core::digest` (same algorithm as
//! `openrt-workspace-assets` and `openrt-timeseries`; never a second system).
//!
//! * Canonical bytes = sorted unique `key\nvalue\n` pairs (byte sort, no
//!   separators that collide with content); deterministic sha256 over those
//!   canonical bytes.
//! * **`digest` never feeds its own input.** A record's digest is computed
//!   over its *other* fields' canonical stream only, so it can never
//!   reference itself (the exact M5.1/M5.2 fail-closed invariant).
//! * Reopen re-derives byte-identical canonical bytes and re-validates the
//!   digest; ANY mismatch → `Err` (fail closed) — we never silently repair,
//!   never return a tampered record as verified.

use std::fmt::Write;

pub const HEX: &[u8; 16] = b"0123456789abcdef";

/// Hex-encode bytes (lowercase, canonical, deterministic).
pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(&mut s, "{:02x}", b).unwrap();
    }
    s
}

/// SHA-256 over a byte slice (hex, lowercase).
pub fn sha256_hex(msg: &[u8]) -> String {
    openrt_core::sha256_hex(msg)
}

/// Canonical digest computed over **sorted unique** `key\nvalue\n` fields.
/// Byte-deterministic; order of input pairs does not matter.
pub fn canonical_digest(pairs: &[(&str, &str)]) -> String {
    let owned: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    openrt_core::canonical_pair_digest(&owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_deterministic_and_field_order_insensitive() {
        let a = canonical_digest(&[("run_id", "r1"), ("source", "s1")]);
        let b = canonical_digest(&[("source", "s1"), ("run_id", "r1")]);
        assert_eq!(a, b);
        let c = canonical_digest(&[("run_id", "r1"), ("source", "s2")]);
        assert_ne!(a, c, "changed field value must change digest");
    }

    #[test]
    fn known_sha256_vector_matches_fips_180_4() {
        // "abc" → the FIPS 180-4 Appendix B known-answer.
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
/// Fail-closed re-derive-and-compare over the persisted canonical bytes.
/// The rule (exact, carried forward unchanged — never weakened):
/// SHA-256 of the item's *other* fields' canonical stream must byte-match
/// the persisted digest; ANY mismatch → `Err`. Caller re-derives the exact
/// canonical bytes it persisted; a tampered/re-missing record NEVER passes.
pub fn verify_digest(canonical: &[u8], persisted_sha256: &str) -> Result<(), DigestError> {
    if openrt_core::sha256_hex(canonical) != persisted_sha256 {
        return Err(DigestError::DigestMismatch);
    }
    Ok(())
}

/// Deterministic sorted-unique canonical stream: `key\nvalue\n` pairs in
/// byte order, duplicates collapsed — the exact discipline the digest field
/// itself is computed over (digest excluded from its own input).
pub fn canonical_bytes(pairs: &[(&str, &str)]) -> Vec<u8> {
    let owned: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    openrt_core::canonical_pair_bytes(&owned)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DigestError {
    #[error("digest mismatch (fail closed — never silently repaired)")]
    DigestMismatch,
}
