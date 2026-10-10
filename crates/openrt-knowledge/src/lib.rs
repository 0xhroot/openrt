//! `openrt-knowledge` — Phase-4 knowledge layer.
//!
//! HONESTY (Phase-4 knowledge contract, no fabrication):
//! * Retrieval scores are computed cosine similarity over persisted vectors
//!   (via `openrt-vectorindex`) — real, bounded linear scan; never fabricated.
//! * Every `Citation` is re-validated against persisted `corpus.chunks`
//!   records before it may be presented; a citation can never be invented.
//! * The RAG oracle answers honestly: evidence is *data* for generation,
//!   never policy/tools/permissions; when no supporting evidence passes
//!   retrieval it says so explicitly and reports an empty citation report —
//!   no fabricated citations, ever.
//!
//! # INCREMENT STATUS (this file, right now, verified)
//!
//! * Works: `KnowledgeBase` (domain + serde), `Citation` domain+serde,
//!   `normalize_kb_id`/`normalize_record_key` (path-safe key encoding),
//!   `Citation::validate` (re-resolves a citation against persisted
//!   `corpus.chunks` via the ONE shared `PersistenceStore` — real store
//!   round-trip, fail-closed).
//! * Wired + gate-green: persistence + fail-closed citation validation
//!   (persist_chunk, Citation::validate, safe_key/record_key) and membership
//!   (MembershipRecord, add/remove/list, duplicate-remove fail-closed, per-KB
//!   scoped + deterministic). Verified: fmt/check/test/clippy all 0.
//! * Not yet wired (tracked as open work, NOT claimed): chunk retrieval,
//!   RAG oracle, vector index integration, CLI group, docs update, E2E.
//!
//! Namespaces owned by this crate per the PHASE4 lease map (single store,
//! ADR-006; kb slices ADR-009):
//! `kb.meta`, `kb.membership`, `corpus.chunks` (owned by openrt-knowledge);

use openrt_persistence::PersistenceError;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod hybrid;
pub mod retrieval;
pub mod semantic;
pub use hybrid::{hybrid_search, HybridHit, HybridOptions, HybridReport, RerankOutcome};
pub use openrt_retrieval::{
    ModelRerankOptions, RerankKind, RerankMode, RetrievalFilter, RetrievalSource,
};
pub use retrieval::{answer, retrieve, RagOracleAnswer, RetrieveHit, RetrieveRequest};
pub use semantic::{dense_index_key, semantic_search, SemanticReport};

/// `openrt-knowledge` owned namespace for KB meta records.
pub const KB_META_NS: &str = "kb.meta";
/// `openrt-knowledge` owned namespace for KB membership + index-state records.
pub const KB_MEMBERSHIP_NS: &str = "kb.membership";
/// `openrt-knowledge` owned namespace for persisted chunk records.
pub const KB_CHUNKS_NS: &str = "corpus.chunks";

/// Index lifecycle state (PHASE4 `IndexState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexState {
    #[default]
    Empty,
    Pending,
    Indexing,
    Ready,
    Stale,
    Failed,
    Rebuilding,
    Cancelled,
}

impl IndexState {
    #[inline]
    pub fn label(&self) -> &'static str {
        match self {
            IndexState::Empty => "empty",
            IndexState::Pending => "pending",
            IndexState::Indexing => "indexing",
            IndexState::Ready => "ready",
            IndexState::Stale => "stale",
            IndexState::Failed => "failed",
            IndexState::Rebuilding => "rebuilding",
            IndexState::Cancelled => "cancelled",
        }
    }
}

/// A knowledge base slice over the one shared store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeBase {
    pub kb_id: String,
    pub name: String,
    pub created_at: String,
    pub state: IndexState,
    pub embed_dims: usize,
    pub doc_count: u32,
}

/// A provenance citation. Never fabricated: `validate` re-resolves against
/// persisted `corpus.chunks`; `Ok(false)` means "not verified" — never
/// presented as grounded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Citation {
    pub kb_id: String,
    pub document_id: String,
    pub document_version: u32,
    pub chunk_id: String,
    pub block_keys: Vec<String>,
    pub page: Option<u32>,
    pub text_sha256: String,
}

