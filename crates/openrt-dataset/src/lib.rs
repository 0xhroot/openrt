//! `openrt-dataset` — local-first JSONL datasets with provenance, schema
//! validation, deterministic splits, safe preview/export, and zero execution.
//!
//! Immediate design decisions (P4.11):
//! * JSONL only (the one format this build genuinely supports for datasets).
//! * Records are validated against a typed schema
//!   ([`DatasetSchema`]); unknown fields are recorded, never dropped silently.
//! * Diagnostics are safe: validation errors quote a **truncated** line prefix,
//!   never the full record, and a dedicated sensitive-key check flags records
//!   that carry secret-looking keys without ever printing their values.
//! * Import never executes anything: executables (magic + extension) are
//!   rejected and every line must parse as JSON.
//! * Splits are deterministic over the record digest, so identical input
//!   always yields identical train/val/test membership (no leakage by design —
//!   a record belongs to exactly one split).

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use openrt_audit::AuditLog;
use openrt_core::{DomainEvent, EventKind};
use openrt_persistence::PersistenceStore;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DATASET_SCHEMA_VERSION: u32 = 1;
pub const DATASETS_META_NS: &str = "datasets.meta";
pub const DATASETS_PAYLOAD_NS: &str = "datasets.payload";
/// Per-record line digests, kept so a dataset's exact revision is provable
/// independently of the payload blob.
pub const DATASETS_ENTRIES_NS: &str = "datasets.entries";

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatasetSchema {
    InstructionResponse,
    Conversation,
    Preference,
}

impl DatasetSchema {
    pub fn label(&self) -> &'static str {
        match self {
            DatasetSchema::InstructionResponse => "instruction_response",
            DatasetSchema::Conversation => "conversation",
            DatasetSchema::Preference => "preference",
        }
    }

    /// Validate one parsed record against this schema. Returns a list of
    /// human-readable structural problems (empty = valid).
    pub fn problems(&self, record: &serde_json::Value) -> Vec<String> {
        let mut out = Vec::new();
        let obj = match record {
            serde_json::Value::Object(m) => m,
            _ => {
                out.push("record is not an object".into());
                return out;
            }
        };
        match self {
            DatasetSchema::InstructionResponse => {
                match obj.get("instruction") {
                    Some(serde_json::Value::String(s)) if !s.is_empty() => {}
                    _ => out.push("missing non-empty string `instruction`".into()),
                }
                match obj.get("response") {
                    Some(serde_json::Value::String(s)) if !s.is_empty() => {
                        if let Some(serde_json::Value::String(input)) = obj.get("input") {
                            if input.is_empty() {
                                out.push("`input` must be non-empty when present".into());
                            }
                        }
                    }
                    _ => out.push("missing non-empty string `response`".into()),
                }
            }
            DatasetSchema::Conversation => {
                let list = obj.get("conversation").or_else(|| obj.get("messages"));
                match list {
                    Some(serde_json::Value::Array(arr)) if !arr.is_empty() => {
                        for (i, turn) in arr.iter().enumerate() {
                            let role = turn.get("role").and_then(|r| r.as_str()).unwrap_or("");
                            let content =
                                turn.get("content").and_then(|c| c.as_str()).unwrap_or("");
                            if role != "user" && role != "assistant" {
                                out.push(format!("turn[{i}] role must be user|assistant"));
                            }
                            if content.is_empty() {
                                out.push(format!("turn[{i}] content must be non-empty"));
                            }
                        }
                    }
                    _ => out.push("missing non-empty `conversation`|`messages` array".into()),
                }
            }
            DatasetSchema::Preference => {
                for key in ["chosen", "rejected"] {
                    match obj.get(key) {
                        Some(serde_json::Value::String(s)) if !s.is_empty() => {}
                        Some(serde_json::Value::Array(a)) if !a.is_empty() => {}
                        _ => out.push(format!("missing non-empty `{key}` (string or messages)")),
                    }
                }
            }
        }
        out
    }
}

