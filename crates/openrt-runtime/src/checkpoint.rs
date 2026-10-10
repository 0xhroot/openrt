//! # P2.6 checkpoint + recovery surface
//!
//! This module is the **runtime-facing** orchestration of checkpointing,
//! recovery, dry-run planning and rollback. The heavy lifting lives in
//! `openrt-checkpoint` (model/planner/manager/rollback); here we:
//!
//! * wrap every [`crate::Runtime::run_workflow`] run in a per-run
//!   [`CheckpointWriter`] that emits Initial / AfterNode / Final checkpoints,
//! * expose the recovery surface ([`Runtime::resume_workflow`],
//!   [`Runtime::retry_failed`], [`Runtime::resume_dry_run`],
//!   [`Runtime::rollback_checkpoint`]) plus checkpoint record management
//!   ([`Runtime::list_checkpoints`], [`Runtime::load_checkpoint`],
//!   [`Runtime::delete_checkpoint`]),
//! * gate each recovery run through a [`Preflight`] check (halt,
//!   resources, session, workflow definition) so ADR-008 is never bypassed.
//!
//! ## Safety invariants
//!
//! * The checkpoint hook **fails closed**: a checkpoint that cannot be
//!   durably written aborts the workflow before the next node is scheduled,
//!   and the previous checkpoint record is preserved untouched.
//! * Recovery never auto-runs a node with an unknown/non-idempotent side
//!   effect (`NodeEffect::Unknown` is the safe default); the planner escalates
//!   those to operator intervention.
//! * Resume while the kill switch is engaged is refused.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use openrt_blackboard::{Blackboard, BLACKBOARD_NS};
use openrt_checkpoint::{
    ArtifactRef, Checkpoint, CheckpointBuild, CheckpointError, CheckpointExecution, CheckpointId,
    CheckpointKind, CheckpointManager, CheckpointStatus, FileSnapshotStore, NodeEffect, NodeState,
    NodeStatus, PolicyCompat, Preflight, RecoveryPlan, RecoveryPolicy, RecoveryScope,
    RollbackAuthorization, RollbackContinuation, RollbackOp, RollbackReport, RollbackResult,
    RollbackStatus, RollbackVerification, RunRefs,
};
use openrt_core::{EventKind, ExecutionId, SessionId, TaskId};
use openrt_orch::{
    CheckpointHook, ResumeSeed, RunNodeState, RunProgress, Workflow, WorkflowEngine, WorkflowError,
    WorkflowRunReport, WorkflowStatus, DAG_NS, RUNS_NS,
};
use openrt_telemetry::EventBus;

use crate::{Error, Runtime};

/// Per-run checkpoint parameters (default: every node effect `Unknown` —
/// the safe default that never auto-replays on recovery).
#[derive(Debug, Clone, Default)]
pub struct CheckpointOptions {
    /// step → side-effect classification. Steps not listed default to
    /// [`NodeEffect::Unknown`].
    pub effects: HashMap<String, NodeEffect>,
    /// Free-form note attached to every checkpoint this run creates.
    pub note: Option<String>,
    /// Workspace-relative files to snapshot into every checkpoint (rollback
    /// targets). Ignored when `workspace_root` is `None` (no external I/O).
    pub snapshot_files: Vec<PathBuf>,
    /// Workspace root used to resolve/verify file snapshots. When `None`,
    /// file snapshots are skipped (not an error).
    pub workspace_root: Option<PathBuf>,
}

/// Immutable per-run identity the [`CheckpointWriter`] needs to materialize
/// checkpoint records.
#[derive(Debug, Clone)]
pub(crate) struct RunMeta {
    session_id: SessionId,
    task_id: Option<TaskId>,
    workflow_id: String,
    steps: Vec<String>,
    deps_by_step: HashMap<String, Vec<String>>,
    /// Phase 10: logical run chain id — inherited across resume so one
    /// run's checkpoint chain shares provenance.
    run_id: String,
    /// Phase 10: fingerprint of the effective permission policy rules at
    /// run start (`sha256:<16 hex>`).
    policy_fingerprint: String,
}

/// The engine's fail-closed checkpoint hook: writes an AfterNode checkpoint
/// after every node result is durably recorded. Any persistence failure
/// surfaces through the engine as `WorkflowError::Checkpoint` and aborts the
/// run (the prior checkpoint chain is preserved).
pub struct CheckpointWriter {
    manager: CheckpointManager,
    store: Arc<dyn openrt_persistence::PersistenceStore>,
    fss: FileSnapshotStore,
    bus: EventBus,
    blackboard: Arc<Blackboard>,
    meta: RunMeta,
    options: CheckpointOptions,
    /// Chain: id of the most recently written checkpoint of this run.
    previous: Mutex<Option<CheckpointId>>,
    /// Last progress snapshot received from the engine (for the Final record).
    last_progress: Mutex<Option<RunProgress>>,
    /// Durable node state attached to the Initial checkpoint of this run
    /// (used to build the Final record when no node ran — e.g. a fully
    /// resumed run where every step was already completed).
    initial_states: Vec<NodeStatus>,
}

impl std::fmt::Debug for CheckpointWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CheckpointWriter")
            .field("workflow_id", &self.meta.workflow_id)
            .finish()
    }
}

impl CheckpointWriter {
    pub(crate) fn new(
        manager: CheckpointManager,
        store: Arc<dyn openrt_persistence::PersistenceStore>,
        bus: EventBus,
        blackboard: Arc<Blackboard>,
        meta: RunMeta,
        options: CheckpointOptions,
        initial_states: Vec<NodeStatus>,
    ) -> Self {
        let fss = FileSnapshotStore::new(store.clone());
        Self {
            manager,
            store,
            fss,
            bus,
            blackboard,
            meta,
            previous: Mutex::new(None),
            last_progress: Mutex::new(None),
            initial_states,
            options,
        }
    }

    /// Build + persist a checkpoint from the engine's current progress.
    async fn persist(
        &self,
        progress: &RunProgress,
        kind: CheckpointKind,
    ) -> Result<CheckpointId, CheckpointError> {
        *self.last_progress.lock().unwrap() = Some(progress.clone());
        let cp = self.build_from(progress, kind).await?;
        let id = self.manager.create(&cp).await?;
        *self.previous.lock().unwrap() = Some(id);
        self.emit_created(&cp).await;
        Ok(id)
    }

    /// Link this writer's chain to the checkpoint being resumed, so the new
    /// run's Initial checkpoint records the resumption point as its
    /// predecessor (a single linked chain across incremental runs).
    pub(crate) fn resume_from(&self, id: CheckpointId) {
        *self.previous.lock().unwrap() = Some(id);
    }

    /// Persist the Final checkpoint. Falls back to the initial node states
    /// when the run produced no engine progress (fully seeded resume).
    pub async fn finalize(
        &self,
        status: CheckpointStatus,
    ) -> Result<CheckpointId, CheckpointError> {
        let last = self.last_progress.lock().unwrap().clone();
        let cp = match last {
            Some(p) => self.build_from(&p, CheckpointKind::Final).await?,
            None => {
                let mut states = self
                    .initial_states
                    .iter()
                    .cloned()
                    .map(|mut s| {
                        // A run that produced no engine feedback may only
                        // claim completion for states not known-bad: Failed
                        // stays Failed, Running stays Running (never silently
                        // promoted — Phase 10).
                        if !matches!(s.state, NodeState::Failed | NodeState::Running) {
                            s.state = NodeState::Completed;
                        }
                        s
                    })
                    .collect::<Vec<_>>();
                states.sort_by(|a, b| a.step.cmp(&b.step));
                self.build_from_states(
                    states,
                    CheckpointKind::Final,
                    None,
                    self.blackboard.snapshot(),
                )
                .await?
            }
        };
        let mut cp = cp;
        cp.status = status;
        let id = self.manager.create(&cp).await?;
        *self.previous.lock().unwrap() = Some(id);
        self.emit_created(&cp).await;
        Ok(id)
    }

