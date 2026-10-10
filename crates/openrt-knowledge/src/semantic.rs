//! `openrt-knowledge::semantic` — dense (neural) retrieval over the derived
//! `openrt-vectorindex` **dense** index (M9.5).
//!
//! Honesty contract:
//! * The embedding provider is injected: production passes a real neural
//!   backend (`openrt_embeddings::OllamaEmbedder`); tests pass the explicitly
//!   labeled reference embedder. Nothing here fabricates vectors.
//! * Chunks embed their REAL persisted text (`PersistedChunk::text`, M9.5).
//!   Chunks persisted without text are honestly skipped and counted — never
//!   embedded from a guess or back-filled.
//! * The dense index lives in its own key space (`<kb>.dense`), never mixed
//!   with the reference/hash vector space (`<kb>.reference`).
//! * Every presented hit re-validates its citation against the persisted
//!   store — `openrt-knowledge` remains the one citation authority.

use std::collections::HashMap;
use std::sync::Arc;

use openrt_embeddings::{Embedding, EmbeddingProvider};
use openrt_persistence::PersistenceStore;
use openrt_vectorindex::{IndexConfig, IndexError, VectorIndex, VectorRecord};

use crate::retrieval::{validate_request, RetrieveHit, RetrieveRequest};
use crate::{safe_key, Citation, KnowledgeError, PersistedChunk, KB_CHUNKS_NS};

/// Dense index key for a KB — distinct from the reference vector space.
pub fn dense_index_key(kb_id: &str) -> String {
    format!("{}.dense", safe_key(kb_id))
}

/// Result of a dense search: verified hits plus an honest account of index
/// coverage (how many chunks were genuinely embeddable).
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticReport {
    /// Hits that re-validated against persisted chunks (single authority).
    pub hits: Vec<RetrieveHit>,
    /// Chunks with real stored text currently embedded in the dense index.
    pub indexed_chunks: usize,
    /// Persisted chunks WITHOUT stored text — honestly excluded from dense
    /// indexing (lexical retrieval still covers them).
    pub skipped_no_text: usize,
    /// The provider's own honest self-description.
    pub provider: String,
}

/// Reconciled dense index scope for a KB: the derived index (stale records
/// dropped, missing texts embedded) plus the full set of textful chunk
/// records and the honest no-text count. Shared by `semantic_search` (M9.5)
/// and `hybrid_search` (M9.6) so both reconcile identically — one dense
/// authority, never two divergent indexes.
pub(crate) struct DenseScope {
    pub index: VectorIndex,
    pub textful: HashMap<(String, String), PersistedChunk>,
    pub skipped_no_text: usize,
}

/// Load a KB's persisted chunks: textful records keyed by
/// `(document_id, chunk_id)` plus the honest count of records that carry no
/// stored text. Per-KB scoping: records of other KBs are never touched.
pub(crate) async fn load_textful(
    store: &dyn PersistenceStore,
    kb_id: &str,
) -> Result<(HashMap<(String, String), PersistedChunk>, usize), KnowledgeError> {
    let prefix = format!("{}.", safe_key(kb_id));
    let mut textful: HashMap<(String, String), PersistedChunk> = HashMap::new();
    let mut skipped_no_text = 0usize;
    for (key, value) in store.list_records(KB_CHUNKS_NS).await? {
        if !key.starts_with(&prefix) {
            continue;
        }
        let chunk: PersistedChunk = serde_json::from_value(value)
            .map_err(|e| KnowledgeError::Corrupt(key.clone(), format!("chunk record: {e}")))?;
        match &chunk.text {
            Some(_) => {
                textful.insert((chunk.document_id.clone(), chunk.chunk_id.clone()), chunk);
            }
            None => skipped_no_text += 1,
        }
    }
    Ok((textful, skipped_no_text))
}

