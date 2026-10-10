//! `openrt-retrieval` — hybrid retrieval + reranking (Phase 9, M9.6).
//!
//! Pure algorithm layer: BM25 lexical scoring, Reciprocal-Rank-Fusion with
//! explicit `retrieval_source` provenance, metadata filters, and rerankers
//! (deterministic + model-backed over the model-provider infrastructure).
//!
//! Deliberately store-agnostic and citation-free: `openrt-knowledge` remains
//! the single deterministic retrieval + citation authority (ADR-007 /
//! OWNERSHIP Phase 9) and calls into this crate for the ranking mechanics.
//! Nothing here fabricates scores: absent lists degrade to honest
//! lexical-only / dense-only fusion, and model reranking either returns a
//! model-produced permutation or a structured [`RerankError`].

pub mod bm25;
pub mod context;
pub mod filters;
pub mod hybrid;
pub mod rerank;

pub use bm25::{tokenize, Bm25Hit, Bm25Index, Candidate};
pub use context::{
    build_evidence_context, wrap_untrusted, ContextBlock, ContextItem, ContextOptions,
    EvidenceContext, BLOCK_BEGIN_PREFIX, BLOCK_END, UNTRUSTED_NOTICE,
};
pub use filters::RetrievalFilter;
pub use hybrid::{
    filter_candidates, fuse, DenseHit, FusionOptions, HybridHit, RerankKind, RetrievalSource,
};
pub use rerank::{deterministic_rerank, model_rerank, ModelRerankOptions, RerankError, RerankMode};
