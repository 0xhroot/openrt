//! P2.6 checkpoint data model, schema, and validation.

use std::collections::HashMap;

use openrt_blackboard::BlackboardSnapshot;
use openrt_core::{CheckpointId, ExecutionId, SessionId, TaskId, Timestamp};
use serde::{Deserialize, Serialize};

/// Persists only this schema version; a future reader rejects unknown versions
/// (ADR-006 versioned surface, closed-world assumption for safety-critical state).
pub const CHECKPOINT_SCHEMA_VERSION: u32 = 1;

// ── enums ────────────────────────────────────────────────────────────────────

/// When in the execution lifecycle this checkpoint was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointKind {
    /// Before the workflow's first wave (resume from beginning).
    Initial,
    /// After a specific node's result was durably recorded.
    AfterNode,
    /// After a wave of nodes was spawned but before any of them finished —
    /// durably marks in-flight work as `Running` (Phase 10 crash safety).
    Scheduled,
    /// Final state captured after the workflow terminated.
    Final,
}

/// Overall workflow status at checkpoint time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// Per-node lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    /// Not yet eligible (deps unsatisfied) or not reached.
    Pending,
    /// Node was running when the snapshot was captured — its outcome is
    /// **unknown** (mission-critical: must not be blindly repeated).
    Running,
    /// Node completed successfully or was skipped by `only_if`.
    Completed,
    /// Node was skipped by its `only_if` condition.
    Skipped,
    /// Node failed and exhausted retries within this run.
    Failed,
}

/// Operator-provided classification of a node's side-effect profile.
/// `Unknown` is the default — unknown side effects are *never* automatically
/// replayed on recovery (mission-critical safety invariant).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeEffect {
    /// Only reads; safe to re-execute at any time.
    ReadOnly,
    /// Identical result on repeated execution (e.g. `echo`, `system.info`).
    Idempotent,
    /// A well-defined inverse exists and was captured as a snapshot.
    Reversible,
    /// Non-idempotent but result was persisted in the blackboard snapshot.
    NonIdempotent,
    /// Side effect outside OpenRT's managed state (terminal, fs, browser…).
    Irreversible,
    /// Unknown (the safe default — never auto-rerun).
    Unknown,
}

/// Why a failed node was or was not classified as retryable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retryable {
    Yes,
    ExhaustedAttempts,
    NonRetryableCategory,
    UnknownCategory,
}

// ── node status ──────────────────────────────────────────────────────────────

/// Durable state of one workflow node, persisted inside a [`Checkpoint`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeStatus {
    pub step: String,
    pub state: NodeState,
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Serialized [`FailureCategory`] when `state == Failed`, empty string
    /// otherwise — keeps serde simple while making the category durable.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub failure_category: String,
    #[serde(default = "default_effect")]
    pub effect: NodeEffect,
}

fn default_effect() -> NodeEffect {
    NodeEffect::Unknown
}

impl NodeStatus {
    /// Whether this node can be safely scheduled for re-execution (retry or
    /// resume) based on its effect classification.
    pub fn can_retry(&self, policy_retryable: bool) -> bool {
        if !policy_retryable {
            return false;
        }
        self.state == NodeState::Failed || self.state == NodeState::Running
    }
}

// ── dependency snapshot ───────────────────────────────────────────────────────

/// Snapshot of the DAG's dependency state at checkpoint time. The `ready`
/// list records nodes that were eligible to run (all deps satisfied) but had
/// not yet completed. `deps_by_step` mirrors the original workflow `depends_on`
/// so the resume path can reconstruct ordering without loading the DAG.
#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DependencyState {
    /// Nodes that were ready at checkpoint time (deps ⊆ completed ∪ skipped).
    #[serde(default)]
    pub ready: Vec<String>,
    /// Mirrors `Workflow.steps[].depends_on`.
    #[serde(default)]
    pub deps_by_step: HashMap<String, Vec<String>>,
}

// ── artifact references ───────────────────────────────────────────────────────

/// A reference to a durable artifact (never a blob).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// `"blackboard_output"`, `"run_report"`, `"file_snapshot"`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
}

// ── execution metadata ───────────────────────────────────────────────────────

