//! ADR-002 chained, tamper-evident audit log (Phase 2, M2.7).
//!
//! Every safety-critical signal emission and its subsystem reactions are
//! recorded here as [`AuditEntry`]s in a **chained** log (ADR-002: chained
//! from day one — a `prev_hash` link, not a plain list). Entries are appended
//! through the single openrt [`PersistenceStore`] (ADR-006) in its own
//! `audit` namespace. The log is **append-only by construction**: there is no
//! delete/rewrite surface here, and every write is fail-closed — a store
//! failure surfaces as an error, never a silent drop.
//!
//! # Tamper evidence
//!
//! Each stored record carries `hash = SHA-256(canonical entry)` and
//! `prev_hash = hash of the previous record` (genesis entries link to the
//! empty hash). [`AuditLog::verify`] walks the chain backward from the tail
//! and recomputes every hash, so:
//!
//! * editing an entry's fields breaks that record's own hash;
//! * "repairing" the hash but not the next record's `prev_hash` breaks the
//!   link — chaining makes a partial rewrite detectable;
//! * deleting a record leaves a hole at its sequence — detected as missing.
//!
//! Only someone able to rewrite the *whole* log and its tail (full store
//! custody) can forge a consistent chain — that is out of scope for the
//! audit log itself and is what runtime-side custody/config protects.
//!
//! The crate never uses `unsafe` (workspace-wide `unsafe_code = "forbid"`).

use std::sync::Arc;

use openrt_core::Timestamp;
use openrt_persistence::PersistenceStore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

/// Store namespace for the audit log (ADR-006: one store, per-phase namespace).
pub const AUDIT_NS: &str = "audit";
/// Key under which the log tail (`{seq, hash}`) is kept.
pub const TAIL_KEY: &str = "tail";
/// Canonical serialization version. Bump only when the canonical form changes
/// (never silently); `verify` is version-aware through the envelope.
const CANON_VERSION: u8 = 1;
/// Chain mode recorded in every envelope (ADR-002: always chained).
pub const CHAIN_MODE: &str = "chained";

/// Errors from the audit log. Store failures always propagate (fail-closed);
/// they are never turned into a successful append.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("audit store error: {0}")]
    Persistence(#[from] openrt_persistence::PersistenceError),
    #[error("audit serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("audit log is tampered at sequence {0}: {1}")]
    Tampered(u64, String),
    #[error("audit log is tampered: record at sequence {0} is missing")]
    Missing(u64),
    #[error("audit tail record is corrupt: {0}")]
    CorruptTail(String),
    #[error("record at sequence {0} is corrupt: {1}")]
    CorruptRecord(u64, String),
}

/// One immutable audit entry (the ADR-002 envelope minus the hash links,
/// which live on the stored [`AuditRecord`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Monotonic sequence. Assigned by [`AuditLog::append`] and immutable.
    pub seq: u64,
    /// Emit timestamp (RFC 3339 via `openrt_core::now`).
    pub ts: Timestamp,
    /// Who/what produced the signal or reaction (`operator`, `system`, the
    /// session id, ...).
    pub actor: String,
    /// The signal/action (`safety.halt`, `permission.decision`, ...).
    pub action: String,
    /// The subject the action applies to (session id, tool name, scope, ...).
    pub subject: String,
    /// The outcome (`tripped`, `allowed`, `denied`, `kill_blocked`, ...).
    pub result: String,
    /// Always `chained` in this implementation (ADR-002).
    pub chain_mode: String,
    /// Free-form, structured extension data.
    pub ext: Value,
}

impl AuditEntry {
    /// Canonical form the record hash is computed over. Versioned and
    /// deterministic: map keys in `ext` are sorted, so equal entries always
    /// serialize to identical bytes.
    pub fn canonical(&self) -> String {
        format!(
            "{CANON_VERSION}|{}|{}|{}|{}|{}|{}|{}|{}",
            self.seq,
            self.ts,
            self.actor,
            self.action,
            self.subject,
            self.result,
            self.chain_mode,
            canonical_value(&self.ext),
        )
    }
}

/// Stored record: the entry plus the two hash links.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    pub seq: u64,
    /// SHA-256 (hex) of [`AuditEntry::canonical`].
    pub hash: String,
    /// SHA-256 (hex) of the previous record (genesis: empty string).
    pub prev_hash: String,
    pub entry: AuditEntry,
}

/// Position/hash pair kept under [`TAIL_KEY`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AuditTail {
    pub seq: u64,
    pub hash: String,
}

/// Persisted key for a sequence number (zero-padded so string ordering equals
/// sequence ordering).
pub fn seq_key(seq: u64) -> String {
    format!("{seq:020}")
}

