//! Honest capability status for embedding backends (M9.5) — always compiled,
//! mirroring `openrt-vlm`'s status model so doctor/CLI can report an honest
//! `UNAVAILABLE` even when the crate is built WITHOUT the HTTP feature
//! (SEC-07: libraries default-build with no HTTP stack).

use std::fmt;

/// Default local endpoint and model (all-minilm: 384 dims — dimensionality
/// is always *measured* by the probe, never assumed).
pub const DEFAULT_EMBED_ENDPOINT: &str = "http://127.0.0.1:11434";
pub const DEFAULT_EMBED_MODEL: &str = "all-minilm";

/// Capability status of an embedding backend, from a REAL probe embedding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbeddingStatus {
    /// Probe embedding succeeded; `dims` was measured from its response.
    Available {
        model: String,
        dims: usize,
        elapsed_ms: u128,
    },
    /// Endpoint unreachable, the server cannot embed, or the crate was built
    /// without an HTTP transport.
    Unavailable { reason: String },
    /// Endpoint is up but does not serve `model`.
    ModelMissing { model: String, reason: String },
}

impl EmbeddingStatus {
    pub fn is_available(&self) -> bool {
        matches!(self, EmbeddingStatus::Available { .. })
    }

    /// Machine-readable label: `AVAILABLE` / `UNAVAILABLE` / `MODEL_MISSING`.
    pub fn label(&self) -> &'static str {
        match self {
            EmbeddingStatus::Available { .. } => "AVAILABLE",
            EmbeddingStatus::Unavailable { .. } => "UNAVAILABLE",
            EmbeddingStatus::ModelMissing { .. } => "MODEL_MISSING",
        }
    }

    /// Measured dimensionality, only when the probe actually embedded.
    pub fn dims(&self) -> Option<usize> {
        match self {
            EmbeddingStatus::Available { dims, .. } => Some(*dims),
            _ => None,
        }
    }
}

impl fmt::Display for EmbeddingStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EmbeddingStatus::Available {
                model,
                dims,
                elapsed_ms,
            } => write!(
                f,
                "AVAILABLE — `{model}` neural embeddings ({dims} dims, probe {elapsed_ms} ms)"
            ),
            EmbeddingStatus::Unavailable { reason } => write!(f, "UNAVAILABLE — {reason}"),
            EmbeddingStatus::ModelMissing { model, reason } => {
                write!(f, "MODEL_MISSING — `{model}`: {reason}")
            }
        }
    }
}
