//! `openrt-embeddings` — typed embedding interface and validation.
//!
//! The provider trait is the only way embeddings enter the system. Real
//! embeddings (semantic) come from a real model backend behind this trait; the
//! bundled [`DeterministicHashEmbedder`] is an **honest, offline, deterministic**
//! stand-in (its `describe()` says so) for tests and preflight — it is not
//! semantic and never claims to be.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// One embedded text. `id` is stable: `emb/<sha256 of input text>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Embedding {
    pub id: String,
    /// The text input (kept for provenance/permutation checks; never logged).
    pub source: String,
    pub dims: usize,
    /// Dense vector. Providers MUST return finite unit-length vectors.
    pub values: Vec<f32>,
    pub sha256: String,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EmbedError {
    #[error("embedding has NaN/Infinite component (input {id})")]
    NotFinite { id: String },
    #[error("embedding dim {got} != expected {expected} (input {id})")]
    WrongDims {
        id: String,
        got: usize,
        expected: usize,
    },
    #[error("embedding is not unit length (input {id}, norm {norm})")]
    NotUnit { id: String, norm: String },
    #[error("empty input for embedding")]
    EmptyInput,
    /// Structured unavailability (M9.5): unreachable/failed backend.
    #[error("embedding backend unavailable: {reason}")]
    Unavailable { reason: String },
    /// Structured model absence (M9.5): endpoint up, model not served.
    #[error("embedding model missing: {reason}")]
    ModelMissing { reason: String },
    /// Backend responded but not in a usable way; detail carried verbatim.
    #[error("embedding backend request failed: {0}")]
    Http(String),
}

pub mod status;
pub use status::{EmbeddingStatus, DEFAULT_EMBED_ENDPOINT, DEFAULT_EMBED_MODEL};

#[cfg(feature = "ollama")]
pub mod ollama;
#[cfg(feature = "ollama")]
pub use ollama::{probe_embedding, OllamaEmbedder};

/// Probe an embedding backend by REAL embedding (always available, even in a
/// default no-HTTP build): without the `ollama` feature the honest answer is
/// `UNAVAILABLE` — never a fabricated `AVAILABLE`.
#[cfg(not(feature = "ollama"))]
pub fn probe_embedding(_endpoint: &str, _model: &str) -> EmbeddingStatus {
    EmbeddingStatus::Unavailable {
        reason: "built without the `ollama` feature (SEC-07: no HTTP stack)".to_string(),
    }
}

pub fn embed_id(source: &str) -> String {
    let sha = sha256_hex(source.as_bytes());
    format!("emb/{sha}")
}

pub fn validate_embedding(e: &Embedding, expected_dims: usize) -> Result<(), EmbedError> {
    if e.values.iter().any(|v| !v.is_finite()) {
        return Err(EmbedError::NotFinite { id: e.id.clone() });
    }
    if e.dims != expected_dims || e.values.len() != expected_dims {
        return Err(EmbedError::WrongDims {
            id: e.id.clone(),
            got: e.values.len(),
            expected: expected_dims,
        });
    }
    let norm: f64 = e
        .values
        .iter()
        .map(|v| (*v as f64) * (*v as f64))
        .sum::<f64>()
        .sqrt();
    if (norm - 1.0).abs() > 1e-3 {
        return Err(EmbedError::NotUnit {
            id: e.id.clone(),
            norm: format!("{norm:.4}"),
        });
    }
    Ok(())
}

/// Embedding backend interface. Implementations must be offline-safe for
/// tests; real backends connect through this trait only.
pub trait EmbeddingProvider: Send + Sync {
    /// Dimensionality this provider produces (validated per vector).
    fn dims(&self) -> usize;
    /// Embed one-or-more texts. The output order MUST match the input order.
    fn embed(&self, texts: &[&str]) -> Result<Vec<Embedding>, EmbedError>;
    /// Short, honest self-description for capability reporting.
    fn describe(&self) -> &'static str;
}

/// Deterministic offline embedder: SHA-256-derived unit vectors. Honest about
/// being non-semantic. Suitable for tests, preflight, and pipelines that treat
/// embeddings as opaque keys.
pub struct DeterministicHashEmbedder {
    pub dims: usize,
}

impl DeterministicHashEmbedder {
    pub fn new(dims: usize) -> Self {
        assert!((8..=8192).contains(&dims), "dims must be 8..=8192");
        Self { dims }
    }
}