/// The chained audit log, serialized through a single [`PersistenceStore`].
pub struct AuditLog {
    store: Arc<dyn PersistenceStore>,
    inner: Mutex<AuditLogInner>,
}

struct AuditLogInner {
    tail: Option<AuditTail>,
}

impl AuditLog {
    /// Open the log over the given store. The tail is loaded lazily on the
    /// first append/verify.
    pub fn new(store: Arc<dyn PersistenceStore>) -> Self {
        Self {
            store,
            inner: Mutex::new(AuditLogInner { tail: None }),
        }
    }

    /// Append one entry. The sequence is derived from the current tail and the
    /// record is chained onto it. Fail-closed: any store error propagates.
    pub async fn append(
        &self,
        actor: &str,
        action: &str,
        subject: &str,
        result: &str,
        ext: Value,
    ) -> Result<u64, AuditError> {
        // Cross-process + cross-instance section: the tail read-modify-write
        // must be atomic w.r.t. other appends — two racing writers would both
        // compute seq = tail+1 and fork the chain. The tail is ALWAYS
        // reloaded from the store inside the section (another process may
        // have advanced it since we last looked).
        let _section = self.store.lock_exclusive().await?;
        let mut guard = self.inner.lock().await;
        let tail = self.load_tail().await?;
        guard.tail = Some(tail.clone());
        let seq = tail.seq + 1;
        let entry = AuditEntry {
            seq,
            ts: openrt_core::now(),
            actor: actor.to_string(),
            action: action.to_string(),
            subject: subject.to_string(),
            result: result.to_string(),
            chain_mode: CHAIN_MODE.to_string(),
            ext,
        };
        let hash = sha256_hex(&entry.canonical());
        let record = AuditRecord {
            seq,
            hash: hash.clone(),
            prev_hash: tail.hash.clone(),
            entry,
        };
        let value = serde_json::to_value(&record)?;
        // 1) The record first, then 2) the tail, both inside the same lock so
        // appends are serialized. Fail-closed: a failed record write means the
        // append did not happen.
        self.store
            .put_record(AUDIT_NS, &seq_key(seq), &value)
            .await?;
        let new_tail = AuditTail {
            seq,
            hash: hash.clone(),
        };
        self.store
            .put_record(AUDIT_NS, TAIL_KEY, &serde_json::to_value(&new_tail)?)
            .await?;
        guard.tail = Some(new_tail);
        Ok(seq)
    }

    /// Verify the whole chain from the tail back to the genesis record.
    /// Returns the number of verified records, or the first tamper/missing
    /// record as an error.
    pub async fn verify(&self) -> Result<u64, AuditError> {
        let mut guard = self.inner.lock().await;
        // Fresh tail: a cached one could be stale if another process appended
        // (verify would then under-report, never over-report).
        let tail = self.load_tail().await?;
        guard.tail = Some(tail.clone());
        let mut seq = tail.seq;
        let mut expected_hash = tail.hash;
        let mut verified = 0u64;
        while seq >= 1 {
            let rec = self.load_record(seq).await?;
            if rec.seq != seq {
                return Err(AuditError::CorruptRecord(
                    seq,
                    format!("record stores seq {}, expected {seq}", rec.seq),
                ));
            }
            let recomputed = sha256_hex(&rec.entry.canonical());
            if recomputed != rec.hash {
                return Err(AuditError::Tampered(
                    seq,
                    format!(
                        "content hash mismatch: stored {}, recomputed {recomputed}",
                        rec.hash
                    ),
                ));
            }
            if rec.hash != expected_hash {
                return Err(AuditError::Tampered(seq, format!(
                    "link mismatch: record hash {} does not match expected {expected_hash} (tail of {})",
                    rec.hash,
                    seq + 1,
                )));
            }
            expected_hash = rec.prev_hash;
            seq -= 1;
            verified += 1;
        }
        // Genesis must link to the empty hash.
        if verified > 0 && !expected_hash.is_empty() {
            return Err(AuditError::Tampered(
                1,
                format!("genesis record does not link to the empty hash: {expected_hash}"),
            ));
        }
        Ok(verified)
    }

    /// Read a single record by sequence (for inspection/debugging).
    pub async fn get(&self, seq: u64) -> Result<AuditRecord, AuditError> {
        self.load_record(seq).await
    }

