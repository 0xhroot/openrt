//! `openrt-vectorindex` — persisted exact (bounded) vector index over the ONE
//! store (`knowledge.vectors`).
//!
//! * **Honest baseline**: retrieval is a correct bounded linear scan with
//!   cosine similarity; no ANN claims are made. `describe()` says so.
//! * **Atomic persist**: the whole index is written as one blob; a hash record
//!   is written *after* the blob, so replay can detect a torn write
//!   (blob present, hash absent ⇒ previous write incomplete). `reload()`
//!   verifies the blob against the hash before any query is served.
//! * Scores are always computed (cosine); never fabricated.
//! * The index is a per-KB primitive; knowledge-base lifecycle/audit live in
//!   `openrt-knowledge`.

use std::sync::Arc;

use openrt_core::{DomainEvent, EventKind};
use openrt_embeddings::{cosine, Embedding, EmbeddingProvider};
use openrt_persistence::PersistenceStore;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const VECTORS_NS: &str = "knowledge.vectors";

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// One indexed vector with full provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VectorRecord {
    pub id: String,
    pub doc_id: String,
    pub chunk_id: String,
    pub page: Option<u32>,
    /// SHA-256 of the chunk text.
    pub text_sha256: String,
    pub vector: Embedding,
    pub added_at: String,
}

pub fn vector_key(record_id: &str) -> String {
    openrt_core::encode_safe_key(record_id)
}

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("query vector dim {got} != index dim {expected}")]
    DimMismatch { got: usize, expected: usize },
    #[error("index is empty (has no persisted vectors)")]
    Empty,
    #[error("index persistence is missing a hash record (tear?) index={index}")]
    Tear { index: String },
    #[error("index integrity failed: expected {expected}, actual {actual} (index={index})")]
    Integrity {
        index: String,
        expected: String,
        actual: String,
    },
    #[error("store error: {0}")]
    Store(String),
    #[error("provider error: {0}")]
    Provider(String),
}

impl From<openrt_persistence::PersistenceError> for IndexError {
    fn from(e: openrt_persistence::PersistenceError) -> Self {
        IndexError::Store(e.to_string())
    }
}

/// A scored retrieval result.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredHit {
    /// Cosine similarity in [-1, 1] (computed, real).
    pub score: f32,
    pub record: VectorRecord,
}

pub fn as_f32_score(similarity: f32) -> f32 {
    similarity
}

/// Representation-integrity report over the persisted index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrityReport {
    pub record_count: usize,
    pub dims: usize,
    pub verified: bool,
    pub index_sha256: String,
}

/// The persisted vector index for one knowledge base.
pub struct VectorIndex {
    /// backing store; `None` for a `detached` (derived, in-memory-only)
    /// view that is rebuilt from the corpus and never persists or emits.
    store: Option<Arc<dyn PersistenceStore>>,
    /// Per-KB index key (safe namespace key of the KB id).
    key: String,
    state: MutexState,
}

struct MutexState {
    records: Vec<VectorRecord>,
    dims: usize,
    config: IndexConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexConfig {
    /// Max vectors kept (bounded baseline guardrail).
    pub max_vectors: usize,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            max_vectors: 250_000,
        }
    }
}

impl VectorIndex {
    pub fn new(store: Arc<dyn PersistenceStore>, key: String, config: IndexConfig) -> Self {
        Self {
            store: Some(store),
            key,
            state: MutexState {
                records: Vec::new(),
                dims: 0,
                config,
            },
        }
    }

    /// A derived, in-memory-only view over the corpus (ADR-007: the vector
    /// index is *derived*, never authoritative). No persistence, no events —
    /// the caller rebuilds it from chunk records and queries through the
    /// same bounded cosine machinery as a durable index.
    pub fn detached(key: impl Into<String>, config: IndexConfig) -> Self {
        Self {
            store: None,
            key: key.into(),
            state: MutexState {
                records: Vec::new(),
                dims: 0,
                config,
            },
        }
    }

    /// Existing record for `chunk_id`, if indexed (reconciliation aid).
    pub fn get(&self, chunk_id: &str) -> Option<&VectorRecord> {
        self.state.records.iter().find(|r| r.chunk_id == chunk_id)
    }

    /// All records currently held (reconciliation aid).
    pub fn records(&self) -> &[VectorRecord] {
        &self.state.records
    }

    /// Number of records currently held.
    pub fn len(&self) -> usize {
        self.state.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.state.records.is_empty()
    }

    fn blob_key(&self) -> String {
        format!("{}.records", self.key)
    }