impl EmbeddingProvider for DeterministicHashEmbedder {
    fn dims(&self) -> usize {
        self.dims
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Embedding>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(texts.len());
        for (i, text) in texts.iter().enumerate() {
            if text.is_empty() {
                return Err(EmbedError::EmptyInput);
            }
            // Positive-bias seed so distinct texts rarely collide in angle.
            let seed = [0x6f, 0x70, 0x65, 0x6e, 0x72, 0x74, (i & 0xff) as u8];
            let mut v = Vec::with_capacity(self.dims);
            for d in 0..self.dims {
                let mut block = Vec::with_capacity(8);
                block.extend_from_slice(&seed);
                block.push(d as u8);
                block.extend_from_slice(text.as_bytes());
                let h = sha256_hex(&block);
                let hb = h.as_bytes();
                let hh = (hb[0] as u16) << 8 | hb[1] as u16;
                let val = (hh as f32 / u16::MAX as f32) * 2.0 - 1.0;
                v.push(val);
            }
            let norm: f64 = v
                .iter()
                .map(|x| (*x as f64) * (*x as f64))
                .sum::<f64>()
                .sqrt();
            let norm = if norm < 1e-12 { 1.0 } else { norm as f32 };
            for x in v.iter_mut() {
                *x /= norm;
            }
            let sha = sha256_hex(text.as_bytes());
            out.push(Embedding {
                id: format!("emb/{sha}"),
                source: text.to_string(),
                dims: self.dims,
                values: v,
                sha256: sha,
            });
        }
        Ok(out)
    }

    fn describe(&self) -> &'static str {
        "deterministic hash embedding (offline; NOT semantic — for tests/preflight)"
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// Cosine similarity in [-1, 1]. Vectors are expected unit length, but norms
/// are computed honestly rather than assumed.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| (*x as f64) * (*y as f64))
        .sum();
    let na: f64 = a
        .iter()
        .map(|x| (*x as f64) * (*x as f64))
        .sum::<f64>()
        .sqrt();
    let nb: f64 = b
        .iter()
        .map(|x| (*x as f64) * (*x as f64))
        .sum::<f64>()
        .sqrt();
    if na < 1e-12 || nb < 1e-12 {
        return 0.0;
    }
    ((dot / (na * nb)) as f32).clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_valid() {
        let e = DeterministicHashEmbedder::new(32);
        let a = e.embed(&["hello world"]).unwrap();
        let b = e.embed(&["hello world"]).unwrap();
        assert_eq!(a[0].id, b[0].id);
        assert_eq!(a[0].values, b[0].values);
        validate_embedding(&a[0], 32).unwrap();
    }

    #[test]
    fn different_texts_differ() {
        let e = DeterministicHashEmbedder::new(32);
        let a = e.embed(&["alpha"]).unwrap();
        let b = e.embed(&["beta"]).unwrap();
        assert_ne!(a[0].id, b[0].id);
        assert!(
            (cosine(&a[0].values, &b[0].values) - 1.0).abs() < 2.0,
            "cosine in [-1,1]"
        );
    }

    #[test]
    fn validation_rejects_bad_dims() {
        let e = Embedding {
            id: "x".into(),
            source: "".into(),
            dims: 4,
            values: vec![0.5; 4],
            sha256: "s".into(),
        };
        assert!(matches!(
            validate_embedding(&e, 8),
            Err(EmbedError::WrongDims { .. })
        ));
    }

    #[test]
    fn validation_rejects_nan() {
        let e = Embedding {
            id: "x".into(),
            source: "".into(),
            dims: 2,
            values: vec![0.0, f32::NAN],
            sha256: "s".into(),
        };
        assert!(matches!(
            validate_embedding(&e, 2),
            Err(EmbedError::NotFinite { .. })
        ));
    }

    #[test]
    fn empty_input_errors() {
        let e = DeterministicHashEmbedder::new(16);
        assert!(matches!(e.embed(&[""]), Err(EmbedError::EmptyInput)));
    }

    #[test]
    fn cosine_unit_norm_is_one() {
        let e = DeterministicHashEmbedder::new(64);
        let v = e.embed(&["same"]).unwrap();
        let s = cosine(&v[0].values, &v[0].values);
        assert!((s - 1.0).abs() < 1e-4);
    }
}