/// Reconcile the derived dense index against current chunk records.
pub(crate) async fn reconcile_dense(
    store: &Arc<dyn PersistenceStore>,
    kb_id: &str,
    provider: &Arc<dyn EmbeddingProvider>,
) -> Result<DenseScope, KnowledgeError> {
    let dims = provider.dims();
    let index_key = dense_index_key(kb_id);
    let mut index = VectorIndex::new(Arc::clone(store), index_key.clone(), IndexConfig::default());
    match index.reload().await {
        Ok(_) => {}
        Err(IndexError::Empty) => {} // first run: build from the corpus
        Err(e) => return Err(KnowledgeError::Corrupt(index_key, e.to_string())),
    }

    // Current KB chunks (per-KB scoping: never touch another KB's records).
    let (textful, skipped_no_text) = load_textful(store.as_ref(), kb_id).await?;

    // Pass 1: drop index records that are stale (chunk gone, text changed,
    // or the chunk lost/never had text).
    let mut index_changed = false;
    for rec in index.records().to_vec() {
        let ident = (rec.doc_id.clone(), rec.chunk_id.clone());
        let stale = match textful.get(&ident) {
            Some(c) => c.text_sha256 != rec.text_sha256,
            None => true,
        };
        if stale {
            index
                .delete(&rec.chunk_id)
                .await
                .map_err(|e| KnowledgeError::Corrupt(index_key.clone(), e.to_string()))?;
            index_changed = true;
        }
    }

    // Pass 2: embed every textful chunk the index does not yet hold.
    let missing: Vec<&PersistedChunk> = textful
        .values()
        .filter(|c| {
            !index
                .get(&c.chunk_id)
                .map(|r| r.doc_id == c.document_id && r.text_sha256 == c.text_sha256)
                .unwrap_or(false)
        })
        .collect();
    if !missing.is_empty() {
        let texts: Vec<String> = missing
            .iter()
            .map(|c| c.text.clone().expect("textful filtered"))
            .collect();
        let p = Arc::clone(provider);
        let embeddings = tokio::task::spawn_blocking(move || {
            let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
            p.embed(&refs)
        })
        .await
        .map_err(|e| KnowledgeError::Serde(format!("embed worker: {e}")))?
        .map_err(|e| KnowledgeError::Serde(format!("embed chunks: {e}")))?;
        if embeddings.len() != missing.len() {
            return Err(KnowledgeError::Serde(format!(
                "provider returned {} embeddings for {} chunks",
                embeddings.len(),
                missing.len()
            )));
        }
        for (chunk, emb) in missing.iter().zip(embeddings) {
            if emb.dims != dims {
                return Err(KnowledgeError::Serde(format!(
                    "embed dim {} != provider dims {dims}",
                    emb.dims
                )));
            }
            index
                .insert(VectorRecord {
                    id: emb.id.clone(),
                    doc_id: chunk.document_id.clone(),
                    chunk_id: chunk.chunk_id.clone(),
                    page: chunk.page,
                    text_sha256: chunk.text_sha256.clone(),
                    vector: emb,
                    added_at: openrt_core::now().to_rfc3339(),
                })
                .await
                .map_err(|e| KnowledgeError::Corrupt(index_key.clone(), e.to_string()))?;
            index_changed = true;
        }
    }
    if index_changed {
        index
            .persist()
            .await
            .map_err(|e| KnowledgeError::Corrupt(index_key.clone(), e.to_string()))?;
    }

    Ok(DenseScope {
        index,
        textful,
        skipped_no_text,
    })
}

/// Embed the QUERY with the real provider (real text, real semantic space)
/// — the blocking call runs off the async executor.
pub(crate) async fn embed_query(
    provider: &Arc<dyn EmbeddingProvider>,
    query: &str,
) -> Result<Embedding, KnowledgeError> {
    let query_text = query.to_string();
    let p = Arc::clone(provider);
    let mut query_embeddings = tokio::task::spawn_blocking(move || p.embed(&[query_text.as_str()]))
        .await
        .map_err(|e| KnowledgeError::Serde(format!("embed query worker: {e}")))?
        .map_err(|e| KnowledgeError::Serde(format!("embed query: {e}")))?;
    query_embeddings
        .pop()
        .ok_or_else(|| KnowledgeError::Serde("embed query returned empty".into()))
}