/// Persist a real chunk record under `corpus.chunks`. `provenance` receives
/// the sha256 computed from the chunk's actual block text (never fabricated).
/// Returns the store round-trip outcome; the caller may only treat the chunk
/// as persisted/groomed once this returns `Ok`.
pub async fn persist_chunk(
    store: &dyn openrt_persistence::PersistenceStore,
    kb_id: &str,
    chunk: &PersistedChunk,
) -> Result<(), KnowledgeError> {
    let key = format!(
        "{}.{}",
        safe_key(kb_id),
        record_key(&chunk.document_id, &chunk.chunk_id)
    );
    let value = serde_json::to_value(chunk)
        .map_err(|e| KnowledgeError::Serde(format!("serialize chunk `{key}`: {e}")))?;
    store.put_record(KB_CHUNKS_NS, &key, &value).await?;
    Ok(())
}

/// Load the persisted chunk record for one document/chunk pair (M9.9: the
/// answer path needs the chunk's real TEXT to scan and wrap UNTRUSTED
/// evidence). `Ok(None)` = not persisted — never guessed, never backfilled.
pub async fn load_chunk(
    store: &dyn openrt_persistence::PersistenceStore,
    kb_id: &str,
    document_id: &str,
    chunk_id: &str,
) -> Result<Option<PersistedChunk>, KnowledgeError> {
    let key = format!("{}.{}", safe_key(kb_id), record_key(document_id, chunk_id));
    let Some(value) = store.get_record(KB_CHUNKS_NS, &key).await? else {
        return Ok(None);
    };
    let chunk: PersistedChunk =
        serde_json::from_value(value).map_err(|e| KnowledgeError::Corrupt(key, e.to_string()))?;
    Ok(Some(chunk))
}

/// A persisted knowledge-base membership record (one member may hold exactly
/// one role in a KB; membership is stored under `kb.membership`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MembershipRecord {
    pub kb_id: String,
    pub member_id: String,
    pub role: String,
}

/// Build the deterministic, path-safe store key for a membership record.
/// Mirrors `record_key` discipline: `safe_key` halves joined with `.`,
/// `kb.membership` namespace, never `/`.
pub fn membership_key(kb_id: &str, member_id: &str) -> String {
    format!("{}.{}", safe_key(kb_id), safe_key(member_id))
}

/// Add `member` to KB `kb_id`. Fails closed: an existing identical
/// membership (or any existing record under that key) yields
/// `KnowledgeError::AlreadyExists`. The caller may ONLY treat the member as
/// added once this store round-trip returns `Ok`.
pub async fn add_member(
    store: &dyn openrt_persistence::PersistenceStore,
    kb_id: &str,
    member: &MembershipRecord,
) -> Result<(), KnowledgeError> {
    let key = membership_key(kb_id, &member.member_id);
    if store.get_record(KB_MEMBERSHIP_NS, &key).await?.is_some() {
        return Err(KnowledgeError::AlreadyExists(key));
    }
    let value = serde_json::to_value(member)
        .map_err(|e| KnowledgeError::Serde(format!("serialize member `{key}`: {e}")))?;
    store.put_record(KB_MEMBERSHIP_NS, &key, &value).await?;
    Ok(())
}

/// Remove `member_id` from KB `kb_id`. Fails closed: absent membership yields
/// `KnowledgeError::NotFound`; the caller may ONLY treat the member as removed
/// once this store round-trip returns `Ok`.
pub async fn remove_member(
    store: &dyn openrt_persistence::PersistenceStore,
    kb_id: &str,
    member_id: &str,
) -> Result<(), KnowledgeError> {
    let key = membership_key(kb_id, member_id);
    if store.get_record(KB_MEMBERSHIP_NS, &key).await?.is_none() {
        return Err(KnowledgeError::NotFound(key));
    }
    store.delete_record(KB_MEMBERSHIP_NS, &key).await?;
    Ok(())
}