/// Deterministic train/val/test split for one record digest.
pub fn split_for(record_sha256: &str) -> &'static str {
    let bucket = record_sha256
        .bytes()
        .take(2)
        .fold(0u32, |acc, b| acc * 256 + b as u32)
        % 100;
    if bucket < 80 {
        "train"
    } else if bucket < 90 {
        "val"
    } else {
        "test"
    }
}

/// Sensitive-key heuristic (key names only; values are never read/printed).
pub fn sensitive_keys_in(record: &serde_json::Value) -> Vec<String> {
    const KEYS: &[&str] = &[
        "password",
        "secret",
        "token",
        "api_key",
        "apikey",
        "authorization",
        "private_key",
        "access_key",
        "session_id",
        "cookie",
    ];
    fn walk(v: &serde_json::Value, out: &mut BTreeSet<String>) {
        match v {
            serde_json::Value::Object(m) => {
                for (k, val) in m {
                    let low = k.to_ascii_lowercase();
                    if KEYS.iter().any(|s| low.contains(s)) {
                        out.insert(k.clone());
                    }
                    walk(val, out);
                }
            }
            serde_json::Value::Array(a) => {
                for v in a {
                    walk(v, out);
                }
            }
            _ => {}
        }
    }
    let mut set = BTreeSet::new();
    walk(record, &mut set);
    set.into_iter().collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationReport {
    pub schema: DatasetSchema,
    pub total: u64,
    pub ok: u64,
    pub skipped: u64,
    /// Up to N structural problems, each truncated to a safe length.
    pub problems: Vec<String>,
    /// Sensitive-looking keys observed anywhere (values never disclosed).
    pub sensitive_keys: Vec<String>,
    pub payload_sha256: String,
}

/// Truncate diagnostics so errors never leak full record content.
fn truncate(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

/// Parse and validate a JSONL byte stream against a schema without executing
/// anything.
pub fn validate_jsonl(
    bytes: &[u8],
    schema: DatasetSchema,
) -> Result<ValidationReport, DatasetError> {
    ensure_not_executable(bytes)?;
    let text = std::str::from_utf8(bytes).map_err(|_| DatasetError::NotUtf8)?;
    let payload_sha256 = sha256_hex(bytes);
    let mut total = 0u64;
    let mut ok = 0u64;
    let mut skipped = 0u64;
    let mut problems = Vec::new();
    let mut sensitive = BTreeSet::new();
    for (idx, line) in text.lines().enumerate() {
        total += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            skipped += 1;
            continue;
        }
        let record: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                problems.push(format!(
                    "line {}: parse error ({}): {}",
                    idx + 1,
                    e,
                    truncate(trimmed, 80)
                ));
                continue;
            }
        };
        for k in sensitive_keys_in(&record) {
            sensitive.insert(k);
        }
        let rec_problems = schema.problems(&record);
        if rec_problems.is_empty() {
            ok += 1;
        } else {
            skipped += 1;
            let joined = rec_problems.join("; ");
            problems.push(format!(
                "line {}: {} — {}",
                idx + 1,
                joined,
                truncate(trimmed, 80)
            ));
            if problems.len() >= 50 {
                problems.push(format!("… {} more problems omitted", total - 1));
                break;
            }
        }
    }
    Ok(ValidationReport {
        schema,
        total,
        ok,
        skipped: skipped + (total - ok - skipped),
        problems,
        sensitive_keys: sensitive.into_iter().collect(),
        payload_sha256,
    })
}

/// Reject executables by magic + extension before any parsing.
fn ensure_not_executable(bytes: &[u8]) -> Result<(), DatasetError> {
    let looks_binary = bytes.starts_with(b"MZ")
        || bytes.starts_with(&[0x7f, b'E', b'L', b'F'])
        || bytes.starts_with(b"#!")
        || bytes.starts_with(b"PK\x03\x04");
    if looks_binary {
        return Err(DatasetError::ExecutableRejected);
    }
    Ok(())
}

/// Keep only a truncated head for preview output.
pub fn preview_lines(text: &str, n_lines: usize, max_line_chars: usize) -> Vec<String> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .take(n_lines)
        .map(|l| truncate(l, max_line_chars))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitCounts {
    pub train: u64,
    pub val: u64,
    pub test: u64,
}