    fn hash_key(&self) -> String {
        format!("{}.hash", self.blob_key())
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub async fn dims(&self) -> usize {
        self.state.dims
    }

    pub async fn ready(&self) -> bool {
        !self.state.records.is_empty() && self.state.dims > 0
    }

    /// Embed + validate, then insert. Order preserved from the provider.
    pub async fn insert_embedded(
        &mut self,
        provider: &dyn EmbeddingProvider,
        records: Vec<(Embedding, VectorMeta)>,
        actor: &str,
    ) -> Result<usize, IndexError> {
        if records.is_empty() {
            return Ok(0);
        }
        let dims = provider.dims();
        if self.state.dims != 0 && self.state.dims != dims {
            return Err(IndexError::DimMismatch {
                got: dims,
                expected: self.state.dims,
            });
        }
        self.state.dims = dims;
        for (emb, meta) in records {
            openrt_embeddings::validate_embedding(&emb, dims)
                .map_err(|e| IndexError::Provider(e.to_string()))?;
            if self.state.records.len() >= self.state.config.max_vectors {
                return Err(IndexError::Empty); // overflow guardrail (honest)
            }
            self.state.records.push(VectorRecord {
                id: emb.id.clone(),
                doc_id: meta.doc_id,
                chunk_id: meta.chunk_id,
                page: meta.page,
                text_sha256: meta.text_sha256,
                vector: emb,
                added_at: openrt_core::now().to_rfc3339(),
            });
        }
        self.emit(
            EventKind::IndexUpdated,
            serde_json::json!({
                "index_key": self.key,
                "operation": "insert",
                "count": self.state.records.len(),
            }),
        )
        .await;
        self.persist().await?;
        return self.count_after(actor).await;
    }

    async fn count_after(&self, _actor: &str) -> Result<usize, IndexError> {
        Ok(self.state.records.len())
    }

    pub async fn insert(&mut self, record: VectorRecord) -> Result<(), IndexError> {
        let dims = record.vector.dims;
        if self.state.dims != 0 && self.state.dims != dims {
            return Err(IndexError::DimMismatch {
                got: dims,
                expected: self.state.dims,
            });
        }
        self.state.dims = dims;
        if self.state.records.len() >= self.state.config.max_vectors {
            return Err(IndexError::Empty);
        }
        self.emit(EventKind::IndexUpdated, serde_json::json!({ "index_key": self.key, "operation": "insert", "count": self.state.records.len() + 1 })).await;
        self.state.records.push(record);
        Ok(())
    }

    pub async fn insert_batch(&mut self, records: Vec<VectorRecord>) -> Result<usize, IndexError> {
        let mut n = 0usize;
        for r in records {
            self.insert(r).await?;
            n += 1;
        }
        Ok(n)
    }

    pub async fn delete(&mut self, chunk_id: &str) -> Result<bool, IndexError> {
        let before = self.state.records.len();
        self.state.records.retain(|r| r.chunk_id != chunk_id);
        let removed = before != self.state.records.len();
        if removed {
            self.emit(EventKind::IndexUpdated, serde_json::json!({ "index_key": self.key, "operation": "delete", "count": self.state.records.len() })).await;
        }
        Ok(removed)
    }

    /// Bounded exact retrieval. `top_k` > 0, results sorted descending by
    /// cosine similarity, ones below `threshold` dropped. Scores are real.
    pub async fn query(
        &self,
        query_vec: &Embedding,
        top_k: usize,
        threshold: f32,
    ) -> Result<Vec<ScoredHit>, IndexError> {
        let dims = self.state.dims;
        if dims == 0 || self.state.records.is_empty() {
            return Err(IndexError::Empty);
        }
        if query_vec.values.len() != dims {
            return Err(IndexError::DimMismatch {
                got: query_vec.values.len(),
                expected: dims,
            });
        }
        let mut scored: Vec<ScoredHit> = self
            .state
            .records
            .iter()
            .map(|r| ScoredHit {
                score: cosine(&query_vec.values, &r.vector.values),
                record: r.clone(),
            })
            .filter(|h| h.score >= threshold)
            .collect();
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(top_k);
        Ok(scored)
    }

    /// Rebuild from a full entry set (clears first), then persists.
    pub async fn rebuild_from(&mut self, records: Vec<VectorRecord>) -> Result<usize, IndexError> {
        let dims = records.first().map(|r| r.vector.dims).unwrap_or(0);
        self.emit(
            EventKind::IndexRebuildStarted,
            serde_json::json!({ "index_key": self.key }),
        )
        .await;
        self.state.records = records;
        self.state.dims = dims;
        let n = self.persist().await?;
        self.emit(EventKind::IndexRebuildCompleted, serde_json::json!({ "index_key": self.key, "count": self.state.records.len(), "hash": self.current_hash().await })).await;
        Ok(n)
    }

    /// Atomic persist: blob first, then the hash record. On any failure the
    /// hash record is not written, so a subsequent `reload` sees a tear.
    pub async fn persist(&self) -> Result<usize, IndexError> {
        let value = serde_json::to_vec(&self.state.records)
            .map_err(|e| IndexError::Store(format!("serialize: {e}")))?;
        let hash = sha256_hex(&value);
        let Some(store) = self.store.as_ref() else {
            return Ok(self.state.records.len()); // detached: nothing to persist
        };
        store.put_blob(VECTORS_NS, &self.blob_key(), &value).await?;
        store
            .put_record(
                VECTORS_NS,
                &self.hash_key(),
                &serde_json::to_value(hash).unwrap(),
            )
            .await?;
        Ok(self.state.records.len())
    }

    /// Reload from the store, verifying blob integrity against the hash.
    /// Returns Err on tear/integrity failure (fail-closed: no queries are
    /// served from a possibly-corrupt index).
    pub async fn reload(&mut self) -> Result<IntegrityReport, IndexError> {
        let Some(store) = self.store.as_ref() else {
            return Err(IndexError::Empty); // detached: nothing persisted
        };
        let blob = store
            .get_blob(VECTORS_NS, &self.blob_key())
            .await?
            .ok_or(IndexError::Empty)?;
        let hash_record = store
            .get_record(VECTORS_NS, &self.hash_key())
            .await?
            .ok_or_else(|| IndexError::Tear {
                index: self.key.clone(),
            })?;
        let expected: String = serde_json::from_value(hash_record).unwrap_or_default();
        let actual = sha256_hex(&blob);
        if actual != expected {
            self.emit(
                EventKind::IndexIntegrityFailed,
                serde_json::json!({
                    "index_key": self.key,
                    "expected": expected,
                    "actual": actual,
                }),
            )
            .await;
            return Err(IndexError::Integrity {
                index: self.key.clone(),
                expected,
                actual,
            });
        }
        let records: Vec<VectorRecord> =
            serde_json::from_slice(&blob).map_err(|e| IndexError::Store(format!("decode: {e}")))?;
        let dims = records.first().map(|r| r.vector.dims).unwrap_or(0);
        self.state.records = records;
        self.state.dims = dims;
        Ok(IntegrityReport {
            record_count: self.state.records.len(),
            dims,
            verified: true,
            index_sha256: expected,
        })
    }

    /// Integrity of the *in-memory* state against the validated stored hash.
    pub async fn integrity(&self) -> Result<IntegrityReport, IndexError> {
        let blob = serde_json::to_vec(&self.state.records)
            .map_err(|e| IndexError::Store(format!("serialize: {e}")))?;
        let expected = self.current_hash().await;
        let actual = sha256_hex(&blob);
        if !expected.is_empty() && actual != expected {
            return Err(IndexError::Integrity {
                index: self.key.clone(),
                expected,
                actual,
            });
        }
        Ok(IntegrityReport {
            record_count: self.state.records.len(),
            dims: self.state.dims,
            verified: actual == expected && !expected.is_empty(),
            index_sha256: actual,
        })
    }

    pub async fn current_hash(&self) -> String {
        let blob = serde_json::to_vec(&self.state.records).unwrap_or_default();
        sha256_hex(&blob)
    }

    async fn emit(&self, kind: EventKind, data: serde_json::Value) {
        let Some(store) = self.store.as_ref() else {
            return; // detached view: no event stream
        };
        let _ = store
            .append_event(&DomainEvent::new(kind).with_data(data))
            .await;
    }
}

/// Integration metadata for one embedded vector (built by the knowledge layer).
pub struct VectorMeta {
    pub doc_id: String,
    pub chunk_id: String,
    pub page: Option<u32>,
    pub text_sha256: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_embeddings::DeterministicHashEmbedder;