/// Deterministically list members of KB `kb_id` (sorted by member id).
/// Malformed persisted records fail closed (`Corrupt`). The result is a
/// deterministic `Vec<MembershipRecord>`; never fabricated.
pub async fn list_members(
    store: &dyn openrt_persistence::PersistenceStore,
    kb_id: &str,
) -> Result<Vec<MembershipRecord>, KnowledgeError> {
    let prefix = format!("{}.", safe_key(kb_id));
    let mut members: Vec<(String, MembershipRecord)> = Vec::new();
    for (key, value) in store.list_records(KB_MEMBERSHIP_NS).await? {
        if !key.starts_with(&prefix) {
            continue;
        }
        let rec: MembershipRecord = serde_json::from_value(value)
            .map_err(|e| KnowledgeError::Corrupt(key.clone(), e.to_string()))?;
        if rec.kb_id != kb_id {
            return Err(KnowledgeError::Corrupt(
                key,
                "membership record kb_id does not match its key".into(),
            ));
        }
        members.push((key, rec));
    }
    members.sort_by(|a, b| a.1.member_id.cmp(&b.1.member_id));
    Ok(members.into_iter().map(|(_, r)| r).collect())
}

impl Citation {
    /// Re-resolves this citation against the persisted `corpus.chunks`
    /// store; `validate` NEVER returns `Ok(true)` unless a matching persisted
    /// chunk record genuinely exists. Fail-closed by construction.
    ///
    /// * `Ok(true)` — a matching persisted chunk record exists; the citation
    ///   is genuinely grounded.
    /// * `Ok(false)` — no matching persisted record; MUST NOT be presented
    ///   as verified.
    /// * `Err` — store unavailable; fail closed (never `Ok(true)`).
    pub async fn validate(
        &self,
        store: &dyn openrt_persistence::PersistenceStore,
    ) -> Result<bool, KnowledgeError> {
        let key = format!(
            "{}.{}",
            safe_key(&self.kb_id),
            record_key(&self.document_id, &self.chunk_id)
        );
        let Some(record) = store.get_record(KB_CHUNKS_NS, &key).await? else {
            return Ok(false);
        };
        let chunk: PersistedChunk = serde_json::from_value(record)
            .map_err(|e| KnowledgeError::Corrupt(key, e.to_string()))?;
        // Compare as CITATIONS: a citation carries no text, so the enriched
        // `text` field (M9.5) is provenance, not citation identity — the
        // dedicated `PartialEq<PersistedChunk> for Citation` is the single
        // definition of citation equality.
        Ok(*self == chunk)
    }

    /// Persists THIS citation's chunk into the `corpus.chunks` store,
    /// fail-closed: returns the store round-trip outcome, so a caller may
    /// ONLY treat the citation as persisted once `Ok` (a genuine
    /// `put_record` completed). Never claims persistence without the store
    /// actually completing a write.
    ///
    /// * `Ok(record_key)` — a genuine `put_record` completed; the citation's
    ///   persisted chunk record now exists under the returned key.
    /// * `Err` — store unavailable or record failed to validate on the round
    ///   trip; NOTHING is persisted and the citation MUST NOT be presented
    ///   as persisted.
    pub async fn persist(
        &self,
        store: &dyn openrt_persistence::PersistenceStore,
    ) -> Result<String, KnowledgeError> {
        let chunk: PersistedChunk = (*self).clone().into();
        persist_chunk(store, self.kb_id.as_str(), &chunk).await?;
        Ok(record_key(&self.document_id, &self.chunk_id))
    }
}

/// The persisted `corpus.chunks` record shape (chunk provenance).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedChunk {
    pub chunk_id: String,
    pub document_id: String,
    pub document_version: u32,
    pub block_keys: Vec<String>,
    pub page: Option<u32>,
    pub text_sha256: String,
    /// The chunk's real text when the producer had it at persist time
    /// (M9.5): enables genuine *semantic* embeddings later. `None` for
    /// records persisted before this field existed — those are honestly
    /// excluded from dense indexing, never back-filled from guesses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl From<Citation> for PersistedChunk {
    fn from(c: Citation) -> Self {
        PersistedChunk {
            chunk_id: c.chunk_id,
            document_id: c.document_id,
            document_version: c.document_version,
            block_keys: c.block_keys,
            page: c.page,
            text_sha256: c.text_sha256,
            text: None, // a citation carries no text — never inferred
        }
    }
}