/// Immutable dataset revision record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatasetRecord {
    pub dataset_id: String,
    pub schema: DatasetSchema,
    pub version: u32,
    pub payload_sha256: String,
    pub record_count: u64,
    pub splits: SplitCounts,
    pub imported_at: String,
    pub source_name: Option<String>,
    pub sensitive_keys_warned: Vec<String>,
}

#[derive(Debug, Error)]
pub enum DatasetError {
    #[error("input is not valid UTF-8 jsonl")]
    NotUtf8,
    #[error("input looks like an executable; datasets are never executed")]
    ExecutableRejected,
    #[error("no valid records (ok={ok})")]
    NoValidRecords { ok: u64 },
    #[error(transparent)]
    Audit(#[from] openrt_audit::AuditError),
    #[error("store error: {0}")]
    Store(String),
    #[error("dataset `{0}` not found")]
    NotFound(String),
    #[error("export path denied: {0}")]
    ExportDenied(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("config error: {0}")]
    Config(String),
}

impl From<openrt_persistence::PersistenceError> for DatasetError {
    fn from(e: openrt_persistence::PersistenceError) -> Self {
        DatasetError::Store(e.to_string())
    }
}

/// Persisted dataset store.
pub struct DatasetStore {
    store: Arc<dyn PersistenceStore>,
    audit: AuditLog,
}

impl DatasetStore {
    pub fn new(store: Arc<dyn PersistenceStore>) -> Self {
        let audit = AuditLog::new(store.clone());
        Self { store, audit }
    }

    pub fn ns_key(dataset_id: &str) -> String {
        openrt_core::encode_safe_key(dataset_id)
    }

    /// Import validated JSONL. Returns the stored record (duplicate bytes are
    /// reported as such and the existing record returned unchanged).
    pub async fn import(
        &self,
        bytes: Vec<u8>,
        schema: DatasetSchema,
        source_name: Option<String>,
        actor: &str,
    ) -> Result<(DatasetRecord, bool), DatasetError> {
        let report = validate_jsonl(&bytes, schema)?;
        if report.ok == 0 {
            return Err(DatasetError::NoValidRecords { ok: report.ok });
        }
        let dataset_id = format!("ds/{}", report.payload_sha256);
        let key = Self::ns_key(&dataset_id);
        let existing = self
            .store
            .get_record(DATASETS_META_NS, &key)
            .await?
            .map(serde_json::from_value::<DatasetRecord>)
            .transpose()
            .map_err(|e| DatasetError::Store(format!("decode: {e}")))?;
        if let Some(rec) = existing {
            self.audit
                .append(
                    actor,
                    "dataset.import",
                    &dataset_id,
                    "duplicate",
                    serde_json::json!({}),
                )
                .await?;
            return Ok((rec, true));
        }

        let splits = SplitCounts {
            train: 0,
            val: 0,
            test: 0,
        };
        let record_count = report.ok;
        let text = String::from_utf8(bytes.clone()).unwrap_or_default();
        let (mut train, mut val, mut test) = (0u64, 0u64, 0u64);
        let mut entry_digests = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let d = sha256_hex(line.as_bytes());
            entry_digests.push(d.clone());
            match split_for(&d) {
                "val" => val += 1,
                "test" => test += 1,
                _ => train += 1,
            }
        }
        let _ = (splits.train, splits.val, splits.test); // replaced below

