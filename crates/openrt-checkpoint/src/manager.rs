//! Checkpoint persistence manager — the **only** surface that touches the
//! foundation store for checkpoint records. Schema validation is performed on
//! every load; corruption surfaces as [`CheckpointError::Corrupt`].

use std::sync::Arc;

use openrt_blackboard::BlackboardSnapshot;
use openrt_core::{CheckpointId, SessionId, TaskId};
use openrt_persistence::{PersistenceError, PersistenceStore};

use crate::model::{
    ArtifactRef, Checkpoint, CheckpointError, CheckpointExecution, CheckpointKind,
    CheckpointMetadata, CheckpointStatus, DependencyState, NodeState, NodeStatus, RunRefs,
    CHECKPOINT_SCHEMA_VERSION,
};

/// Foundation store namespace for checkpoint records.
pub const CHECKPOINT_NS: &str = "checkpoints";

/// Foundation store namespace for dry-run reports (keyed by checkpoint id).
pub const DRYRUN_NS: &str = "dryrun";

/// Options for building a checkpoint from live engine state. The runtime
/// supplies these; the manager assembles the durable record.
pub struct CheckpointBuild {
    pub workflow_id: String,
    pub session_id: SessionId,
    pub task_id: Option<TaskId>,
    pub kind: CheckpointKind,
    pub status: CheckpointStatus,
    pub execution: CheckpointExecution,
    pub previous_checkpoint_id: Option<CheckpointId>,
    pub note: Option<String>,
    /// Phase 10 run provenance pinned to every checkpoint of this run.
    pub refs: RunRefs,
}

/// CheckpointManager — CRUD + schema validation for checkpoint records.
pub struct CheckpointManager {
    store: Arc<dyn PersistenceStore>,
}

impl CheckpointManager {
    pub fn new(store: Arc<dyn PersistenceStore>) -> Self {
        Self { store }
    }

    /// Persist a checkpoint, performing schema validation first, then seal
    /// it with its integrity digest (every record leaving this write path is
    /// verifiable on load — Phase 10 fail-closed integrity).
    pub async fn create(&self, cp: &Checkpoint) -> Result<CheckpointId, CheckpointError> {
        cp.validate()?;
        let mut sealed = cp.clone();
        sealed.seal()?;
        let id = sealed.checkpoint_id.to_string();
        let value = serde_json::to_value(&sealed)?;
        self.store.put_record(CHECKPOINT_NS, &id, &value).await?;
        Ok(cp.checkpoint_id)
    }

    /// Load a checkpoint by id. Validates the record on load.
    pub async fn load(&self, id: &CheckpointId) -> Result<Checkpoint, CheckpointError> {
        let id_str = id.to_string();
        let raw = self
            .store
            .get_record(CHECKPOINT_NS, &id_str)
            .await
            .map_err(|e| match e {
                PersistenceError::Io { .. } => CheckpointError::Persistence(e),
                _ => CheckpointError::LoadFailed(id_str.clone(), e.to_string()),
            })?;
        let Some(raw) = raw else {
            return Err(CheckpointError::NotFound(id_str.clone()));
        };
        let cp: Checkpoint = serde_json::from_value(raw).map_err(|e| {
            CheckpointError::Corrupt(id_str.clone(), format!("deserialization failed: {e}"))
        })?;
        cp.validate()?;
        cp.verify_integrity()?;
        Ok(cp)
    }

    /// List checkpoint metadata. Optionally filter by workflow id.
    pub async fn list(
        &self,
        workflow_id: Option<&str>,
    ) -> Result<Vec<CheckpointMetadata>, CheckpointError> {
        let records = self.store.list_records(CHECKPOINT_NS).await?;
        let mut result = Vec::new();
        for (_key, raw) in records {
            let cp: Checkpoint = serde_json::from_value(raw).map_err(|e| {
                CheckpointError::Corrupt("unknown".into(), format!("deserialization failed: {e}"))
            })?;
            if let Some(wf) = workflow_id {
                if cp.workflow_id != wf {
                    continue;
                }
            }
            result.push(cp.metadata());
        }
        result.sort_by_key(|c| c.created_at);
        Ok(result)
    }

    /// Delete a checkpoint record.
    pub async fn delete(&self, id: &CheckpointId) -> Result<(), CheckpointError> {
        let id_str = id.to_string();
        self.store
            .delete_record(CHECKPOINT_NS, &id_str)
            .await
            .map_err(CheckpointError::Persistence)
    }

    /// Return the most recent checkpoint for a workflow, if any.
    pub async fn latest(&self, workflow_id: &str) -> Option<Checkpoint> {
        let list = self.store.list_records(CHECKPOINT_NS).await.ok()?;
        let mut best: Option<Checkpoint> = None;
        for (_key, raw) in list {
            if let Ok(cp) = serde_json::from_value::<Checkpoint>(raw) {
                if cp.workflow_id == workflow_id {
                    match &best {
                        Some(b) if b.created_at >= cp.created_at => {}
                        _ => best = Some(cp),
                    }
                }
            }
        }
        best
    }

