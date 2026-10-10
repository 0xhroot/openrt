//! Deterministic evidence checks (M9.8): the layer that always runs first,
//! backend-free and unbounded-free.
//!
//! Two questions, both answerable without a model:
//!
//! 1. do the cited spans genuinely resolve against persisted evidence?
//!    ([`Citation::validate`] — knowledge stays THE citation authority);
//! 2. is the claim lexically present in the supplied evidence text?
//!
//! This layer can only *fail* a claim ([`Grounding::Ungrounded`] /
//! [`Grounding::Weak`]); a lexical match is reported as lexical grounding,
//! never disguised as semantic entailment.

use std::collections::HashSet;

use openrt_knowledge::Citation;
use openrt_persistence::PersistenceStore;
use openrt_retrieval::bm25::tokenize;
use serde::{Deserialize, Serialize};

use crate::{map_knowledge, VerifyError};

/// Minimum share of the claim's content tokens that must appear in the
/// evidence text for the claim to count as deterministically grounded.
pub const MIN_GROUNDING: f32 = 0.5;

/// Minimal function-word set. Lexical grounding is computed over *content*
/// words only, so a shared "the"/"is" can never fake support for a claim.
const FUNCTION_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "been", "but", "by", "for", "from", "had", "has",
    "have", "in", "into", "is", "it", "its", "of", "on", "or", "that", "the", "their", "then",
    "there", "these", "they", "this", "to", "was", "were", "with",
];

/// Unique content tokens of `text` (lowercased, split on non-alphanumerics,
/// function words dropped) — the token set grounding is computed over.
fn content_tokens(text: &str) -> HashSet<String> {
    tokenize(text)
        .into_iter()
        .filter(|t| !FUNCTION_WORDS.contains(&t.as_str()))
        .collect()
}

/// Deterministic grounding tier for a claim against its evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grounding {
    /// Every supplied citation resolves and the claim is lexically present
    /// in the evidence (≥ [`MIN_GROUNDING`] of its tokens).
    Grounded,
    /// Something resolves, but the claim cannot be confirmed from text
    /// alone (missing evidence text, or partial overlap).
    Weak,
    /// Nothing usable backs the claim: no evidence at all, no resolving
    /// citation, or zero lexical overlap.
    Ungrounded,
}

/// Resolution status of one supplied citation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CitationStatus {
    pub document_id: String,
    pub chunk_id: String,
    pub resolved: bool,
}

/// Result of the deterministic evidence check — numbers, never a verdict
/// about meaning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeterministicReport {
    pub grounding: Grounding,
    pub citations: Vec<CitationStatus>,
    pub citations_total: usize,
    pub citations_resolved: usize,
    /// Evidence characters actually considered (already clipped to the
    /// caller's `max_evidence_chars` budget by [`crate::clip_evidence`]).
    pub evidence_chars: usize,
    /// Unique claim content tokens / how many of them also appear in the
    /// evidence (function words excluded on both sides).
    pub claim_tokens: usize,
    pub matched_tokens: usize,
    /// `matched_tokens / claim_tokens` (0.0 for an empty claim).
    pub lexical_coverage: f32,
    pub notes: Vec<String>,
}

impl DeterministicReport {
    /// The supplied citations that did NOT resolve against persisted
    /// `corpus.chunks` (labels `document/chunk`).
    pub fn unresolved(&self) -> Vec<String> {
        self.citations
            .iter()
            .filter(|c| !c.resolved)
            .map(|c| format!("{}/{}", c.document_id, c.chunk_id))
            .collect()
    }
}