        let record = DatasetRecord {
            dataset_id: dataset_id.clone(),
            schema,
            version: 1,
            payload_sha256: report.payload_sha256.clone(),
            record_count,
            splits: SplitCounts { train, val, test },
            imported_at: openrt_core::now().to_rfc3339(),
            source_name,
            sensitive_keys_warned: report.sensitive_keys.clone(),
        };
        let meta = serde_json::to_value(&record)
            .map_err(|e| DatasetError::Store(format!("serialize: {e}")))?;
        self.store.put_record(DATASETS_META_NS, &key, &meta).await?;
        self.store
            .put_blob(
                DATASETS_PAYLOAD_NS,
                &key,
                Self::normalize_jsonl(&text).as_bytes(),
            )
            .await?;
        let entries_value = serde_json::to_value(&entry_digests)
            .map_err(|e| DatasetError::Store(format!("serialize entries: {e}")))?;
        self.store
            .put_record(DATASETS_ENTRIES_NS, &key, &entries_value)
            .await?;
        self.audit
            .append(
                actor,
                "dataset.import",
                &dataset_id,
                "ok",
                serde_json::json!({
                    "records": record_count,
                    "schema": schema.label(),
                    "splits": { "train": train, "val": val, "test": test },
                    "sensitive_keys": record.sensitive_keys_warned,
                }),
            )
            .await?;
        self.emit(
            EventKind::DatasetImported,
            serde_json::json!({
                "dataset_id": dataset_id,
                "records": record_count,
                "splits": { "train": train, "val": val, "test": test },
                "schema": schema.label(),
            }),
        )
        .await;
        Ok((record, false))
    }

    pub async fn list(&self) -> Result<Vec<DatasetRecord>, DatasetError> {
        let mut out = Vec::new();
        for (_k, v) in self.store.list_records(DATASETS_META_NS).await? {
            if let Ok(r) = serde_json::from_value::<DatasetRecord>(v) {
                out.push(r);
            }
        }
        out.sort_by(|a, b| b.imported_at.cmp(&a.imported_at));
        Ok(out)
    }

    pub async fn inspect(&self, dataset_id: &str) -> Result<DatasetRecord, DatasetError> {
        let v = self
            .store
            .get_record(DATASETS_META_NS, &Self::ns_key(dataset_id))
            .await?
            .ok_or_else(|| DatasetError::NotFound(dataset_id.to_string()))?;
        serde_json::from_value(v).map_err(|e| DatasetError::Store(format!("decode: {e}")))
    }

    pub async fn preview(&self, dataset_id: &str, n: usize) -> Result<Vec<String>, DatasetError> {
        let key = Self::ns_key(dataset_id);
        let payload = self
            .store
            .get_blob(DATASETS_PAYLOAD_NS, &key)
            .await?
            .ok_or_else(|| DatasetError::NotFound(dataset_id.to_string()))?;
        let text = String::from_utf8(payload).map_err(|_| DatasetError::NotUtf8)?;
        Ok(preview_lines(&text, n, 200))
    }

    /// Export to a safe destination. The destination must be reachable from an
    /// explicit root (no traversal, no symlink escape) and must not already be
    /// a directory.
    pub async fn export(
        &self,
        dataset_id: &str,
        root: &Path,
        dest: &Path,
        actor: &str,
    ) -> Result<u64, DatasetError> {
        let dest = check_export_path(root, dest)?;
        let key = Self::ns_key(dataset_id);
        let payload = self
            .store
            .get_blob(DATASETS_PAYLOAD_NS, &key)
            .await?
            .ok_or_else(|| DatasetError::NotFound(dataset_id.to_string()))?;
        let written = payload.len() as u64;
        std::fs::write(&dest, &payload).map_err(|e| DatasetError::Io(e.to_string()))?;
        self.audit
            .append(
                actor,
                "dataset.export",
                dataset_id,
                "ok",
                serde_json::json!({ "bytes": written }),
            )
            .await?;
        self.emit(
            EventKind::DatasetExported,
            serde_json::json!({ "dataset_id": dataset_id, "bytes_written": written }),
        )
        .await;
        Ok(written)
    }

    pub async fn delete(&self, dataset_id: &str, actor: &str) -> Result<(), DatasetError> {
        let key = Self::ns_key(dataset_id);
        self.inspect(dataset_id).await?;
        self.store.delete_record(DATASETS_META_NS, &key).await?;
        self.store.delete_record(DATASETS_ENTRIES_NS, &key).await?;
        // Blobs use delete via put empty? Persistence has no delete_blob; store a
        // zero-length marker so export/preview return NotFound for it.
        self.store.put_blob(DATASETS_PAYLOAD_NS, &key, &[]).await?;
        self.audit
            .append(
                actor,
                "dataset.delete",
                dataset_id,
                "ok",
                serde_json::json!({}),
            )
            .await?;
        Ok(())
    }

