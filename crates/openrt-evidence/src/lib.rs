//! `openrt-evidence` — the append-only evidence graph + knowledge graph
//! (Phase 9, M9.7).
//!
//! Two namespaces on the ONE shared store (ADR-006):
//!
//! * [`EVIDENCE_NS`] — **append-only**. `ClaimRecord`s (content-addressed by
//!   `sha256(text)`) and `EvidenceEdge`s chaining claims ↔ source passages ↔
//!   documents. Every edge carries `prev_hash`/`edge_hash` so tampering,
//!   deletion or reordering is detected by [`verify_integrity`]. Nothing in
//!   this crate ever overwrites or deletes an existing evidence record
//!   through its API.
//! * [`GRAPH_NS`] — **evolving** (never duplicated): knowledge-graph
//!   `Entity`/`Relation` records, upserted in place with `created_at`
//!   preserved.
//!
//! Citations are **re-resolved against the knowledge layer's persisted
//! `corpus.chunks`** via `openrt_knowledge::Citation::validate` — knowledge
//! remains THE citation authority; this crate never re-implements citation
//! equality.

pub mod evidence;
pub mod injection;
pub mod kg;

pub use evidence::{
    claims_for_document, evidence_for_claim, link_claim, list_claims, list_edges,
    passages_for_claim, record_claim, resolve_citation, verify_integrity, ClaimRecord, EdgeKind,
    EvidenceEdge, IntegrityReport,
};
pub use injection::{
    detect_injection, get_trust, list_trust, mark_operator_trusted, scan_evidence, trust_label,
    EvidenceTrustRecord,
};
pub use kg::{add_relation, list_entities, list_relations, upsert_entity, Entity, Relation};

use thiserror::Error;

/// `openrt-evidence` owned namespace: append-only claims + grounding edges.
pub const EVIDENCE_NS: &str = "knowledge.evidence";
/// `openrt-evidence` owned namespace: evolving knowledge-graph records.
pub const GRAPH_NS: &str = "knowledge.graph";

/// Errors from the evidence/knowledge-graph layer. Fail-closed by
/// construction: a `Result::Err` never means "recorded"/"verified".
#[derive(Debug, Error)]
pub enum EvidenceError {
    #[error("store error: {0}")]
    Store(#[from] openrt_persistence::PersistenceError),
    #[error("serialize error: {0}")]
    Serde(String),
    #[error("corrupt record `{0}`: {1}")]
    Corrupt(String, String),
    #[error("record `{0}` not found")]
    NotFound(String),
    #[error("append-only violation for `{0}`: {1}")]
    AppendViolation(String, String),
    #[error("citation does not resolve against persisted evidence: {0}")]
    UnresolvedCitation(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("knowledge layer: {0}")]
    Knowledge(String),
}

/// Map a `KnowledgeError` into `EvidenceError`: store failures stay store
/// failures (retryable), everything else is reported as knowledge-layer.
pub(crate) fn map_knowledge(e: openrt_knowledge::KnowledgeError) -> EvidenceError {
    use openrt_knowledge::KnowledgeError as K;
    match e {
        K::Store(s) => EvidenceError::Store(s),
        other => EvidenceError::Knowledge(other.to_string()),
    }
}