    /// Materialize a new checkpoint from engine state. This is the canonical
    /// assembly point — runtime callers pass in their snapshot and the
    /// manager produces a validated [`Checkpoint`].
    pub fn materialize(
        &self,
        build: CheckpointBuild,
        workflow_steps: &[String],
        deps_by_step: std::collections::HashMap<String, Vec<String>>,
        node_states: Vec<NodeStatus>,
        blackboard: BlackboardSnapshot,
        artifact_refs: Vec<ArtifactRef>,
    ) -> Result<Checkpoint, CheckpointError> {
        let mut completed = Vec::new();
        let mut pending = Vec::new();
        let mut failed = Vec::new();
        let mut uncertain = Vec::new();
        let mut ready = Vec::new();
        let mut retry_counters = std::collections::HashMap::new();

        let steps_set: std::collections::HashSet<&str> =
            workflow_steps.iter().map(|s| s.as_str()).collect();

        // Validate: every node_state step must be in the workflow.
        for ns in &node_states {
            if !steps_set.contains(ns.step.as_str()) {
                return Err(CheckpointError::UnknownStep(ns.step.clone()));
            }
        }

        // Compute ready: steps whose deps ⊆ completed ∪ skipped.
        let completed_set: std::collections::HashSet<&str> = node_states
            .iter()
            .filter(|n| matches!(n.state, NodeState::Completed | NodeState::Skipped))
            .map(|n| n.step.as_str())
            .collect();

        for step in workflow_steps {
            if completed_set.contains(step.as_str()) {
                continue;
            }
            let deps = deps_by_step.get(step).map(|v| v.as_slice()).unwrap_or(&[]);
            if deps.iter().all(|d| completed_set.contains(d.as_str())) {
                ready.push(step.clone());
            }
        }
        ready.sort();

        for ns in &node_states {
            retry_counters.insert(ns.step.clone(), ns.attempts);
            match ns.state {
                NodeState::Completed | NodeState::Skipped => completed.push(ns.step.clone()),
                NodeState::Pending => pending.push(ns.step.clone()),
                NodeState::Failed => failed.push(ns.step.clone()),
                NodeState::Running => uncertain.push(ns.step.clone()),
            }
        }

        completed.sort();
        pending.sort();
        failed.sort();
        uncertain.sort();

        let mut cp = Checkpoint {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            checkpoint_id: CheckpointId::new(),
            session_id: build.session_id,
            task_id: build.task_id,
            workflow_id: build.workflow_id,
            created_at: openrt_core::now(),
            kind: build.kind,
            status: build.status,
            node_states,
            completed,
            pending,
            failed,
            uncertain,
            current_node: None,
            dependency_state: DependencyState {
                ready,
                deps_by_step,
            },
            retry_counters,
            blackboard,
            artifact_refs,
            execution: CheckpointExecution {
                // Carry the caller's execution payload through — dropping it
                // made the production RunMetadataRestore path unreachable
                // (run_snapshot never persisted).
                execution_id: build.execution.execution_id,
                run_report_ref: build.execution.run_report_ref,
                previous_checkpoint_id: build.previous_checkpoint_id,
                note: build.note,
                run_snapshot: build.execution.run_snapshot,
            },
            refs: build.refs,
            digest: None,
        };
        cp.seal()?;
        Ok(cp)
    }

    /// Persist a dry-run report keyed by checkpoint id.
    pub async fn save_dry_run(
        &self,
        checkpoint_id: &CheckpointId,
        report: &crate::plan::DryRunReport,
    ) -> Result<(), CheckpointError> {
        let id_str = checkpoint_id.to_string();
        let value = serde_json::to_value(report)?;
        self.store
            .put_record(DRYRUN_NS, &id_str, &value)
            .await
            .map_err(CheckpointError::Persistence)
    }