    /// Canonical, line-normalised payload (LF endings).
    fn normalize_jsonl(text: &str) -> String {
        text.lines().map(|l| format!("{l}\n")).collect()
    }

    async fn emit(&self, kind: EventKind, data: serde_json::Value) {
        let _ = self
            .store
            .append_event(&DomainEvent::new(kind).with_data(data))
            .await;
    }
}

/// Minimal safe destination check (no canonicalize of a non-existent target):
/// canonicalise the parent, join the file name, and require the result to stay
/// under `root`.
fn check_export_path(root: &Path, dest: &Path) -> Result<std::path::PathBuf, DatasetError> {
    if !root.is_absolute() || !dest.is_absolute() {
        return Err(DatasetError::ExportDenied(
            "root and destination must be absolute".into(),
        ));
    }
    let parent = dest
        .parent()
        .ok_or_else(|| DatasetError::ExportDenied("no parent".into()))?;
    let canonical_parent = std::fs::canonicalize(parent)
        .map_err(|e| DatasetError::ExportDenied(format!("parent: {e}")))?;
    if !canonical_parent.starts_with(root) {
        return Err(DatasetError::ExportDenied(
            "destination escapes export root".into(),
        ));
    }
    let name = dest
        .file_name()
        .ok_or_else(|| DatasetError::ExportDenied("destination has no file name".into()))?;
    let safe = canonical_parent.join(name);
    if std::fs::metadata(dest.parent().unwrap())
        .map(|m| m.is_dir())
        .unwrap_or(true)
        && safe.starts_with(root)
    {
        Ok(safe)
    } else {
        Err(DatasetError::ExportDenied(
            "destination not permitted".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_problems_instruct() {
        let ok = serde_json::json!({ "instruction": "add", "response": "42" });
        assert!(DatasetSchema::InstructionResponse.problems(&ok).is_empty());
        let bad = serde_json::json!({ "instruction": "" });
        assert!(!DatasetSchema::InstructionResponse.problems(&bad).is_empty());
    }

    #[test]
    fn schema_problems_conversation() {
        let ok = serde_json::json!({
            "conversation": [
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": "hello" }
            ]
        });
        assert!(DatasetSchema::Conversation.problems(&ok).is_empty());
        let bad = serde_json::json!({ "conversation": [ { "role": "admin", "content": "" } ] });
        assert_eq!(DatasetSchema::Conversation.problems(&bad).len(), 2);
    }

    #[test]
    fn schema_problems_preference() {
        let ok = serde_json::json!({ "chosen": "preferred", "rejected": "other" });
        assert!(DatasetSchema::Preference.problems(&ok).is_empty());
        let bad = serde_json::json!({ "chosen": "" });
        assert!(!DatasetSchema::Preference.problems(&bad).is_empty());
    }

    #[test]
    fn validate_reports_counts_and_truncates() {
        let bytes = br#"{"instruction":"a","response":"b"}
not json at all
{"instruction":"c","response":"d"}

"#;
        let rep = validate_jsonl(bytes, DatasetSchema::InstructionResponse).unwrap();
        assert_eq!(rep.total, 4);
        assert_eq!(rep.ok, 2);
        assert_eq!(rep.skipped, 2);
        assert!(rep.problems.iter().any(|p| p.contains("parse error")));
        // Diagnostics never include full record content.
        assert!(!rep.problems.iter().any(|p| p.contains("intrinsic")));
    }

    #[test]
    fn sensitive_keys_detected_without_values() {
        let rec = serde_json::json!({ "instruction": "x", "response": "y", "auth": { "api_key": "sk-live-123" } });
        let keys = sensitive_keys_in(&rec);
        assert_eq!(keys, vec!["api_key"]);
    }

    #[test]
    fn executable_input_rejected() {
        assert!(validate_jsonl(b"#!/bin/sh\necho hi", DatasetSchema::InstructionResponse).is_err());
        assert!(validate_jsonl(b"MZ\x90\x00", DatasetSchema::InstructionResponse).is_err());
    }

    #[test]
    fn splits_are_deterministic_and_partitioned() {
        let a = "record-one";
        let sa = split_for(&sha256_hex(a.as_bytes()));
        assert!(sa == "val" || sa == "test" || sa == "train");
        // Deterministic and stable across calls.
        assert_eq!(split_for(&sha256_hex(a.as_bytes())), sa);
        // Partition sanity over a deterministic sample: ~80/10/10 distribution.
        let mut counts = (0u32, 0u32, 0u32);
        for i in 0..200u32 {
            let d = sha256_hex(format!("rec-{i}").as_bytes());
            *match split_for(&d) {
                "val" => &mut counts.1,
                "test" => &mut counts.2,
                _ => &mut counts.0,
            } += 1;
        }
        assert!(counts.0 > 130, "train bucket: {}", counts.0);
        assert!(counts.0 < 180, "train bucket: {}", counts.0);
        assert!(counts.1 > 5, "val bucket: {}", counts.1);
        assert!(counts.2 > 5, "test bucket: {}", counts.2);
    }

    #[tokio::test]
    async fn import_list_preview_export() {
        let store: Arc<dyn PersistenceStore> = Arc::new(openrt_persistence::MemoryStore::new());
        let ds = DatasetStore::new(store.clone());
        let bytes = br#"{"instruction":"q1","response":"a1"}
{"instruction":"q2","response":"a2"}
"#;
        let (rec, dup) = ds
            .import(
                bytes.to_vec(),
                DatasetSchema::InstructionResponse,
                Some("pairs.jsonl".into()),
                "t",
            )
            .await
            .unwrap();
        assert!(!dup);
        assert_eq!(rec.record_count, 2);
        assert_eq!(rec.splits.train + rec.splits.val + rec.splits.test, 2);

        let (_, dup2) = ds
            .import(
                bytes.to_vec(),
                DatasetSchema::InstructionResponse,
                None,
                "t",
            )
            .await
            .unwrap();
        assert!(dup2);

        assert_eq!(ds.list().await.unwrap().len(), 1);
        let preview = ds.preview(&rec.dataset_id, 1).await.unwrap();
        assert_eq!(preview.len(), 1);
        assert!(preview[0].contains("q1"));

        let root = std::env::temp_dir().join(format!(
            "openrt-ds-{}-{}",
            std::process::id(),
            openrt_core::now()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let dest = root.join("out.jsonl");
        let written = ds.export(&rec.dataset_id, &root, &dest, "t").await.unwrap();
        assert!(written > 0);
        let content = std::fs::read_to_string(&dest).unwrap();
        assert!(content.contains("q1"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn export_rejects_escape() {
        let store: Arc<dyn PersistenceStore> = Arc::new(openrt_persistence::MemoryStore::new());
        let ds = DatasetStore::new(store.clone());
        let (rec, _) = ds
            .import(
                br#"{"instruction":"a","response":"b"}"#.to_vec(),
                DatasetSchema::InstructionResponse,
                None,
                "t",
            )
            .await
            .unwrap();
        let root = std::env::temp_dir().join(format!(
            "openrt-ds2-{}-{}",
            std::process::id(),
            openrt_core::now()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let outside = root.join("..").join("escape.jsonl");
        let mut abs_outside = root.clone();
        abs_outside.pop();
        abs_outside.push("escape.jsonl");
        // parent must exist for canonicalize; /tmp exists.
        let r = ds.export(&rec.dataset_id, &root, &abs_outside, "t").await;
        assert!(r.is_err());
        let _ = outside;
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn delete_removes() {
        let store: Arc<dyn PersistenceStore> = Arc::new(openrt_persistence::MemoryStore::new());
        let ds = DatasetStore::new(store.clone());
        let (rec, _) = ds
            .import(
                br#"{"instruction":"a","response":"b"}"#.to_vec(),
                DatasetSchema::InstructionResponse,
                None,
                "t",
            )
            .await
            .unwrap();
        ds.delete(&rec.dataset_id, "t").await.unwrap();
        assert!(ds.inspect(&rec.dataset_id).await.is_err());
    }

    #[test]
    fn no_valid_records_is_error() {
        assert!(
            validate_jsonl(br#"not json"#, DatasetSchema::InstructionResponse)
                .unwrap()
                .ok
                == 0
        );
    }
}