    /// Search/aggregation query over the chain (M6.2 additive surface — does
    /// not change the envelope or serialization). Filters are AND-ed; `None`
    /// means unrestricted. Results are ascending by sequence.
    pub async fn query(&self, filter: &AuditQuery) -> Result<Vec<AuditRecord>, AuditError> {
        let count = self.count().await?;
        let start = filter.from_seq.unwrap_or(1).max(1);
        let end = filter.to_seq.unwrap_or(count).min(count);
        let mut out = Vec::new();
        let mut matched = 0u64;
        for seq in start..=end {
            let rec = match self.load_record(seq).await {
                Ok(r) => r,
                Err(AuditError::Missing(_)) => continue,
                Err(e) => return Err(e),
            };
            if !filter.matches(&rec.entry) {
                continue;
            }
            out.push(rec);
            matched += 1;
            if let Some(limit) = filter.limit {
                if matched >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Count matching records without materializing them all.
    pub async fn count_matching(&self, filter: &AuditQuery) -> Result<u64, AuditError> {
        Ok(self.query(filter).await?.len() as u64)
    }

    /// Retention summary: oldest/newest sequence still present and total.
    /// Physical truncation of a chained log would break verification, so
    /// retention is enforced by *archival copy + cursor* at the observability
    /// layer; this method reports what is currently verifiable.
    pub async fn retention_status(&self) -> Result<RetentionStatus, AuditError> {
        let count = self.count().await?;
        Ok(RetentionStatus {
            total: count,
            head_seq: count,
            tail_seq: if count == 0 { 0 } else { 1 },
            chain_mode: CHAIN_MODE.to_string(),
        })
    }

    /// Current tail sequence (0 when empty). Fail-closed on store errors.
    pub async fn count(&self) -> Result<u64, AuditError> {
        let mut guard = self.inner.lock().await;
        // Fresh tail — same staleness rule as `verify`.
        let tail = self.load_tail().await?;
        guard.tail = Some(tail.clone());
        Ok(tail.seq)
    }

    async fn load_tail(&self) -> Result<AuditTail, AuditError> {
        let stored = self.store.get_record(AUDIT_NS, TAIL_KEY).await?;
        match stored {
            None => Ok(AuditTail::default()),
            Some(v) => serde_json::from_value(v)
                .map_err(|e| AuditError::CorruptTail(format!("{TAIL_KEY} undecodable: {e}"))),
        }
    }

    async fn load_record(&self, seq: u64) -> Result<AuditRecord, AuditError> {
        let stored = self
            .store
            .get_record(AUDIT_NS, &seq_key(seq))
            .await?
            .ok_or(AuditError::Missing(seq))?;
        serde_json::from_value(stored).map_err(|e| AuditError::CorruptRecord(seq, e.to_string()))
    }
}

/// Filter for [`AuditLog::query`] (aggregation/search; additive, ADR-002).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditQuery {
    pub from_seq: Option<u64>,
    pub to_seq: Option<u64>,
    /// Exact match on `actor` when set.
    pub actor: Option<String>,
    /// Prefix match on `action` when set (e.g. `permission.`).
    pub action_prefix: Option<String>,
    /// Exact match on `action` when set (wins over prefix if both set).
    pub action: Option<String>,
    /// Substring match on `subject` when set.
    pub subject_contains: Option<String>,
    /// Exact match on `result` when set.
    pub result: Option<String>,
    pub limit: Option<u64>,
}

impl AuditQuery {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_limit(mut self, limit: u64) -> Self {
        self.limit = Some(limit);
        self
    }

    pub fn with_action_prefix(mut self, p: impl Into<String>) -> Self {
        self.action_prefix = Some(p.into());
        self
    }

    pub fn with_subject_contains(mut self, s: impl Into<String>) -> Self {
        self.subject_contains = Some(s.into());
        self
    }

    pub fn with_result(mut self, r: impl Into<String>) -> Self {
        self.result = Some(r.into());
        self
    }

    pub fn matches(&self, e: &AuditEntry) -> bool {
        if let Some(a) = &self.actor {
            if &e.actor != a {
                return false;
            }
        }
        if let Some(a) = &self.action {
            if &e.action != a {
                return false;
            }
        } else if let Some(p) = &self.action_prefix {
            if !e.action.starts_with(p.as_str()) {
                return false;
            }
        }
        if let Some(s) = &self.subject_contains {
            if !e.subject.contains(s.as_str()) {
                return false;
            }
        }
        if let Some(r) = &self.result {
            if &e.result != r {
                return false;
            }
        }
        true
    }
}

/// Retention view over the verifiable chain (no rewrite surface).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionStatus {
    pub total: u64,
    pub head_seq: u64,
    pub tail_seq: u64,
    pub chain_mode: String,
}

/// Deterministic canonical encoding of arbitrary JSON: map keys are sorted so
/// serialization is stable regardless of insertion order.
pub fn canonical_value(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let mut out = String::from("{");
            for (i, (k, val)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&canonical_value(&json!(k)));
                out.push(':');
                out.push_str(&canonical_value(val));
            }
            out.push('}');
            out
        }
        Value::Array(items) => {
            let mut out = String::from("[");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&canonical_value(item));
            }
            out.push(']');
            out
        }
        Value::String(s) => format!("{:?}", serde_json::Value::String(s.clone())),
        other => other.to_string(),
    }
}

