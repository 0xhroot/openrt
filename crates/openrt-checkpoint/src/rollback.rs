//! File snapshot storage and rollback operations for P2.6.
//!
//! Snapshots are stored in the foundation KV store under the `rollback`
//! namespace and are always *base64-encoded* with a *sha256 checksum* of the
//! original bytes. All snapshot operations verify the checksum before
//! restoring.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use openrt_persistence::PersistenceStore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Foundation store namespace for file snapshots.
pub const ROLLBACK_NS: &str = "rollback";

/// Maximum size of a single file snapshot in bytes (safety limit).
pub const MAX_SNAPSHOT_BYTES: usize = 10 * 1024 * 1024; // 10 MiB

/// A durable reference to a file's contents at snapshot time, stored in the
/// foundation KV store (never in the checkpoint itself).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileSnapshotRecord {
    pub snapshot_id: String,
    /// Workspace-relative path of the file (canonical, forward-slashed).
    pub rel_path: String,
    pub size: u64,
    /// Hex-encoded sha256 of the original file bytes.
    pub sha256: String,
    /// Base64-encoded original file contents.
    pub content: String,
}

/// Supported rollback operations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RollbackOp {
    FileRestore {
        snapshot_id: String,
        rel_path: String,
    },
    BlackboardRestore {
        /// List of blackboard keys to restore from checkpoint snapshot.
        keys: Vec<String>,
    },
    RunMetadataRestore,
}

/// Result of a single rollback operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollbackResult {
    pub op: RollbackOp,
    pub status: RollbackStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollbackStatus {
    Done,
    Unavailable,
    Failed,
}

/// Operator authorization for a production rollback (M10.3). The operator
/// identity must be in the runtime's trusted-operator set, and policy
/// fingerprint drift between the checkpoint and the current runtime must be
/// acknowledged explicitly — a rollback never silently downgrades policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackAuthorization {
    /// Operator identity (trusted-operator set checked by the runtime).
    pub operator: String,
    /// Explicit acknowledgement that the checkpoint was created under a
    /// different (or unknown) permission policy fingerprint. Required
    /// unless the recorded fingerprint matches the runtime's current one.
    #[serde(default)]
    pub acknowledge_policy_drift: bool,
}

/// Compatibility between the checkpoint's recorded permission-policy
/// fingerprint and the runtime's current one (M10.3 rollback gate input).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyCompat {
    /// Checkpoint sealed under exactly the runtime's current policy.
    Compatible,
    /// Policy changed since the checkpoint was sealed (requires explicit
    /// drift acknowledgement — no silent policy downgrade).
    Mismatched,
    /// Checkpoint predates policy fingerprinting (legacy record).
    Unknown,
}

/// Post-restore verification: every `Done` operation re-read and compared
/// against the checkpoint's snapshot (M10.3 step 6). Mismatches are surfaced
/// in the report and the `recovery.rollback` audit record — never hidden.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RollbackVerification {
    /// Number of `Done` operations whose effects were re-read and checked.
    pub checked: usize,
    /// Human-readable mismatch descriptions (empty == fully verified).
    pub mismatches: Vec<String>,
}

/// Continuation record (M10.3): where the operator continues after this
/// rollback, persisted with the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackContinuation {
    /// The rolled-back-to checkpoint to resume from.
    pub resume_from: String,
    pub workflow_id: String,
    /// Next operator action (`"resume"`).
    pub next_action: String,
}

/// Aggregate rollback report persisted in the `rollback` store namespace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollbackReport {
    pub checkpoint_id: String,
    pub results: Vec<RollbackResult>,
    /// Policy fingerprint compatibility seen by the M10.3 gate.
    #[serde(default)]
    pub policy_compat: Option<PolicyCompat>,
    /// The authorizing operator (`None` on legacy reports).
    #[serde(default)]
    pub operator: Option<String>,
    /// Post-restore verification outcome.
    #[serde(default)]
    pub verification: Option<RollbackVerification>,
    /// History ledger immutability assertion (always `Some(true)` on
    /// reports from the authorized path — a violation fails the call).
    #[serde(default)]
    pub history_immutable: Option<bool>,
    /// Where to continue after this rollback.
    #[serde(default)]
    pub continuation: Option<RollbackContinuation>,
}

/// Store for creating and restoring file snapshots.
pub struct FileSnapshotStore {
    store: Arc<dyn PersistenceStore>,
}

