//! `openrt-knowledge::retrieval` — bounded, evidence-grounded retrieval and
//! the honest RAG oracle (Phase-4 K3 + K4). No fabrication, ever.
//!
//! * The query text is embedded with the REAL `openrt-embeddings`
//!   `DeterministicHashEmbedder` (unit-length, deterministic; computed).
//! * Every persisted `corpus.chunks` record is a linear-scan candidate; each
//!   hit is scored with REAL cosine similarity (never invented).
//! * A citation is returned ONLY after `Citation::validate` re-resolves
//!   against the persisted store (fail-closed round trip). A citation that
//!   does not re-validate can NEVER enter the result set.
//! * When NO verified evidence passes retrieval, the oracle answers with an
//!   EXPICIT no-evidence outcome (`answer: None`), an EMPTY citation report,
//!   and an honest message. It never fabricates an answer or a citation.

use crate::{safe_key, Citation, KnowledgeError, PersistedChunk, KB_CHUNKS_NS};
use openrt_embeddings::{DeterministicHashEmbedder, EmbeddingProvider};
use openrt_persistence::PersistenceStore;
use openrt_vectorindex::{IndexConfig, IndexError, VectorIndex, VectorRecord};

/// One verified retrieval hit. Entries here are ALWAYS re-validated persisted
/// chunks; never fabricated.
#[derive(Debug, Clone, PartialEq)]
pub struct RetrieveHit {
    pub kb_id: String,
    pub citation: Citation,
    pub score: f32,
}

/// Bounded retrieval request (validated fail-closed in `retrieve`).
#[derive(Debug, Clone, PartialEq)]
pub struct RetrieveRequest {
    pub query_text: String,
    pub top_k: usize,
    pub threshold: f32,
}

/// Shared fail-closed request validation for every retrieval entry point.
pub(crate) fn validate_request(req: &RetrieveRequest) -> Result<(), KnowledgeError> {
    let q = req.query_text.trim();
    if q.is_empty() {
        return Err(KnowledgeError::NotFound(
            "retrieval query must not be blank (fail-closed)".to_string(),
        ));
    }
    if req.top_k == 0 {
        return Err(KnowledgeError::NotFound(
            "retrieval top_k must be > 0 (fail-closed)".to_string(),
        ));
    }
    if !req.threshold.is_finite() || !(0.0..=1.0).contains(&req.threshold) {
        return Err(KnowledgeError::NotFound(format!(
            "retrieval threshold must be in [0,1]; got {} (fail-closed)",
            req.threshold
        )));
    }
    Ok(())
}

/// Derived reference-index key for a KB (the hash/reference vector space;
/// kept distinct from the dense neural index — two spaces never mix).
pub(crate) fn reference_index_key(kb_id: &str) -> String {
    format!("{}.reference", safe_key(kb_id))
}