    /// Assemble a checkpoint from live engine progress (node states + pending).
    async fn build_from(
        &self,
        progress: &RunProgress,
        kind: CheckpointKind,
    ) -> Result<Checkpoint, CheckpointError> {
        let mut node_states: Vec<NodeStatus> = Vec::new();
        for n in &progress.nodes {
            node_states.push(NodeStatus {
                step: n.step.clone(),
                state: match n.state {
                    RunNodeState::Completed => NodeState::Completed,
                    RunNodeState::Skipped => NodeState::Skipped,
                    RunNodeState::Failed => NodeState::Failed,
                    RunNodeState::Running => NodeState::Running,
                },
                attempts: n.attempts,
                artifact_key: n.artifact_key.clone(),
                error: n.error.clone(),
                failure_category: n.failure_category.clone().unwrap_or_default(),
                effect: self
                    .options
                    .effects
                    .get(&n.step)
                    .copied()
                    .unwrap_or(NodeEffect::Unknown),
            });
        }
        for step in &progress.pending {
            node_states.push(NodeStatus {
                step: step.clone(),
                state: NodeState::Pending,
                attempts: 0,
                artifact_key: None,
                error: None,
                failure_category: String::new(),
                effect: self
                    .options
                    .effects
                    .get(step)
                    .copied()
                    .unwrap_or(NodeEffect::Unknown),
            });
        }
        node_states.sort_by(|a, b| a.step.cmp(&b.step));
        self.build_from_states(
            node_states,
            kind,
            Some(progress.execution_id),
            self.blackboard.snapshot(),
        )
        .await
    }

    /// Assemble + return (without persisting) a checkpoint from explicit node
    /// states and a blackboard snapshot.
    async fn build_from_states(
        &self,
        node_states: Vec<NodeStatus>,
        kind: CheckpointKind,
        execution_id: Option<ExecutionId>,
        blackboard: openrt_blackboard::BlackboardSnapshot,
    ) -> Result<Checkpoint, CheckpointError> {
        let mut artifact_refs: Vec<ArtifactRef> = Vec::new();
        for ns in &node_states {
            if let Some(key) = &ns.artifact_key {
                artifact_refs.push(ArtifactRef {
                    kind: "blackboard_output".into(),
                    key: Some(key.clone()),
                    path: None,
                    snapshot_id: None,
                });
            }
        }
        // Optional file snapshots (rollback targets); fail closed when the
        // operator asked for them and they cannot be captured.
        if let (Some(root), files) = (&self.options.workspace_root, &self.options.snapshot_files) {
            for rel in files {
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                let rec = self.fss.snapshot_file(root, &rel_str).await?;
                artifact_refs.push(ArtifactRef {
                    kind: "file_snapshot".into(),
                    key: None,
                    path: Some(rel_str),
                    snapshot_id: Some(rec.snapshot_id.clone()),
                });
            }
        }
        // Run-metadata snapshot: whatever `runs/<wf>` held before this record
        // went out (rollback target). Absent on the first run.
        let run_snapshot = self
            .store
            .get_record(RUNS_NS, &self.meta.workflow_id)
            .await
            .ok()
            .flatten();
        let execution = CheckpointExecution {
            execution_id,
            run_report_ref: Some(format!("{RUNS_NS}/{}", self.meta.workflow_id)),
            previous_checkpoint_id: *self.previous.lock().unwrap(),
            note: self.options.note.clone(),
            run_snapshot,
        };
        // Phase 10 provenance: history cursor = current ledger tail (the
        // time-machine anchor); unavailable history stays honestly None.
        let history_cursor = openrt_timeseries::tail_seq(&*self.store).await.ok();
        let refs = RunRefs {
            run_id: Some(self.meta.run_id.clone()),
            history_cursor,
            policy_version: Some(self.meta.policy_fingerprint.clone()),
            genome_id: None,
            genome_version: None,
            skill_digest: None,
            memory_refs: vec![],
        };
        let build = CheckpointBuild {
            workflow_id: self.meta.workflow_id.clone(),
            session_id: self.meta.session_id,
            task_id: self.meta.task_id,
            kind,
            status: CheckpointStatus::Running,
            execution,
            previous_checkpoint_id: *self.previous.lock().unwrap(),
            note: self.options.note.clone(),
            refs,
        };
        self.manager.materialize(
            build,
            &self.meta.steps,
            self.meta.deps_by_step.clone(),
            node_states,
            blackboard,
            artifact_refs,
        )
    }

    /// Persist the run's Initial checkpoint from the initial node states
    /// (all-pending on a fresh run; carried-over from the resumed checkpoint).
    pub async fn write_initial(&self) -> Result<CheckpointId, CheckpointError> {
        let cp = self
            .build_from_states(
                self.initial_states.clone(),
                CheckpointKind::Initial,
                None,
                self.blackboard.snapshot(),
            )
            .await?;
        let id = self.manager.create(&cp).await?;
        *self.previous.lock().unwrap() = Some(id);
        self.emit_created(&cp).await;
        Ok(id)
    }

    async fn emit_created(&self, cp: &Checkpoint) {
        self.bus
            .emit_correlated(
                EventKind::CheckpointCreated,
                Some(cp.session_id),
                cp.task_id,
                None,
                serde_json::json!({
                    "checkpoint_id": cp.checkpoint_id.to_string(),
                    "workflow_id": cp.workflow_id,
                    "status": cp.status,
                    "kind": cp.kind,
                    "nodes_total": cp.node_states.len(),
                    "nodes_completed": cp.completed.len(),
                }),
            )
            .await;
    }

    /// Emit a typed recovery event, tagged with this run's correlation ids.
    async fn emit(&self, kind: EventKind, data: serde_json::Value) {
        self.bus
            .emit_correlated(
                kind,
                Some(self.meta.session_id),
                self.meta.task_id,
                None,
                data,
            )
            .await;
    }
}

#[async_trait]
impl CheckpointHook for CheckpointWriter {
    async fn on_node_completed(&self, progress: &RunProgress) -> Result<(), String> {
        match self.persist(progress, CheckpointKind::AfterNode).await {
            Ok(_) => Ok(()),
            Err(e) => {
                // Fail closed: surface the exact reason; the prior checkpoint
                // is preserved because the write never happened.
                self.emit(
                    EventKind::ErrorOccurred,
                    serde_json::json!({ "phase": "checkpoint", "error": e.to_string() }),
                )
                .await;
                Err(e.to_string())
            }
        }
    }

    async fn on_nodes_scheduled(&self, progress: &RunProgress) -> Result<(), String> {
        match self.persist(progress, CheckpointKind::Scheduled).await {
            Ok(_) => Ok(()),
            Err(e) => {
                // Fail closed: the spawn window is exactly when a stale
                // `Pending` would later be misread as "never started".
                self.emit(
                    EventKind::ErrorOccurred,
                    serde_json::json!({ "phase": "checkpoint", "error": e.to_string() }),
                )
                .await;
                Err(e.to_string())
            }
        }
    }
}
/// Phase 11 (Track E): RAII registration of a run's cooperative pause flag.
/// Created when a workflow run enters [`Runtime::run_workflow_checkpointed`]
/// and dropped when that run's scope ends — the drop clears `active` and the
/// flag so a stale request can never leak into a later run. Lock poisoning is
/// unreachable in practice: only tiny map operations happen while holding.
struct PauseRegistration {
    map: Arc<Mutex<HashMap<String, (Arc<std::sync::atomic::AtomicBool>, bool)>>>,
    workflow_id: String,
    /// The exact flag this run inserted — the drop only clears the entry if
    /// it still owns it (a newer run of the same workflow replaced it).
    flag: Arc<std::sync::atomic::AtomicBool>,
}