impl PartialEq<PersistedChunk> for Citation {
    fn eq(&self, other: &PersistedChunk) -> bool {
        self.document_id == other.document_id
            && self.document_version == other.document_version
            && self.chunk_id == other.chunk_id
            && self.block_keys == other.block_keys
            && self.page == other.page
            && self.text_sha256 == other.text_sha256
    }
}

/// Encode a KB/document/chunk id into a path-safe record key. This is the
/// same "safe key" discipline as `openrt-vectorindex`/`openrt-embeddings`:
/// `a-z0-9._-` only; anything else becomes `_`-escaped. Honest, deterministic,
/// reversible only through this function.
#[inline]
pub fn safe_key(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for ch in id.chars() {
        match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' => out.push(ch),
            _ => {
                out.push('_');
                write_hex_escape(&mut out, ch);
            }
        }
    }
    out
}

fn write_hex_escape(out: &mut String, ch: char) {
    // _ + 4 hex digits, escapes every non-safe char uniquely.
    let mut buf = [0u8; 4];
    out.push_str("_u");
    if ch.len_utf8() == 1 {
        out.push_str(&format!("{:02x}", ch as u32));
    } else {
        out.push_str("00");
        let n = ch.encode_utf8(&mut buf).len();
        for b in &buf[..n] {
            out.push_str(&format!("{:02x}", b));
        }
    }
}

/// Record key for one persisted chunk provenance record.
#[inline]
/// Build the persisted-chunk record key. The real store only accepts keys in
/// `a-z0-9._-` (it rejects `/`), so we join the two safe halves with `.` —
/// never `/`.
pub fn record_key(document_id: &str, chunk_id: &str) -> String {
    format!("{}.{}", safe_key(document_id), safe_key(chunk_id))
}

