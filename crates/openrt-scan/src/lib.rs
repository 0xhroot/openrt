//! Signed offline security signature bundles and artifact scanning (ADR-003).
//!
//! # Responsibilities
//!
//! * Deterministic signature bundles (ADR-003): a [`BundleContent`] is
//!   canonicalized to stable JSON bytes, sha256'd, then signed with ed25519.
//!   The signature travels with the content, so a bundle can be re-verified
//!   offline with only the public key.
//! * Fail-closed artifact scanning: a [`Scanner`] holds the set of bundles the
//!   runtime is willing to trust and emits a [`ScanVerdict`] for each artifact —
//!   `Clean`, `Blocked`, or `Inconclusive`. An artifact that no signed policy
//!   can positively clear is never `Clean`.
//! * Safetensors header validation: pure, deterministic bounds checks against
//!   the declared JSON header (a tensor's `data_offsets` may never reach past
//!   the end of the buffer).
//! * Scan records land under the `scan.records` namespace of the one store
//!   (ADR-006), keyed by `encode_safe_key(artifact_id)`.
//!
//! The crate is deterministic in default-feature tests: keys are fixed byte
//! arrays, no network, no `unsafe`.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use openrt_persistence::{PersistenceError, PersistenceStore};
use openrt_verify::{canonical_value, sha256_hex};
use serde::{Deserialize, Serialize};

/// Namespace for scan records (ADR-006 one-store rule).
pub const SCAN_NS: &str = "scan.records";

/// Current signature-bundle scheme version.
pub const SCHEME_VERSION: u32 = 1;

/// Scan and bundle errors (all deterministic).
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("bad signature: {0}")]
    BadSignature(String),
    #[error("invalid bundle: {0}")]
    InvalidBundle(String),
    #[error("safetensors validation failed: {reason}")]
    Safetensors { reason: String },
    #[error("persistence error: {0}")]
    Persistence(#[from] PersistenceError),
    #[error("internal error: {0}")]
    Internal(String),
}

/// Result of scanning one artifact against the trusted bundle set.
///
/// Serialized as an internally-tagged object (`{"verdict":"clean", ...}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum ScanVerdict {
    /// The artifact passed the signed policy.
    Clean {
        bundle_id: String,
        bundle_version: u32,
    },
    /// The artifact is rejected; `reason` says why.
    Blocked { reason: String },
    /// No signed policy could clear this artifact; treat as not-approved.
    Inconclusive { reason: String },
}

impl ScanVerdict {
    /// Whether this verdict positively clears the artifact.
    pub fn is_clean(&self) -> bool {
        matches!(self, ScanVerdict::Clean { .. })
    }
}

/// Policy limiting how large a safetensors JSON header a bundle will accept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetensorsPolicy {
    /// Maximum accepted header size in bytes.
    pub max_header_bytes: usize,
}

impl Default for SafetensorsPolicy {
    fn default() -> Self {
        Self {
            max_header_bytes: 16 * 1024 * 1024,
        }
    }
}

/// The policy payload a signer vouches for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleContent {
    /// RFC 3339 timestamp of signature generation.
    pub generated_at: String,
    /// The scheme version this policy was authored under.
    pub scheme_version: u32,
    /// sha256 hex digests of artifact bytes that are always blocked.
    pub known_bad: Vec<String>,
    /// Safetensors header policy; absent means safetensors cannot be cleared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safetensors_policy: Option<SafetensorsPolicy>,
}

/// An ed25519-signed policy bundle (ADR-003). Trustworthy only after
/// [`verify_signature`] passes against a key the host trusts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureBundle {
    /// Deterministic bundle identity (the content digest).
    pub bundle_id: String,
    /// Identifier of the signing key, as known to the issuer.
    pub key_id: String,
    /// sha256 hex of the canonical serialized `content`.
    pub content_sha256: String,
    /// base64 of the 64-byte ed25519 signature over the canonical content.
    pub signature_b64: String,
    pub content: BundleContent,
}

/// One record of a scan event, persisted under `scan.records`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanRecord {
    pub artifact_id: String,
    pub verdict: ScanVerdict,
    pub bundle_id: Option<String>,
    /// RFC 3339 timestamp of the scan.
    pub scanned_at: String,
}

/// Sign [`BundleContent`] into a [`SignatureBundle`].
///
/// Deterministic: the content is canonicalized to stable JSON bytes, digest and
/// signature are computed over exactly those bytes. The bundle id is the
/// content digest, so a bundle's identity is content-addressed.
pub fn sign_bundle(secret: &SigningKey, key_id: &str, content: BundleContent) -> SignatureBundle {
    let canonical = canonical_content(&content);
    let canonical_bytes = canonical.as_bytes();
    let content_sha256 = sha256_hex(canonical_bytes);
    let signature = secret.sign(canonical_bytes);
    let signature_b64 = STANDARD.encode(signature.to_bytes());
    SignatureBundle {
        bundle_id: content_sha256.clone(),
        key_id: key_id.to_string(),
        content_sha256,
        signature_b64,
        content,
    }
}