/// Run the deterministic evidence check over already-clipped `evidence_text`.
///
/// * `Ok(report)` — the check ran; `report.grounding` is the tier.
/// * `Err` — the store or a citation record failed (fail closed; never
///   reported as "grounded").
pub async fn deterministic_check(
    store: &dyn PersistenceStore,
    claim: &str,
    citations: &[Citation],
    evidence_text: &str,
) -> Result<DeterministicReport, VerifyError> {
    let mut statuses = Vec::with_capacity(citations.len());
    let mut citations_resolved = 0usize;
    for c in citations {
        let resolved = c.validate(store).await.map_err(map_knowledge)?;
        if resolved {
            citations_resolved += 1;
        }
        statuses.push(CitationStatus {
            document_id: c.document_id.clone(),
            chunk_id: c.chunk_id.clone(),
            resolved,
        });
    }

    let evidence_chars = evidence_text.chars().count();
    let claim_tokens = content_tokens(claim);
    let evidence_tokens = content_tokens(evidence_text);
    let matched_tokens = claim_tokens.intersection(&evidence_tokens).count();
    let lexical_coverage = if claim_tokens.is_empty() {
        0.0
    } else {
        matched_tokens as f32 / claim_tokens.len() as f32
    };

    let mut notes = Vec::new();
    let grounding = if citations.is_empty() && evidence_text.trim().is_empty() {
        notes.push("no evidence supplied".to_string());
        Grounding::Ungrounded
    } else if !citations.is_empty() && citations_resolved == 0 {
        notes.push("cited evidence does not resolve against persisted chunks".to_string());
        Grounding::Ungrounded
    } else if evidence_text.trim().is_empty() {
        notes.push("evidence text unavailable; lexical grounding not computable".to_string());
        Grounding::Weak
    } else if lexical_coverage >= MIN_GROUNDING {
        Grounding::Grounded
    } else if lexical_coverage <= 0.0 {
        notes.push("claim shares no content tokens with the cited evidence".to_string());
        Grounding::Ungrounded
    } else {
        notes.push(format!(
            "lexical grounding {lexical_coverage:.2} below the {MIN_GROUNDING:.2} floor"
        ));
        Grounding::Weak
    };
    let unresolved_count = citations.len() - citations_resolved;
    if unresolved_count > 0 {
        notes.push(format!(
            "{unresolved_count} of {} cited span(s) do not resolve",
            citations.len()
        ));
    }

    Ok(DeterministicReport {
        grounding,
        citations: statuses,
        citations_total: citations.len(),
        citations_resolved,
        evidence_chars,
        claim_tokens: claim_tokens.len(),
        matched_tokens,
        lexical_coverage,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_knowledge::{persist_chunk, PersistedChunk};
    use openrt_persistence::MemoryStore;
    use std::sync::Arc;

    const KB: &str = "kb-det";

    fn citation_for(doc: &str, chunk: &str, text: &str) -> Citation {
        Citation {
            kb_id: KB.into(),
            document_id: doc.into(),
            document_version: 1,
            chunk_id: chunk.into(),
            block_keys: vec![format!("b/{chunk}")],
            page: Some(1),
            text_sha256: openrt_core::sha256_hex(text.as_bytes()),
        }
    }

    async fn store_with_chunk(doc: &str, chunk: &str, text: &str) -> Arc<dyn PersistenceStore> {
        let s = Arc::new(MemoryStore::new()) as Arc<dyn PersistenceStore>;
        let c = PersistedChunk {
            chunk_id: chunk.into(),
            document_id: doc.into(),
            document_version: 1,
            block_keys: vec![format!("b/{chunk}")],
            page: Some(1),
            text_sha256: openrt_core::sha256_hex(text.as_bytes()),
            text: Some(text.into()),
        };
        persist_chunk(&*s, KB, &c).await.unwrap();
        s
    }

    #[tokio::test]
    async fn resolving_citation_plus_overlap_is_grounded() {
        let text = "the launch window is dawn";
        let s = store_with_chunk("d1", "c1", text).await;
        let r = deterministic_check(&*s, text, &[citation_for("d1", "c1", text)], text)
            .await
            .unwrap();
        assert_eq!(r.grounding, Grounding::Grounded, "{r:?}");
        assert_eq!(r.citations_resolved, 1);
        assert_eq!(r.lexical_coverage, 1.0);
        assert!(r.unresolved().is_empty());
    }

    #[tokio::test]
    async fn unpersisted_citation_fails_the_check_fail_closed() {
        let text = "the launch window is dawn";
        let s = store_with_chunk("d1", "c1", text).await;
        let ghost = citation_for("absent", "absent", text);
        let r = deterministic_check(&*s, text, &[ghost], text)
            .await
            .unwrap();
        assert_eq!(r.grounding, Grounding::Ungrounded, "{r:?}");
        assert_eq!(r.citations_resolved, 0);
        assert_eq!(r.unresolved(), vec!["absent/absent".to_string()]);
    }

    #[tokio::test]
    async fn unrelated_claim_with_resolving_citation_is_ungrounded() {
        let evidence = "the launch window is dawn";
        let s = store_with_chunk("d1", "c1", evidence).await;
        let r = deterministic_check(
            &*s,
            "the moon is made of cheese",
            &[citation_for("d1", "c1", evidence)],
            evidence,
        )
        .await
        .unwrap();
        assert_eq!(r.grounding, Grounding::Ungrounded, "{r:?}");
        assert_eq!(r.lexical_coverage, 0.0);
    }

    #[tokio::test]
    async fn partial_overlap_is_weak_not_grounded() {
        let evidence = "the launch window is dawn";
        let s = store_with_chunk("d1", "c1", evidence).await;
        let r = deterministic_check(
            &*s,
            "the dawn launch report was delayed by weather",
            &[citation_for("d1", "c1", evidence)],
            evidence,
        )
        .await
        .unwrap();
        assert_eq!(r.grounding, Grounding::Weak, "{r:?}");
        assert!(r.lexical_coverage > 0.0 && r.lexical_coverage < MIN_GROUNDING);
    }

    #[tokio::test]
    async fn resolving_citation_without_text_is_weak_honestly() {
        let evidence = "the launch window is dawn";
        let s = store_with_chunk("d1", "c1", evidence).await;
        let r = deterministic_check(
            &*s,
            "the launch window is dawn",
            &[citation_for("d1", "c1", evidence)],
            "",
        )
        .await
        .unwrap();
        assert_eq!(r.grounding, Grounding::Weak, "{r:?}");
        assert_eq!(r.evidence_chars, 0);
        assert_eq!(r.citations_resolved, 1);
    }

    #[tokio::test]
    async fn nothing_supplied_is_ungrounded() {
        let s = Arc::new(MemoryStore::new()) as Arc<dyn PersistenceStore>;
        let r = deterministic_check(&*s, "any claim", &[], "")
            .await
            .unwrap();
        assert_eq!(r.grounding, Grounding::Ungrounded, "{r:?}");
        assert!(r.notes.iter().any(|n| n.contains("no evidence supplied")));
    }
}