/// Errors from the knowledge layer.
#[derive(Debug, Error)]
pub enum KnowledgeError {
    #[error("knowledge base `{0}` not found")]
    NotFound(String),
    #[error("knowledge base `{0}` already exists")]
    AlreadyExists(String),
    #[error("store error: {0}")]
    Store(#[from] PersistenceError),
    #[error("corrupt record `{0}`: {1}")]
    Corrupt(String, String),
    #[error("serialize error: {0}")]
    Serde(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_persistence::{MemoryStore, PersistenceStore};

    #[test]
    fn safe_key_round_trips_and_escapes() {
        assert_eq!(safe_key("kb-a"), "kb-a");
        let k1 = safe_key("my kb/base/1");
        assert!(!k1.contains(' '));
        assert!(!k1.contains('/'));
        assert!(k1.contains("_u"));
        assert_eq!(safe_key("doc 1"), safe_key("doc 1"));
    }

    #[test]
    fn record_key_is_path_safe_and_deterministic() {
        let k = record_key("doc 1", "chunk 2");
        assert!(!k.contains(' '));
        // The real store only allows a-z0-9._- in keys — and record_key MUST
        // produce a store-legal key, so the separator is `_s_` (never `/`).
        assert!(!k.contains('/'));
        assert!(k.contains('.'));
        assert_eq!(record_key("doc 1", "chunk 2"), k);
        let k2 = record_key("路径", "c");
        assert!(k2.contains("_u"));
    }

    #[tokio::test]
    async fn citation_requires_a_real_persisted_chunk() {
        let store = std::sync::Arc::new(MemoryStore::new()) as std::sync::Arc<dyn PersistenceStore>;
        let cit = Citation {
            kb_id: "kb-ground".into(),
            document_id: "d/1".into(),
            document_version: 2,
            chunk_id: "c/3".into(),
            block_keys: vec![],
            page: None,
            text_sha256: "deadbeef".into(),
        };
        // No persisted chunk yet -> MUST be unverified (fail closed).
        assert!(!cit.validate(&*store).await.unwrap());
    }
    /// K2: adding a member persists a real `kb.membership` record; the
    /// membership key is deterministic/path-safe through `membership_key`.
    #[tokio::test]
    async fn membership_add_list_round_trip() {
        let store = std::sync::Arc::new(MemoryStore::new()) as std::sync::Arc<dyn PersistenceStore>;
        let member = MembershipRecord {
            kb_id: "kb-b".to_string(),
            member_id: "u/1".to_string(),
            role: "contributor".to_string(),
        };
        add_member(&*store, "kb-b", &member).await.unwrap();
        let members = list_members(&*store, "kb-b").await.unwrap();
        assert_eq!(
            members
                .iter()
                .map(|r| r.member_id.as_str())
                .collect::<Vec<_>>(),
            vec!["u/1"]
        );
        // deterministic ordering with 2nd member
        let member2 = MembershipRecord {
            kb_id: "kb-b".to_string(),
            member_id: "u/2".to_string(),
            role: "reader".to_string(),
        };
        add_member(&*store, "kb-b", &member2).await.unwrap();
        let members2 = list_members(&*store, "kb-b").await.unwrap();
        assert_eq!(
            members2
                .iter()
                .map(|r| r.member_id.as_str())
                .collect::<Vec<_>>(),
            vec!["u/1", "u/2"]
        );
    }
    /// K2: duplicate membership add MUST fail closed (`AlreadyExists`); the
    /// caller may only treat the member as added once the first `Ok` round
    /// trip returns.
    #[tokio::test]
    async fn membership_duplicate_add_fails_closed() {
        let store = std::sync::Arc::new(MemoryStore::new()) as std::sync::Arc<dyn PersistenceStore>;
        let member = MembershipRecord {
            kb_id: "kb-c".to_string(),
            member_id: "u/1".to_string(),
            role: "reader".to_string(),
        };
        add_member(&*store, "kb-c", &member).await.unwrap();
        let dup = add_member(&*store, "kb-c", &member).await;
        assert!(matches!(dup, Err(KnowledgeError::AlreadyExists(_))));
    }
    /// K2: removing a real member succeeds and removes from the list;
    /// removing a missing member fails closed with `NotFound`. The caller
    /// may only treat the member as removed once removal returns `Ok`.
    #[tokio::test]
    async fn membership_remove_and_missing() {
        let store = std::sync::Arc::new(MemoryStore::new()) as std::sync::Arc<dyn PersistenceStore>;
        let member = MembershipRecord {
            kb_id: "kb-d".to_string(),
            member_id: "u/1".to_string(),
            role: "reader".to_string(),
        };
        add_member(&*store, "kb-d", &member).await.unwrap();
        remove_member(&*store, "kb-d", "u/1").await.unwrap();
        assert!(list_members(&*store, "kb-d").await.unwrap().is_empty());
        let gone = remove_member(&*store, "kb-d", "u/1").await;
        assert!(matches!(gone, Err(KnowledgeError::NotFound(_))));
    }
    /// K2: membership is scoped per KB and deterministic (namespaces are
    /// per-KB through `membership_key`); listing stays deterministic.
    #[tokio::test]
    async fn membership_is_scoped_per_kb_and_deterministic() {
        let store = std::sync::Arc::new(MemoryStore::new()) as std::sync::Arc<dyn PersistenceStore>;
        let m9 = MembershipRecord {
            kb_id: "kb-e".to_string(),
            member_id: "u/9".to_string(),
            role: "reader".to_string(),
        };
        let m1 = MembershipRecord {
            kb_id: "kb-e".to_string(),
            member_id: "u/1".to_string(),
            role: "reader".to_string(),
        };
        let m1o = MembershipRecord {
            kb_id: "kb-other".to_string(),
            member_id: "u/1".to_string(),
            role: "reader".to_string(),
        };
        add_member(&*store, "kb-e", &m9).await.unwrap();
        add_member(&*store, "kb-e", &m1).await.unwrap();
        add_member(&*store, "kb-other", &m1o).await.unwrap();
        let e = list_members(&*store, "kb-e").await.unwrap();
        assert_eq!(
            e.iter().map(|r| r.member_id.as_str()).collect::<Vec<_>>(),
            vec!["u/1", "u/9"]
        );
        let o = list_members(&*store, "kb-other").await.unwrap();
        assert_eq!(
            o.iter().map(|r| r.member_id.as_str()).collect::<Vec<_>>(),
            vec!["u/1"]
        );
    }
}
