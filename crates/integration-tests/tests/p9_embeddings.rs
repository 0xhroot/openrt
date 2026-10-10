//! Phase 9 (M9.5) — the neural embedding fabric end-to-end:
//! * dead endpoints and absent models surface as the structured
//!   `UNAVAILABLE` / `MODEL_MISSING` statuses — never a fabricated probe;
//! * the dense path embeds REAL chunk text (textless chunks honestly
//!   skipped, never guessed) and grounds hits on the persisted corpus;
//! * semantic sanity with the real local model (cat/kitten closer than
//!   cat/quantum) when the backend is actually available;
//! * doctor carries `embeddings.config` + `embeddings.backend`, and a PASS
//!   may only come from a genuinely available backend.

use std::sync::Arc;

use openrt_config::{Config, RuntimeConfig};
use openrt_embeddings::{probe_embedding, EmbeddingProvider, EmbeddingStatus};
use openrt_knowledge::{persist_chunk, semantic_search, PersistedChunk, RetrieveRequest};
use openrt_models::mock::MockProvider;
use openrt_persistence::MemoryStore;

/// Default endpoint/model probed by the workspace (matches config defaults).
const ENDPOINT: &str = "http://127.0.0.1:11434";
const MODEL: &str = "all-minilm";

fn chunk(id: &str, text: Option<&str>) -> PersistedChunk {
    let body = text.unwrap_or_default();
    PersistedChunk {
        chunk_id: id.to_string(),
        document_id: "doc/emb".to_string(),
        document_version: 1,
        block_keys: vec![format!("b/{id}")],
        page: Some(1),
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

/// M9.5 — a dead endpoint reports UNAVAILABLE with a reason (always runs;
/// localhost port 1 refuses instantly, no external network involved).
#[test]
fn dead_endpoint_reports_unavailable() {
    let status = probe_embedding("http://127.0.0.1:1", MODEL);
    assert_eq!(status.label(), "UNAVAILABLE", "{status}");
    let text = status.to_string();
    assert!(
        text.len() > "UNAVAILABLE — ".len(),
        "reason must be carried: {text}"
    );
}

/// M9.5 — probing the real endpoint reports one of the three honest states,
/// each with its invariants (dims measured, never assumed).
#[test]
fn probe_reports_honest_status() {
    let status = probe_embedding(ENDPOINT, MODEL);
    match &status {
        EmbeddingStatus::Available {
            model,
            dims,
            elapsed_ms: _,
        } => {
            assert!(model.contains("all-minilm"), "model: {model}");
            assert!(*dims > 0, "dims must be measured > 0");
        }
        EmbeddingStatus::ModelMissing { model, reason } => {
            assert_eq!(model, MODEL);
            assert!(!reason.is_empty(), "{reason}");
        }
        EmbeddingStatus::Unavailable { reason } => {
            // Honest when no Ollama endpoint runs on this host.
            assert!(!reason.is_empty(), "{reason}");
        }
    }
}

/// M9.5 — an endpoint that is alive but lacks the requested model reports
/// MODEL_MISSING (structured, actionable). Skips when no endpoint is
/// reachable at all.
#[test]
fn missing_model_is_structured() {
    let status = probe_embedding(ENDPOINT, "openrt-absent-embed-xyz");
    match &status {
        EmbeddingStatus::Unavailable { reason } => {
            eprintln!("SKIP: no Ollama endpoint reachable: {reason}");
        }
        EmbeddingStatus::ModelMissing { model, reason } => {
            assert_eq!(model, "openrt-absent-embed-xyz");
            assert!(!reason.is_empty(), "{reason}");
        }
        EmbeddingStatus::Available { model, .. } => {
            panic!("absent model must not be available: {model}")
        }
    }
}

/// M9.5 — the dense path with the labeled reference provider (always runs,
/// fully offline): real text grounds a hit, textless chunks are honestly
/// skipped and counted — never embedded from a guess.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reference_provider_dense_path_is_honest() {
    let store: Arc<dyn openrt_persistence::PersistenceStore> = Arc::new(MemoryStore::new());
    let text = "the launch window opens at dawn";
    persist_chunk(&*store, "kb-ref", &chunk("c/1", Some(text)))
        .await
        .unwrap();
    persist_chunk(&*store, "kb-ref", &chunk("c/legacy", None))
        .await
        .unwrap();
    let provider: Arc<dyn EmbeddingProvider> =
        Arc::new(openrt_embeddings::DeterministicHashEmbedder::new(384));
    assert!(
        provider.describe().contains("NOT semantic"),
        "{}",
        provider.describe()
    );
    let report = semantic_search(Arc::clone(&store), "kb-ref", provider, &req(text, 3, 0.0))
        .await
        .unwrap();
    assert_eq!(report.hits.len(), 1, "exact real text must ground a hit");
    assert_eq!(report.hits[0].citation.chunk_id, "c/1");
    assert_eq!(report.indexed_chunks, 1);
    assert_eq!(
        report.skipped_no_text, 1,
        "legacy chunk reported, not guessed"
    );
}

/// M9.5 — with the REAL local embedding model, semantic search grounds hits
/// on persisted chunks and orders genuinely related text closer than
/// unrelated text. Skips honestly when the backend is absent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_model_semantic_sanity() {
    let status = probe_embedding(ENDPOINT, MODEL);
    let Some(dims) = status.dims() else {
        eprintln!("SKIP: embedding backend not available: {status}");
        return;
    };
    let store: Arc<dyn openrt_persistence::PersistenceStore> = Arc::new(MemoryStore::new());
    let cat = "a small kitten curls up beside a cat on the warm windowsill";
    let quantum = "entangled particles reveal spin correlations across labs";
    persist_chunk(&*store, "kb-sane", &chunk("c/cat", Some(cat)))
        .await
        .unwrap();
    persist_chunk(&*store, "kb-sane", &chunk("c/qm", Some(quantum)))
        .await
        .unwrap();
    persist_chunk(&*store, "kb-sane", &chunk("c/legacy", None))
        .await
        .unwrap();
    let provider: Arc<dyn EmbeddingProvider> = Arc::new(openrt_embeddings::OllamaEmbedder::new(
        ENDPOINT, MODEL, dims,
    ));
    assert!(
        provider.describe().contains("ollama"),
        "{}",
        provider.describe()
    );
    let report = semantic_search(
        Arc::clone(&store),
        "kb-sane",
        provider,
        &req("kitten cat", 3, 0.0),
    )
    .await
    .unwrap();
    assert_eq!(report.indexed_chunks, 2, "{report:?}");
    assert_eq!(report.skipped_no_text, 1, "{report:?}");
    assert!(report.hits.len() >= 1, "semantic hits expected: {report:?}");
    // Real text really is in the semantic space: the kitten chunk must be
    // the top hit for a kitten query, closer than the quantum chunk.
    let top = &report.hits[0];
    assert_eq!(
        top.citation.chunk_id, "c/cat",
        "semantic top hit: {report:?}"
    );
    let cat_score = report
        .hits
        .iter()
        .find(|h| h.citation.chunk_id == "c/cat")
        .map(|h| h.score)
        .unwrap();
    let qm_score = report
        .hits
        .iter()
        .find(|h| h.citation.chunk_id == "c/qm")
        .map(|h| h.score)
        .unwrap_or(0.0);
    assert!(
        cat_score > qm_score,
        "kitten text must outrank quantum text: {cat_score} vs {qm_score}"
    );
    // Every presented hit was re-validated against the store (single
    // citation authority) — its text must really be the persisted text.
    for h in &report.hits {
        assert_eq!(
            openrt_embeddings::sha256_hex(
                (if h.citation.chunk_id == "c/cat" {
                    cat
                } else {
                    quantum
                })
                .as_bytes()
            ),
            h.citation.text_sha256
        );
    }
}