/// Verify a bundle's digest and ed25519 signature against a public key.
///
/// Fails with [`ScanError::BadSignature`] on any mismatch: recomputed digest
/// differing from `content_sha256`, undecodable base64, a non-64-byte
/// signature, or an invalid strict ed25519 signature.
pub fn verify_signature(
    bundle: &SignatureBundle,
    public_key: &VerifyingKey,
) -> Result<(), ScanError> {
    let canonical = canonical_content(&bundle.content);
    let canonical_bytes = canonical.as_bytes();
    let actual = sha256_hex(canonical_bytes);
    if actual != bundle.content_sha256 {
        return Err(ScanError::BadSignature(format!(
            "content sha256 mismatch: declared {}, recomputed {actual}",
            bundle.content_sha256
        )));
    }
    let raw = STANDARD
        .decode(&bundle.signature_b64)
        .map_err(|e| ScanError::BadSignature(format!("invalid base64 signature: {e}")))?;
    let sig_bytes: [u8; 64] = raw
        .try_into()
        .map_err(|_| ScanError::BadSignature("signature must be exactly 64 bytes".into()))?;
    let signature = Signature::from_bytes(&sig_bytes);
    public_key
        .verify_strict(canonical_bytes, &signature)
        .map_err(|e| ScanError::BadSignature(format!("ed25519 signature is invalid: {e}")))?;
    Ok(())
}

/// Registry of signature bundles a host is willing to trust. Bundles are
/// added as-is (verification against trusted keys is the caller's job).
#[derive(Debug, Clone, Default)]
pub struct Scanner {
    pub bundles: Vec<SignatureBundle>,
}

impl Scanner {
    /// An empty scanner: nothing is ever `Clean`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an available bundle (no verification performed here).
    pub fn add_bundle(&mut self, bundle: SignatureBundle) {
        self.bundles.push(bundle);
    }

    /// Scan one artifact, fail-closed.
    ///
    /// Uses the newest bundle whose scheme version meets
    /// `min_scheme_version`; if none does the verdict is Inconclusive.
    pub fn scan(
        &self,
        artifact_kind: ArtifactKind,
        data: &[u8],
        min_scheme_version: u32,
    ) -> ScanVerdict {
        let Some(selected) = self
            .bundles
            .iter()
            .filter(|b| b.content.scheme_version >= min_scheme_version)
            .max_by_key(|b| b.content.scheme_version)
        else {
            return ScanVerdict::Inconclusive {
                reason: format!("no signed bundle meeting scheme version {min_scheme_version}"),
            };
        };
        let digest = sha256_hex(data);
        if selected.content.known_bad.contains(&digest) {
            return ScanVerdict::Blocked {
                reason: "known_bad hash".into(),
            };
        }
        match artifact_kind {
            ArtifactKind::Binary => ScanVerdict::Clean {
                bundle_id: selected.bundle_id.clone(),
                bundle_version: selected.content.scheme_version,
            },
            ArtifactKind::Safetensors => {
                let Some(policy) = &selected.content.safetensors_policy else {
                    return ScanVerdict::Inconclusive {
                        reason: "no safetensors policy in bundle".into(),
                    };
                };
                match validate_safetensors_header(data, policy.max_header_bytes) {
                    Ok(()) => ScanVerdict::Clean {
                        bundle_id: selected.bundle_id.clone(),
                        bundle_version: selected.content.scheme_version,
                    },
                    Err(err) => ScanVerdict::Blocked {
                        reason: err.to_string(),
                    },
                }
            }
        }
    }
}

/// The kind of artifact bytes being scanned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// A Hugging Face safetensors weight file.
    Safetensors,
    /// Any other verified bytes (weights, configs, ...).
    Binary,
}