/// Non-secret, non-payload execution bookkeeping stored in a checkpoint.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CheckpointExecution {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<ExecutionId>,
    /// Where the latest `WorkflowRunReport` is persisted (`runs/<wf>.json`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_report_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_checkpoint_id: Option<CheckpointId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Embedded run metadata snapshot (see rollback docs): `Some` when
    /// an `Initial` checkpoint captured the previous completed run's
    /// `WorkflowRunReport` before the current execution replaced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_snapshot: Option<serde_json::Value>,
}

// ── phase 10 run provenance ──────────────────────────────────────────────────

/// Authorities a run was pinned to when this checkpoint was created
/// (Phase 10). Every field is optional so pre-Phase-10 records deserialize;
/// a missing value stays **honestly missing** — never silently backfilled
/// from current configuration (fail-closed provenance).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunRefs {
    /// Logical run this checkpoint belongs to: survives resume/rollback as
    /// one chain (the resumed run inherits the checkpoint's `run_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Append-only history ledger seq this checkpoint synced to (the
    /// time-machine anchor; `None` when history was unavailable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_cursor: Option<u64>,
    /// Fingerprint of the effective permission policy rules at run start
    /// (sha256 of the serialized rules, truncated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_version: Option<String>,
    /// Agent genome the session was created under (Phase 10 M10.7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genome_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genome_version: Option<String>,
    /// Digest of the active skill set at checkpoint time (M10.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_digest: Option<String>,
    /// Durable memory artifact keys referenced by this run (M5.3 refs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memory_refs: Vec<String>,
}

impl RunRefs {
    pub fn is_empty(&self) -> bool {
        self.run_id.is_none()
            && self.history_cursor.is_none()
            && self.policy_version.is_none()
            && self.genome_id.is_none()
            && self.genome_version.is_none()
            && self.skill_digest.is_none()
            && self.memory_refs.is_empty()
    }
}

// ── checkpoint ───────────────────────────────────────────────────────────────

/// A durable, validated, serializable point-in-time snapshot of a workflow
/// run. This is the **only** artifact that persists checkpoint state; there
/// is no hidden or parallel store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub schema_version: u32,
    pub checkpoint_id: CheckpointId,
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    pub workflow_id: String,
    pub created_at: Timestamp,
    pub kind: CheckpointKind,
    pub status: CheckpointStatus,
    pub node_states: Vec<NodeStatus>,
    /// Nodes that completed or were skipped (subset of `node_states`).
    pub completed: Vec<String>,
    /// Nodes eligible to run whose deps are satisfied.
    pub pending: Vec<String>,
    /// Nodes that failed and exhausted retries (or had non-retryable errors).
    pub failed: Vec<String>,
    /// Nodes whose outcome is uncertain (was `Running` at snapshot time).
    #[serde(default)]
    pub uncertain: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_node: Option<String>,
    pub dependency_state: DependencyState,
    /// Step → cumulative attempts (this run; resumed runs add to the total).
    #[serde(default)]
    pub retry_counters: HashMap<String, u32>,
    /// Deterministic, sorted copy of all execution artifacts (blackboard
    /// `output.*` keys, etc.) — authoritative source for resume.
    pub blackboard: BlackboardSnapshot,
    #[serde(default)]
    pub artifact_refs: Vec<ArtifactRef>,
    pub execution: CheckpointExecution,
    /// Phase 10 run provenance (see [`RunRefs`]) — empty on legacy records.
    #[serde(default, skip_serializing_if = "RunRefs::is_empty")]
    pub refs: RunRefs,
    /// Integrity digest over the canonical serialization (Phase 10): set by
    /// [`Checkpoint::seal`], verified on every load. `None` only on records
    /// that never passed through the manager's write path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