impl FileSnapshotStore {
    pub fn new(store: Arc<dyn PersistenceStore>) -> Self {
        Self { store }
    }

    /// Snapshot a file from the workspace. Fails if the file is missing,
    /// escapes the workspace root, or exceeds `MAX_SNAPSHOT_BYTES`.
    pub async fn snapshot_file(
        &self,
        workspace_root: &Path,
        rel_path: &str,
    ) -> Result<FileSnapshotRecord, crate::model::CheckpointError> {
        let abs = resolve_under_root(workspace_root, rel_path)?;
        let bytes = std::fs::read(&abs).map_err(|e| crate::model::CheckpointError::Io {
            path: abs.clone(),
            source: e,
        })?;
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(crate::model::CheckpointError::Invalid(format!(
                "file `{rel_path}` is {} bytes; exceeds {} limit",
                bytes.len(),
                MAX_SNAPSHOT_BYTES
            )));
        }
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let hash = hex::encode(hasher.finalize());
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
        let snapshot_id = format!("snap-{}", uuid::Uuid::new_v4());

        let record = FileSnapshotRecord {
            snapshot_id: snapshot_id.clone(),
            rel_path: rel_path.to_string(),
            size: bytes.len() as u64,
            sha256: hash,
            content: b64,
        };
        let value =
            serde_json::to_value(&record).map_err(crate::model::CheckpointError::Serialization)?;
        self.store
            .put_record(ROLLBACK_NS, &snapshot_id, &value)
            .await
            .map_err(crate::model::CheckpointError::Persistence)?;
        Ok(record)
    }

    /// Restore a file from a snapshot, verifying the stored checksum against
    /// the current file contents.
    pub async fn rollback_file(
        &self,
        workspace_root: &Path,
        record: &FileSnapshotRecord,
    ) -> Result<(), crate::model::CheckpointError> {
        let abs = resolve_under_root(workspace_root, &record.rel_path)?;
        // Verify current file checksum matches what was snapshotted.
        let current_bytes = std::fs::read(&abs).map_err(|e| crate::model::CheckpointError::Io {
            path: abs.clone(),
            source: e,
        })?;
        let mut hasher = Sha256::new();
        hasher.update(&current_bytes);
        let current_hash = hex::encode(hasher.finalize());
        if current_hash != record.sha256 {
            return Err(crate::model::CheckpointError::Invalid(format!(
                "file `{}` checksum mismatch: current={}, snapshot={}",
                record.rel_path, current_hash, record.sha256
            )));
        }
        let decoded =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &record.content)
                .map_err(|e| {
                    crate::model::CheckpointError::Corrupt(
                        record.snapshot_id.clone(),
                        format!("base64 decode failed: {e}"),
                    )
                })?;
        // Atomic write: write to temp, rename.
        let tmp = abs.with_extension("tmp.openrt-snap");
        std::fs::write(&tmp, &decoded).map_err(|e| crate::model::CheckpointError::Io {
            path: tmp.clone(),
            source: e,
        })?;
        std::fs::rename(&tmp, &abs).map_err(|e| crate::model::CheckpointError::Io {
            path: abs.clone(),
            source: e,
        })?;
        Ok(())
    }

    /// Load a snapshot record by id.
    pub async fn load(&self, snapshot_id: &str) -> Option<FileSnapshotRecord> {
        let raw = self.store.get_record(ROLLBACK_NS, snapshot_id).await.ok()?;
        serde_json::from_value(raw?).ok()
    }

    /// Verify a restored file's on-disk bytes against its snapshot record
    /// (size + sha256). Used by the M10.3 post-restore verification step.
    pub async fn verify_restored(
        &self,
        workspace_root: &Path,
        snapshot_id: &str,
    ) -> Result<(), crate::model::CheckpointError> {
        let record = self.load(snapshot_id).await.ok_or_else(|| {
            crate::model::CheckpointError::Invalid(format!(
                "snapshot `{snapshot_id}` not found for verification"
            ))
        })?;
        let abs = resolve_under_root(workspace_root, &record.rel_path)?;
        let bytes = std::fs::read(&abs).map_err(|e| crate::model::CheckpointError::Io {
            path: abs.clone(),
            source: e,
        })?;
        if bytes.len() as u64 != record.size {
            return Err(crate::model::CheckpointError::Invalid(format!(
                "file `{}` size {} != snapshot {}",
                record.rel_path,
                bytes.len(),
                record.size
            )));
        }
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let hash = hex::encode(hasher.finalize());
        if hash != record.sha256 {
            return Err(crate::model::CheckpointError::Invalid(format!(
                "file `{}` sha256 {} != snapshot {}",
                record.rel_path, hash, record.sha256
            )));
        }
        Ok(())
    }
}