/// M9.5 — doctor carries both embeddings rows; a PASS on `embeddings.backend`
/// may only come from a genuinely available backend (probe cross-check).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_embeddings_rows_match_probe() {
    let ws = std::env::temp_dir().join(format!(
        "openrt-p9-embed-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&ws).unwrap();
    let config = Config {
        runtime: RuntimeConfig {
            workspace_dir: Some(ws.clone()),
            ..RuntimeConfig::default()
        },
        ..Config::default()
    };
    let rt = openrt_runtime::RuntimeBuilder::new()
        .config(config)
        .store(Arc::new(MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
        .build()
        .await
        .unwrap();
    let report = rt.doctor().await;
    let cfg_row = report
        .checks
        .iter()
        .find(|c| c.name == "embeddings.config")
        .unwrap_or_else(|| {
            panic!(
                "doctor must include embeddings.config; got {:?}",
                report.checks
            )
        });
    assert_eq!(
        cfg_row.status,
        openrt_runtime::DoctorStatus::Pass,
        "{cfg_row:?}"
    );
    assert!(cfg_row.detail.contains(MODEL), "{cfg_row:?}");
    assert!(cfg_row.detail.contains(ENDPOINT), "{cfg_row:?}");

    let backend = report
        .checks
        .iter()
        .find(|c| c.name == "embeddings.backend")
        .unwrap_or_else(|| {
            panic!(
                "doctor must include embeddings.backend; got {:?}",
                report.checks
            )
        });
    assert!(
        matches!(
            backend.status,
            openrt_runtime::DoctorStatus::Pass | openrt_runtime::DoctorStatus::Warn
        ),
        "{backend:?}"
    );
    // Cross-check against the same real probe: PASS ⇔ genuinely available.
    let status = probe_embedding(ENDPOINT, MODEL);
    if backend.status == openrt_runtime::DoctorStatus::Pass {
        assert!(
            status.is_available(),
            "doctor PASS but probe says: {status}; row: {backend:?}"
        );
    } else {
        assert!(
            !status.is_available(),
            "probe available but doctor not Pass: {backend:?}"
        );
    }
}