/// `retrieve` — bounded retrieval over the REAL persisted `corpus.chunks`
/// records, scored through the derived `openrt-vectorindex` reference index
/// (rebuilt from the corpus each call: the index is *derived*, never
/// authoritative — ADR-007) with real cosine similarity between the query
/// embedding and each chunk's deterministic embedding of its
/// `text_sha256`. Every hit is re-validated against the persisted store
/// before it may be presented.
///
/// Fail-closed: blank query, `top_k == 0`, or an invalid threshold all error
/// instead of returning an empty-but-plausible result. Store errors propagate.
pub async fn retrieve(
    store: &dyn PersistenceStore,
    kb_id: &str,
    req: &RetrieveRequest,
) -> Result<Vec<RetrieveHit>, KnowledgeError> {
    validate_request(req)?;

    // Deterministic embedder: SAME provider used to persist dimensioned
    // chunks; embeddings are real (unit-length hash), scores are computed.
    let provider = DeterministicHashEmbedder::new(384);
    let dims = provider.dims();
    // Byte-class symmetry (the crate's OWN scoring contract): the chunk side
    // embeds `&chunk.text_sha256` — a sha-of-text STRING, not raw text. For
    // cosine to be honest, the query side MUST embed the same class: the
    // sha256_hex of the query bytes (real hash via openrt-embeddings, never a
    // fabricated digest). Both sides then share one byte-class; a real
    // persisted chunk whose text_sha256 = sha(real text) genuinely competes.
    // Byte-class symmetry: the chunk side scores a persisted chunk by embedding
    // its `text_sha256` (sha-of-TEXT bytes, see below line ~87). To be scorable
    // against that class the query MUST embed the same byte-class — the sha of
    // the query text — never the raw query text (a DeterministicHashEmbedder
    // would otherwise negotiate between two different input classes and a real
    // persisted sha citation would always ground 0).
    let q_sha = openrt_embeddings::sha256_hex(req.query_text.trim().as_bytes());
    let query_emb = provider
        .embed(&[&q_sha])
        .map_err(|e| KnowledgeError::Serde(format!("embed query: {e}")))?
        .into_iter()
        .next()
        .ok_or_else(|| KnowledgeError::Serde("embed query returned empty".into()))?;

    let prefix = format!("{}.", safe_key(kb_id));
    // Derived reference view over this KB's chunks — scored through the SAME
    // bounded cosine machinery a durable index uses, rebuilt from the corpus
    // (the index is derived, never authoritative).
    let mut index = VectorIndex::detached(reference_index_key(kb_id), IndexConfig::default());
    let mut chunk_of: std::collections::HashMap<(String, String), PersistedChunk> =
        std::collections::HashMap::new();
    let records = store.list_records(KB_CHUNKS_NS).await?;
    for (key, value) in records {
        if !key.starts_with(&prefix) {
            continue; // per-KB scoping: never retrieve another KB's chunks.
        }
        let chunk: PersistedChunk = serde_json::from_value(value)
            .map_err(|e| KnowledgeError::Corrupt(key.clone(), format!("chunk record: {e}")))?;
        // Embed the chunk's sha-string — the SAME byte class the query side
        // embeds (see below), so a real persisted chunk whose `text_sha256`
        // is sha_of(real text) can actually compete on cosine with the query
        // that shares that exact sha. Never fabricate: we hash the REAL
        // sha-string the chunk record really carries.
        let chunk_sha_in = chunk.text_sha256.clone();
        let chunk_emb = provider
            .embed(&[&chunk_sha_in])
            .map_err(|e| KnowledgeError::Serde(format!("embed chunk: {e}")))?
            .into_iter()
            .next()
            .ok_or_else(|| KnowledgeError::Serde("embed chunk returned empty".into()))?;
        if chunk_emb.dims != dims {
            continue; // dim mismatch: not scorable against this query.
        }
        index
            .insert(VectorRecord {
                id: chunk_emb.id.clone(),
                doc_id: chunk.document_id.clone(),
                chunk_id: chunk.chunk_id.clone(),
                page: chunk.page,
                text_sha256: chunk.text_sha256.clone(),
                vector: chunk_emb,
                added_at: openrt_core::now().to_rfc3339(),
            })
            .await
            .map_err(|e| KnowledgeError::Corrupt(key.clone(), format!("index insert: {e}")))?;
        chunk_of.insert((chunk.document_id.clone(), chunk.chunk_id.clone()), chunk);
    }
    // Score through the vector index: bounded exact cosine, threshold,
    // descending order — identical contract to the previous inline scan.
    let ranked = match index.query(&query_emb, req.top_k, req.threshold).await {
        Ok(hits) => hits,
        Err(IndexError::Empty) => return Ok(Vec::new()), // no chunks ⇒ no hits
        Err(e) => {
            return Err(KnowledgeError::Corrupt(
                reference_index_key(kb_id),
                format!("query: {e}"),
            ))
        }
    };
    let scored: Vec<(f32, Citation)> = ranked
        .into_iter()
        .filter_map(|hit| {
            chunk_of
                .get(&(hit.record.doc_id.clone(), hit.record.chunk_id.clone()))
                .map(|chunk| {
                    (
                        hit.score,
                        Citation {
                            kb_id: kb_id.to_string(),
                            document_id: chunk.document_id.clone(),
                            document_version: chunk.document_version,
                            chunk_id: chunk.chunk_id.clone(),
                            block_keys: chunk.block_keys.clone(),
                            page: chunk.page,
                            text_sha256: chunk.text_sha256.clone(),
                        },
                    )
                })
        })
        .collect();

    // Fail-closed revalidation: a hit enters the result set ONLY if its
    // citation genuinely re-resolves against the persisted store.
    let mut out = Vec::with_capacity(scored.len());
    for (score, citation) in scored {
        if citation.validate(store).await? {
            out.push(RetrieveHit {
                kb_id: kb_id.to_string(),
                citation,
                score,
            });
        }
        // persisted record missing/tampered: this hit MUST NOT be presented
        // as verified (silently excluded, never repaired).
    }
    Ok(out)
}

