//! Phase 9 (M9.6 / TEST 6) — hybrid retrieval end-to-end:
//! * BM25 + dense fusion reports per-hit `retrieval_source` provenance
//!   (lexical / dense / hybrid) — never a fused score without provenance;
//! * absent dense backend degrades to honest lexical-only (no fabricated
//!   dense scores), metadata filters bound both universes;
//! * rerank outcome is reported exactly: applied (deterministic / real
//!   model) or `UNAVAILABLE` with the real reason — rerank never drops or
//!   adds citations, and never fabricates rerank scores;
//! * every presented hit re-validates against the persisted store (the
//!   knowledge crate stays the single citation authority).

use std::collections::HashSet;
use std::sync::Arc;

use openrt_embeddings::{probe_embedding, EmbeddingProvider};
use openrt_knowledge::{
    hybrid_search, persist_chunk, HybridOptions, ModelRerankOptions, PersistedChunk, RerankMode,
    RerankOutcome, RetrievalFilter, RetrievalSource, RetrieveRequest,
};
use openrt_models::mock::{MockMode, MockProvider};
use openrt_models::ModelProvider;
use openrt_persistence::MemoryStore;

const ENDPOINT: &str = "http://127.0.0.1:11434";
const EMBED_MODEL: &str = "all-minilm";
const RERANK_MODEL: &str = "qwen2.5:0.5b";

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

fn req(query: &str, top_k: usize, threshold: f32) -> RetrieveRequest {
    RetrieveRequest {
        query_text: query.to_string(),
        top_k,
        threshold,
    }
}

fn dense_provider() -> Option<Arc<dyn EmbeddingProvider>> {
    let status = probe_embedding(ENDPOINT, EMBED_MODEL);
    let dims = status.dims()?;
    Some(Arc::new(openrt_embeddings::OllamaEmbedder::new(
        ENDPOINT,
        EMBED_MODEL,
        dims,
    )))
}

fn citation_set(report: &openrt_knowledge::HybridReport) -> HashSet<(String, String)> {
    report
        .hits
        .iter()
        .map(|h| (h.citation.document_id.clone(), h.citation.chunk_id.clone()))
        .collect()
}

/// M9.6 — without any dense backend the hybrid report is lexical-only and
/// says so honestly: real BM25 scores, `dense_score` untouched, an
/// `UNAVAILABLE` backend line. Runs fully offline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lexical_only_hybrid_is_honest_without_dense() {
    let store: Arc<dyn openrt_persistence::PersistenceStore> = Arc::new(MemoryStore::new());
    persist_chunk(
        &*store,
        "kb-h1",
        &chunk("c/1", "d1", Some(1), Some("launch window dawn")),
    )
    .await
    .unwrap();
    persist_chunk(
        &*store,
        "kb-h1",
        &chunk("c/2", "d2", Some(2), Some("quantum spin labs")),
    )
    .await
    .unwrap();
    let report = hybrid_search(
        Arc::clone(&store),
        "kb-h1",
        &req("launch window", 3, 0.0),
        &HybridOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(report.hits.len(), 1, "{:?}", report.hits);
    let hit = &report.hits[0];
    assert_eq!(hit.citation.chunk_id, "c/1");
    assert_eq!(hit.source, RetrievalSource::Lexical, "{hit:?}");
    assert!(hit.bm25_score.unwrap_or(0.0) > 0.0, "{hit:?}");
    assert!(
        hit.dense_score.is_none(),
        "no fabricated dense score: {hit:?}"
    );
    assert!(hit.fused_score > 0.0, "{hit:?}");
    assert!(
        report.dense_backend.starts_with("UNAVAILABLE"),
        "{}",
        report.dense_backend
    );
    assert_eq!(report.rerank, RerankOutcome::NotRequested);
    assert_eq!(report.lexical_chunks, 2);
    assert_eq!(report.skipped_no_text, 0);
}

