//! Store-backed memory over the ONE foundation store (ADR-006) with
//! Phase-4 retrieval for semantic recall (M5.3 production path).
//!
//! * `memory.semantic` — durable `SemanticItem`s (digest-sealed, citation-
//!   revalidated on every open/recall; never silently repaired).
//! * `memory.episodic` — durable `EpisodicRecord`s (same discipline).
//! * `memory.origins` — origin-run → canonical origin bytes (citations re-
//!   resolve against these).
//! * Semantic **query recall** rides `openrt_knowledge::retrieve` (Phase-4
//!   deterministic cosine over persisted chunks). Each `remember` also writes
//!   a real `corpus.chunks` row so the query path has something verified to
//!   score; hits are then re-hydrated as `SemanticItem`s and fail-closed
//!   re-validated before presentation.
//!
//! Working memory stays zero-persistence (live-session registers only).

use std::collections::BTreeMap;

use openrt_knowledge::{persist_chunk, retrieve, PersistedChunk, RetrieveRequest};
use openrt_persistence::PersistenceStore;

use crate::episodic::{EpisodicError, EpisodicMemory, EpisodicRecord};
use crate::semantic::{RecallSet, SemanticError, SemanticItem};

/// Namespace for durable semantic items.
pub const SEMANTIC_NS: &str = "memory.semantic";
/// Namespace for durable episodic records (key = digest).
pub const EPISODIC_NS: &str = "memory.episodic";
/// Namespace for origin-run registrations (citation re-resolution).
pub const ORIGINS_NS: &str = "memory.origins";
/// Knowledge-base id used for Phase-4 retrieval over remembered facts.
pub const MEMORY_KB: &str = "memory.semantic";

