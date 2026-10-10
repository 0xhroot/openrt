//! Pair-canonical digests for workspace assets — thin wrappers over the ONE
//! shared implementation in `openrt-core::digest` (same bytes as
//! `openrt-memory` and `openrt-timeseries`; never a second algorithm).

use crate::AssetError;

pub use openrt_core::sha256_hex;

pub fn canonical_bytes(pairs: &[(String, String)]) -> Vec<u8> {
    openrt_core::canonical_pair_bytes(pairs)
}

pub fn canonical_digest(pairs: &[(String, String)]) -> String {
    openrt_core::canonical_pair_digest(pairs)
}

pub fn verify_digest(
    canonical: &[u8],
    persisted_sha256: &str,
    kind: &str,
    id: &str,
) -> Result<(), AssetError> {
    if openrt_core::sha256_hex(canonical) != persisted_sha256 {
        return Err(AssetError::DigestMismatch {
            kind: kind.to_string(),
            id: id.to_string(),
        });
    }
    Ok(())
}

pub fn ensure_path_safe_id(raw: &str) -> Result<(), AssetError> {
    if raw.is_empty()
        || !raw.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-'
        })
    {
        return Err(AssetError::Malformed {
            detail: format!("id {:?} must match [a-z0-9._-]", raw),
        });
    }
    Ok(())
}