/// Dense search: embed the query with the real provider, reconcile the
/// derived dense index against the current chunk records (embed only what
/// is missing or changed), then query and re-validate citations.
pub async fn semantic_search(
    store: Arc<dyn PersistenceStore>,
    kb_id: &str,
    provider: Arc<dyn EmbeddingProvider>,
    req: &RetrieveRequest,
) -> Result<SemanticReport, KnowledgeError> {
    validate_request(req)?;
    let index_key = dense_index_key(kb_id);
    let scope = reconcile_dense(&store, kb_id, &provider).await?;
    let query_emb = embed_query(&provider, &req.query_text).await?;

    let ranked = match scope
        .index
        .query(&query_emb, req.top_k, req.threshold)
        .await
    {
        Ok(hits) => hits,
        Err(IndexError::Empty) => Vec::new(), // no indexed text yet: honest empty
        Err(IndexError::DimMismatch { got, expected }) => {
            return Err(KnowledgeError::Serde(format!(
                "dense index dims mismatch (index {expected}, query {got}) — \
                 the derived index must be rebuilt for this provider"
            )))
        }
        Err(e) => return Err(KnowledgeError::Corrupt(index_key, e.to_string())),
    };
    let textful = &scope.textful;

    // Fail-closed citation re-validation — `openrt-knowledge` stays the one
    // citation authority for lexical and dense retrieval alike.
    let mut hits = Vec::with_capacity(ranked.len());
    for h in ranked {
        let Some(chunk) = textful.get(&(h.record.doc_id.clone(), h.record.chunk_id.clone())) else {
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
            hits.push(RetrieveHit {
                kb_id: kb_id.to_string(),
                citation,
                score: h.score,
            });
        }
    }

    Ok(SemanticReport {
        hits,
        indexed_chunks: scope.index.len(),
        skipped_no_text: scope.skipped_no_text,
        provider: provider.describe().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{persist_chunk, retrieval::RetrieveRequest};
    use openrt_embeddings::{DeterministicHashEmbedder, EmbeddingProvider};
    use openrt_persistence::{MemoryStore, PersistenceStore};

    fn chunk(id: &str, text: Option<&str>) -> PersistedChunk {
        let body = text.unwrap_or_default();
        PersistedChunk {
            chunk_id: id.to_string(),
            document_id: "doc/1".to_string(),
            document_version: 1,
            block_keys: vec![format!("b/{id}")],
            page: Some(1),
            text_sha256: openrt_embeddings::sha256_hex(body.as_bytes()),
            text: text.map(str::to_string),
        }
    }

    fn store() -> Arc<dyn PersistenceStore> {
        Arc::new(MemoryStore::new())
    }

    fn provider() -> Arc<dyn EmbeddingProvider> {
        Arc::new(DeterministicHashEmbedder::new(384))
    }

    #[tokio::test]
    async fn semantic_search_grounded_on_real_text_with_reference_provider() {
        let store = store();
        let text = "the launch window opens at dawn";
        persist_chunk(&*store, "kb-sem", &chunk("c/1", Some(text)))
            .await
            .unwrap();
        let req = RetrieveRequest {
            query_text: text.to_string(),
            top_k: 3,
            threshold: 0.0,
        };
        let report = semantic_search(Arc::clone(&store), "kb-sem", provider(), &req)
            .await
            .unwrap();
        assert_eq!(report.hits.len(), 1, "exact text must ground a hit");
        assert_eq!(report.hits[0].citation.chunk_id, "c/1");
        assert_eq!(report.indexed_chunks, 1);
        assert_eq!(report.skipped_no_text, 0);
        assert!(
            report.provider.contains("deterministic"),
            "{}",
            report.provider
        );
        // The derived dense index is persisted under the dense key space.
        let recs = store
            .list_records(openrt_vectorindex::VECTORS_NS)
            .await
            .unwrap();
        assert!(
            recs.iter().any(|(k, _)| k.contains("kb-sem.dense")),
            "dense index must persist in its own key space: {recs:?}"
        );
    }

    #[tokio::test]
    async fn textless_chunks_are_honestly_skipped_not_guessed() {
        let store = store();
        persist_chunk(&*store, "kb-none", &chunk("c/legacy", None))
            .await
            .unwrap();
        let req = RetrieveRequest {
            query_text: "anything at all".to_string(),
            top_k: 3,
            threshold: 0.0,
        };
        let report = semantic_search(Arc::clone(&store), "kb-none", provider(), &req)
            .await
            .unwrap();
        assert!(report.hits.is_empty());
        assert_eq!(report.indexed_chunks, 0);
        assert_eq!(report.skipped_no_text, 1, "coverage must be reported");
    }

    #[tokio::test]
    async fn dense_and_reference_key_spaces_never_mix() {
        let store = store();
        let text = "alpha beta gamma";
        persist_chunk(&*store, "kb-space", &chunk("c/1", Some(text)))
            .await
            .unwrap();
        // Reference path (retrieve) and dense path (semantic_search) both run.
        let req = RetrieveRequest {
            query_text: text.to_string(),
            top_k: 3,
            threshold: 0.0,
        };
        let _ = crate::retrieve(&*store, "kb-space", &req).await.unwrap();
        let report = semantic_search(Arc::clone(&store), "kb-space", provider(), &req)
            .await
            .unwrap();
        assert_eq!(report.hits.len(), 1);
        let recs = store
            .list_records(openrt_vectorindex::VECTORS_NS)
            .await
            .unwrap();
        // A detached reference view never persists; only the dense blob does.
        assert!(
            !recs.iter().any(|(k, _)| k.contains("kb-space.reference")),
            "reference views must not persist: {recs:?}"
        );
        assert!(recs.iter().any(|(k, _)| k.contains("kb-space.dense")));
    }

    #[tokio::test]
    async fn stale_dense_records_are_reconciled() {
        let store = store();
        let text = "first revision";
        persist_chunk(&*store, "kb-rec", &chunk("c/1", Some(text)))
            .await
            .unwrap();
        let req = RetrieveRequest {
            query_text: text.to_string(),
            top_k: 3,
            threshold: 0.0,
        };
        let r1 = semantic_search(Arc::clone(&store), "kb-rec", provider(), &req)
            .await
            .unwrap();
        assert_eq!(r1.indexed_chunks, 1);

        // Same chunk id, NEW text (revision): the index must re-embed, not
        // keep serving the stale vector.
        let revised = "second revision completely different";
        let mut c2 = chunk("c/1", Some(revised));
        c2.text_sha256 = openrt_embeddings::sha256_hex(revised.as_bytes());
        persist_chunk(&*store, "kb-rec", &c2).await.unwrap();
        let r2 = semantic_search(Arc::clone(&store), "kb-rec", provider(), &req)
            .await
            .unwrap();
        assert_eq!(r2.indexed_chunks, 1);
        // Query for the OLD text must no longer score 1.0 against a stale vec.
        let old_req = RetrieveRequest {
            query_text: text.to_string(),
            top_k: 3,
            threshold: 0.99, // exact-match-level threshold
        };
        let r3 = semantic_search(Arc::clone(&store), "kb-rec", provider(), &old_req)
            .await
            .unwrap();
        assert!(
            r3.hits.is_empty(),
            "stale vector must be gone; hits: {:?}",
            r3.hits
        );
    }

    #[tokio::test]
    async fn blank_query_fails_closed_like_retrieve() {
        let store = store();
        let req = RetrieveRequest {
            query_text: "   ".to_string(),
            top_k: 3,
            threshold: 0.0,
        };
        assert!(
            semantic_search(Arc::clone(&store), "kb-x", provider(), &req)
                .await
                .is_err()
        );
    }
}