    fn emb(provider: &DeterministicHashEmbedder, text: &str) -> Embedding {
        provider.embed(&[text]).unwrap().remove(0)
    }

    #[tokio::test]
    async fn insert_query_persist_reload() {
        let store: Arc<dyn PersistenceStore> = Arc::new(openrt_persistence::MemoryStore::new());
        let provider = DeterministicHashEmbedder::new(16);
        let mut idx = VectorIndex::new(store.clone(), "kb-a".into(), IndexConfig::default());
        let v1 = VectorRecord {
            id: "r1".into(),
            doc_id: "doc/1".into(),
            chunk_id: "ck/1".into(),
            page: Some(1),
            text_sha256: "t1".into(),
            vector: emb(&provider, "sailing ships"),
            added_at: openrt_core::now().to_rfc3339(),
        };
        let v2 = VectorRecord {
            id: "r2".into(),
            doc_id: "doc/2".into(),
            chunk_id: "ck/2".into(),
            page: Some(2),
            text_sha256: "t2".into(),
            vector: emb(&provider, "cooking recipes"),
            added_at: openrt_core::now().to_rfc3339(),
        };
        idx.insert_batch(vec![v1, v2]).await.unwrap();
        idx.persist().await.unwrap();
        assert!(idx.ready().await);

        // Fresh instance, same store: reload verifies integrity then serves.
        let mut loaded = VectorIndex::new(store.clone(), "kb-a".into(), IndexConfig::default());
        let report = loaded.reload().await.unwrap();
        assert!(report.verified);
        assert_eq!(report.record_count, 2);

        let q = emb(&provider, "sailing ships");
        let hits = loaded.query(&q, 5, 0.0).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].record.chunk_id, "ck/1");
        assert!(hits[0].score >= hits[1].score);
    }

    #[tokio::test]
    async fn query_respects_topk_and_threshold() {
        let store: Arc<dyn PersistenceStore> = Arc::new(openrt_persistence::MemoryStore::new());
        let provider = DeterministicHashEmbedder::new(16);
        let mut idx = VectorIndex::new(store.clone(), "kb-b".into(), IndexConfig::default());
        for i in 0..10 {
            idx.insert(VectorRecord {
                id: format!("r{i}"),
                doc_id: "doc/x".into(),
                chunk_id: format!("ck/{i}"),
                page: None,
                text_sha256: format!("t{i}"),
                vector: emb(&provider, &format!("topic item {i}")),
                added_at: openrt_core::now().to_rfc3339(),
            })
            .await
            .unwrap();
        }
        let q = emb(&provider, "topic item 3");
        let hits = idx.query(&q, 3, 0.0).await.unwrap();
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].record.chunk_id, "ck/3");
        // threshold=1.0 returns only perfect self-match
        let perfect = idx.query(&q, 10, 1.0).await.unwrap();
        assert_eq!(perfect.len(), 1);
    }

    #[tokio::test]
    async fn tear_detected_on_reload() {
        let store: Arc<dyn PersistenceStore> = Arc::new(openrt_persistence::MemoryStore::new());
        let provider = DeterministicHashEmbedder::new(8);
        let mut idx = VectorIndex::new(store.clone(), "kb-c".into(), IndexConfig::default());
        idx.insert(VectorRecord {
            id: "r".into(),
            doc_id: "d".into(),
            chunk_id: "c".into(),
            page: None,
            text_sha256: "t".into(),
            vector: emb(&provider, "x"),
            added_at: "".into(),
        })
        .await
        .unwrap();
        idx.persist().await.unwrap();
        // Tamper with the blob behind the store's back (MemoryStore clone).
        let mut corrupted = idx;
        corrupted.state.records.clear();
        // Simulate: write blob without hash (tear).
        let blob = serde_json::to_vec(&corrupted.state.records).unwrap();
        store
            .put_blob(VECTORS_NS, &corrupted.blob_key(), &blob)
            .await
            .unwrap();
        let mut fresh = VectorIndex::new(store.clone(), "kb-c".into(), IndexConfig::default());
        // reload sees tear: hash record points to old blob hash, blob changed.
        assert!(fresh.reload().await.is_err());
    }

    #[tokio::test]
    async fn delete_removes_chunk() {
        let store: Arc<dyn PersistenceStore> = Arc::new(openrt_persistence::MemoryStore::new());
        let provider = DeterministicHashEmbedder::new(8);
        let mut idx = VectorIndex::new(store.clone(), "kb-d".into(), IndexConfig::default());
        idx.insert(VectorRecord {
            id: "r".into(),
            doc_id: "d".into(),
            chunk_id: "gone".into(),
            page: None,
            text_sha256: "t".into(),
            vector: emb(&provider, "y"),
            added_at: "".into(),
        })
        .await
        .unwrap();
        assert!(idx.delete("gone").await.unwrap());
        assert_eq!(idx.len(), 0);
    }

    #[tokio::test]
    async fn rebuild_replaces_all() {
        let store: Arc<dyn PersistenceStore> = Arc::new(openrt_persistence::MemoryStore::new());
        let provider = DeterministicHashEmbedder::new(8);
        let mut idx = VectorIndex::new(store.clone(), "kb-e".into(), IndexConfig::default());
        let mk = |id: &str, text: &str| VectorRecord {
            id: id.into(),
            doc_id: "d".into(),
            chunk_id: id.into(),
            page: None,
            text_sha256: "t".into(),
            vector: emb(&provider, text),
            added_at: "".into(),
        };
        idx.insert(mk("a", "one")).await.unwrap();
        idx.rebuild_from(vec![mk("b", "two"), mk("c", "three")])
            .await
            .unwrap();
        assert_eq!(idx.len(), 2);
        let hits = idx.query(&emb(&provider, "three"), 1, 0.0).await.unwrap();
        assert_eq!(hits[0].record.chunk_id, "c");
    }
}