impl Checkpoint {
    /// Validate internal consistency (list coherence, schema version).
    pub fn validate(&self) -> Result<(), CheckpointError> {
        if self.schema_version != CHECKPOINT_SCHEMA_VERSION {
            return Err(CheckpointError::UnsupportedVersion {
                got: self.schema_version,
                expected: CHECKPOINT_SCHEMA_VERSION,
            });
        }
        let in_state: std::collections::HashSet<&str> =
            self.node_states.iter().map(|n| n.step.as_str()).collect();
        let all_lists = self
            .completed
            .iter()
            .chain(self.pending.iter())
            .chain(self.failed.iter())
            .chain(self.uncertain.iter());
        for name in all_lists {
            if !in_state.contains(name.as_str()) {
                return Err(CheckpointError::Invalid(format!(
                    "list entry `{name}` missing from node_states"
                )));
            }
        }
        // retry_counters keys ⊆ node_states steps
        for key in self.retry_counters.keys() {
            if !in_state.contains(key.as_str()) {
                return Err(CheckpointError::Invalid(format!(
                    "retry_counters key `{key}` missing from node_states"
                )));
            }
        }
        // Phase 10: list ↔ state coherence. An in-flight (`Running`) node
        // must appear in `uncertain` (and vice versa) — this is the durable
        // bridge that makes `UncertainPolicy` reachable after a crash;
        // completed/pending/failed lists must match their node states so a
        // tampered or half-written record fails closed at load.
        let state_of = |step: &str| -> Option<NodeState> {
            self.node_states
                .iter()
                .find(|n| n.step == step)
                .map(|n| n.state)
        };
        for name in &self.uncertain {
            if state_of(name) != Some(NodeState::Running) {
                return Err(CheckpointError::Invalid(format!(
                    "uncertain entry `{name}` is not Running in node_states"
                )));
            }
        }
        for ns in &self.node_states {
            let in_list = |list: &[String]| list.iter().any(|s| s == &ns.step);
            match ns.state {
                NodeState::Running if !in_list(&self.uncertain) => {
                    return Err(CheckpointError::Invalid(format!(
                        "Running node `{}` missing from uncertain",
                        ns.step
                    )));
                }
                NodeState::Failed if !in_list(&self.failed) => {
                    return Err(CheckpointError::Invalid(format!(
                        "Failed node `{}` missing from failed",
                        ns.step
                    )));
                }
                NodeState::Pending if !in_list(&self.pending) => {
                    return Err(CheckpointError::Invalid(format!(
                        "Pending node `{}` missing from pending",
                        ns.step
                    )));
                }
                NodeState::Completed | NodeState::Skipped => {
                    if !in_list(&self.completed) {
                        return Err(CheckpointError::Invalid(format!(
                            "Completed/Skipped node `{}` missing from completed",
                            ns.step
                        )));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Canonical integrity digest: sha256 over the serialization with the
    /// `digest` field cleared (so sealing is idempotent and verifiable).
    ///
    /// The path goes through [`serde_json::to_value`] first: all maps are
    /// normalized to sorted-key JSON objects there, so `HashMap` iteration
    /// order (random hasher seeds) can never change the digest between seal
    /// and verify.
    pub fn compute_digest(&self) -> Result<String, CheckpointError> {
        let mut cleared = self.clone();
        cleared.digest = None;
        let value = serde_json::to_value(&cleared)?;
        let bytes = serde_json::to_vec(&value)?;
        Ok(openrt_core::sha256_hex(&bytes))
    }

    /// Seal this checkpoint with its integrity digest (called on the write
    /// path — every record leaving [`CheckpointManager::create`] is sealed).
    pub fn seal(&mut self) -> Result<(), CheckpointError> {
        let digest = self.compute_digest()?;
        self.digest = Some(digest);
        Ok(())
    }

    /// Fail-closed integrity verification: a missing or mismatched digest is
    /// corruption, never a warning.
    pub fn verify_integrity(&self) -> Result<(), CheckpointError> {
        let Some(stored) = &self.digest else {
            return Err(CheckpointError::Corrupt(
                self.checkpoint_id.to_string(),
                "missing integrity digest".into(),
            ));
        };
        let computed = self.compute_digest()?;
        if computed != *stored {
            return Err(CheckpointError::Corrupt(
                self.checkpoint_id.to_string(),
                format!("digest mismatch (stored {stored}, computed {computed})"),
            ));
        }
        Ok(())
    }

    /// Build a summary [`CheckpointMetadata`] for listing/inspection.
    pub fn metadata(&self) -> CheckpointMetadata {
        let nodes_total = self.node_states.len();
        let nodes_completed = self.completed.len();
        CheckpointMetadata {
            checkpoint_id: self.checkpoint_id,
            session_id: self.session_id,
            task_id: self.task_id,
            workflow_id: self.workflow_id.clone(),
            created_at: self.created_at,
            kind: self.kind,
            status: self.status,
            nodes_total,
            nodes_completed,
            note: self.execution.note.clone(),
            run_id: self.refs.run_id.clone(),
        }
    }
}

// ── metadata (lightweight listing view) ───────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointMetadata {
    pub checkpoint_id: CheckpointId,
    pub session_id: SessionId,
    #[serde(default)]
    pub task_id: Option<TaskId>,
    pub workflow_id: String,
    pub created_at: Timestamp,
    pub kind: CheckpointKind,
    pub status: CheckpointStatus,
    pub nodes_total: usize,
    pub nodes_completed: usize,
    #[serde(default)]
    pub note: Option<String>,
    /// Phase 10: logical run chain this checkpoint belongs to.
    #[serde(default)]
    pub run_id: Option<String>,
}

// ── error ────────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error("checkpoint {0} not found")]
    NotFound(String),
    #[error("unsupported checkpoint schema version {got} (expected {expected})")]
    UnsupportedVersion { got: u32, expected: u32 },
    #[error("corrupt record `{0}`: {1}")]
    Corrupt(String, String),
    #[error("checkpoint load failed for {0}: {1}")]
    LoadFailed(String, String),
    #[error("checkpoint references unknown workflow step `{0}`")]
    UnknownStep(String),
    #[error("invalid checkpoint: {0}")]
    Invalid(String),
    #[error("persistence error: {0}")]
    Persistence(#[from] openrt_persistence::PersistenceError),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("halting action: {0}")]
    Halted(String),
    #[error("recovery needs operator intervention: {0}")]
    NeedsIntervention(String),
    #[error("rollback for `{0}` is unavailable: {1}")]
    RollbackUnavailable(String, String),
    #[error("io error at {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("recovery refused: {0}")]
    Refused(String),
}

impl CheckpointError {
    /// Whether this error implies data corruption that should be surfaced
    /// rather than retried.
    pub fn is_corruption(&self) -> bool {
        matches!(
            self,
            Self::UnsupportedVersion { .. } | Self::Corrupt(..) | Self::Invalid(..)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_core::{SessionId, TaskId};

    fn mk_checkpoint(status: CheckpointStatus) -> Checkpoint {
        Checkpoint {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            checkpoint_id: CheckpointId::new(),
            session_id: SessionId::new(),
            task_id: Some(TaskId::new()),
            workflow_id: "wf-test".into(),
            created_at: openrt_core::now(),
            kind: CheckpointKind::Initial,
            status,
            node_states: vec![
                NodeStatus {
                    step: "plan".into(),
                    state: NodeState::Completed,
                    attempts: 1,
                    artifact_key: Some("output.plan".into()),
                    error: None,
                    failure_category: String::new(),
                    effect: NodeEffect::ReadOnly,
                },
                NodeStatus {
                    step: "code".into(),
                    state: NodeState::Pending,
                    attempts: 0,
                    artifact_key: None,
                    error: None,
                    failure_category: String::new(),
                    effect: NodeEffect::Unknown,
                },
            ],
            completed: vec!["plan".into()],
            pending: vec!["code".into()],
            failed: vec![],
            uncertain: vec![],
            current_node: None,
            dependency_state: DependencyState::default(),
            retry_counters: HashMap::from([("plan".into(), 1)]),
            blackboard: BlackboardSnapshot::default(),
            artifact_refs: vec![],
            execution: CheckpointExecution::default(),
            refs: RunRefs::default(),
            digest: None,
        }
    }

    #[test]
    fn round_trip() {
        let cp = mk_checkpoint(CheckpointStatus::Running);
        let json = serde_json::to_string(&cp).unwrap();
        let back: Checkpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(cp, back);
    }

    #[test]
    fn legacy_record_without_phase10_fields_deserializes() {
        let cp = mk_checkpoint(CheckpointStatus::Running);
        let mut value = serde_json::to_value(&cp).unwrap();
        value.as_object_mut().unwrap().remove("refs");
        value.as_object_mut().unwrap().remove("digest");
        let back: Checkpoint = serde_json::from_value(value).unwrap();
        assert!(back.refs.is_empty());
        assert!(back.digest.is_none());
        // A legacy (unsealed) record fails integrity closed — it must be
        // re-written through the manager before it can be trusted.
        assert!(matches!(
            back.verify_integrity(),
            Err(CheckpointError::Corrupt(..))
        ));
    }

    #[test]
    fn seal_then_verify_roundtrip_and_tamper_fails_closed() {
        let mut cp = mk_checkpoint(CheckpointStatus::Running);
        cp.seal().unwrap();
        assert!(cp.digest.is_some());
        cp.verify_integrity().unwrap();
        // Sealing twice is idempotent (digest excludes itself).
        let d = cp.digest.clone();
        cp.seal().unwrap();
        assert_eq!(cp.digest, d);
        // Tampering with provenance breaks the digest even though it passes
        // list-coherence validation.
        let mut tampered = cp.clone();
        tampered.refs.policy_version = Some("sha256:deadbeef".into());
        assert!(matches!(
            tampered.verify_integrity(),
            Err(CheckpointError::Corrupt(..))
        ));
    }

    #[test]
    fn validate_requires_running_nodes_in_uncertain() {
        let mut cp = mk_checkpoint(CheckpointStatus::Running);
        cp.node_states[1].state = NodeState::Running; // `code` is Running…
                                                      // …but not listed as uncertain → corruption, fail closed.
        assert!(matches!(
            cp.validate(),
            Err(CheckpointError::Invalid(msg)) if msg.contains("uncertain")
        ));
        cp.uncertain.push("code".into());
        assert!(cp.validate().is_ok());
    }

    #[test]
    fn validate_requires_failed_nodes_in_failed_list() {
        let mut cp = mk_checkpoint(CheckpointStatus::Failed);
        cp.node_states[1].state = NodeState::Failed;
        assert!(matches!(
            cp.validate(),
            Err(CheckpointError::Invalid(msg)) if msg.contains("failed")
        ));
        cp.pending.clear();
        cp.failed.push("code".into());
        assert!(cp.validate().is_ok());
    }

    #[test]
    fn run_refs_serialize_only_when_present() {
        let mut cp = mk_checkpoint(CheckpointStatus::Running);
        let json = serde_json::to_value(&cp).unwrap();
        assert!(json.get("refs").is_none());
        cp.refs = RunRefs {
            run_id: Some("run-1".into()),
            history_cursor: Some(7),
            policy_version: Some("sha256:abc".into()),
            genome_id: Some("g1".into()),
            genome_version: Some("1".into()),
            skill_digest: Some("s1".into()),
            memory_refs: vec!["memory.fact.1".into()],
        };
        let json = serde_json::to_value(&cp).unwrap();
        assert_eq!(json["refs"]["run_id"], "run-1");
        assert_eq!(json["refs"]["history_cursor"], 7);
        let back: Checkpoint = serde_json::from_value(json).unwrap();
        assert_eq!(back.refs, cp.refs);
    }

    #[test]
    fn validate_passes_on_consistent_checkpoint() {
        let cp = mk_checkpoint(CheckpointStatus::Running);
        assert!(cp.validate().is_ok());
    }

    #[test]
    fn validate_rejects_inconsistent_list_entry() {
        let mut cp = mk_checkpoint(CheckpointStatus::Running);
        cp.pending.push("nonexistent".into());
        assert!(cp.validate().is_err());
    }

    #[test]
    fn validate_rejects_wrong_version() {
        let mut cp = mk_checkpoint(CheckpointStatus::Completed);
        cp.schema_version = 999;
        assert!(matches!(
            cp.validate(),
            Err(CheckpointError::UnsupportedVersion {
                got: 999,
                expected: 1
            })
        ));
    }

    #[test]
    fn validate_rejects_retry_counter_for_unknown_step() {
        let mut cp = mk_checkpoint(CheckpointStatus::Running);
        cp.retry_counters.insert("ghost".into(), 1);
        assert!(cp.validate().is_err());
    }

    #[test]
    fn metadata_derivation() {
        let cp = mk_checkpoint(CheckpointStatus::Completed);
        let meta = cp.metadata();
        assert_eq!(meta.nodes_total, 2);
        assert_eq!(meta.nodes_completed, 1);
        assert_eq!(meta.status, CheckpointStatus::Completed);
    }
}
