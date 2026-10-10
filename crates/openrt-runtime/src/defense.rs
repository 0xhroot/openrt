//! Phase 9 M9.9: prompt-injection defense at the answer path.
//!
//! The ONE production path where retrieved evidence is shaped for a model:
//!
//! 1. retrieval runs through `openrt-knowledge`'s hybrid authority (real
//!    BM25 over persisted chunk text; every hit citation re-validated);
//! 2. every returned passage is SCANNED — detection flags persist on the
//!    `knowledge.evidence` trust record, whose label defaults to
//!    `Untrusted` and only changes through explicit operator action;
//! 3. the trust-labeled context builder wraps the passages so document text
//!    can never be read as operator instruction;
//! 4. any injection categories found are audited through the existing
//!    chained ADR-002 audit log (categories + chunk refs only — never
//!    corpus content).

use std::sync::Arc;

use openrt_core::InjectionCategory;
use openrt_evidence::{scan_evidence, trust_label};
use openrt_knowledge::{hybrid_search, Citation, HybridOptions, RetrieveRequest};
use openrt_retrieval::context::{
    build_evidence_context, ContextItem, ContextOptions, EvidenceContext,
};
use serde::{Deserialize, Serialize};

use crate::Runtime;

/// Answer-path request: retrieve evidence for `query` and build the
/// hardened context a model may see.
#[derive(Debug, Clone, PartialEq)]
pub struct EvidenceContextRequest {
    pub kb_id: String,
    pub query: String,
    pub top_k: usize,
    pub threshold: f32,
    pub context: ContextOptions,
}

impl EvidenceContextRequest {
    /// Bounded defaults (fail-closed knowledge validation still applies).
    pub fn new(kb_id: impl Into<String>, query: impl Into<String>) -> Self {
        Self {
            kb_id: kb_id.into(),
            query: query.into(),
            top_k: 8,
            threshold: 0.0,
            context: ContextOptions::default(),
        }
    }
}

/// What the answer path produced, with everything a reviewer needs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceContextReport {
    /// The hardened, trust-labeled context (markers + labels + warnings).
    pub context: EvidenceContext,
    /// Retrieval hits in rank order (re-validated citations — knowledge
    /// stays THE citation authority).
    pub citations: Vec<Citation>,
    /// Union of detection categories across all retrieved passages.
    pub categories: Vec<InjectionCategory>,
    /// Passages carrying at least one flag (in or out of the context).
    pub flagged: usize,
    /// Chained-audit sequence of the injection entry, `None` when nothing
    /// was flagged (no audit spam for clean contexts).
    pub audit_seq: Option<u64>,
}

impl Runtime {
    /// M9.9 — production answer-path enforcement: retrieve → scan (flags +
    /// UNTRUSTED labels on `knowledge.evidence`) → hardened context builder
    /// → chained audit of any injection categories. `Ok` is returned only
    /// after all of it (an audit failure fails the call, fail-closed).
    pub async fn evidence_context(
        &self,
        req: EvidenceContextRequest,
    ) -> Result<EvidenceContextReport, crate::Error> {
        // 1. retrieval through the single hybrid authority (lexical-only:
        //    no model backend is required for defense, and dense absence is
        //    reported honestly by the hybrid report).
        let report = hybrid_search(
            Arc::clone(&self.store),
            &req.kb_id,
            &RetrieveRequest {
                query_text: req.query.clone(),
                top_k: req.top_k,
                threshold: req.threshold,
            },
            &HybridOptions::default(),
        )
        .await?;

        // 2. scan + label every retrieved passage (persisted flags; default
        //    UNTRUSTED label; chunk text comes from the ONE chunk store).
        let mut items = Vec::with_capacity(report.hits.len());
        let mut citations = Vec::with_capacity(report.hits.len());
        for hit in &report.hits {
            let citation = hit.citation.clone();
            let chunk = openrt_knowledge::load_chunk(
                &*self.store,
                &req.kb_id,
                &citation.document_id,
                &citation.chunk_id,
            )
            .await?;
            let text = chunk.and_then(|c| c.text);
            let (trust, flags) = match &text {
                Some(t) => {
                    let rec = scan_evidence(&*self.store, &req.kb_id, &citation, t).await?;
                    (rec.trust, rec.flags)
                }
                // No stored text: label still flows (default UNTRUSTED);
                // nothing to scan, nothing to wrap — excluded honestly.
                None => (
                    trust_label(&*self.store, &req.kb_id, &citation).await?,
                    Vec::new(),
                ),
            };
            items.push(ContextItem {
                document_id: citation.document_id.clone(),
                chunk_id: citation.chunk_id.clone(),
                page: citation.page,
                text,
                trust,
                flags,
            });
            citations.push(citation);
        }

        // 3. the ONE context builder: markers, labels, warnings, budgets.
        let context = build_evidence_context(&items, &req.context);
        let categories = context.categories.clone();
        let flagged = context.flagged();

        // 4. audit injection categories through the chained ADR-002 log
        //    (ids/categories only — never corpus text).
        let audit_seq = if categories.is_empty() {
            None
        } else {
            let flagged_chunks: Vec<serde_json::Value> = context
                .blocks
                .iter()
                .filter(|b| !b.categories.is_empty())
                .map(|b| {
                    serde_json::json!({
                        "document_id": b.document_id,
                        "chunk_id": b.chunk_id,
                        "included": b.included,
                        "categories": b.categories.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
                    })
                })
                .collect();
            let seq = self
                .audit
                .append(
                    "system",
                    "evidence.injection_flagged",
                    &req.kb_id,
                    "flagged",
                    serde_json::json!({
                        "categories": categories.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
                        "flagged": flagged,
                        "chunks": flagged_chunks,
                    }),
                )
                .await?;
            Some(seq)
        };

        Ok(EvidenceContextReport {
            context,
            citations,
            categories,
            flagged,
            audit_seq,
        })
    }
}