/// Pure, deterministic safetensors header validation.
///
/// The first 8 bytes are the little-endian header length; the header is a JSON
/// object. Returns `Ok(())` only when the declared length fits both the buffer
/// and the policy limit, the JSON parses to an object, and every tensor's
/// `data_offsets: [s, e]` satisfies `s <= e` and `e <= data.len()`.
pub fn validate_safetensors_header(data: &[u8], max_header_bytes: usize) -> Result<(), ScanError> {
    if data.len() < 8 {
        return Err(ScanError::Safetensors {
            reason: "truncated: fewer than 8 bytes".into(),
        });
    }
    let header_len =
        u64::from_le_bytes(data[..8].try_into().map_err(|_| ScanError::Safetensors {
            reason: "cannot read header length".into(),
        })?);
    let available = data.len().saturating_sub(8);
    if header_len > available as u64 {
        return Err(ScanError::Safetensors {
            reason: format!("header length {header_len} exceeds available bytes {available}"),
        });
    }
    if header_len > max_header_bytes as u64 {
        return Err(ScanError::Safetensors {
            reason: format!("header length {header_len} exceeds max of {max_header_bytes} bytes"),
        });
    }
    let header_bytes = &data[8..8 + header_len as usize];
    let header: serde_json::Value =
        serde_json::from_slice(header_bytes).map_err(|e| ScanError::Safetensors {
            reason: format!("invalid JSON header: {e}"),
        })?;
    let obj = header.as_object().ok_or_else(|| ScanError::Safetensors {
        reason: "safetensors header must be a JSON object".into(),
    })?;
    for (name, entry) in obj {
        if let Some(offsets) = entry.get("data_offsets") {
            let arr = offsets.as_array().ok_or_else(|| ScanError::Safetensors {
                reason: format!("tensor `{name}` data_offsets must be an array"),
            })?;
            if arr.len() != 2 {
                return Err(ScanError::Safetensors {
                    reason: format!("tensor `{name}` data_offsets must have exactly 2 elements"),
                });
            }
            let s = arr[0].as_u64().ok_or_else(|| ScanError::Safetensors {
                reason: format!("tensor `{name}` data_offsets[0] must be a number"),
            })?;
            let e = arr[1].as_u64().ok_or_else(|| ScanError::Safetensors {
                reason: format!("tensor `{name}` data_offsets[1] must be a number"),
            })?;
            if s > e {
                return Err(ScanError::Safetensors {
                    reason: format!("tensor `{name}` data_offsets start {s} > end {e}"),
                });
            }
            if e > data.len() as u64 {
                return Err(ScanError::Safetensors {
                    reason: format!(
                        "tensor `{name}` data_offsets end {e} exceeds data length {}",
                        data.len()
                    ),
                });
            }
        }
    }
    Ok(())
}

/// Canonical JSON bytes of a [`BundleContent`] (sorted-key, stable).
fn canonical_content(content: &BundleContent) -> String {
    let value = serde_json::to_value(content).expect("BundleContent always serializes");
    canonical_value(&value)
}

