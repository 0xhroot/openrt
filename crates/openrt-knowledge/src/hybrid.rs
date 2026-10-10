//! `openrt-knowledge::hybrid` — BM25 + dense hybrid retrieval with
//! reranking (Phase 9, M9.6), produced by the `openrt-retrieval` algorithm
//! layer and grounded by `openrt-knowledge`'s citation authority.
//!
//! Honesty contract:
//! * `retrieval_source` (lexical / dense / hybrid) is reported per hit —
//!   never a fused score without provenance;
//! * dense absence degrades to an honest lexical-only report, never to
//!   invented dense scores;
//! * rerank outcome is reported exactly: applied (deterministic or model)
//!   or `Unavailable` with the real reason — the fused order is kept on
//!   failure and no rerank scores are fabricated;
//! * every presented hit re-validates its citation against the persisted
//!   store — same single citation authority as `retrieve`/`semantic_search`.

use std::collections::HashMap;
use std::sync::Arc;

use openrt_embeddings::EmbeddingProvider;
use openrt_models::ModelProvider;
use openrt_persistence::PersistenceStore;
use openrt_retrieval::{
    deterministic_rerank, fuse, model_rerank, Candidate, DenseHit, FusionOptions,
    ModelRerankOptions, RerankKind, RerankMode, RetrievalFilter, RetrievalSource,
};
use openrt_vectorindex::IndexError;

use crate::retrieval::{validate_request, RetrieveRequest};
use crate::semantic::{dense_index_key, embed_query, load_textful, reconcile_dense};
use crate::{Citation, KnowledgeError};

/// One hybrid hit: citation plus full score provenance (M9.6/TEST 6).
#[derive(Debug, Clone, PartialEq)]
pub struct HybridHit {
    pub citation: Citation,
    /// Reciprocal-rank-fusion score (all contributing lists summed).
    pub fused_score: f32,
    /// Which list(s) produced this hit.
    pub source: RetrievalSource,
    pub bm25_score: Option<f32>,
    pub dense_score: Option<f32>,
    pub rerank_score: Option<f32>,
    pub rerank_kind: Option<RerankKind>,
}

/// Honest rerank outcome for a hybrid search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RerankOutcome {
    /// Reranking was not requested; hits are in fused order.
    NotRequested,
    /// A reranker genuinely ran and produced the returned order.
    Applied(RerankKind),
    /// No reranker ran (absent provider / model failure / timeout); hits
    /// keep fused order and the real reason is carried verbatim.
    Unavailable { reason: String },
}

/// Hybrid search result with coverage provenance.
#[derive(Debug, Clone, PartialEq)]
pub struct HybridReport {
    /// Hits that re-validated against persisted chunks (single authority).
    pub hits: Vec<HybridHit>,
    /// Exactly what reranking happened (applied / unavailable + reason).
    pub rerank: RerankOutcome,
    /// The dense backend's honest self-description (or its absence reason).
    pub dense_backend: String,
    /// Textful chunks in scope that BM25 could search (after the filter).
    pub lexical_chunks: usize,
    /// Persisted chunks WITHOUT stored text — excluded everywhere honestly.
    pub skipped_no_text: usize,
}

/// Options for [`hybrid_search`].
#[derive(Clone)]
pub struct HybridOptions {
    /// Metadata filter bounding both the lexical and dense universes.
    pub filter: RetrievalFilter,
    /// Dense backend; `None` = honest lexical-only hybrid.
    pub dense: Option<Arc<dyn EmbeddingProvider>>,
    /// Which reranker to apply after fusion.
    pub rerank: RerankMode,
    /// Real model provider + model id for `RerankMode::Model`.
    pub model: Option<(Arc<dyn ModelProvider>, String)>,
    /// Resource/time caps for model reranking.
    pub model_options: ModelRerankOptions,
}

impl Default for HybridOptions {
    fn default() -> Self {
        Self {
            filter: RetrievalFilter::all(),
            dense: None,
            rerank: RerankMode::Off,
            model: None,
            model_options: ModelRerankOptions::default(),
        }
    }
}