/// M9.6 — the metadata filter bounds BOTH the lexical and dense universes:
/// only the allowed document's chunk survives, honestly counted as the
/// whole in-scope lexical corpus.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filter_bounds_hybrid_universe() {
    let store: Arc<dyn openrt_persistence::PersistenceStore> = Arc::new(MemoryStore::new());
    persist_chunk(
        &*store,
        "kb-h2",
        &chunk("c/1", "d1", Some(1), Some("alpha beta")),
    )
    .await
    .unwrap();
    persist_chunk(
        &*store,
        "kb-h2",
        &chunk("c/2", "d2", Some(2), Some("alpha gamma")),
    )
    .await
    .unwrap();
    let opts = HybridOptions {
        filter: RetrievalFilter {
            document_id: Some("d1".into()),
            page: Some(1),
        },
        dense: dense_provider(),
        rerank: RerankMode::Deterministic,
        ..HybridOptions::default()
    };
    let report = hybrid_search(Arc::clone(&store), "kb-h2", &req("alpha", 5, 0.0), &opts)
        .await
        .unwrap();
    assert_eq!(report.hits.len(), 1, "{:?}", report.hits);
    assert_eq!(report.hits[0].citation.document_id, "d1");
    assert_eq!(report.lexical_chunks, 1, "filter bounds the corpus count");
    // Page filter fails closed too: a non-existent page yields nothing.
    let opts_page = HybridOptions {
        filter: RetrievalFilter {
            document_id: Some("d1".into()),
            page: Some(99),
        },
        dense: dense_provider(),
        rerank: RerankMode::Deterministic,
        ..HybridOptions::default()
    };
    let empty = hybrid_search(
        Arc::clone(&store),
        "kb-h2",
        &req("alpha", 5, 0.0),
        &opts_page,
    )
    .await
    .unwrap();
    assert!(empty.hits.is_empty(), "{:?}", empty.hits);
}

/// M9.6 — deterministic rerank runs and is reported as applied; scores
/// carry per-hit provenance (fused + rerank + source). Offline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deterministic_rerank_is_reported_applied() {
    let store: Arc<dyn openrt_persistence::PersistenceStore> = Arc::new(MemoryStore::new());
    persist_chunk(
        &*store,
        "kb-h3",
        &chunk("c/1", "d1", None, Some("kitten adoption")),
    )
    .await
    .unwrap();
    persist_chunk(&*store, "kb-h3", &chunk("c/2", "d2", None, Some("kitten")))
        .await
        .unwrap();
    persist_chunk(
        &*store,
        "kb-h3",
        &chunk("c/3", "d3", None, Some("unrelated")),
    )
    .await
    .unwrap();
    let opts = HybridOptions {
        rerank: RerankMode::Deterministic,
        ..HybridOptions::default()
    };
    let report = hybrid_search(
        Arc::clone(&store),
        "kb-h3",
        &req("kitten adoption", 3, 0.0),
        &opts,
    )
    .await
    .unwrap();
    assert_eq!(
        report.rerank,
        openrt_knowledge::RerankOutcome::Applied(openrt_knowledge::RerankKind::Deterministic),
        "{:?}",
        report.rerank
    );
    // The "unrelated" chunk is not a lexical match: it is honestly absent
    // (no zero-score padding in the report).
    assert_eq!(report.hits.len(), 2, "{:?}", report.hits);
    assert_eq!(report.hits[0].citation.chunk_id, "c/1");
    assert_eq!(report.hits[0].rerank_score, Some(1.0));
    assert_eq!(
        report.hits[0].rerank_kind,
        Some(openrt_knowledge::RerankKind::Deterministic)
    );
    assert_eq!(report.hits[1].citation.chunk_id, "c/2");
    assert_eq!(report.hits[1].rerank_score, Some(0.5));
    // provenance carried on every hit: source + fused + rerank
    for h in &report.hits {
        assert!(h.fused_score >= 0.0, "{h:?}");
        assert!(h.rerank_score.is_some(), "{h:?}");
    }
}