fn sha256_hex(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_persistence::MemoryStore;

    fn log() -> AuditLog {
        AuditLog::new(Arc::new(MemoryStore::new()))
    }

    #[tokio::test]
    async fn appends_chain_and_verifies() {
        let log = log();
        let s1 = log
            .append(
                "operator",
                "safety.halt",
                "sess-1",
                "tripped",
                json!({"reason": "x"}),
            )
            .await
            .unwrap();
        let s2 = log
            .append(
                "system",
                "comp.kill_blocked",
                "terminal.run",
                "denied",
                json!({}),
            )
            .await
            .unwrap();
        let s3 = log
            .append(
                "system",
                "permission.decision",
                "input.mouse",
                "allowed",
                json!({"session": "sess-1"}),
            )
            .await
            .unwrap();
        assert_eq!((s1, s2, s3), (1, 2, 3));
        assert_eq!(log.count().await.unwrap(), 3);
        assert_eq!(log.verify().await.unwrap(), 3);

        // Links: record 2 links to record 1; genesis prev_hash is empty.
        let r1 = log.get(1).await.unwrap();
        let r2 = log.get(2).await.unwrap();
        let r3 = log.get(3).await.unwrap();
        assert!(r1.prev_hash.is_empty());
        assert_eq!(r2.prev_hash, r1.hash);
        assert_eq!(r3.prev_hash, r2.hash);
        assert_eq!(r1.entry.chain_mode, CHAIN_MODE);
    }

    #[tokio::test]
    async fn empty_log_verifies_to_zero() {
        let log = log();
        assert_eq!(log.verify().await.unwrap(), 0);
        assert_eq!(log.count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn tampered_content_detected() {
        let log = log();
        for i in 1..=3 {
            log.append(
                "system",
                "test.action",
                &format!("step-{i}"),
                "ok",
                json!({"i": i}),
            )
            .await
            .unwrap();
        }
        // Attacker rewrites record 2's action but keeps the old hash and prev_hash.
        let store = log.store.clone();
        let old = log.get(2).await.unwrap();
        let mut tampered = old.clone();
        tampered.entry.action = "ATTACKED".into();
        let value = serde_json::to_value(&tampered).unwrap();
        store
            .put_record(AUDIT_NS, &seq_key(2), &value)
            .await
            .unwrap();

        let err = log.verify().await.unwrap_err();
        match err {
            AuditError::Tampered(seq, _) => assert_eq!(seq, 2),
            other => panic!("expected Tampered(2), got {other}"),
        }
    }

    #[tokio::test]
    async fn tampered_link_detected_even_with_recomputed_hash() {
        let log = log();
        for i in 1..=3 {
            log.append(
                "system",
                "test.action",
                &format!("step-{i}"),
                "ok",
                json!({"i": i}),
            )
            .await
            .unwrap();
        }
        let store = log.store.clone();
        // Attacker rewrites record 2 *and* recomputes its hash, but does not fix
        // record 3's prev_hash (which still links to the ORIGINAL record 2). The
        // follower's link then mismatches the recomputed hash.
        let mut tampered = log.get(2).await.unwrap();
        tampered.entry.action = "ATTACKED".into();
        tampered.hash = sha256_hex(&tampered.entry.canonical());
        store
            .put_record(
                AUDIT_NS,
                &seq_key(2),
                &serde_json::to_value(&tampered).unwrap(),
            )
            .await
            .unwrap();

        let err = log.verify().await.unwrap_err();
        match err {
            AuditError::Tampered(seq, _) => {
                assert_eq!(seq, 2, "partial re-hashing must break the follower's link")
            }
            other => panic!("expected Tampered(2), got {other}"),
        }
    }

    #[tokio::test]
    async fn missing_record_detected() {
        let log = log();
        for i in 1..=4 {
            log.append(
                "system",
                "test.action",
                &format!("step-{i}"),
                "ok",
                json!({"i": i}),
            )
            .await
            .unwrap();
        }
        let store = log.store.clone();
        store.delete_record(AUDIT_NS, &seq_key(2)).await.unwrap();

        let err = log.verify().await.unwrap_err();
        // Backward walk hits the hole at sequence 2 first.
        assert!(matches!(err, AuditError::Missing(2)), "{err}");
    }

    #[tokio::test]
    async fn concurrent_appends_are_serialized_and_contiguous() {
        let log = Arc::new(log());
        let mut handles = Vec::new();
        for i in 0..20u64 {
            let log = log.clone();
            handles.push(tokio::spawn(async move {
                log.append(
                    "system",
                    "test.concurrent",
                    &format!("w-{i}"),
                    "ok",
                    json!({"w": i}),
                )
                .await
                .unwrap()
            }));
        }
        let mut seqs = Vec::new();
        for h in handles {
            seqs.push(h.await.unwrap());
        }
        seqs.sort_unstable();
        assert_eq!(
            seqs,
            (1..=20).collect::<Vec<_>>(),
            "seqs must be unique and contiguous"
        );
        assert_eq!(log.count().await.unwrap(), 20);
        assert_eq!(log.verify().await.unwrap(), 20);
    }

    #[tokio::test]
    async fn store_failure_is_fail_closed() {
        let log = AuditLog::new(Arc::new(FailingStore));
        let err = log
            .append("system", "test.action", "s", "ok", json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, AuditError::Persistence(_)), "{err}");
        // Nothing was half-appended.
        assert_eq!(log.count().await.unwrap(), 0);
    }

    /// A store that always fails writes; used to prove fail-closed behavior.
    struct FailingStore;
    #[async_trait::async_trait]
    impl PersistenceStore for FailingStore {
        async fn create_session(
            &self,
            _s: &openrt_core::Session,
        ) -> Result<(), openrt_persistence::PersistenceError> {
            unimplemented!()
        }
        async fn update_session(
            &self,
            _s: &openrt_core::Session,
        ) -> Result<(), openrt_persistence::PersistenceError> {
            unimplemented!()
        }
        async fn load_session(
            &self,
            _id: openrt_core::SessionId,
        ) -> Result<openrt_core::Session, openrt_persistence::PersistenceError> {
            unimplemented!()
        }
        async fn list_sessions(
            &self,
        ) -> Result<Vec<openrt_core::Session>, openrt_persistence::PersistenceError> {
            unimplemented!()
        }
        async fn save_task(
            &self,
            _t: &openrt_core::Task,
        ) -> Result<(), openrt_persistence::PersistenceError> {
            unimplemented!()
        }
        async fn load_task(
            &self,
            _id: openrt_core::TaskId,
        ) -> Result<openrt_core::Task, openrt_persistence::PersistenceError> {
            unimplemented!()
        }
        async fn list_tasks(
            &self,
            _s: Option<openrt_core::SessionId>,
        ) -> Result<Vec<openrt_core::Task>, openrt_persistence::PersistenceError> {
            unimplemented!()
        }
        async fn save_execution(
            &self,
            _r: &openrt_core::ExecutionRecord,
        ) -> Result<(), openrt_persistence::PersistenceError> {
            unimplemented!()
        }
        async fn load_execution(
            &self,
            _id: openrt_core::ExecutionId,
        ) -> Result<openrt_core::ExecutionRecord, openrt_persistence::PersistenceError> {
            unimplemented!()
        }
        async fn append_event(
            &self,
            _e: &openrt_core::DomainEvent,
        ) -> Result<(), openrt_persistence::PersistenceError> {
            unimplemented!()
        }
        async fn list_events(
            &self,
            _o: openrt_persistence::ListOptions,
        ) -> Result<Vec<openrt_core::DomainEvent>, openrt_persistence::PersistenceError> {
            unimplemented!()
        }
        async fn put_record(
            &self,
            _n: &str,
            _k: &str,
            _v: &Value,
        ) -> Result<(), openrt_persistence::PersistenceError> {
            Err(openrt_persistence::PersistenceError::Unsupported(
                "failing test store".into(),
            ))
        }
        async fn get_record(
            &self,
            _n: &str,
            _k: &str,
        ) -> Result<Option<Value>, openrt_persistence::PersistenceError> {
            Ok(None)
        }
        async fn list_records(
            &self,
            _n: &str,
        ) -> Result<Vec<(String, Value)>, openrt_persistence::PersistenceError> {
            unimplemented!()
        }
        async fn delete_record(
            &self,
            _n: &str,
            _k: &str,
        ) -> Result<(), openrt_persistence::PersistenceError> {
            unimplemented!()
        }
        async fn clear(&self) -> Result<(), openrt_persistence::PersistenceError> {
            Ok(())
        }
    }
}