impl PauseRegistration {
    fn flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.flag.clone()
    }
}

impl Drop for PauseRegistration {
    fn drop(&mut self) {
        if let Ok(mut map) = self.map.lock() {
            if let Some((current, active)) = map.get_mut(&self.workflow_id) {
                if Arc::ptr_eq(current, &self.flag) {
                    *active = false;
                    self.flag.store(false, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }
    }
}

impl Runtime {
    /// Whether the resource gate currently permits new work (P2.6). Consulted
    /// before any recovery-enabled run may proceed.
    ///
    /// This is a *query*, so it must not consume an outstanding operator
    /// override. It used to call `check()`, which does consume, so a single
    /// status probe could silently spend the operator's one-shot grant and the
    /// work it was meant for would still be refused. A status display has no
    /// business spending an authorization.
    pub fn resources_ok(&self) -> bool {
        matches!(
            self.resource_gate.check_peek(),
            openrt_resources::ResourceDecision::Allow
        )
    }

    /// Resume a workflow from a checkpoint ([`RecoveryScope::Resume`]).
    pub async fn resume_workflow(
        &self,
        session_id: SessionId,
        workflow: Workflow,
        checkpoint_id: CheckpointId,
        options: CheckpointOptions,
        policy: RecoveryPolicy,
    ) -> Result<WorkflowRunReport, Error> {
        self.run_workflow_checkpointed(
            session_id,
            workflow,
            options,
            Some((checkpoint_id, policy, RecoveryScope::Resume)),
        )
        .await
    }

    /// Retry failed nodes from a checkpoint ([`RecoveryScope::RetryOnly`]);
    /// never enqueues brand-new pending work.
    pub async fn retry_failed(
        &self,
        session_id: SessionId,
        workflow: Workflow,
        checkpoint_id: CheckpointId,
        options: CheckpointOptions,
        policy: RecoveryPolicy,
    ) -> Result<WorkflowRunReport, Error> {
        self.run_workflow_checkpointed(
            session_id,
            workflow,
            options,
            Some((checkpoint_id, policy, RecoveryScope::RetryOnly)),
        )
        .await
    }

    /// Phase 11 Track E: copy the room's steer entries for `workflow_id`
    /// into the blackboard — idempotent (`steer.<room>.<index>` only written
    /// when absent) and append-only from the room's perspective.
    async fn hydrate_room_steer(
        &self,
        blackboard: &Blackboard,
        workflow_id: &str,
    ) -> Result<(), Error> {
        for room in openrt_workspace::list_rooms(&*self.store).await? {
            if room.active_workflow.as_deref() != Some(workflow_id) {
                continue;
            }
            for (i, entry) in room.steer.iter().enumerate() {
                let key = format!("steer.{}.{}", room.id, i);
                if blackboard.get(&key)?.is_none() {
                    blackboard
                        .put(
                            &key,
                            serde_json::json!({
                                "at": entry.at,
                                "operator": entry.operator,
                                "input": entry.input,
                            }),
                        )
                        .await?;
                }
            }
        }
        Ok(())
    }

    /// Phase 11 Track E: register the cooperative pause flag for a run that
    /// is about to start. The returned registration clears the entry when it
    /// drops (end of the run's scope), so `active` never outlives a run and
    /// a stale flag can never leak into a later run.
    fn register_workflow_pause(&self, workflow_id: &str) -> PauseRegistration {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.workflow_pause
            .lock()
            .unwrap()
            .insert(workflow_id.to_string(), (flag.clone(), true));
        PauseRegistration {
            map: self.workflow_pause.clone(),
            workflow_id: workflow_id.to_string(),
            flag,
        }
    }

    /// Phase 11 Track E: request a cooperative pause of the in-process run
    /// for `workflow_id`. Fails closed when no run is active here — a pause
    /// is never queued onto a run that is not executing (it is a live
    /// control action, not persisted state; boot never re-applies it).
    pub async fn workflow_pause(&self, workflow_id: &str, operator: &str) -> Result<(), Error> {
        {
            let mut map = self.workflow_pause.lock().unwrap();
            match map.get_mut(workflow_id) {
                Some((flag, true)) => flag.store(true, std::sync::atomic::Ordering::SeqCst),
                _ => {
                    return Err(Error::Orchestration(WorkflowError::Condition(format!(
                        "workflow `{workflow_id}` has no active run in this process — pause \
                         is cooperative and applies only to a run executing now"
                    ))));
                }
            }
        }
        self.audit(
            operator,
            "workflow.pause",
            workflow_id,
            "requested",
            serde_json::json!({}),
        )
        .await?;
        Ok(())
    }

    /// Phase 11 Track E: explicitly resume the latest incomplete run of
    /// `workflow_id` from its newest checkpoint — the ONLY resume path
    /// (boot recovery only records incomplete runs, never resumes them).
    pub async fn workflow_resume(
        &self,
        workflow_id: &str,
        operator: &str,
    ) -> Result<WorkflowRunReport, Error> {
        let manager = CheckpointManager::new(self.store.clone());
        let checkpoint = manager.latest(workflow_id).await.ok_or_else(|| {
            Error::Orchestration(WorkflowError::Condition(format!(
                "workflow `{workflow_id}` has no checkpoint to resume from"
            )))
        })?;
        // Only an incomplete chain is resumable — a terminal Final record
        // (completed/failed/cancelled) ends the story; ask for a new run.
        if checkpoint.status != CheckpointStatus::Running {
            return Err(Error::Orchestration(WorkflowError::Condition(format!(
                "workflow `{workflow_id}` latest checkpoint is `{}` — nothing to resume",
                match checkpoint.status {
                    CheckpointStatus::Completed => "completed",
                    CheckpointStatus::Failed => "failed",
                    CheckpointStatus::Cancelled => "cancelled",
                    CheckpointStatus::Running => "running",
                }
            ))));
        }
        let raw = self
            .store
            .get_record(DAG_NS, &checkpoint.workflow_id)
            .await?
            .ok_or_else(|| {
                Error::Orchestration(WorkflowError::Condition(format!(
                    "workflow definition `{}` not found",
                    checkpoint.workflow_id
                )))
            })?;
        let workflow: Workflow = serde_json::from_value(raw).map_err(|e| {
            Error::Orchestration(WorkflowError::Condition(format!(
                "workflow definition `{}` is invalid: {e}",
                checkpoint.workflow_id
            )))
        })?;
        self.audit(
            operator,
            "workflow.resume",
            workflow_id,
            "resumed",
            serde_json::json!({ "checkpoint": checkpoint.checkpoint_id }),
        )
        .await?;
        self.resume_workflow(
            checkpoint.session_id,
            workflow,
            checkpoint.checkpoint_id,
            CheckpointOptions::default(),
            RecoveryPolicy::default(),
        )
        .await
    }

    /// Core run: shared by fresh runs, resumes and retries.
    pub(crate) async fn run_workflow_checkpointed(
        &self,
        session_id: SessionId,
        workflow: Workflow,
        options: CheckpointOptions,
        resume: Option<(CheckpointId, RecoveryPolicy, RecoveryScope)>,
    ) -> Result<WorkflowRunReport, Error> {
        // Serialize concurrent workflow/task runs on the same session (SEC-08).
        let _guard = self.session_locks.lock(session_id).await;
        let session = self
            .store
            .load_session(session_id)
            .await
            .map_err(Error::Persistence)?;

        let manager = CheckpointManager::new(self.store.clone());

        // Recovery planning (preflight + planner) before any side effect.
        let planned: Option<(Checkpoint, RecoveryPlan, RecoveryPolicy, RecoveryScope)> =
            match &resume {
                Some((checkpoint_id, policy, scope)) => {
                    let (cp, plan) = self
                        .load_and_plan(
                            &manager,
                            *checkpoint_id,
                            &workflow,
                            policy,
                            *scope,
                            session_id,
                        )
                        .await?;
                    Some((cp, plan, policy.clone(), *scope))
                }
                None => None,
            };

        let prompt = workflow
            .name
            .clone()
            .unwrap_or_else(|| format!("workflow {}", workflow.id));
        let mut task = openrt_core::Task::new(session_id, &prompt);
        self.store.save_task(&task).await?;
        self.bus
            .emit_correlated(
                EventKind::TaskCreated,
                Some(session_id),
                Some(task.id),
                None,
                serde_json::json!({ "prompt": prompt, "workflow_id": workflow.id }),
            )
            .await;

        task.agent_id = Some(self.orchestrator);
        task.transition(openrt_core::TaskStatus::Running)?;
        self.store.save_task(&task).await?;
        self.bus
            .emit_correlated(
                EventKind::TaskStarted,
                Some(session_id),
                Some(task.id),
                task.agent_id,
                serde_json::json!({
                    "agent_id": self.orchestrator.to_string(),
                    "kind": "orchestrator",
                    "resumed": planned.is_some(),
                }),
            )
            .await;

        let mut exec = openrt_core::ExecutionRecord::new(session_id, task.id, task.agent_id);
        exec.status = openrt_core::TaskStatus::Running;
        exec.started_at = task.started_at;
        self.store.save_execution(&exec).await?;

        // M10.7 lifecycle: the workflow run begins.
        self.hook_layer
            .fire(openrt_workspace_assets::HookTarget::WorkflowStarted)
            .await?;

        // Shared, durable blackboard (hydrated from the checkpoint on resume).
        let blackboard = Arc::new(Blackboard::with_store(self.store.clone(), BLACKBOARD_NS).await?);

        let steps: Vec<String> = workflow.steps.iter().map(|s| s.id.clone()).collect();
        let deps_by_step: HashMap<String, Vec<String>> = deps_by_step(&workflow);

        // Seed + initial node states.
        let mut seed = ResumeSeed::default();
        let initial_states: Vec<NodeStatus> = match &planned {
            Some((checkpoint, plan, policy, _scope)) => {
                for (key, value) in &checkpoint.blackboard.entries {
                    let _ = blackboard.put(key, value.clone()).await;
                }
                seed = seed_from_checkpoint(checkpoint, &workflow, policy);
                self.emit_recovery_events(checkpoint, plan, policy, &task)
                    .await;
                checkpoint.node_states.clone()
            }
            None => workflow
                .steps
                .iter()
                .map(|s| NodeStatus {
                    step: s.id.clone(),
                    state: NodeState::Pending,
                    attempts: 0,
                    artifact_key: None,
                    error: None,
                    failure_category: String::new(),
                    effect: options
                        .effects
                        .get(&s.id)
                        .copied()
                        .unwrap_or(NodeEffect::Unknown),
                })
                .collect(),
        };

        // Phase 11 Track E: hydrate the owning room's append-only steer log
        // into the blackboard (`steer.<room>.<index>` keys, put-if-absent so
        // resumed runs never duplicate or rewrite an entry).
        self.hydrate_room_steer(&blackboard, &workflow.id).await?;

        // Phase 10: one logical run chain across resume — a resumed run
        // inherits the checkpoint's run_id; a fresh run starts a new one.
        let run_id = planned
            .as_ref()
            .and_then(|(cp, _, _, _)| cp.refs.run_id.clone())
            .unwrap_or_else(|| task.id.to_string());
        let policy_fingerprint = self.policy_fingerprint();
        let meta = RunMeta {
            session_id,
            task_id: Some(task.id),
            workflow_id: workflow.id.clone(),
            steps,
            deps_by_step,
            run_id,
            policy_fingerprint,
        };
        let writer = Arc::new(CheckpointWriter::new(
            manager,
            self.store.clone(),
            self.bus.clone(),
            blackboard.clone(),
            meta,
            options,
            initial_states.clone(),
        ));
        // Resumed runs extend the resumed checkpoint's chain with their own
        // records (Initial → AfterNode… → Final).
        if let Some((checkpoint, _plan, _policy, _scope)) = &planned {
            writer.resume_from(checkpoint.checkpoint_id);
        }

        // Phase 11 Track E: cooperative pause — registered for exactly this
        // run's scope (cleared when the registration drops at run end), and
        // consulted by the engine at checkpoint boundaries only.
        let pause_reg = self.register_workflow_pause(&workflow.id);
        let mut engine = WorkflowEngine::new(
            self.role_agents(),
            Arc::new(self.bus.clone()),
            self.store.clone(),
            blackboard.clone(),
            self.engine_cancel.child(),
            self.orchestrator,
        )
        .with_correlation(Some(session_id), Some(task.id))
        .with_checkpoint_hook(writer.clone())
        .with_pause_flag(pause_reg.flag())
        // Fine-grained scheduler: consult the SAME ResourceGate before every
        // new step spawn. Denial while idle fails the run closed (TEST 4).
        .with_resource_admission(Arc::new({
            let gate = self.resource_gate.clone();
            move || match gate.check() {
                openrt_resources::ResourceDecision::Allow => Ok(()),
                openrt_resources::ResourceDecision::Denied { reason } => Err(reason),
            }
        }))
        // Phase 8: workflow sandbox admission — declared step sandbox profiles
        // validate against the SAME SandboxManager before the first step
        // (unknown profile / unavailable backend fails the run closed).
        .with_sandbox_admission(Arc::new(SandboxProfileAdmission(self.sandbox.clone())));

        struct SandboxProfileAdmission(std::sync::Arc<openrt_sandbox::SandboxManager>);
        #[async_trait::async_trait]
        impl openrt_orch::StepSandbox for SandboxProfileAdmission {
            async fn admit_profile(&self, profile: &str) -> Result<(), String> {
                self.0.check_profile(profile).map_err(|f| f.render())
            }
        }

        // Initial checkpoint — persisted before the first wave.
        writer.write_initial().await.map_err(Error::Checkpoint)?;

        let report = match &planned {
            Some(_) => engine.run_resume(&workflow, seed).await,
            None => engine.run(&workflow).await,
        };

        let report = match report {
            Ok(r) => r,
            Err(e) => {
                // Best-effort Final(Failed) marking, then surface the failure.
                let _ = writer.finalize(CheckpointStatus::Failed).await;
                return Err(map_engine_error(e));
            }
        };

        // Final checkpoint — durable according to the run's outcome.
        // Phase 11: `Paused` is NOT terminal — the chain is left Running (no
        // Final record) so boot recovery only *records* the incomplete run and
        // an explicit `workflow resume` continues it (never auto-resumed).
        let final_status = match report.status {
            WorkflowStatus::Completed => Some(CheckpointStatus::Completed),
            WorkflowStatus::Failed => Some(CheckpointStatus::Failed),
            WorkflowStatus::Cancelled => Some(CheckpointStatus::Cancelled),
            WorkflowStatus::Paused => None,
        };
        if let Some(final_status) = final_status {
            writer
                .finalize(final_status)
                .await
                .map_err(Error::Checkpoint)?;
        }

        self.finalize_workflow_run(session, &workflow.id, &mut task, &mut exec, &report)
            .await?;
        // M10.7 lifecycle: the workflow run reached its report.
        match report.status {
            WorkflowStatus::Paused => {
                self.hook_layer
                    .fire(openrt_workspace_assets::HookTarget::WorkflowPaused)
                    .await?;
            }
            _ => {
                self.hook_layer
                    .fire(openrt_workspace_assets::HookTarget::WorkflowCompleted)
                    .await?;
            }
        }
        Ok(report)
    }

    /// Preflight gate for recovery: halt state, resources, session existence,
    /// workflow definition existence (ADR-006 / ADR-008).
    async fn preflight(&self, checkpoint: &Checkpoint) -> Preflight {
        Preflight {
            halted: self.kill_switch.engaged().is_some(),
            resources_ok: self.resources_ok(),
            session_exists: self.store.load_session(checkpoint.session_id).await.is_ok(),
            workflow_def_exists: self
                .store
                .get_record(DAG_NS, &checkpoint.workflow_id)
                .await
                .is_ok(),
            policy_ready: true,
        }
    }

    /// Load + validate a checkpoint, gate it through a preflight, and run the
    /// recovery planner. Emits `RecoveryStarted` and, on intervention,
    /// `RecoveryNeedsIntervention`, then refuses to proceed.
    async fn load_and_plan(
        &self,
        manager: &CheckpointManager,
        checkpoint_id: CheckpointId,
        workflow: &Workflow,
        policy: &RecoveryPolicy,
        scope: RecoveryScope,
        session_id: SessionId,
    ) -> Result<(Checkpoint, RecoveryPlan), Error> {
        let checkpoint = match manager.load(&checkpoint_id).await {
            Ok(cp) => cp,
            Err(e) => {
                self.bus
                    .emit_correlated(
                        EventKind::CheckpointLoadFailed,
                        Some(session_id),
                        None,
                        Some(self.orchestrator),
                        serde_json::json!({
                            "checkpoint_id": checkpoint_id.to_string(),
                            "reason": e.to_string(),
                        }),
                    )
                    .await;
                return Err(Error::Checkpoint(e));
            }
        };
        let preflight = self.preflight(&checkpoint).await;
        if let Err(e) = preflight.ok() {
            return Err(Error::Checkpoint(e));
        }
        let steps: Vec<String> = workflow.steps.iter().map(|s| s.id.clone()).collect();
        let deps = deps_by_step(workflow);
        let plan = openrt_checkpoint::plan_recovery(&checkpoint, &steps, &deps, policy, scope)
            .map_err(Error::Checkpoint)?;

        self.bus
            .emit_correlated(
                EventKind::RecoveryStarted,
                Some(checkpoint.session_id),
                checkpoint.task_id,
                Some(self.orchestrator),
                serde_json::json!({
                    "checkpoint_id": checkpoint.checkpoint_id.to_string(),
                    "workflow_id": checkpoint.workflow_id,
                    "scope": scope,
                    "plan": {
                        "resume": plan.resume_nodes,
                        "retry": plan.retry_nodes,
                        "skip": plan.skip_nodes,
                        "intervention": plan.intervention_nodes,
                    },
                }),
            )
            .await;

        // ADR-002: a recovery operation is a first-class audited signal. A
        // recorder failure aborts the recovery before any node is scheduled.
        self.audit
            .append(
                "system",
                "checkpoint.recovery",
                &checkpoint.checkpoint_id.to_string(),
                match plan.status {
                    openrt_checkpoint::PlanStatus::Ready => "ready",
                    openrt_checkpoint::PlanStatus::Blocked => "blocked",
                    openrt_checkpoint::PlanStatus::NeedsIntervention => "needs_intervention",
                },
                serde_json::json!({
                    "workflow_id": checkpoint.workflow_id,
                    "resume": plan.resume_nodes,
                    "retry": plan.retry_nodes,
                    "skip": plan.skip_nodes,
                }),
            )
            .await
            .map_err(Error::Audit)?;

        if plan.status != openrt_checkpoint::PlanStatus::Ready {
            for node in &plan.intervention_nodes {
                self.bus
                    .emit_correlated(
                        EventKind::RecoveryNeedsIntervention,
                        Some(checkpoint.session_id),
                        checkpoint.task_id,
                        Some(self.orchestrator),
                        serde_json::json!({
                            "checkpoint_id": checkpoint.checkpoint_id.to_string(),
                            "node": node,
                            "reason": "not safe to auto-resume; operator action required",
                        }),
                    )
                    .await;
            }
            let reason = if plan.intervention_nodes.is_empty() {
                "recovery plan cannot proceed automatically".to_string()
            } else {
                format!(
                    "nodes {} require operator intervention",
                    plan.intervention_nodes.join(", ")
                )
            };
            return Err(Error::Checkpoint(CheckpointError::NeedsIntervention(
                reason,
            )));
        }
        Ok((checkpoint, plan))
    }

    /// Emit `WorkflowResumed` + per-node `RetryScheduled` events for the plan.
    async fn emit_recovery_events(
        &self,
        checkpoint: &Checkpoint,
        plan: &RecoveryPlan,
        policy: &RecoveryPolicy,
        task: &openrt_core::Task,
    ) {
        self.bus
            .emit_correlated(
                EventKind::WorkflowResumed,
                Some(checkpoint.session_id),
                checkpoint.task_id,
                Some(self.orchestrator),
                serde_json::json!({
                    "checkpoint_id": checkpoint.checkpoint_id.to_string(),
                    "workflow_id": checkpoint.workflow_id,
                    "resume_nodes": plan.resume_nodes,
                    "retry_nodes": plan.retry_nodes,
                    "skip_nodes": plan.skip_nodes,
                }),
            )
            .await;
        for node in &plan.retry_nodes {
            let prior = checkpoint
                .node_states
                .iter()
                .find(|n| &n.step == node)
                .map(|n| n.attempts)
                .unwrap_or(0);
            self.bus
                .emit_correlated(
                    EventKind::RetryScheduled,
                    Some(checkpoint.session_id),
                    checkpoint.task_id,
                    Some(self.orchestrator),
                    serde_json::json!({
                        "node": node,
                        "attempt": prior + 1,
                        "max_attempts": policy.max_attempts,
                    }),
                )
                .await;
        }
        let _ = task;
    }

    /// Mirror the workflow report into the task/execution/session records.
    async fn finalize_workflow_run(
        &self,
        session: openrt_core::Session,
        workflow_id: &str,
        task: &mut openrt_core::Task,
        exec: &mut openrt_core::ExecutionRecord,
        report: &WorkflowRunReport,
    ) -> Result<(), Error> {
        let prompt = task.prompt.clone();
        let summary = report
            .steps
            .iter()
            .map(|s| format!("{}({} {})", s.step, s.kind, s.status))
            .collect::<Vec<_>>()
            .join(", ");
        match report.status {
            WorkflowStatus::Completed => {
                task.transition(openrt_core::TaskStatus::Completed)?;
                task.result = Some(format!("workflow {workflow_id} completed: {summary}"));
                self.bus
                    .emit_correlated(
                        EventKind::TaskCompleted,
                        Some(session.id),
                        Some(task.id),
                        task.agent_id,
                        serde_json::json!({
                            "result": task.result,
                            "workflow_id": workflow_id,
                        }),
                    )
                    .await;
            }
            WorkflowStatus::Failed => {
                task.transition(openrt_core::TaskStatus::Failed)?;
                task.error = Some(format!("workflow {workflow_id} failed: {summary}"));
                self.bus
                    .emit_correlated(
                        EventKind::TaskFailed,
                        Some(session.id),
                        Some(task.id),
                        task.agent_id,
                        serde_json::json!({
                            "reason": "error",
                            "error": task.error,
                            "workflow_id": workflow_id,
                        }),
                    )
                    .await;
            }
            WorkflowStatus::Cancelled => {
                task.transition(openrt_core::TaskStatus::Cancelled)?;
                task.error = Some("workflow cancelled".into());
                self.bus
                    .emit_correlated(
                        EventKind::TaskFailed,
                        Some(session.id),
                        Some(task.id),
                        task.agent_id,
                        serde_json::json!({ "reason": "cancelled", "error": task.error }),
                    )
                    .await;
            }
            WorkflowStatus::Paused => {
                // Phase 11: pause is an operator action, not a failure — the
                // task waits (active, so boot records it interrupted rather
                // than claiming success) and carries no error.
                task.transition(openrt_core::TaskStatus::Waiting)?;
                task.result = Some(format!("workflow {workflow_id} paused: {summary}"));
                self.bus
                    .emit_correlated(
                        EventKind::WorkflowPaused,
                        Some(session.id),
                        Some(task.id),
                        task.agent_id,
                        serde_json::json!({
                            "result": task.result,
                            "workflow_id": workflow_id,
                        }),
                    )
                    .await;
            }
        }

        // Fold a summary into the session transcript, like a normal task.
        let mut session = session;
        session.push(openrt_core::Message::user(prompt));
        session.push(openrt_core::Message::assistant(format!(
            "[workflow {workflow_id}: {}] {}",
            report.status,
            task.result.as_deref().unwrap_or("")
        )));
        self.store.update_session(&session).await?;

        exec.status = task.status;
        exec.completed_at = task.completed_at;
        exec.result = task.result.clone();
        exec.error = task.error.clone();
        exec.turn_count = task.turns.len() as u32;
        self.store.save_execution(exec).await?;
        self.store.save_task(task).await?;
        Ok(())
    }

    /// Plan recovery **without executing anything** and persist a dry-run
    /// report keyed by checkpoint id in the `dryrun` namespace. Zero side
    /// effects on the workflow state; preflight failures refuse outright.
    pub async fn resume_dry_run(
        &self,
        session_id: SessionId,
        workflow: Workflow,
        checkpoint_id: CheckpointId,
        policy: RecoveryPolicy,
    ) -> Result<openrt_checkpoint::DryRunReport, Error> {
        let manager = CheckpointManager::new(self.store.clone());
        let checkpoint = match manager.load(&checkpoint_id).await {
            Ok(cp) => cp,
            Err(e) => {
                self.bus
                    .emit_correlated(
                        EventKind::CheckpointLoadFailed,
                        Some(session_id),
                        None,
                        Some(self.orchestrator),
                        serde_json::json!({
                            "checkpoint_id": checkpoint_id.to_string(),
                            "reason": e.to_string(),
                        }),
                    )
                    .await;
                return Err(Error::Checkpoint(e));
            }
        };
        let preflight = self.preflight(&checkpoint).await;
        preflight.ok().map_err(Error::Checkpoint)?;

        let steps: Vec<String> = workflow.steps.iter().map(|s| s.id.clone()).collect();
        let deps = deps_by_step(&workflow);
        let plan = openrt_checkpoint::plan_recovery(
            &checkpoint,
            &steps,
            &deps,
            &policy,
            RecoveryScope::Resume,
        )
        .map_err(Error::Checkpoint)?;
        let report = openrt_checkpoint::DryRunReport::from_plan(&plan);
        manager
            .save_dry_run(&checkpoint_id, &report)
            .await
            .map_err(Error::Checkpoint)?;

        self.bus
            .emit_correlated(
                EventKind::RecoveryStarted,
                Some(checkpoint.session_id),
                checkpoint.task_id,
                Some(self.orchestrator),
                serde_json::json!({
                    "checkpoint_id": checkpoint.checkpoint_id.to_string(),
                    "workflow_id": checkpoint.workflow_id,
                    "dry_run": true,
                    "plan_status": plan.status,
                    "plan": {
                        "resume": plan.resume_nodes,
                        "retry": plan.retry_nodes,
                        "skip": plan.skip_nodes,
                        "intervention": plan.intervention_nodes,
                    },
                }),
            )
            .await;
        for node in plan.intervention_nodes.iter() {
            self.bus
                .emit_correlated(
                    EventKind::RecoveryNeedsIntervention,
                    Some(checkpoint.session_id),
                    checkpoint.task_id,
                    Some(self.orchestrator),
                    serde_json::json!({
                        "checkpoint_id": checkpoint.checkpoint_id.to_string(),
                        "node": node,
                        "reason": "not safe to auto-resume; operator action required (dry run)",
                    }),
                )
                .await;
        }
        Ok(report)
    }

    /// Fingerprint of the active permission policy — recorded in `RunRefs`
    /// when a checkpoint is sealed and re-checked by the M10.3 rollback
    /// policy-compatibility gate.
    pub fn policy_fingerprint(&self) -> String {
        let hex = serde_json::to_vec(&self.permission_policy.rules)
            .map(|b| openrt_core::sha256_hex(&b))
            .unwrap_or_else(|_| "unserializable".to_string());
        let take = hex.len().min(16);
        format!("sha256:{}", &hex[..take])
    }

    /// Production rollback path (M10.3): integrity validation → policy
    /// compatibility → plan → operator authorization → restore → verify →
    /// chained audit (`recovery.rollback`) → continuation record. Never
    /// rewrites `history.*` (ledger immutability is asserted and recorded);
    /// a corrupt checkpoint, an untrusted operator, or unacknowledged policy
    /// drift all fail closed before any state is touched.
    pub async fn rollback_checkpoint(
        &self,
        checkpoint_id: &CheckpointId,
        auth: &RollbackAuthorization,
        workspace_root: Option<PathBuf>,
    ) -> Result<RollbackReport, Error> {
        // 1. Integrity validation — `load` runs schema validation + digest
        //    verification; a corrupt record never reaches the restore step.
        let manager = CheckpointManager::new(self.store.clone());
        let checkpoint = manager
            .load(checkpoint_id)
            .await
            .map_err(Error::Checkpoint)?;

        // 2. Policy compatibility: the fingerprint recorded when this
        //    checkpoint was sealed vs. the runtime's current policy.
        let current_fp = self.policy_fingerprint();
        let policy_compat = match checkpoint.refs.policy_version.as_deref() {
            Some(fp) if fp == current_fp => PolicyCompat::Compatible,
            Some(_) => PolicyCompat::Mismatched,
            None => PolicyCompat::Unknown,
        };

        // 3. Plan — derived from the checkpoint before anything is authorized.
        let ops = openrt_checkpoint::rollback::plan_rollbacks(&checkpoint);

        // 4. Operator authorization — fail-closed, before any mutation.
        if !self.asset_operators().iter().any(|o| o == &auth.operator) {
            return Err(Error::Improve(
                openrt_selfimprove::ImproveError::UnauthorizedOperator {
                    operator: auth.operator.clone(),
                },
            ));
        }
        if policy_compat != PolicyCompat::Compatible && !auth.acknowledge_policy_drift {
            return Err(Error::Checkpoint(CheckpointError::Refused(format!(
                "rollback of `{checkpoint_id}` refused: checkpoint policy fingerprint is                  {policy_compat:?} w.r.t. the current runtime policy; the operator must                  explicitly acknowledge policy drift (no silent policy downgrade)"
            ))));
        }

        // History-ledger immutability baseline (step 7 input): a rollback
        // must never rewrite `history.*` — append-only, prefix unchanged.
        let tail_before = openrt_timeseries::tail_seq(&*self.store).await.ok();
        let history_prefix = match tail_before {
            Some(t) if t > 0 => openrt_timeseries::materialize_as_of(&*self.store, t)
                .await
                .ok()
                .map(|rows| (t, serde_json::to_value(&rows).unwrap_or_default())),
            _ => None,
        };

        // 5. Restore — per-op, each emitting RollbackStarted plus a terminal
        //    RollbackCompleted / RollbackUnavailable / ErrorOccurred event.
        let fss = FileSnapshotStore::new(self.store.clone());
        let mut results: Vec<RollbackResult> = Vec::new();
        for op in &ops {
            self.bus
                .emit_correlated(
                    EventKind::RollbackStarted,
                    Some(checkpoint.session_id),
                    checkpoint.task_id,
                    Some(self.orchestrator),
                    serde_json::json!({
                        "checkpoint_id": checkpoint_id.to_string(),
                        "op": rollback_op_name(op),
                    }),
                )
                .await;
            let result = match op {
                RollbackOp::FileRestore {
                    snapshot_id,
                    rel_path,
                } => match &workspace_root {
                    Some(root) => match fss.load(snapshot_id).await {
                        None => RollbackResult {
                            op: op.clone(),
                            status: RollbackStatus::Unavailable,
                            detail: format!("snapshot `{snapshot_id}` not found in rollback store"),
                        },
                        Some(record) => match fss.rollback_file(root, &record).await {
                            Ok(()) => RollbackResult {
                                op: op.clone(),
                                status: RollbackStatus::Done,
                                detail: format!("restored `{rel_path}`"),
                            },
                            Err(e) => RollbackResult {
                                op: op.clone(),
                                status: RollbackStatus::Failed,
                                detail: format!("restore failed: {e}"),
                            },
                        },
                    },
                    None => RollbackResult {
                        op: op.clone(),
                        status: RollbackStatus::Unavailable,
                        detail: "no workspace root configured for file restore".into(),
                    },
                },
                RollbackOp::BlackboardRestore { keys } => {
                    let bb =
                        Arc::new(Blackboard::with_store(self.store.clone(), BLACKBOARD_NS).await?);
                    let mut failures = Vec::new();
                    for key in keys {
                        let Some(value) = checkpoint
                            .blackboard
                            .entries
                            .iter()
                            .find(|(k, _)| k == key)
                            .map(|(_, v)| v.clone())
                        else {
                            failures.push(format!("{key} missing from checkpoint snapshot"));
                            continue;
                        };
                        if let Err(e) = bb.put(key, value).await {
                            failures.push(format!("{key}: {e}"));
                        }
                    }
                    if failures.is_empty() {
                        RollbackResult {
                            op: op.clone(),
                            status: RollbackStatus::Done,
                            detail: format!("restored {} blackboard keys", keys.len()),
                        }
                    } else {
                        RollbackResult {
                            op: op.clone(),
                            status: RollbackStatus::Failed,
                            detail: failures.join("; "),
                        }
                    }
                }
                RollbackOp::RunMetadataRestore => match &checkpoint.execution.run_snapshot {
                    Some(snapshot) => {
                        match self
                            .store
                            .put_record(RUNS_NS, &checkpoint.workflow_id, snapshot)
                            .await
                        {
                            Ok(()) => RollbackResult {
                                op: op.clone(),
                                status: RollbackStatus::Done,
                                detail: format!(
                                    "restored run metadata `{RUNS_NS}/{}`",
                                    checkpoint.workflow_id
                                ),
                            },
                            Err(e) => RollbackResult {
                                op: op.clone(),
                                status: RollbackStatus::Failed,
                                detail: format!("run metadata restore failed: {e}"),
                            },
                        }
                    }
                    None => RollbackResult {
                        op: op.clone(),
                        status: RollbackStatus::Unavailable,
                        detail: "no run metadata snapshot captured in checkpoint".into(),
                    },
                },
            };
            match result.status {
                RollbackStatus::Done => {
                    self.bus
                        .emit_correlated(
                            EventKind::RollbackCompleted,
                            Some(checkpoint.session_id),
                            checkpoint.task_id,
                            Some(self.orchestrator),
                            serde_json::json!({
                                "checkpoint_id": checkpoint_id.to_string(),
                                "op": rollback_op_name(op),
                            }),
                        )
                        .await;
                }
                RollbackStatus::Unavailable => {
                    self.bus
                        .emit_correlated(
                            EventKind::RollbackUnavailable,
                            Some(checkpoint.session_id),
                            checkpoint.task_id,
                            Some(self.orchestrator),
                            serde_json::json!({
                                "checkpoint_id": checkpoint_id.to_string(),
                                "op": rollback_op_name(op),
                                "reason": result.detail,
                            }),
                        )
                        .await;
                }
                RollbackStatus::Failed => {
                    self.bus
                        .emit_correlated(
                            EventKind::ErrorOccurred,
                            Some(checkpoint.session_id),
                            checkpoint.task_id,
                            Some(self.orchestrator),
                            serde_json::json!({
                                "phase": "rollback",
                                "error": result.detail,
                                "checkpoint_id": checkpoint_id.to_string(),
                                "op": rollback_op_name(op),
                            }),
                        )
                        .await;
                }
            }
            results.push(result);
        }

        // 6. Verify — every `Done` operation is re-read and compared against
        //    the checkpoint's snapshot; mismatches are surfaced, never hidden.
        let verification = self
            .verify_rollback(&checkpoint, &ops, &results, workspace_root.as_deref())
            .await;

        // 7. History immutability — the prefix captured above must still
        //    materialize identically (`materialize_as_of` re-verifies the
        //    chain), and no previously recorded row may disappear.
        let mut history_immutable = true;
        let mut history_note = String::new();
        if let Some((t, want)) = &history_prefix {
            match openrt_timeseries::materialize_as_of(&*self.store, *t).await {
                Ok(rows) => {
                    let got = serde_json::to_value(&rows).unwrap_or_default();
                    if got != *want {
                        history_immutable = false;
                        history_note = format!("prefix of {t} history row(s) changed");
                    }
                }
                Err(e) => {
                    history_immutable = false;
                    history_note = format!("history chain no longer verifies: {e}");
                }
            }
        }
        if let Some(t) = tail_before {
            if let Ok(after) = openrt_timeseries::tail_seq(&*self.store).await {
                if after < t {
                    history_immutable = false;
                    history_note = format!("history rows deleted ({t} -> {after})");
                }
            }
        }

        // 8. Chained audit (`recovery.rollback`) — fail-closed: an audit
        //    failure fails the call, and an immutability violation is
        //    audited before the error is raised.
        let any_failed = results.iter().any(|r| {
            matches!(
                r.status,
                RollbackStatus::Failed | RollbackStatus::Unavailable
            )
        });
        let audit_result = if !history_immutable {
            "history_violated"
        } else if !verification.mismatches.is_empty() {
            "verify_failed"
        } else if any_failed {
            "partial"
        } else {
            "completed"
        };
        self.audit
            .append(
                &auth.operator,
                "recovery.rollback",
                &checkpoint_id.to_string(),
                audit_result,
                serde_json::json!({
                    "workflow_id": checkpoint.workflow_id,
                    "session_id": checkpoint.session_id.to_string(),
                    "policy_compat": policy_compat,
                    "acknowledged_policy_drift": auth.acknowledge_policy_drift,
                    "ops": results
                        .iter()
                        .map(|r| serde_json::json!({
                            "op": rollback_op_name(&r.op),
                            "status": r.status,
                        }))
                        .collect::<Vec<_>>(),
                    "verification": {
                        "checked": verification.checked,
                        "mismatches": verification.mismatches,
                    },
                    "history_immutable": history_immutable,
                }),
            )
            .await
            .map_err(Error::Audit)?;
        if !history_immutable {
            return Err(Error::Checkpoint(CheckpointError::Refused(format!(
                "history ledger immutability violated during rollback of `{checkpoint_id}`:                  {history_note}"
            ))));
        }

        // 9. Continuation record: the persisted report carries where the
        //    operator continues from (the rolled-back-to checkpoint).
        let report = RollbackReport {
            checkpoint_id: checkpoint_id.to_string(),
            results,
            policy_compat: Some(policy_compat),
            operator: Some(auth.operator.clone()),
            verification: Some(verification),
            history_immutable: Some(true),
            continuation: Some(RollbackContinuation {
                resume_from: checkpoint_id.to_string(),
                workflow_id: checkpoint.workflow_id.clone(),
                next_action: "resume".into(),
            }),
        };
        let key = format!("{}.{}", checkpoint_id, uuid::Uuid::new_v4());
        self.store
            .put_record(
                openrt_checkpoint::rollback::ROLLBACK_NS,
                &key,
                &serde_json::to_value(&report)
                    .map_err(openrt_checkpoint::CheckpointError::Serialization)?,
            )
            .await?;
        Ok(report)
    }

    /// M10.3 verification: re-read every `Done` rollback operation's effect
    /// and compare it with the checkpoint's snapshot. Operates on a copy of
    /// the planned ops zipped with their results; unavailable/failed ops are
    /// not checked (their report status already says so).
    async fn verify_rollback(
        &self,
        checkpoint: &Checkpoint,
        ops: &[RollbackOp],
        results: &[RollbackResult],
        workspace_root: Option<&std::path::Path>,
    ) -> RollbackVerification {
        let mut v = RollbackVerification::default();
        for (op, res) in ops.iter().zip(results.iter()) {
            if res.status != RollbackStatus::Done {
                continue;
            }
            v.checked += 1;
            match op {
                RollbackOp::FileRestore {
                    snapshot_id,
                    rel_path,
                } => match workspace_root {
                    Some(root) => {
                        let fss = FileSnapshotStore::new(self.store.clone());
                        if let Err(e) = fss.verify_restored(root, snapshot_id).await {
                            v.mismatches.push(format!("file `{rel_path}`: {e}"));
                        }
                    }
                    None => v.mismatches.push(format!(
                        "file `{rel_path}`: no workspace root available for verification"
                    )),
                },
                RollbackOp::BlackboardRestore { keys } => {
                    let bb = match Blackboard::with_store(self.store.clone(), BLACKBOARD_NS).await {
                        Ok(b) => b,
                        Err(e) => {
                            v.mismatches.push(format!("blackboard: {e}"));
                            continue;
                        }
                    };
                    for key in keys {
                        match checkpoint.blackboard.entries.iter().find(|(k, _)| k == key) {
                            None => v.mismatches.push(format!(
                                "blackboard `{key}`: missing from checkpoint snapshot"
                            )),
                            Some((_, want)) => match bb.get(key) {
                                Ok(Some(got)) if &got == want => {}
                                Ok(Some(_)) => v.mismatches.push(format!(
                                    "blackboard `{key}`: restored value differs from snapshot"
                                )),
                                Ok(None) => v
                                    .mismatches
                                    .push(format!("blackboard `{key}`: absent after restore")),
                                Err(e) => v.mismatches.push(format!("blackboard `{key}`: {e}")),
                            },
                        }
                    }
                }
                RollbackOp::RunMetadataRestore => match &checkpoint.execution.run_snapshot {
                    None => v
                        .mismatches
                        .push("run metadata: no snapshot in checkpoint".to_string()),
                    Some(want) => {
                        match self
                            .store
                            .get_record(RUNS_NS, &checkpoint.workflow_id)
                            .await
                        {
                            Ok(Some(got)) if &got == want => {}
                            Ok(Some(_)) => v.mismatches.push(
                                "run metadata: restored record differs from snapshot".to_string(),
                            ),
                            Ok(None) => v
                                .mismatches
                                .push("run metadata: absent after restore".to_string()),
                            Err(e) => v.mismatches.push(format!("run metadata: {e}")),
                        }
                    }
                },
            }
        }
        v
    }

    /// List checkpoint metadata, optionally filtered by workflow id.
    pub async fn list_checkpoints(
        &self,
        workflow_id: Option<&str>,
    ) -> Result<Vec<openrt_checkpoint::CheckpointMetadata>, Error> {
        CheckpointManager::new(self.store.clone())
            .list(workflow_id)
            .await
            .map_err(Error::Checkpoint)
    }

    /// Load a checkpoint by id (schema-validated).
    pub async fn load_checkpoint(&self, id: &CheckpointId) -> Result<Checkpoint, Error> {
        CheckpointManager::new(self.store.clone())
            .load(id)
            .await
            .map_err(Error::Checkpoint)
    }

    /// Delete a checkpoint record; emits `CheckpointDeleted`.
    pub async fn delete_checkpoint(&self, id: &CheckpointId) -> Result<(), Error> {
        let manager = CheckpointManager::new(self.store.clone());
        let checkpoint = manager.load(id).await.map_err(Error::Checkpoint)?;
        manager.delete(id).await.map_err(Error::Checkpoint)?;
        self.bus
            .emit_correlated(
                EventKind::CheckpointDeleted,
                Some(checkpoint.session_id),
                checkpoint.task_id,
                Some(self.orchestrator),
                serde_json::json!({ "checkpoint_id": id.to_string() }),
            )
            .await;
        Ok(())
    }
}

/// step → `depends_on` mirror of the workflow.
fn deps_by_step(workflow: &Workflow) -> HashMap<String, Vec<String>> {
    workflow
        .steps
        .iter()
        .map(|s| (s.id.clone(), s.depends_on.clone()))
        .collect()
}

/// Build the engine's [`ResumeSeed`] from a checkpoint + recovery policy.
fn seed_from_checkpoint(
    checkpoint: &Checkpoint,
    workflow: &Workflow,
    policy: &RecoveryPolicy,
) -> ResumeSeed {
    let mut outputs: HashMap<String, String> = HashMap::new();
    for (key, value) in &checkpoint.blackboard.entries {
        if let Some(step) = key.strip_prefix("output.") {
            outputs.insert(step.to_string(), output_text(value));
        }
    }
    let mut seed = ResumeSeed::default();
    for ns in &checkpoint.node_states {
        // Cumulative attempt budgets carry across every state, not just
        // completed steps: a failed / resumed node must keep the attempts it
        // already consumed so the engine's counters stay truthful.
        seed.attempts_by_step.insert(ns.step.clone(), ns.attempts);
        if matches!(ns.state, NodeState::Completed | NodeState::Skipped) {
            let text = outputs
                .get(&ns.step)
                .cloned()
                .unwrap_or_else(|| "skipped".to_string());
            seed.done.insert(ns.step.clone(), text);
        }
    }
    // Absolute cumulative recovery budget for every step.
    for step in &workflow.steps {
        seed.attempt_limits
            .insert(step.id.clone(), policy.max_attempts);
    }
    seed
}

/// Extract human-readable text from a blackboard output value.
fn output_text(value: &serde_json::Value) -> String {
    value
        .get("text")
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| value.to_string())
}

/// Stable, human-readable name for a rollback operation.
fn rollback_op_name(op: &RollbackOp) -> String {
    match op {
        RollbackOp::FileRestore { rel_path, .. } => format!("file_restore:{rel_path}"),
        RollbackOp::BlackboardRestore { keys } => {
            format!("blackboard_restore:{}", keys.join(","))
        }
        RollbackOp::RunMetadataRestore => "run_metadata_restore".to_string(),
    }
}

/// Map an engine error back onto the runtime boundary; hook failures (failed
/// checkpoint writes) surface as typed checkpoint errors (fail closed).
fn map_engine_error(e: WorkflowError) -> Error {
    match e {
        WorkflowError::Checkpoint(msg) => Error::Checkpoint(CheckpointError::Invalid(msg)),
        other => Error::Orchestration(other),
    }
}