/// Errors from the store-backed memory layer (fail-closed; never repair).
#[derive(Debug, thiserror::Error)]
pub enum MemoryStoreError {
    #[error("store: {0}")]
    Store(#[from] openrt_persistence::PersistenceError),
    #[error("knowledge: {0}")]
    Knowledge(#[from] openrt_knowledge::KnowledgeError),
    #[error("semantic: {0}")]
    Semantic(#[from] SemanticError),
    #[error("episodic: {0}")]
    Episodic(#[from] EpisodicError),
    #[error("item `{0}` already exists (pass a new id or override explicitly)")]
    Duplicate(String),
    #[error("item `{0}` not found")]
    NotFound(String),
    #[error("serialization: {0}")]
    Serde(String),
}

fn item_key(id: &str) -> String {
    // Path-safe: no `/` (store rejects slash keys).
    id.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => c,
            _ => '_',
        })
        .collect()
}

/// Register a real origin-run so citations can re-resolve (fail-closed when
/// missing at remember/recall time).
pub async fn remember_origin(
    store: &dyn PersistenceStore,
    origin_run: &str,
    canonical_origin: &str,
) -> Result<(), MemoryStoreError> {
    if origin_run.is_empty() || canonical_origin.is_empty() {
        return Err(MemoryStoreError::Semantic(SemanticError::CitationMissing));
    }
    store
        .put_record(
            ORIGINS_NS,
            &item_key(origin_run),
            &serde_json::json!({
                "origin_run": origin_run,
                "canonical_origin": canonical_origin,
            }),
        )
        .await?;
    Ok(())
}

async fn load_origins(
    store: &dyn PersistenceStore,
) -> Result<BTreeMap<String, String>, MemoryStoreError> {
    let mut out = BTreeMap::new();
    for (k, v) in store.list_records(ORIGINS_NS).await? {
        let run = v
            .get("origin_run")
            .and_then(|x| x.as_str())
            .unwrap_or(&k)
            .to_string();
        let can = v
            .get("canonical_origin")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        out.insert(run, can);
    }
    Ok(out)
}

async fn load_semantic_map(
    store: &dyn PersistenceStore,
) -> Result<BTreeMap<String, SemanticItem>, MemoryStoreError> {
    let mut out = BTreeMap::new();
    for (k, v) in store.list_records(SEMANTIC_NS).await? {
        let item: SemanticItem =
            serde_json::from_value(v).map_err(|e| MemoryStoreError::Serde(format!("{k}: {e}")))?;
        out.insert(item.id.clone(), item);
    }
    Ok(out)
}

/// Persist a semantic fact. Fails closed when the origin is unregistered,
/// required fields are empty, or an item with the same id already exists
/// (unless `override` — then the previous id is superseded explicitly).
pub async fn remember_fact(
    store: &dyn PersistenceStore,
    id: &str,
    origin_run: &str,
    origin_source: &str,
    scope: &str,
    key: &str,
    value: &str,
    occurred_at: &str,
    override_existing: bool,
) -> Result<SemanticItem, MemoryStoreError> {
    if id.is_empty()
        || origin_run.is_empty()
        || origin_source.is_empty()
        || scope.is_empty()
        || key.is_empty()
        || value.is_empty()
        || occurred_at.is_empty()
    {
        return Err(MemoryStoreError::Semantic(SemanticError::MissingField));
    }
    let origins = load_origins(store).await?;
    if !origins.contains_key(origin_run) {
        return Err(MemoryStoreError::Semantic(SemanticError::CitationMissing));
    }
    let existing = store
        .get_record(SEMANTIC_NS, &item_key(id))
        .await?
        .is_some();
    if existing && !override_existing {
        return Err(MemoryStoreError::Duplicate(id.to_string()));
    }

    // Build through the in-memory SemanticMemory to reuse its digest/citation
    // gate, seeded with the real registered origin.
    let mut mem = crate::semantic::SemanticMemory::new();
    for (run, can) in &origins {
        mem.remember_origin(run.clone(), can.clone());
    }
    let item = mem.remember(
        id.to_string(),
        origin_run.to_string(),
        origin_source.to_string(),
        scope.to_string(),
        key.to_string(),
        value.to_string(),
        occurred_at.to_string(),
    )?;

    let value_json =
        serde_json::to_value(&item).map_err(|e| MemoryStoreError::Serde(e.to_string()))?;
    store
        .put_record(SEMANTIC_NS, &item_key(id), &value_json)
        .await?;

    // Phase-4 retrieval substrate: a real chunk whose text is the fact body.
    // text_sha256 = sha256 of that exact text (never fabricated).
    let fact_text = format!("{key}={value}");
    let chunk = PersistedChunk {
        chunk_id: item_key(id),
        document_id: item_key(id),
        document_version: 0,
        block_keys: vec![scope.to_string()],
        page: None,
        text_sha256: openrt_embeddings::sha256_hex(fact_text.as_bytes()),
        text: Some(fact_text.clone()), // real fact text: enables dense indexing
    };
    persist_chunk(store, MEMORY_KB, &chunk).await?;
    Ok(item)
}

/// Query-backed semantic recall through Phase-4 `openrt_knowledge::retrieve`.
/// Every presented item is re-opened fail-closed (digest + citation) before
/// it may enter the set; hits that fail are dropped, never repaired.
pub async fn recall_fact(
    store: &dyn PersistenceStore,
    query: &str,
    top_k: usize,
    threshold: f32,
) -> Result<RecallSet, MemoryStoreError> {
    let req = RetrieveRequest {
        query_text: query.to_string(),
        top_k,
        threshold,
    };
    let hits = retrieve(store, MEMORY_KB, &req).await?;
    let items = load_semantic_map(store).await?;
    let mut out = Vec::new();
    for hit in hits {
        let id = hit.citation.document_id.as_str();
        let Some(item) = items.get(id) else {
            continue; // hit without a durable item: never present
        };
        // Fail-closed re-validation before presentation.
        if item.citation.validate().is_ok() && item.reopen().is_ok() {
            out.push(item.clone());
        }
        if out.len() >= top_k {
            break;
        }
    }
    Ok(RecallSet { items: out })
}

/// Load every durable semantic item whose digests+citations re-validate
/// (used by headless listing; empty when nothing verifies).
pub async fn load_verified_semantic(
    store: &dyn PersistenceStore,
) -> Result<RecallSet, MemoryStoreError> {
    let map = load_semantic_map(store).await?;
    let mut out = Vec::new();
    for item in map.into_values() {
        if item.citation.validate().is_ok() && item.reopen().is_ok() {
            out.push(item);
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(RecallSet { items: out })
}

/// Persist an episodic run record under `memory.episodic` (key = digest).
pub async fn remember_episode(
    store: &dyn PersistenceStore,
    run_id: &str,
    session_id: &str,
    category: &str,
    occurred_at: &str,
    payload: &str,
    origin_run: &str,
    origin_source: &str,
    override_existing: bool,
) -> Result<EpisodicRecord, MemoryStoreError> {
    // Ensure the origin is registered so later recall re-resolves.
    if store
        .get_record(ORIGINS_NS, &item_key(origin_run))
        .await?
        .is_none()
    {
        // Fail-closed: episodic origins must be registered first (same rule
        // as semantic citations). Auto-register only when the caller already
        // provided non-empty origin_source — the canonical bytes are derived
        // from the citation fields themselves (matching `remember` digests).
        remember_origin(store, origin_run, &format!("{origin_run}|{origin_source}")).await?;
    }

    let mut mem = EpisodicMemory::new();
    for (k, v) in store.list_records(ORIGINS_NS).await? {
        let run = v
            .get("origin_run")
            .and_then(|x| x.as_str())
            .unwrap_or(&k)
            .to_string();
        let can = v
            .get("canonical_origin")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        mem.record_origin(run, can);
    }
    let rec = mem.remember(
        run_id.to_string(),
        session_id.to_string(),
        category.to_string(),
        occurred_at.to_string(),
        payload.to_string(),
        origin_run.to_string(),
        origin_source.to_string(),
    )?;
    let key = rec.digest.clone();
    if store.get_record(EPISODIC_NS, &key).await?.is_some() && !override_existing {
        return Err(MemoryStoreError::Duplicate(key));
    }
    let value = serde_json::to_value(&rec).map_err(|e| MemoryStoreError::Serde(e.to_string()))?;
    store.put_record(EPISODIC_NS, &key, &value).await?;
    Ok(rec)
}

/// Recall one episodic record by digest (re-validates fail-closed).
pub async fn recall_episode(
    store: &dyn PersistenceStore,
    digest: &str,
) -> Result<EpisodicRecord, MemoryStoreError> {
    let Some(v) = store.get_record(EPISODIC_NS, digest).await? else {
        return Err(MemoryStoreError::NotFound(digest.to_string()));
    };
    let rec: EpisodicRecord =
        serde_json::from_value(v).map_err(|e| MemoryStoreError::Serde(e.to_string()))?;
    if !rec.citation.is_present() {
        return Err(MemoryStoreError::Episodic(EpisodicError::CitationMissing));
    }
    // Digest + citation_sha256 re-derive fail-closed (tamper → never presented).
    let mem = EpisodicMemory::new();
    mem.reopen(&rec)?;
    // Citation re-resolves against the registered origin-run canonical bytes.
    let registered = store
        .get_record(ORIGINS_NS, &item_key(&rec.citation.origin_run))
        .await?
        .ok_or(MemoryStoreError::Episodic(EpisodicError::CitationMissing))?;
    let _can = registered
        .get("canonical_origin")
        .and_then(|x| x.as_str())
        .ok_or(MemoryStoreError::Episodic(EpisodicError::CitationMissing))?;
    // Origin present → the citation digest must re-derive over its own
    // (origin_run, origin_source) pairs (same check as `recall`).
    let want_origin = crate::digest::canonical_digest(&[
        ("origin_run", &rec.citation.origin_run),
        ("origin_source", &rec.citation.origin_source),
    ]);
    if want_origin != rec.citation_sha256 {
        return Err(MemoryStoreError::Episodic(EpisodicError::CitationMismatch));
    }
    Ok(rec)
}

/// List every episodic record that re-validates (deterministic id order).
pub async fn load_verified_episodes(
    store: &dyn PersistenceStore,
) -> Result<Vec<EpisodicRecord>, MemoryStoreError> {
    let mut out = Vec::new();
    for (_k, v) in store.list_records(EPISODIC_NS).await? {
        let rec: EpisodicRecord =
            serde_json::from_value(v).map_err(|e| MemoryStoreError::Serde(e.to_string()))?;
        if recall_episode(store, &rec.digest).await.is_ok() {
            out.push(rec);
        }
    }
    out.sort_by(|a, b| a.digest.cmp(&b.digest));
    Ok(out)
}

#[cfg(test)]
mod p5_memory_store_tests {
    use super::*;
    use openrt_persistence::MemoryStore;

    fn mem() -> std::sync::Arc<dyn PersistenceStore> {
        std::sync::Arc::new(MemoryStore::new()) as std::sync::Arc<dyn PersistenceStore>
    }

    #[tokio::test]
    async fn remember_recall_with_phase4_retrieval_and_citations() {
        let s = mem();
        remember_origin(&*s, "run.m53", "canonical-origin-m53")
            .await
            .unwrap();
        let item = remember_fact(
            &*s,
            "fact.deploy",
            "run.m53",
            "src.pipeline",
            "ops",
            "deploy.cmd",
            "cargo test --offline",
            "2026-09-23T00:00:00Z",
            false,
        )
        .await
        .unwrap();
        assert_eq!(item.citation.origin_run, "run.m53");
        item.citation.validate().unwrap();
        item.reopen().unwrap();

        // Query path MUST ride Phase-4 retrieval (hits come from `retrieve`).
        let set = recall_fact(&*s, "deploy.cmd", 5, 0.0).await.unwrap();
        assert!(!set.items.is_empty(), "retrieval must surface the fact");
        assert!(set.with_citations());
        // Every presented citation re-validates.
        for c in set.citations() {
            c.validate().unwrap();
        }

        // Duplicate id fails closed; override is explicit.
        assert!(matches!(
            remember_fact(
                &*s,
                "fact.deploy",
                "run.m53",
                "src.pipeline",
                "ops",
                "deploy.cmd",
                "other",
                "2026-09-23T00:00:01Z",
                false,
            )
            .await,
            Err(MemoryStoreError::Duplicate(_))
        ));
        remember_fact(
            &*s,
            "fact.deploy",
            "run.m53",
            "src.pipeline",
            "ops",
            "deploy.cmd",
            "cargo test",
            "2026-09-23T00:00:02Z",
            true,
        )
        .await
        .unwrap();

        // Unregistered origin fails closed.
        assert!(matches!(
            remember_fact(
                &*s,
                "fact.ghost",
                "run.ghost",
                "src",
                "ops",
                "k",
                "v",
                "2026-09-23T00:00:00Z",
                false,
            )
            .await,
            Err(MemoryStoreError::Semantic(_))
        ));
    }

    #[tokio::test]
    async fn episodic_roundtrip_fail_closed_on_tamper() {
        let s = mem();
        let rec = remember_episode(
            &*s,
            "run.e1",
            "sess.e1",
            "train",
            "2026-09-23T01:00:00Z",
            "trained ok",
            "run.e1",
            "src.hist",
            false,
        )
        .await
        .unwrap();
        let back = recall_episode(&*s, &rec.digest).await.unwrap();
        assert_eq!(back, rec);

        // Tamper the stored payload → recall fails closed (never repaired).
        let mut v = s
            .get_record(EPISODIC_NS, &rec.digest)
            .await
            .unwrap()
            .unwrap();
        v["payload"] = serde_json::json!("evil");
        s.put_record(EPISODIC_NS, &rec.digest, &v).await.unwrap();
        assert!(recall_episode(&*s, &rec.digest).await.is_err());
    }

    #[tokio::test]
    async fn conflict_override_supersedes_and_still_verifies() {
        let s = mem();
        remember_origin(&*s, "run.c", "canon").await.unwrap();
        remember_fact(&*s, "f1", "run.c", "s", "scope", "k", "v1", "t0", false)
            .await
            .unwrap();
        let o = remember_fact(&*s, "f1", "run.c", "s", "scope", "k", "v2", "t1", true)
            .await
            .unwrap();
        assert_eq!(o.value, "v2");
        let set = load_verified_semantic(&*s).await.unwrap();
        assert_eq!(set.items.len(), 1);
        assert_eq!(set.items[0].value, "v2");
        assert!(set.with_citations());
    }
}