/// Hybrid search: BM25 over real chunk text + dense ranking from the
/// reconciled derived index, fused with deterministic RRF, then optionally
/// reranked — and every hit citation-revalidated (single authority).
///
/// Fail-closed on invalid requests (`validate_request`); store errors
/// propagate; honest degradation otherwise.
pub async fn hybrid_search(
    store: Arc<dyn PersistenceStore>,
    kb_id: &str,
    req: &RetrieveRequest,
    opts: &HybridOptions,
) -> Result<HybridReport, KnowledgeError> {
    validate_request(req)?;
    let query = req.query_text.trim();

    // Dense half: reconcile the derived index (whole KB — a retrieval
    // filter must never cause deletions of other chunks' records), embed
    // the query, and collect the threshold-gated dense ranking.
    let (textful, skipped_no_text, dense_hits, dense_backend) = match &opts.dense {
        Some(provider) => {
            let mut scope = reconcile_dense(&store, kb_id, provider).await?;
            let query_emb = embed_query(provider, &req.query_text).await?;
            // Bounded candidate pool for fusion: deeper than final top_k so
            // RRF sees real ranks, but capped.
            let dense_k = req.top_k.saturating_mul(5).clamp(20, 250);
            let ranked = match scope.index.query(&query_emb, dense_k, req.threshold).await {
                Ok(h) => h,
                Err(IndexError::Empty) => Vec::new(),
                Err(IndexError::DimMismatch { got, expected }) => {
                    return Err(KnowledgeError::Serde(format!(
                        "dense index dims mismatch (index {expected}, query {got}) — \
                         the derived index must be rebuilt for this provider"
                    )))
                }
                Err(e) => {
                    return Err(KnowledgeError::Corrupt(
                        dense_index_key(kb_id),
                        e.to_string(),
                    ))
                }
            };
            let hits: Vec<DenseHit> = ranked
                .into_iter()
                .map(|h| DenseHit::new(h.record.doc_id, h.record.chunk_id, h.score))
                .collect();
            let backend = provider.describe().to_string();
            let skipped = scope.skipped_no_text;
            let textful = std::mem::take(&mut scope.textful);
            (textful, skipped, hits, backend)
        }
        None => {
            let (textful, skipped) = load_textful(store.as_ref(), kb_id).await?;
            (
                textful,
                skipped,
                Vec::new(),
                "UNAVAILABLE — no embedding provider configured (lexical-only hybrid)".to_string(),
            )
        }
    };

    // Lexical universe: textful chunks in KB scope passing the metadata
    // filter. (Dense hits are bounded by this same universe inside `fuse`.)
    let candidates: Vec<Candidate> = textful
        .values()
        .filter(|c| opts.filter.matches(&c.document_id, c.page))
        .map(|c| {
            Candidate::new(
                c.document_id.clone(),
                c.chunk_id.clone(),
                c.page,
                c.text.clone().expect("textful filtered"),
            )
        })
        .collect();
    let lexical_chunks = candidates.len();
    let texts: HashMap<(String, String), String> = candidates
        .iter()
        .map(|c| ((c.document_id.clone(), c.chunk_id.clone()), c.text.clone()))
        .collect();

    let fused = fuse(
        &candidates,
        &dense_hits,
        query,
        &FusionOptions {
            rrf_k: 60.0,
            top_k: req.top_k,
        },
    );

    let (ranked_hits, rerank) = match opts.rerank {
        RerankMode::Off => (fused, RerankOutcome::NotRequested),
        RerankMode::Deterministic => (
            deterministic_rerank(query, fused, &texts),
            RerankOutcome::Applied(RerankKind::Deterministic),
        ),
        RerankMode::Model => match &opts.model {
            None => (
                fused,
                RerankOutcome::Unavailable {
                    reason: "no model provider configured for rerank — keeping fused order"
                        .to_string(),
                },
            ),
            Some((provider, model)) => {
                let fallback = fused.clone();
                match model_rerank(
                    provider.as_ref(),
                    model,
                    query,
                    fused,
                    &texts,
                    &opts.model_options,
                )
                .await
                {
                    Ok(h) => (h, RerankOutcome::Applied(RerankKind::Model)),
                    Err(e) => (
                        fallback,
                        RerankOutcome::Unavailable {
                            reason: e.to_string(),
                        },
                    ),
                }
            }
        },
    };

    // Fail-closed citation re-validation — the single citation authority
    // for lexical, dense, and hybrid retrieval alike.
    let mut hits = Vec::with_capacity(ranked_hits.len());
    for h in ranked_hits {
        let Some(chunk) = textful.get(&(h.document_id.clone(), h.chunk_id.clone())) else {
            continue;
        };
        let citation = Citation {
            kb_id: kb_id.to_string(),
            document_id: chunk.document_id.clone(),
            document_version: chunk.document_version,
            chunk_id: chunk.chunk_id.clone(),
            block_keys: chunk.block_keys.clone(),
            page: chunk.page,
            text_sha256: chunk.text_sha256.clone(),
        };
        if citation.validate(&*store).await? {
            hits.push(HybridHit {
                citation,
                fused_score: h.fused_score,
                source: h.source,
                bm25_score: h.bm25_score,
                dense_score: h.dense_score,
                rerank_score: h.rerank_score,
                rerank_kind: h.rerank_kind,
            });
        }
    }

    Ok(HybridReport {
        hits,
        rerank,
        dense_backend,
        lexical_chunks,
        skipped_no_text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{persist_chunk, PersistedChunk};
    use openrt_embeddings::DeterministicHashEmbedder;
    use openrt_persistence::MemoryStore;

    fn chunk(id: &str, doc: &str, page: Option<u32>, text: Option<&str>) -> PersistedChunk {
        let body = text.unwrap_or_default();
        PersistedChunk {
            chunk_id: id.to_string(),
            document_id: doc.to_string(),
            document_version: 1,
            block_keys: vec![format!("b/{id}")],
            page,
            text_sha256: openrt_embeddings::sha256_hex(body.as_bytes()),
            text: text.map(str::to_string),
        }
    }

    fn store() -> Arc<dyn PersistenceStore> {
        Arc::new(MemoryStore::new())
    }

    fn req(query: &str, top_k: usize, threshold: f32) -> RetrieveRequest {
        RetrieveRequest {
            query_text: query.to_string(),
            top_k,
            threshold,
        }
    }

    #[tokio::test]
    async fn hybrid_without_dense_is_lexical_only_and_honest() {
        let store = store();
        persist_chunk(
            &*store,
            "kb-h",
            &chunk("c/1", "d1", Some(1), Some("launch window dawn")),
        )
        .await
        .unwrap();
        persist_chunk(
            &*store,
            "kb-h",
            &chunk("c/2", "d2", Some(2), Some("quantum spin labs")),
        )
        .await
        .unwrap();
        let report = hybrid_search(
            Arc::clone(&store),
            "kb-h",
            &req("launch window", 3, 0.0),
            &HybridOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(report.hits.len(), 1, "{:?}", report.hits);
        assert_eq!(report.hits[0].citation.chunk_id, "c/1");
        assert_eq!(report.hits[0].source, RetrievalSource::Lexical);
        assert!(report.hits[0].bm25_score.unwrap() > 0.0);
        assert!(report.hits[0].dense_score.is_none());
        assert!(report.hits[0].fused_score > 0.0);
        assert!(
            report.dense_backend.starts_with("UNAVAILABLE —"),
            "{}",
            report.dense_backend
        );
        assert_eq!(report.rerank, RerankOutcome::NotRequested);
        assert_eq!(report.lexical_chunks, 2);
        assert_eq!(report.skipped_no_text, 0);
    }

    #[tokio::test]
    async fn hybrid_with_reference_dense_reports_source_provenance() {
        let store = store();
        persist_chunk(
            &*store,
            "kb-hd",
            &chunk("c/1", "d1", None, Some("kitten yarn basket")),
        )
        .await
        .unwrap();
        persist_chunk(
            &*store,
            "kb-hd",
            &chunk("c/2", "d2", None, Some("puppy dog park")),
        )
        .await
        .unwrap();
        let opts = HybridOptions {
            dense: Some(Arc::new(DeterministicHashEmbedder::new(384))),
            ..HybridOptions::default()
        };
        let report = hybrid_search(Arc::clone(&store), "kb-hd", &req("kitten", 2, 0.0), &opts)
            .await
            .unwrap();
        assert_eq!(report.hits.len(), 2);
        // The lexical match must carry hybrid/lexical provenance with a
        // real dense score attached once both lists contributed.
        let top = &report.hits[0];
        assert_eq!(top.citation.chunk_id, "c/1");
        assert!(
            matches!(
                top.source,
                RetrievalSource::Hybrid | RetrievalSource::Lexical
            ),
            "{:?}",
            top.source
        );
        assert!(
            report.dense_backend.contains("hash"),
            "{}",
            report.dense_backend
        );
    }

    #[tokio::test]
    async fn filter_bounds_hybrid_universe() {
        let store = store();
        persist_chunk(
            &*store,
            "kb-f",
            &chunk("c/1", "d1", Some(1), Some("alpha beta")),
        )
        .await
        .unwrap();
        persist_chunk(
            &*store,
            "kb-f",
            &chunk("c/2", "d2", Some(2), Some("alpha gamma")),
        )
        .await
        .unwrap();
        let opts = HybridOptions {
            filter: RetrievalFilter {
                document_id: Some("d1".into()),
                page: None,
            },
            ..HybridOptions::default()
        };
        let report = hybrid_search(Arc::clone(&store), "kb-f", &req("alpha", 5, 0.0), &opts)
            .await
            .unwrap();
        assert_eq!(report.hits.len(), 1);
        assert_eq!(report.hits[0].citation.document_id, "d1");
        assert_eq!(report.lexical_chunks, 1);
    }

    #[tokio::test]
    async fn deterministic_rerank_applies_and_is_reported() {
        let store = store();
        persist_chunk(
            &*store,
            "kb-r",
            &chunk("c/1", "d1", None, Some("kitten adoption")),
        )
        .await
        .unwrap();
        persist_chunk(&*store, "kb-r", &chunk("c/2", "d2", None, Some("kitten")))
            .await
            .unwrap();
        let opts = HybridOptions {
            rerank: RerankMode::Deterministic,
            ..HybridOptions::default()
        };
        let report = hybrid_search(
            Arc::clone(&store),
            "kb-r",
            &req("kitten adoption", 2, 0.0),
            &opts,
        )
        .await
        .unwrap();
        assert_eq!(
            report.rerank,
            RerankOutcome::Applied(RerankKind::Deterministic)
        );
        assert_eq!(report.hits[0].citation.chunk_id, "c/1");
        assert_eq!(report.hits[0].rerank_kind, Some(RerankKind::Deterministic));
        assert_eq!(report.hits[0].rerank_score, Some(1.0));
        // partial-coverage doc ranks second with its true coverage
        assert_eq!(report.hits[1].citation.chunk_id, "c/2");
        assert_eq!(report.hits[1].rerank_score, Some(0.5));
    }

    #[tokio::test]
    async fn model_rerank_without_provider_is_honest_unavailable() {
        let store = store();
        persist_chunk(&*store, "kb-m", &chunk("c/1", "d1", None, Some("alpha")))
            .await
            .unwrap();
        let opts = HybridOptions {
            rerank: RerankMode::Model,
            model: None,
            ..HybridOptions::default()
        };
        let report = hybrid_search(Arc::clone(&store), "kb-m", &req("alpha", 1, 0.0), &opts)
            .await
            .unwrap();
        match &report.rerank {
            RerankOutcome::Unavailable { reason } => {
                assert!(reason.contains("no model provider"), "{reason}")
            }
            other => panic!("expected honest Unavailable, got {other:?}"),
        }
        assert_eq!(report.hits.len(), 1, "fused order kept: {:?}", report.hits);
        assert!(
            report.hits[0].rerank_score.is_none(),
            "no fabricated rerank"
        );
        assert!(report.hits[0].rerank_kind.is_none());
    }

    #[tokio::test]
    async fn blank_query_fails_closed() {
        let store = store();
        assert!(hybrid_search(
            Arc::clone(&store),
            "kb-x",
            &req("  ", 3, 0.0),
            &HybridOptions::default()
        )
        .await
        .is_err());
    }
}
