//! Shared digest discipline (the ONE implementation of the Phase-5 pair
//! canonical form + SHA-256). Used by `openrt-memory`, `openrt-workspace-assets`,
//! and `openrt-timeseries` so the three never diverge on the algorithm while
//! each keeps its own persisted format (ADR-006: one authority, no parallel
//! digest systems).
//!
//! Canonical bytes = sorted unique `key\nvalue\n` pairs (byte sort);
//! deterministic SHA-256 (lowercase hex) over those bytes. Digest never feeds
//! its own input. ANY mismatch on reopen → fail closed.

use sha2::{Digest, Sha256};
use std::fmt::Write;

/// Hex-encode bytes (lowercase, canonical, deterministic).
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(&mut s, "{:02x}", b).unwrap();
    }
    s
}

/// SHA-256 over a byte slice (hex, lowercase).
pub fn sha256_hex(msg: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(msg);
    hex_encode(&h.finalize())
}

/// Deterministic sorted-unique canonical stream: `key\nvalue\n` pairs in
/// byte order, duplicates collapsed.
pub fn canonical_pair_bytes(pairs: &[(String, String)]) -> Vec<u8> {
    let mut uniq: Vec<(String, String)> = pairs.to_vec();
    uniq.sort();
    uniq.dedup();
    let mut out = Vec::new();
    for (k, v) in uniq {
        out.extend_from_slice(k.as_bytes());
        out.push(b'\n');
        out.extend_from_slice(v.as_bytes());
        out.push(b'\n');
    }
    out
}

/// Canonical digest over sorted unique `key\nvalue\n` pairs.
pub fn canonical_pair_digest(pairs: &[(String, String)]) -> String {
    sha256_hex(&canonical_pair_bytes(pairs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_sha256_vector_matches_fips_180_4() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn pair_digest_is_order_insensitive_and_value_sensitive() {
        let a = canonical_pair_digest(&[
            ("run_id".into(), "r1".into()),
            ("source".into(), "s1".into()),
        ]);
        let b = canonical_pair_digest(&[
            ("source".into(), "s1".into()),
            ("run_id".into(), "r1".into()),
        ]);
        assert_eq!(a, b);
        let c = canonical_pair_digest(&[
            ("run_id".into(), "r1".into()),
            ("source".into(), "s2".into()),
        ]);
        assert_ne!(a, c);
    }
}