/// Resolve `rel_path` under `root`, verifying it doesn't escape.
fn resolve_under_root(
    root: &Path,
    rel_path: &str,
) -> Result<PathBuf, crate::model::CheckpointError> {
    let joined = root.join(rel_path);
    let abs = joined
        .canonicalize()
        .map_err(|e| crate::model::CheckpointError::Io {
            path: joined.clone(),
            source: e,
        })?;
    let root_abs = root
        .canonicalize()
        .map_err(|e| crate::model::CheckpointError::Io {
            path: root.to_path_buf(),
            source: e,
        })?;
    if !abs.starts_with(&root_abs) {
        return Err(crate::model::CheckpointError::Invalid(format!(
            "path `{rel_path}` escapes workspace root"
        )));
    }
    Ok(abs)
}

/// Build a list of rollback operations for a checkpoint.
pub fn plan_rollbacks(checkpoint: &crate::model::Checkpoint) -> Vec<RollbackOp> {
    let mut ops = Vec::new();
    for art in &checkpoint.artifact_refs {
        if art.kind == "file_snapshot" {
            if let (Some(sid), Some(path)) = (&art.snapshot_id, &art.path) {
                ops.push(RollbackOp::FileRestore {
                    snapshot_id: sid.clone(),
                    rel_path: path.clone(),
                });
            }
        }
    }
    // Blackboard rollback: all output.* keys in checkpoint's blackboard.
    let mut bb_keys: Vec<String> = checkpoint
        .blackboard
        .entries
        .iter()
        .map(|(k, _)| k)
        .filter(|k| k.starts_with("output."))
        .cloned()
        .collect();
    bb_keys.sort();
    if !bb_keys.is_empty() {
        ops.push(RollbackOp::BlackboardRestore { keys: bb_keys });
    }
    // Run metadata rollback.
    if checkpoint.execution.run_snapshot.is_some() {
        ops.push(RollbackOp::RunMetadataRestore);
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn unique_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("openrt-checkpoint-{tag}-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn resolve_under_root_ok() {
        let tmp = unique_dir("res-ok");
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("hello.txt"), b"hello").unwrap();
        let p = resolve_under_root(&tmp, "hello.txt").unwrap();
        assert!(p.exists());
    }

    #[test]
    fn resolve_under_root_escape_rejected() {
        let tmp = unique_dir("res-escape");
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("hello.txt"), b"hello").unwrap();
        let result = resolve_under_root(&tmp, "../etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn plan_rollbacks_from_checkpoint() {
        use crate::model::*;
        use openrt_blackboard::BlackboardSnapshot;
        use openrt_core::{now, CheckpointId};
        use std::collections::HashMap;

        let mut bb = BlackboardSnapshot::default();
        bb.entries
            .push(("output.plan".into(), serde_json::json!("done")));
        bb.entries
            .push(("other.key".into(), serde_json::json!("irrelevant")));

        let cp = Checkpoint {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            checkpoint_id: CheckpointId::new(),
            session_id: openrt_core::SessionId::new(),
            task_id: None,
            workflow_id: "wf".into(),
            created_at: now(),
            kind: CheckpointKind::AfterNode,
            status: CheckpointStatus::Running,
            node_states: vec![],
            completed: vec![],
            pending: vec![],
            failed: vec![],
            uncertain: vec![],
            current_node: None,
            dependency_state: DependencyState::default(),
            retry_counters: HashMap::new(),
            blackboard: bb,
            artifact_refs: vec![ArtifactRef {
                kind: "file_snapshot".into(),
                key: None,
                path: Some("src/main.rs".into()),
                snapshot_id: Some("snap-123".into()),
            }],
            execution: CheckpointExecution {
                run_snapshot: Some(serde_json::json!({"old": "data"})),
                ..Default::default()
            },
            refs: crate::model::RunRefs::default(),
            digest: None,
        };
        let ops = plan_rollbacks(&cp);
        assert!(ops.contains(&RollbackOp::FileRestore {
            snapshot_id: "snap-123".into(),
            rel_path: "src/main.rs".into(),
        }));
        assert!(ops.contains(&RollbackOp::BlackboardRestore {
            keys: vec!["output.plan".into()]
        }));
        assert!(ops.contains(&RollbackOp::RunMetadataRestore));
    }
}
