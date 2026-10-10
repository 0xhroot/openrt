//! `openrt-verifier` — answer/claim verification + the hallucination
//! firewall (Phase 9, M9.8).
//!
//! Honesty rules (M9.8 / TEST 8):
//!
//! * **Deterministic evidence checks run first.** Citations re-resolve
//!   against the knowledge layer's persisted `corpus.chunks` (knowledge
//!   stays THE citation authority — this crate never re-implements citation
//!   equality) and the claim is lexically grounded against the supplied
//!   evidence text. This layer can *fail* a claim; it can never fabricate
//!   support for one.
//! * **Model verification is optional and bounded.** When a real backend is
//!   reachable, it must answer strict
//!   `SUPPORTED | UNSUPPORTED | INSUFFICIENT` JSON inside explicit time and
//!   size caps; anything else is a structured [`VerifyError::InvalidResponse`],
//!   never a verdict. With no backend, the report says so through
//!   [`Independence::Degraded`] — there is no echo-as-verifier substitute in
//!   any production path.
//! * **Cross-model `INDEPENDENT` only for genuinely distinct models.** The
//!   same model id run twice is deduplicated and reported as an honest
//!   degradation; disagreement is inconclusive (`Flag`), never averaged
//!   away.
//! * **Model verdicts may only demote.** A model cannot lift a claim past
//!   the deterministic evidence floor: `Unsupported` blocks, `Insufficient`
//!   flags, but a model `SUPPORTED` over an evidence gap stays flagged.
//! * Every verdict is persisted under [`VERDICTS_NS`] (`verify.verdicts`)
//!   with the claim, evidence refs, backends, limits and result — never
//!   model secrets.

pub mod cross;
pub mod deterministic;
pub mod firewall;
pub mod model;
pub mod persist;
pub mod types;

use std::time::Duration;

pub use cross::{cross_model_verify, CrossReport};
pub use deterministic::{
    deterministic_check, CitationStatus, DeterministicReport, Grounding, MIN_GROUNDING,
};
pub use firewall::{
    clip_evidence, verify_claim, verify_claims, BackendSpec, FirewallReport, LimitsSnapshot,
    VerifyInput, VerifyOptions,
};
pub use model::{model_verify, parse_verdict, Backend, BackendFailure, ModelVerdict};
pub use persist::{list_verdicts, record_verdict, EvidenceRefRecord, VerdictRecord, VERDICTS_NS};
pub use types::{Basis, FirewallAction, Independence, Verdict, VerifyLimits};

/// Errors from the verification layer. Fail-closed by construction: a
/// `Result::Err` never means "verified" and a structured unavailability is
/// never presented as a verdict.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("invalid verification request: {0}")]
    Invalid(String),
    #[error("verification limits exceeded: {0}")]
    Limits(String),
    #[error("model verification unavailable: {0}")]
    Unavailable(String),
    #[error("model verification timed out after {0:?}")]
    Timeout(Duration),
    #[error("model verification returned an invalid response: {0}")]
    InvalidResponse(String),
    #[error("store error: {0}")]
    Store(#[from] openrt_persistence::PersistenceError),
    #[error("serialize error: {0}")]
    Serde(String),
    #[error("corrupt verdict record `{0}`: {1}")]
    Corrupt(String, String),
    #[error("knowledge layer: {0}")]
    Knowledge(String),
}

/// Map a `KnowledgeError` into a [`VerifyError`]: store failures stay store
/// failures (retryable), everything else is reported as knowledge-layer.
pub(crate) fn map_knowledge(e: openrt_knowledge::KnowledgeError) -> VerifyError {
    use openrt_knowledge::KnowledgeError as K;
    match e {
        K::Store(s) => VerifyError::Store(s),
        other => VerifyError::Knowledge(other.to_string()),
    }
}