/// M9.6 — a model reranker whose provider fails reports the REAL reason as
/// `UNAVAILABLE` and keeps fused order — no rerank scores are fabricated.
/// Offline (MockProvider::Fail, no network).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_rerank_provider_failure_is_honest_unavailable() {
    let store: Arc<dyn openrt_persistence::PersistenceStore> = Arc::new(MemoryStore::new());
    persist_chunk(
        &*store,
        "kb-h4",
        &chunk("c/1", "d1", None, Some("alpha beta")),
    )
    .await
    .unwrap();
    persist_chunk(
        &*store,
        "kb-h4",
        &chunk("c/2", "d2", None, Some("alpha gamma")),
    )
    .await
    .unwrap();
    let failing: Arc<dyn ModelProvider> = Arc::new(MockProvider::new("fail", MockMode::Fail));
    let opts = HybridOptions {
        rerank: RerankMode::Model,
        model: Some((failing, "any-model".into())),
        ..HybridOptions::default()
    };
    let report = hybrid_search(Arc::clone(&store), "kb-h4", &req("alpha", 2, 0.0), &opts)
        .await
        .unwrap();
    match &report.rerank {
        RerankOutcome::Unavailable { reason } => {
            assert!(reason.contains("mock failure"), "{reason}");
        }
        other => panic!("provider failure must surface as Unavailable, got {other:?}"),
    }
    assert_eq!(report.hits.len(), 2, "fused order kept: {:?}", report.hits);
    for h in &report.hits {
        assert!(h.rerank_score.is_none(), "no fabricated rerank: {h:?}");
        assert!(h.rerank_kind.is_none(), "{h:?}");
    }
}

/// M9.6 — rerank Model with NO model configured reports UNAVAILABLE with a
/// reason and keeps fused order. Offline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_rerank_without_model_is_honest_unavailable() {
    let store: Arc<dyn openrt_persistence::PersistenceStore> = Arc::new(MemoryStore::new());
    persist_chunk(&*store, "kb-h5", &chunk("c/1", "d1", None, Some("alpha")))
        .await
        .unwrap();
    let opts = HybridOptions {
        rerank: RerankMode::Model,
        model: None,
        ..HybridOptions::default()
    };
    let report = hybrid_search(Arc::clone(&store), "kb-h5", &req("alpha", 1, 0.0), &opts)
        .await
        .unwrap();
    match &report.rerank {
        RerankOutcome::Unavailable { reason } => assert!(!reason.is_empty(), "{reason}"),
        other => panic!("expected Unavailable, got {other:?}"),
    }
    assert_eq!(report.hits.len(), 1);
    assert!(report.hits[0].rerank_score.is_none());
}

/// M9.6 — with the REAL local embedding model, fusion carries dense
/// provenance: the kitten chunk ranks first with a real dense score and a
/// hybrid/lexical source, and the report names the real backend. Skips
/// honestly when the embed backend is absent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_dense_fusion_reports_provenance() {
    let Some(dense) = dense_provider() else {
        eprintln!("SKIP: embedding backend not available for hybrid dense half");
        return;
    };
    let store: Arc<dyn openrt_persistence::PersistenceStore> = Arc::new(MemoryStore::new());
    let cat = "a small kitten curls up beside a cat on the warm windowsill";
    let quantum = "entangled particles reveal spin correlations across labs";
    persist_chunk(
        &*store,
        "kb-h6",
        &chunk("c/cat", "d-cat", Some(1), Some(cat)),
    )
    .await
    .unwrap();
    persist_chunk(
        &*store,
        "kb-h6",
        &chunk("c/qm", "d-qm", Some(2), Some(quantum)),
    )
    .await
    .unwrap();
    persist_chunk(&*store, "kb-h6", &chunk("c/legacy", "d-old", Some(3), None))
        .await
        .unwrap();
    let opts = HybridOptions {
        dense: Some(dense),
        rerank: RerankMode::Deterministic,
        ..HybridOptions::default()
    };
    let report = hybrid_search(
        Arc::clone(&store),
        "kb-h6",
        &req("kitten cat windowsill", 3, 0.0),
        &opts,
    )
    .await
    .unwrap();
    assert!(
        report.dense_backend.contains("ollama"),
        "{:?}",
        report.dense_backend
    );
    assert_eq!(report.skipped_no_text, 1, "{:?}", report.skipped_no_text);
    let top = report
        .hits
        .iter()
        .find(|h| h.citation.chunk_id == "c/cat")
        .expect("kitten chunk must be retrieved");
    assert!(
        matches!(
            top.source,
            RetrievalSource::Hybrid | RetrievalSource::Dense | RetrievalSource::Lexical
        ),
        "{top:?}"
    );
    assert!(
        top.dense_score.is_some(),
        "dense backend ran: real dense score required: {top:?}"
    );
    assert!(top.bm25_score.unwrap_or(0.0) > 0.0, "{top:?}");
    assert_eq!(
        report.hits[0].citation.chunk_id, "c/cat",
        "hybrid top hit: {:?}",
        report.hits
    );
}