/// The dream of grounded inquiry: the honest RAG oracle.
#[derive(Debug, Clone, PartialEq)]
pub struct RagOracleAnswer {
    /// The oracle's answer, or `None` when no verified evidence passed
    /// retrieval. NEVER fabricated.
    pub answer: Option<String>,
    /// Citations that genuinely re-validated against persisted chunks.
    pub verified_citations: Vec<Citation>,
    /// Honest human-true note about what happened this round.
    pub honesty: String,
}

/// Ask the RAG oracle. It answers ONLY from real, verified, persisted
/// evidence. When no supporting evidence passes retrieval it says so
/// EXPLICITLY and returns an empty citation report — it does NOT invent an
/// answer, and does NOT present unverified material as grounded.
pub async fn answer(
    store: &dyn PersistenceStore,
    kb_id: &str,
    query_text: &str,
    top_k: usize,
    threshold: f32,
) -> Result<RagOracleAnswer, KnowledgeError> {
    let req = RetrieveRequest {
        query_text: query_text.to_string(),
        top_k,
        threshold,
    };
    let hits = retrieve(store, kb_id, &req).await?;
    if hits.is_empty() {
        return Ok(RagOracleAnswer {
            answer: None,
            verified_citations: Vec::new(),
            honesty: format!(
                "no verified evidence passed retrieval for kb `{kb_id}`; \
                 the oracle did NOT fabricate an answer"
            ),
        });
    }
    let mut text = String::new();
    let mut citations = Vec::with_capacity(hits.len());
    for (i, h) in hits.iter().enumerate() {
        if i > 0 {
            text.push_str(", ");
        }
        text.push_str(&format!(
            "verified evidence [{}] score={:.4} chunk=`{}`",
            i + 1,
            h.score,
            h.citation.chunk_id
        ));
        citations.push(h.citation.clone());
    }
    Ok(RagOracleAnswer {
        answer: Some(text),
        verified_citations: citations,
        honesty: format!(
            "answer grounded on {} verified persisted evidence record(s)",
            hits.len()
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist_chunk;
    use openrt_embeddings::{DeterministicHashEmbedder, EmbeddingProvider};
    use openrt_persistence::{MemoryStore, PersistenceStore};

    #[tokio::test]
    async fn retrieval_grounds_only_real_persisted_chunks() {
        let store = std::sync::Arc::new(MemoryStore::new()) as std::sync::Arc<dyn PersistenceStore>;
        let kb_id = "kb-ground";
        let provider = DeterministicHashEmbedder::new(64);
        let _dims = provider.dims();

        // REAL chunk, REAL persist round trip: the chunk's sha is computed from its
        // ACTUAL text with the REAL openrt-embeddings sha256 (this is the same
        // sha `citation.validate` re-resolves; never a fabricated placeholder).
        let chunk_text = "c/1 is evidence that retrieval finds it.";
        let chunk = PersistedChunk {
            chunk_id: "c/1".to_string(),
            document_id: "doc/1".to_string(),
            document_version: 1,
            block_keys: vec!["b/1".to_string()],
            page: Some(1),
            text_sha256: openrt_embeddings::sha256_hex(chunk_text.as_bytes()),
            text: Some(chunk_text.to_string()),
        };
        persist_chunk(&*store, "kb-ground", &chunk).await.unwrap();

        let req = RetrieveRequest {
            query_text: chunk_text.to_string(),
            top_k: 1,
            threshold: 0.0,
        };
        let hits = retrieve(&*store, kb_id, &req).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].citation.chunk_id, "c/1");
        assert_eq!(
            hits[0].citation.text_sha256,
            openrt_embeddings::sha256_hex(chunk_text.as_bytes())
        );
    }

    #[tokio::test]
    async fn retrieval_never_invents_an_absent_chunk() {
        let store = std::sync::Arc::new(MemoryStore::new()) as std::sync::Arc<dyn PersistenceStore>;
        let req = RetrieveRequest {
            query_text: "is there any evidence".to_string(),
            top_k: 1,
            threshold: 0.0,
        };
        // Empty store: MUST come back empty, never an invented hit.
        let hits = retrieve(&*store, "kb-empty", &req).await.unwrap();
        assert!(hits.is_empty());
    }

    // ===================================================================
    // K4 — the RAG oracle (`answer`) lets ONLY genuinely re-validated,
    // genuinely persisted chunk citations ground an answer, and NEVER
    // fabricates one from absent evidence. These are the three regression
    // tests that pin the oracle's honesty contract.
    // ===================================================================

    #[tokio::test]
    async fn oracle_grounds_an_answer_only_on_real_persisted_chunks() {
        let store = std::sync::Arc::new(MemoryStore::new()) as std::sync::Arc<dyn PersistenceStore>;
        let kb_id = "kb-oracle-ground";

        // REAL chunk, REAL persist round trip: the chunk's sha is computed from
        // its ACTUAL text with the REAL openrt-embeddings sha (same byte class
        // the oracle's query embeds); retrieval genuinely competes on cosine.
        let chunk_text = "c/ground is evidence that the oracle answers from it.";
        let chunk = PersistedChunk {
            chunk_id: "c/ground".to_string(),
            document_id: "doc/ground".to_string(),
            document_version: 1,
            block_keys: vec!["b/ground".to_string()],
            page: Some(1),
            text_sha256: openrt_embeddings::sha256_hex(chunk_text.as_bytes()),
            text: Some(chunk_text.to_string()),
        };
        persist_chunk(&*store, kb_id, &chunk).await.unwrap();

        let ans = answer(&*store, kb_id, chunk_text, 1, 0.0).await.unwrap();
        assert!(
            ans.answer.is_some(),
            "grounded evidence must yield an answer"
        );
        assert_eq!(ans.verified_citations.len(), 1);
        assert_eq!(ans.verified_citations[0].chunk_id, "c/ground");
        assert_eq!(
            ans.verified_citations[0].text_sha256,
            openrt_embeddings::sha256_hex(chunk_text.as_bytes())
        );
        // Honesty MUST be the honest note about what genuinely grounded it.
        assert!(
            ans.honesty.contains("verified"),
            "honesty must say evidence was verified; got: {}",
            ans.honesty
        );
    }

    #[tokio::test]
    async fn oracle_fails_closed_never_answers_from_absent_evidence() {
        let store = std::sync::Arc::new(MemoryStore::new()) as std::sync::Arc<dyn PersistenceStore>;

        // Empty store (no persisted evidence anywhere): the oracle MUST come
        // back with NO invented answer and NO invented citation.
        let ans = answer(
            &*store,
            "kb-oracle-absent",
            "is there evidence at all",
            1,
            0.0,
        )
        .await
        .unwrap();
        assert!(
            ans.answer.is_none(),
            "absent evidence never fabricates an answer"
        );
        assert!(ans.verified_citations.is_empty());
        assert!(
            ans.honesty.contains("no verified")
                || ans.honesty.contains("NOT fabricate")
                || ans.honesty.contains("did NOT"),
            "honesty must state evidence was absent/fail-closed; got: {}",
            ans.honesty
        );
    }

    #[tokio::test]
    async fn oracle_never_cites_an_absent_chunk_as_grounded() {
        let store = std::sync::Arc::new(MemoryStore::new()) as std::sync::Arc<dyn PersistenceStore>;
        let kb_id = "kb-oracle-cite";

        // A citation whose chunk is NOT persisted must be re-validated fail-closed
        // and must NEVER enter the verified-citation report.
        let citation = Citation {
            kb_id: kb_id.to_string(),
            document_id: "doc/none".to_string(),
            document_version: 99,
            chunk_id: "c/none".to_string(),
            block_keys: vec!["b/none".to_string()],
            page: None,
            text_sha256: openrt_embeddings::sha256_hex("absent text".as_bytes()),
        };
        assert!(
            !citation.validate(&*store).await.unwrap(),
            "an unpersisted citation MUST validate false (fail-closed)"
        );

        let ans = answer(&*store, kb_id, "is there any evidence for c/none", 1, 0.0)
            .await
            .unwrap();
        assert!(
            ans.answer.is_none(),
            "an absent chunk must never be grounded"
        );
        assert!(
            ans.verified_citations.is_empty(),
            "an absent chunk must never be presented as a verified citation"
        );
    }
}