/// Persist a [`ScanRecord`] under `scan.records/<encoded-artifact-id>`.
pub async fn record_scan(
    store: &dyn PersistenceStore,
    record: &ScanRecord,
) -> Result<(), PersistenceError> {
    let value = serde_json::to_value(record)?;
    let key = openrt_core::encode_safe_key(&record.artifact_id);
    store.put_record(SCAN_NS, &key, &value).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_persistence::MemoryStore;

    fn test_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn test_public() -> VerifyingKey {
        VerifyingKey::from_bytes(&test_key().verifying_key().to_bytes()).unwrap()
    }

    fn content(
        scheme_version: u32,
        known_bad: Vec<String>,
        safetensors_policy: Option<SafetensorsPolicy>,
    ) -> BundleContent {
        BundleContent {
            generated_at: "2026-01-01T00:00:00Z".into(),
            scheme_version,
            known_bad,
            safetensors_policy,
        }
    }

    fn bundle(content: BundleContent) -> SignatureBundle {
        sign_bundle(&test_key(), "test-key-01", content)
    }

    fn minimal_safetensors(header: &str) -> Vec<u8> {
        let mut out = (header.len() as u64).to_le_bytes().to_vec();
        out.extend_from_slice(header.as_bytes());
        out
    }

    #[test]
    fn signing_roundtrip() {
        let content = content(
            SCHEME_VERSION,
            vec![sha256_hex(b"bad")],
            Some(SafetensorsPolicy::default()),
        );
        let signed = sign_bundle(&test_key(), "test-key-01", content);
        assert_eq!(signed.signature_b64.len(), 88, "base64 of 64 bytes");
        assert_eq!(signed.content_sha256.len(), 64, "hex sha256");
        assert_eq!(signed.bundle_id, signed.content_sha256);
        verify_signature(&signed, &test_public()).unwrap();
    }

    #[test]
    fn tampered_bundle_fails_verification() {
        let mut signed = bundle(content(
            SCHEME_VERSION,
            vec![],
            Some(SafetensorsPolicy::default()),
        ));
        signed.content.known_bad.push("tamper".into());
        let err = verify_signature(&signed, &test_public()).unwrap_err();
        assert!(matches!(err, ScanError::BadSignature(_)));
    }

    #[test]
    fn scan_blocks_known_bad_hash() {
        let mut scanner = Scanner::new();
        scanner.add_bundle(bundle(content(
            SCHEME_VERSION,
            vec![sha256_hex(b"bad")],
            None,
        )));
        let verdict = scanner.scan(ArtifactKind::Binary, b"bad", SCHEME_VERSION);
        assert!(matches!(verdict, ScanVerdict::Blocked { reason } if reason == "known_bad hash"));
    }

    #[test]
    fn stale_bundle_is_inconclusive() {
        let mut scanner = Scanner::new();
        scanner.add_bundle(bundle(content(1, vec![], None)));
        let verdict = scanner.scan(ArtifactKind::Binary, b"anything", 2);
        assert!(matches!(
            verdict,
            ScanVerdict::Inconclusive { reason } if reason == "no signed bundle meeting scheme version 2"
        ));
    }

    #[test]
    fn valid_minimal_safetensors_is_clean() {
        let data = minimal_safetensors(r#"{"metadata":{}}"#);
        let mut scanner = Scanner::new();
        scanner.add_bundle(bundle(content(
            SCHEME_VERSION,
            vec![],
            Some(SafetensorsPolicy {
                max_header_bytes: 1024,
            }),
        )));
        let verdict = scanner.scan(ArtifactKind::Safetensors, &data, SCHEME_VERSION);
        match &verdict {
            ScanVerdict::Clean { bundle_version, .. } => {
                assert_eq!(*bundle_version, SCHEME_VERSION)
            }
            other => panic!("expected Clean, got {other:?}"),
        }
        assert!(verdict.is_clean());
    }

    #[test]
    fn malformed_safetensors_is_blocked() {
        let mut scanner = Scanner::new();
        scanner.add_bundle(bundle(content(
            SCHEME_VERSION,
            vec![],
            Some(SafetensorsPolicy {
                max_header_bytes: 1024,
            }),
        )));

        let truncated = {
            let h = r#"{"metadata":{}}"#;
            let mut out = (h.len() as u64 + 1000).to_le_bytes().to_vec();
            out.extend_from_slice(h.as_bytes());
            out
        };
        assert!(matches!(
            scanner.scan(ArtifactKind::Safetensors, &truncated, SCHEME_VERSION),
            ScanVerdict::Blocked { .. }
        ));

        let out_of_range = minimal_safetensors(
            r#"{"metadata":{},"tensor0":{"dtype":"F32","shape":[1],"data_offsets":[0,999999]}}"#,
        );
        assert!(matches!(
            scanner.scan(ArtifactKind::Safetensors, &out_of_range, SCHEME_VERSION),
            ScanVerdict::Blocked { reason } if reason.contains("data_offsets")
        ));
    }

    #[test]
    fn safetensors_without_policy_is_inconclusive() {
        let mut scanner = Scanner::new();
        scanner.add_bundle(bundle(content(SCHEME_VERSION, vec![], None)));
        let verdict = scanner.scan(
            ArtifactKind::Safetensors,
            b"not-a-safetensors",
            SCHEME_VERSION,
        );
        assert!(matches!(
            verdict,
            ScanVerdict::Inconclusive { reason } if reason == "no safetensors policy in bundle"
        ));
    }

    #[tokio::test]
    async fn record_scan_round_trips_through_store() {
        let store = MemoryStore::new();
        let record = ScanRecord {
            artifact_id: "scan-artifact-001".into(),
            verdict: ScanVerdict::Clean {
                bundle_id: "bundle-1".into(),
                bundle_version: SCHEME_VERSION,
            },
            bundle_id: Some("bundle-1".into()),
            scanned_at: openrt_core::now().to_rfc3339(),
        };
        record_scan(&store, &record).await.unwrap();
        let key = openrt_core::encode_safe_key(&record.artifact_id);
        let raw = store
            .get_record(SCAN_NS, &key)
            .await
            .unwrap()
            .expect("record present");
        let parsed: ScanRecord = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed, record);
    }

    #[test]
    fn key_encoding_is_path_safe() {
        let key = openrt_core::encode_safe_key("meta-llama/Llama-3.1-8B");
        assert_eq!(key, "meta-2dllama-2f-4clama-2d3.1-2d8-42");
        assert!(!key.contains('/'));
        assert!(key.bytes().all(|b| b.is_ascii_lowercase()
            || b.is_ascii_digit()
            || matches!(b, b'.' | b'_' | b'-')));
        assert_eq!(
            openrt_core::decode_safe_key(&key).as_deref(),
            Some("meta-llama/Llama-3.1-8B")
        );
    }
}