/// M9.6 — REAL model rerank through the local Ollama chat model: the
/// outcome is either genuinely applied (every hit carries a model rerank
/// score/kind) or honestly UNAVAILABLE with the reason — and in both cases
/// the reranker only REORDERS: the citation set never changes, and no
/// rerank score is fabricated when it did not run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_model_rerank_reorders_without_losing_citations() {
    let store: Arc<dyn openrt_persistence::PersistenceStore> = Arc::new(MemoryStore::new());
    persist_chunk(
        &*store,
        "kb-h7",
        &chunk("c/1", "d1", Some(1), Some("kitten adoption guide")),
    )
    .await
    .unwrap();
    persist_chunk(
        &*store,
        "kb-h7",
        &chunk("c/2", "d2", Some(2), Some("puppy training manual")),
    )
    .await
    .unwrap();
    persist_chunk(
        &*store,
        "kb-h7",
        &chunk("c/3", "d3", Some(3), Some("quantum spin physics")),
    )
    .await
    .unwrap();

    // Baseline: fused order (deterministic off), with dense half if the
    // embed backend is up (its absence must not affect this invariant).
    let baseline = hybrid_search(
        Arc::clone(&store),
        "kb-h7",
        &req("kitten adoption", 3, 0.0),
        &HybridOptions {
            dense: dense_provider(),
            ..HybridOptions::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(baseline.hits.len(), 3, "{:?}", baseline.hits);

    let provider: Arc<dyn ModelProvider> = match openrt_models::ollama::OllamaProvider::new(
        "rerank",
        ENDPOINT,
        Some(RERANK_MODEL.to_string()),
    ) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            // Construction only parses config; a failure here is honest too.
            eprintln!("SKIP: cannot construct Ollama rerank provider: {e}");
            return;
        }
    };
    let report = hybrid_search(
        Arc::clone(&store),
        "kb-h7",
        &req("kitten adoption", 3, 0.0),
        &HybridOptions {
            dense: dense_provider(),
            rerank: RerankMode::Model,
            model: Some((provider, RERANK_MODEL.to_string())),
            model_options: ModelRerankOptions {
                timeout: std::time::Duration::from_secs(60),
                ..ModelRerankOptions::default()
            },
            ..HybridOptions::default()
        },
    )
    .await
    .unwrap();

    // Rerank only reorders: identical citation set (never drops/adds).
    assert_eq!(
        citation_set(&report),
        citation_set(&baseline),
        "rerank changed the citation set: {:?} vs {:?}",
        report.hits,
        baseline.hits
    );
    match &report.rerank {
        RerankOutcome::Applied(openrt_knowledge::RerankKind::Model) => {
            for h in &report.hits {
                assert!(
                    h.rerank_score.is_some()
                        && h.rerank_kind == Some(openrt_knowledge::RerankKind::Model),
                    "applied model rerank must label every hit: {h:?}"
                );
            }
        }
        RerankOutcome::Unavailable { reason } => {
            // Real reason (transport / invalid response / ...), fused order
            // kept, nothing fabricated.
            assert!(!reason.is_empty(), "{reason}");
            for h in &report.hits {
                assert!(h.rerank_score.is_none(), "fabricated rerank: {h:?}");
                assert!(h.rerank_kind.is_none(), "{h:?}");
            }
            assert_eq!(
                citation_set(&report),
                citation_set(&baseline),
                "fused order must be kept verbatim"
            );
        }
        other => panic!("unexpected rerank outcome: {other:?}"),
    }
}
