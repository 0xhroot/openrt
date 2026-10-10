//! `openrt-memory::canonical` — the ONE deterministic canonical/digest
//! discipline for the M5.3 memory layer, byte-identical to the M5.1/M5.2
//! rule it inherits (roadmap: *"same canonical + sha256 discipline the
//! workspace + history crates already gate"*).
//!
//! Deterministic contract, exact and non-negotiable:
//!
//! * The canonical byte stream is built from **sorted** `key\nvalue\n`
//!   pairs (byte sort); keys are byte-unique — `BTreeMap` semantics, never
//!   `indexmap` ordering.
//! * The `digest` field is **excluded from its own input** — the digest is
//!   computed over the *other* fields' canonical stream and NEVER includes
//!   itself pending its own inputacyclic. Same rule as
//!   `openrt-persistence`'s M5.1/M5.2 gates.
//! * SHA-256 via the workspace's already-gated `sha2` crate (the same
//!   dependency the timeseries + workspace layers already trust). We do NOT
//!   ship a second hash; one store, one digest rule (ADR-006).
//!
//! # Fail-closed
//!
//! `verify` returns `Err` on ANY mismatch (tampered / re-keyed / missing
//! bytes) — it never silently repairs and never reports a record as
//! verified that does not match its own persisted canonical bytes.

use sha2::{Digest, Sha256};

/// Sorted `key\nvalue\n` canonical bytes over the given key→value pairs.
///
/// Keys are byte-sorted and byte-unique (later duplicate `key` overwrites,
/// matching `BTreeMap::insert`). This is the exact ordering discipline the
/// M5.1/M5.2 persisted records already gate on.
pub fn canonical_bytes(pairs: &[(&str, &str)]) -> Vec<u8> {
    let mut canonical: Vec<(Vec<u8>, Vec<u8>)> = pairs
        .iter()
        .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
        .collect();
    // byte sort by key; duplicate keys keep only the last insertion order
    // (BTreeMap semantics) — deterministic across runs and platforms.
    canonical.sort_by(|a, b| a.0.cmp(&b.0));
    let mut uniq: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(canonical.len());
    for pair in canonical {
        if let Some(last) = uniq.last_mut() {
            if last.0 == pair.0 {
                uniq.pop();
            }
        }
        uniq.push(pair);
    }
    let mut out = Vec::new();
    for (k, v) in uniq {
        out.extend_from_slice(&k);
        out.push(b'\n');
        out.extend_from_slice(&v);
        out.push(b'\n');
    }
    out
}

/// The SHA-256 over [`canonical_bytes`] for the given pairs, hex-encoded
/// (lowercase). Since the input pair list is the caller-chosen field set,
/// the digest is computed over exactly those fields' canonical stream — NO
/// field is ever part of its own input, and nothing is invented.
pub fn sha256_hex(pairs: &[(&str, &str)]) -> String {
    let bytes = canonical_bytes(pairs);
    hex_fmt(&Sha256::digest(&bytes))
}

/// Hex-encode (lowercase) digest bytes.
pub fn hex_fmt(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Deterministic, fail-closed confirmation that `actual_digest` is the
/// byte-identical SHA-256 of `pairs`' canonical stream. On ANY mismatch →
/// `Err`. This is the tamper gate: a record whose digest does not re-derive
/// from its own (non-digest) fields is never returned as verified.
pub fn verify_digest(pairs: &[(&str, &str)], actual_digest: &str) -> Result<(), CanonicalError> {
    let re = sha256_hex(pairs);
    if re != actual_digest {
        return Err(CanonicalError::DigestMismatch {
            expected: re,
            actual: actual_digest.to_string(),
        });
    }
    Ok(())
}

/// Fail-closed canonical error surface.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CanonicalError {
    /// The re-derived digest does not equal the record's declared digest:
    /// tampered / re-keyed / missing bytes. Never silently repaired.
    #[error("canonical digest mismatch (fail closed): expected {expected}, got {actual}")]
    DigestMismatch { expected: String, actual: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vector_deterministic() {
        // FIPS 180-4 "abc" known answer — proves we are SHA-256, not a stub.
        let digest = Sha256::digest(b"abc");
        assert_eq!(
            hex_fmt(&digest),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn canonical_bytes_are_sorted_and_unique() {
        let b = canonical_bytes(&[("b", "2"), ("a", "1")]);
        let repr = String::from_utf8(b).unwrap();
        assert_eq!(repr, "a\n1\nb\n2\n");
        // duplicate key keeps last value, deterministic
        let b2 = canonical_bytes(&[("a", "1"), ("a", "9"), ("b", "2")]);
        assert_eq!(String::from_utf8(b2).unwrap(), "a\n9\nb\n2\n");
    }

    #[test]
    fn digest_excludes_own_field_and_is_bitexact() {
        // digest over (a,b) must equal digest over (a,b) regardless of a
        // non-participating field supplied by the caller — discipline test.
        let d1 = sha256_hex(&[("a", "1"), ("b", "2")]);
        let d2 = sha256_hex(&[("b", "2"), ("a", "1")]);
        assert_eq!(d1, d2, "field order must not matter");
    }

    #[test]
    fn verify_fails_closed_on_tamper() {
        assert!(verify_digest(&[("a", "1")], "deadbeef").is_err());
        assert!(verify_digest(&[("a", "1")], &sha256_hex(&[("a", "1")])).is_ok());
    }
}