    /// Load a dry-run report for a given checkpoint.
    pub async fn load_dry_run(
        &self,
        checkpoint_id: &CheckpointId,
    ) -> Option<crate::plan::DryRunReport> {
        let id_str = checkpoint_id.to_string();
        let raw = self.store.get_record(DRYRUN_NS, &id_str).await.ok()?;
        serde_json::from_value(raw?).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NodeEffect;
    use openrt_blackboard::BlackboardSnapshot;
    use openrt_core::{now, SessionId};
    use std::collections::HashMap;

    fn unique_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("openrt-checkpoint-{tag}-{}", uuid::Uuid::new_v4()))
    }

    async fn make_manager(tag: &str) -> CheckpointManager {
        let dir = unique_dir(tag);
        let store = Arc::new(
            openrt_persistence::LocalFileStore::open(&dir)
                .await
                .unwrap(),
        );
        CheckpointManager::new(store)
    }

    #[tokio::test]
    async fn create_and_load() {
        let mgr = make_manager("mgr").await;
        let cp = mk_test_checkpoint();
        let id = mgr.create(&cp).await.unwrap();
        let loaded = mgr.load(&id).await.unwrap();
        assert_eq!(loaded.checkpoint_id, id);
        assert_eq!(loaded.workflow_id, "wf-test");
    }

    #[tokio::test]
    async fn list_filter_by_workflow() {
        let mgr = make_manager("mgr").await;
        let mut cp1 = mk_test_checkpoint();
        cp1.workflow_id = "wf-a".into();
        mgr.create(&cp1).await.unwrap();
        let mut cp2 = mk_test_checkpoint();
        cp2.workflow_id = "wf-b".into();
        mgr.create(&cp2).await.unwrap();
        let list_a = mgr.list(Some("wf-a")).await.unwrap();
        assert_eq!(list_a.len(), 1);
        let list_all = mgr.list(None).await.unwrap();
        assert_eq!(list_all.len(), 2);
    }

    #[tokio::test]
    async fn delete_checkpoint() {
        let mgr = make_manager("mgr").await;
        let cp = mk_test_checkpoint();
        let id = mgr.create(&cp).await.unwrap();
        mgr.delete(&id).await.unwrap();
        assert!(matches!(
            mgr.load(&id).await,
            Err(CheckpointError::NotFound(..))
        ));
    }

    #[tokio::test]
    async fn latest_returns_most_recent() {
        let mgr = make_manager("mgr").await;
        let mut cp1 = mk_test_checkpoint();
        cp1.workflow_id = "wf-latest".into();
        let id1 = mgr.create(&cp1).await.unwrap();
        let mut cp2 = mk_test_checkpoint();
        cp2.workflow_id = "wf-latest".into();
        let id2 = mgr.create(&cp2).await.unwrap();
        let latest = mgr.latest("wf-latest").await.unwrap();
        assert_eq!(latest.checkpoint_id, id2);
        assert_ne!(latest.checkpoint_id, id1);
    }

    #[tokio::test]
    async fn materialize_produces_consistent_checkpoint() {
        let mgr = make_manager("mgr").await;
        let steps = vec!["a".into(), "b".into()];
        let deps: HashMap<String, Vec<String>> =
            vec![("b".into(), vec!["a".into()])].into_iter().collect();
        let node_states = vec![
            NodeStatus {
                step: "a".into(),
                state: NodeState::Completed,
                attempts: 1,
                artifact_key: Some("output.a".into()),
                error: None,
                failure_category: String::new(),
                effect: NodeEffect::ReadOnly,
            },
            NodeStatus {
                step: "b".into(),
                state: NodeState::Pending,
                attempts: 0,
                artifact_key: None,
                error: None,
                failure_category: String::new(),
                effect: NodeEffect::Unknown,
            },
        ];
        let build = CheckpointBuild {
            workflow_id: "wf-mat".into(),
            session_id: SessionId::new(),
            task_id: None,
            kind: CheckpointKind::AfterNode,
            status: CheckpointStatus::Running,
            execution: CheckpointExecution::default(),
            previous_checkpoint_id: None,
            note: Some("test".into()),
            refs: RunRefs::default(),
        };
        let cp = mgr
            .materialize(
                build,
                &steps,
                deps,
                node_states,
                BlackboardSnapshot::default(),
                vec![],
            )
            .unwrap();
        assert!(cp.validate().is_ok());
        assert_eq!(cp.completed, vec!["a"]);
        assert_eq!(cp.pending, vec!["b"]);
        assert!(cp.failed.is_empty());
        assert!(cp.uncertain.is_empty());
        assert!(cp.execution.note.as_deref() == Some("test"));
        // persist and reload
        let id = mgr.create(&cp).await.unwrap();
        let loaded = mgr.load(&id).await.unwrap();
        assert_eq!(loaded, cp);
    }

    fn mk_test_checkpoint() -> Checkpoint {
        Checkpoint {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            checkpoint_id: CheckpointId::new(),
            session_id: SessionId::new(),
            task_id: None,
            workflow_id: "wf-test".into(),
            created_at: now(),
            kind: CheckpointKind::Initial,
            status: CheckpointStatus::Running,
            node_states: vec![NodeStatus {
                step: "plan".into(),
                state: NodeState::Pending,
                attempts: 0,
                artifact_key: None,
                error: None,
                failure_category: String::new(),
                effect: NodeEffect::Unknown,
            }],
            completed: vec![],
            pending: vec!["plan".into()],
            failed: vec![],
            uncertain: vec![],
            current_node: None,
            dependency_state: DependencyState::default(),
            retry_counters: std::collections::HashMap::new(),
            blackboard: BlackboardSnapshot::default(),
            artifact_refs: vec![],
            execution: CheckpointExecution::default(),
            refs: RunRefs::default(),
            digest: None,
        }
    }
}
