//! Phase 10 M10.4: crash recovery at boot + live task handles.
//!
//! One authority for reconciling incomplete work after a process restart:
//!
//! * **Boot scan** ([`Runtime::boot_recover`], invoked by
//!   [`crate::RuntimeBuilder::build`]): stale tasks are transitioned to
//!   [`TaskStatus::Interrupted`] (never marked successful), incomplete
//!   checkpoint chains are planned into a [`RunResumePoint`] via the
//!   `openrt-checkpoint` planner (no node is executed), and stale sandbox
//!   instances are recorded. Every boot persists a [`RecoveryRecord`] under
//!   [`RECOVERY_NS`]; boots with findings also emit `RecoveryStarted`
//!   (phase `boot`) and append a chained audit row (`recovery.boot`).
//!
//! * **Live task handles** (`task_inspect`/`task_status`/`task_cancel`/
//!   `task_terminate`/`task_observe`/`task_logs`/`task_artifacts`/
//!   `task_recover`): operate over the ONE task store plus the runtime's
//!   in-process token registry — never arbitrary process control.
//!   Workflow-backed tasks are refused by `task_recover` (their checkpoint
//!   chain is the recovery authority).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use openrt_checkpoint::{
    plan_recovery, CheckpointError, CheckpointId, CheckpointManager, CheckpointMetadata,
    CheckpointStatus, PlanStatus, RecoveryPolicy, RecoveryScope,
};
use openrt_core::{
    now, CancellationToken, EventKind, PauseToken, SessionId, TaskId, TaskStatus, Timestamp,
    TurnRecord,
};
use openrt_sandbox::SandboxState;

use crate::{Error, Runtime};

/// Persistence namespace for boot recovery reports (Phase 10 records family).
pub const RECOVERY_NS: &str = "recovery.records";

// ── records ──────────────────────────────────────────────────────────────────

/// A task that was active when its owning process died (reconciled at boot).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterruptedTaskInfo {
    pub task_id: TaskId,
    pub session_id: SessionId,
    /// The active status discovered at boot (`Running`, `Queued`, ...).
    pub previous_status: TaskStatus,
}

/// Where an incomplete workflow run may safely resume — computed by the
/// `openrt-checkpoint` planner at boot; *nothing is executed* until an
/// operator (or the resume API) asks for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunResumePoint {
    pub checkpoint_id: CheckpointId,
    /// `"ready"`, `"blocked"`, or `"needs_intervention"`.
    pub plan_status: String,
    pub resume_nodes: Vec<String>,
    pub retry_nodes: Vec<String>,
    pub intervention_nodes: Vec<String>,
    /// Steps that must NOT be re-executed (completed/skipped upstream).
    pub skip_nodes: Vec<String>,
}

/// A checkpoint chain whose latest record is still `Running` at boot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IncompleteRunInfo {
    pub workflow_id: String,
    pub session_id: SessionId,
    pub task_id: Option<TaskId>,
    pub resume: RunResumePoint,
}

/// A sandbox instance left non-terminal by a dead process. Recorded for the
/// operator (`sandbox destroy`); boot recovery never kills processes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StaleSandboxInfo {
    pub sandbox_id: String,
    pub profile: String,
    pub state: SandboxState,
    pub pid: Option<u32>,
}

/// One boot's reconciliation report — persisted under [`RECOVERY_NS`] for
/// every boot (a clean boot is still evidence).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecoveryRecord {
    /// Stable key (`boot-<uuid>`).
    pub record_id: String,
    /// Random id of this boot (one record per `RuntimeBuilder::build`).
    pub boot_id: String,
    pub boot_at: Timestamp,
    pub interrupted_tasks: Vec<InterruptedTaskInfo>,
    pub incomplete_runs: Vec<IncompleteRunInfo>,
    pub stale_sandboxes: Vec<StaleSandboxInfo>,
}

impl RecoveryRecord {
    pub fn is_clean(&self) -> bool {
        self.interrupted_tasks.is_empty()
            && self.incomplete_runs.is_empty()
            && self.stale_sandboxes.is_empty()
    }

    pub fn total_findings(&self) -> usize {
        self.interrupted_tasks.len() + self.incomplete_runs.len() + self.stale_sandboxes.len()
    }
}

/// Everything knowable about one live task without executing it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskInspection {
    pub task: openrt_core::Task,
    /// A cancellation token for this task exists in THIS process.
    pub in_process: bool,
    /// Latest boot recovery record that listed this task as interrupted.
    pub recovery_record: Option<String>,
    /// The task belongs to a checkpointed workflow (its recovery authority
    /// is `resume_workflow`, not `task_recover`).
    pub workflow_backed: bool,
}

/// One artifact a task produced: workflow checkpoint refs, blackboard
/// `output.*` entries, or the plain task result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskArtifact {
    /// `"file_snapshot"`, `"blackboard_output"`, `"run_report"`,
    /// `"final_output"`, ... (checkpoint ref kinds pass through).
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
}

fn plan_status_str(status: PlanStatus) -> &'static str {
    match status {
        PlanStatus::Ready => "ready",
        PlanStatus::Blocked => "blocked",
        PlanStatus::NeedsIntervention => "needs_intervention",
    }
}

fn map_load_task(id: TaskId) -> impl Fn(openrt_persistence::PersistenceError) -> Error {
    move |e| match e {
        openrt_persistence::PersistenceError::TaskNotFound(_) => {
            Error::Task(openrt_core::TaskError::NotFound(id))
        }
        other => Error::Persistence(other),
    }
}

/// Registers a task's cancellation + pause signals for the duration of a run
/// and guarantees deregistration on every exit path (including panics).
struct ActiveTaskGuard {
    map: Arc<Mutex<HashMap<TaskId, crate::runtime::ActiveTask>>>,
    id: TaskId,
}

impl ActiveTaskGuard {
    fn register(
        map: Arc<Mutex<HashMap<TaskId, crate::runtime::ActiveTask>>>,
        id: TaskId,
        token: CancellationToken,
        pause: std::sync::Arc<PauseToken>,
    ) -> Self {
        map.lock().unwrap().insert(
            id,
            crate::runtime::ActiveTask {
                cancel: token,
                pause,
            },
        );
        Self { map, id }
    }
}

impl Drop for ActiveTaskGuard {
    fn drop(&mut self) {
        self.map.lock().unwrap().remove(&self.id);
    }
}

// ── impl ─────────────────────────────────────────────────────────────────────

impl Runtime {
    fn checkpoints(&self) -> CheckpointManager {
        CheckpointManager::new(self.store.clone())
    }

    /// Phase 10 M10.4 — crash recovery at boot. Called (fail-closed) by
    /// [`crate::RuntimeBuilder::build`] before the runtime is handed out.
    ///
    /// Reconciles: active tasks → [`TaskStatus::Interrupted`] (never
    /// successful), incomplete checkpoint chains → recorded resume points
    /// (planned, never executed), stale sandboxes → recorded for the
    /// operator. Persists a [`RecoveryRecord`] for every boot; findings also
    /// emit `RecoveryStarted` (phase `boot`) and append audit
    /// `recovery.boot`. Public for `openrt recovery scan` (on-demand
    /// re-scan — writes a fresh evidence record either way).
    pub async fn boot_recover(&self) -> Result<(), Error> {
        let boot_id = uuid::Uuid::new_v4().to_string();
        let record_id = format!("boot-{boot_id}");

        // 1. Stale tasks: active in the store, no live handle in this
        //    (freshly built) process → interrupted, never promoted.
        let mut interrupted_tasks = Vec::new();
        for mut task in self.store.list_tasks(None).await? {
            if !task.status.is_active() {
                continue;
            }
            // Defensive: never interrupt a task that somehow is already
            // running in this process (impossible at boot, cheap to honor).
            if self.active_tasks.lock().unwrap().contains_key(&task.id) {
                continue;
            }
            let previous_status = task.status;
            task.transition(TaskStatus::Interrupted)?;
            self.store.save_task(&task).await?;
            interrupted_tasks.push(InterruptedTaskInfo {
                task_id: task.id,
                session_id: task.session_id,
                previous_status,
            });
        }

        // 2. Incomplete runs: latest checkpoint per workflow still
        //    `Running` → safe resume point via the checkpoint planner.
        let mut incomplete_runs = Vec::new();
        let manager = self.checkpoints();
        let metas = manager.list(None).await?;
        let mut latest: HashMap<String, CheckpointMetadata> = HashMap::new();
        for meta in metas {
            match latest.get(&meta.workflow_id) {
                Some(cur) if meta.created_at <= cur.created_at => {}
                _ => {
                    latest.insert(meta.workflow_id.clone(), meta);
                }
            }
        }
        for meta in latest.into_values() {
            if meta.status != CheckpointStatus::Running {
                continue;
            }
            let checkpoint = manager.load(&meta.checkpoint_id).await?;
            // Steps = node states ∪ dependency-map keys (a crashed chain may
            // lack node entries for steps never reached).
            let mut steps: Vec<String> = checkpoint
                .node_states
                .iter()
                .map(|n| n.step.clone())
                .collect();
            for dep_key in checkpoint.dependency_state.deps_by_step.keys() {
                if !steps.iter().any(|s| s == dep_key) {
                    steps.push(dep_key.clone());
                }
            }
            steps.sort();
            let deps = checkpoint.dependency_state.deps_by_step.clone();
            let plan = plan_recovery(
                &checkpoint,
                &steps,
                &deps,
                &RecoveryPolicy::default(),
                RecoveryScope::Resume,
            )
            .map_err(Error::Checkpoint)?;
            incomplete_runs.push(IncompleteRunInfo {
                workflow_id: checkpoint.workflow_id.clone(),
                session_id: checkpoint.session_id,
                task_id: checkpoint.task_id,
                resume: RunResumePoint {
                    checkpoint_id: checkpoint.checkpoint_id,
                    plan_status: plan_status_str(plan.status).to_string(),
                    resume_nodes: plan.resume_nodes.clone(),
                    retry_nodes: plan.retry_nodes.clone(),
                    intervention_nodes: plan.intervention_nodes.clone(),
                    skip_nodes: plan.skip_nodes.clone(),
                },
            });
        }

        // 3. Stale sandboxes: non-terminal instances from a dead process —
        //    recorded only; boot recovery never destroys processes.
        let mut stale_sandboxes = Vec::new();
        let instances = self
            .sandbox
            .list()
            .await
            .map_err(|f| Error::Sandbox(f.to_string()))?;
        for instance in instances {
            if instance.ended_at.is_none()
                && matches!(
                    instance.state,
                    SandboxState::Created | SandboxState::Admitted | SandboxState::Running
                )
            {
                stale_sandboxes.push(StaleSandboxInfo {
                    sandbox_id: instance.id.to_string(),
                    profile: instance.profile,
                    state: instance.state,
                    pid: instance.pid,
                });
            }
        }

        // 4. Persist the report (evidence for every boot).
        let record = RecoveryRecord {
            record_id: record_id.clone(),
            boot_id,
            boot_at: now(),
            interrupted_tasks,
            incomplete_runs,
            stale_sandboxes,
        };
        let value = serde_json::to_value(&record).map_err(|e| {
            openrt_persistence::PersistenceError::Corrupt(RECOVERY_NS.into(), e.to_string())
        })?;
        self.store
            .put_record(RECOVERY_NS, &record_id, &value)
            .await?;

        // 5. Findings only: one boot-level recovery event + one chained
        //    audit row (fail-closed — the record above is already durable).
        if !record.is_clean() {
            self.bus
                .emit_correlated(
                    EventKind::RecoveryStarted,
                    None,
                    None,
                    None,
                    serde_json::json!({
                        "phase": "boot",
                        "record_id": record.record_id,
                        "interrupted_tasks": record.interrupted_tasks.len(),
                        "incomplete_runs": record.incomplete_runs.len(),
                        "stale_sandboxes": record.stale_sandboxes.len(),
                    }),
                )
                .await;
            let actor = self
                .asset_operators()
                .first()
                .cloned()
                .unwrap_or_else(|| "system".to_string());
            self.audit
                .append(
                    &actor,
                    "recovery.boot",
                    &record.record_id,
                    "completed",
                    serde_json::json!({
                        "interrupted_tasks": record.interrupted_tasks.len(),
                        "incomplete_runs": record.incomplete_runs.len(),
                        "stale_sandboxes": record.stale_sandboxes.len(),
                    }),
                )
                .await?;
        }
        Ok(())
    }

    /// All boot recovery reports, oldest first.
    pub async fn recovery_records(&self) -> Result<Vec<RecoveryRecord>, Error> {
        let raw = self.store.list_records(RECOVERY_NS).await?;
        let mut records = Vec::with_capacity(raw.len());
        for (key, value) in raw {
            let record: RecoveryRecord = serde_json::from_value(value)
                .map_err(|e| openrt_persistence::PersistenceError::Corrupt(key, e.to_string()))?;
            records.push(record);
        }
        records.sort_by_key(|r| r.boot_at);
        Ok(records)
    }

    // ---- live task handles (M10.4) ----

    /// Full inspection of one task: persisted state + in-process liveness +
    /// any recovery record that interrupted it.
    pub async fn task_inspect(&self, id: TaskId) -> Result<TaskInspection, Error> {
        let task = self.store.load_task(id).await.map_err(map_load_task(id))?;
        let in_process = self.active_tasks.lock().unwrap().contains_key(&id);
        let recovery_record = self
            .recovery_records()
            .await?
            .into_iter()
            .rev()
            .find(|r| r.interrupted_tasks.iter().any(|t| t.task_id == id))
            .map(|r| r.record_id);
        let workflow_backed = self
            .checkpoints()
            .list(None)
            .await?
            .iter()
            .any(|m| m.task_id == Some(id));
        Ok(TaskInspection {
            task,
            in_process,
            recovery_record,
            workflow_backed,
        })
    }

    /// Persisted status of one task.
    pub async fn task_status(&self, id: TaskId) -> Result<TaskStatus, Error> {
        Ok(self
            .store
            .load_task(id)
            .await
            .map_err(map_load_task(id))?
            .status)
    }

    /// Cooperative cancellation of a live in-process task: trips its token
    /// and returns the persisted state. Terminal/interrupted tasks are a
    /// no-op; an active task with no live handle here is refused (fail-closed
    /// — another process owns it, or it awaits boot reconciliation).
    pub async fn task_cancel(&self, id: TaskId) -> Result<openrt_core::Task, Error> {
        if let Some(active) = self.active_tasks.lock().unwrap().get(&id) {
            active.cancel.cancel();
            return self.store.load_task(id).await.map_err(map_load_task(id));
        }
        let task = self.store.load_task(id).await.map_err(map_load_task(id))?;
        if task.status.is_active() {
            return Err(Error::Task(openrt_core::TaskError::StaleActive(id)));
        }
        Ok(task)
    }

    /// Part C: cooperative pause of a live in-process task. Trips its
    /// *resettable* pause signal; the agent stops at its next turn boundary
    /// and persists the task as `Paused` (not terminal — tool rounds are
    /// folded into the session). A task with no live handle here that is
    /// still active is refused (fail-closed — another process owns it);
    /// already-paused and terminal tasks are a no-op returning their state.
    pub async fn task_pause(&self, id: TaskId) -> Result<openrt_core::Task, Error> {
        if let Some(active) = self.active_tasks.lock().unwrap().get(&id) {
            active.pause.pause();
            return self.store.load_task(id).await.map_err(map_load_task(id));
        }
        let task = self.store.load_task(id).await.map_err(map_load_task(id))?;
        if task.status.is_active() {
            return Err(Error::Task(openrt_core::TaskError::StaleActive(id)));
        }
        Ok(task)
    }

    /// Part C: resume a task, `Paused` or `Interrupted`.
    ///
    /// * Live in this process and paused: clears the in-process pause signal
    ///   so the same run continues cooperatively.
    /// * Durably `Paused`/`Interrupted` with no live handle: re-runs the task
    ///   from its preserved transcript (the `task_recover` path).
    /// * Terminal: refused (`IllegalTransition`), never re-run.
    pub async fn task_resume(&self, id: TaskId) -> Result<openrt_core::Task, Error> {
        if let Some(active) = self.active_tasks.lock().unwrap().get(&id) {
            active.pause.resume();
            return self.store.load_task(id).await.map_err(map_load_task(id));
        }
        let task = self.store.load_task(id).await.map_err(map_load_task(id))?;
        match task.status {
            TaskStatus::Paused | TaskStatus::Interrupted => self.task_recover(id).await,
            s if s.is_terminal() => Err(Error::Task(openrt_core::TaskError::IllegalTransition {
                from: s,
                to: TaskStatus::Running,
            })),
            _s => Err(Error::Task(openrt_core::TaskError::StaleActive(id))),
        }
    }

    /// Terminate a task now: trips the in-process token (if any) and
    /// durably marks it `Cancelled` — the operator's record wins over an
    /// in-flight run's own bookkeeping.
    pub async fn task_terminate(&self, id: TaskId) -> Result<openrt_core::Task, Error> {
        if let Some(active) = self.active_tasks.lock().unwrap().get(&id) {
            active.cancel.cancel();
        }
        let mut task = self.store.load_task(id).await.map_err(map_load_task(id))?;
        if !task.status.is_terminal() {
            task.transition(TaskStatus::Cancelled)?;
            self.store.save_task(&task).await?;
            self.bus
                .emit_correlated(
                    EventKind::TaskFailed,
                    Some(task.session_id),
                    Some(task.id),
                    None,
                    serde_json::json!({ "reason": "terminated", "error": task.error }),
                )
                .await;
        }
        Ok(task)
    }

    /// Watch a task until it reaches a terminal state or `timeout` elapses
    /// (`TaskError::Timeout`).
    pub async fn task_observe(
        &self,
        id: TaskId,
        timeout: std::time::Duration,
    ) -> Result<openrt_core::Task, Error> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let task = self.store.load_task(id).await.map_err(map_load_task(id))?;
            if task.status.is_terminal() {
                return Ok(task);
            }
            if std::time::Instant::now() >= deadline {
                return Err(Error::Task(openrt_core::TaskError::Timeout));
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// The task's recorded turns (model/tool/output history).
    pub async fn task_logs(&self, id: TaskId) -> Result<Vec<TurnRecord>, Error> {
        let task = self.store.load_task(id).await.map_err(map_load_task(id))?;
        Ok(task.turns)
    }

    /// Artifacts the task produced: latest linked checkpoint refs +
    /// blackboard `output.*` entries (workflow-backed), plus the plain task
    /// result.
    pub async fn task_artifacts(&self, id: TaskId) -> Result<Vec<TaskArtifact>, Error> {
        let task = self.store.load_task(id).await.map_err(map_load_task(id))?;
        let mut out = Vec::new();
        let manager = self.checkpoints();
        let linked: Vec<CheckpointMetadata> = manager
            .list(None)
            .await?
            .into_iter()
            .filter(|m| m.task_id == Some(id))
            .collect();
        if let Some(latest) = linked.into_iter().max_by_key(|m| m.created_at) {
            let checkpoint = manager.load(&latest.checkpoint_id).await?;
            for r in &checkpoint.artifact_refs {
                out.push(TaskArtifact {
                    kind: r.kind.clone(),
                    key: r.key.clone(),
                    path: r.path.clone(),
                    snapshot_id: r.snapshot_id.clone(),
                    value: None,
                });
            }
            for (key, value) in &checkpoint.blackboard.entries {
                if key.starts_with("output.") {
                    out.push(TaskArtifact {
                        kind: "blackboard_output".into(),
                        key: Some(key.clone()),
                        path: None,
                        snapshot_id: None,
                        value: Some(value.clone()),
                    });
                }
            }
        }
        if let Some(result) = &task.result {
            out.push(TaskArtifact {
                kind: "final_output".into(),
                key: None,
                path: None,
                snapshot_id: None,
                value: Some(serde_json::Value::String(result.clone())),
            });
        }
        Ok(out)
    }

    /// Recover an `Interrupted` or `Paused` task by re-running it in this
    /// process. (`Paused` — Part C — is not active and not terminal: an
    /// operator resume re-runs the same prompt over the preserved transcript.)
    ///
    /// Refuses: terminal tasks (illegal transition), tasks with a live
    /// handle here (`AlreadyRunning` — use `task_resume` to clear the
    /// in-process pause signal), active tasks without one (`StaleActive` —
    /// restart to reconcile), and workflow-backed tasks
    /// (`CheckpointError::Refused` — the checkpoint chain is the recovery
    /// authority; use `resume_workflow` so uncertain steps are never
    /// blindly re-executed).
    pub async fn task_recover(&self, id: TaskId) -> Result<openrt_core::Task, Error> {
        let task = self.store.load_task(id).await.map_err(map_load_task(id))?;
        if task.status.is_terminal() {
            return Err(Error::Task(openrt_core::TaskError::IllegalTransition {
                from: task.status,
                to: TaskStatus::Running,
            }));
        }
        if self.active_tasks.lock().unwrap().contains_key(&id) {
            return Err(Error::Task(openrt_core::TaskError::AlreadyRunning(id)));
        }
        if task.status.is_active() {
            return Err(Error::Task(openrt_core::TaskError::StaleActive(id)));
        }
        // task.status == Interrupted | Paused from here.
        if self
            .checkpoints()
            .list(None)
            .await?
            .iter()
            .any(|m| m.task_id == Some(id))
        {
            return Err(Error::Checkpoint(CheckpointError::Refused(format!(
                "task {id} is workflow-backed; resume its checkpoint chain instead (recovery resume)"
            ))));
        }

        let _guard = self.session_locks.lock(task.session_id).await;
        let session = self.store.load_session(task.session_id).await?;
        let cancel = CancellationToken::new();
        let pause = std::sync::Arc::new(PauseToken::new());
        let prompt = task.prompt.clone();
        let started = std::time::Instant::now();
        let active =
            ActiveTaskGuard::register(self.active_tasks.clone(), id, cancel.clone(), pause.clone());
        let budget = self.task_execution_budget();
        let done = self
            .snapshot_pipeline()
            .run_supervised(&session, task, &prompt, &cancel, Some(pause), budget)
            .await;
        drop(active);
        let final_task = done?;
        // Same production bookkeeping as `run_task` (M5.7 metric + M5.4
        // genome consumption) — a recovered run is a real run.
        self.metrics_observe_task(started.elapsed().as_secs_f64() * 1000.0)
            .await?;
        self.consume_active_genome_on_run().await?;
        Ok(final_task)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use openrt_core::{DomainEvent, Task};
    use openrt_models::mock::MockProvider;
    use openrt_persistence::{MemoryStore, PersistenceStore};
    use openrt_telemetry::ChannelSink;

    async fn build(store: Arc<MemoryStore>) -> (Arc<Runtime>, Arc<ChannelSink>) {
        let sink = Arc::new(ChannelSink::new());
        let rt = crate::RuntimeBuilder::new()
            .config(openrt_config::Config::default())
            .store(store)
            .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
            .sink(sink.clone())
            .bus_capacity(32)
            .build()
            .await
            .unwrap();
        (rt, sink)
    }

    async fn wait_for(sink: &ChannelSink, kind: EventKind, n: usize) -> Vec<DomainEvent> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let events = sink.snapshot();
            let count = events.iter().filter(|e| e.kind == kind).count();
            if count >= n || std::time::Instant::now() > deadline {
                return events;
            }
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    fn running_task(session_id: SessionId, prompt: &str) -> Task {
        let mut task = Task::new(session_id, prompt);
        task.transition(TaskStatus::Running).unwrap();
        task
    }

    #[tokio::test]
    async fn boot_recovery_interrupts_stale_tasks_and_records_them() {
        let store = Arc::new(MemoryStore::new());
        let stale = running_task(SessionId::new(), "crashed mid-run");
        store.save_task(&stale).await.unwrap();
        let mut failed = Task::new(SessionId::new(), "already finished");
        failed.transition(TaskStatus::Running).unwrap();
        failed.transition(TaskStatus::Failed).unwrap();
        store.save_task(&failed).await.unwrap();

        let (rt, sink) = build(store.clone()).await;

        // Stale task reconciled — never claimed successful.
        let tasks = store.list_tasks(None).await.unwrap();
        assert_eq!(tasks.len(), 2);
        let stale_after = tasks.iter().find(|t| t.id == stale.id).unwrap();
        assert_eq!(stale_after.status, TaskStatus::Interrupted);
        assert!(stale_after.completed_at.is_none());
        let failed_after = tasks.iter().find(|t| t.id == failed.id).unwrap();
        assert_eq!(failed_after.status, TaskStatus::Failed);
        assert!(failed_after.completed_at.is_some());

        // Report persisted + boot event emitted.
        let records = rt.recovery_records().await.unwrap();
        assert_eq!(records.len(), 1);
        let record = records.last().unwrap();
        assert!(!record.is_clean());
        assert_eq!(record.total_findings(), 1);
        assert_eq!(record.interrupted_tasks.len(), 1);
        assert_eq!(record.interrupted_tasks[0].task_id, stale.id);
        assert_eq!(
            record.interrupted_tasks[0].previous_status,
            TaskStatus::Running
        );
        let events = wait_for(&sink, EventKind::RecoveryStarted, 1).await;
        let boot = events
            .iter()
            .find(|e| e.kind == EventKind::RecoveryStarted)
            .unwrap();
        assert_eq!(boot.data["phase"], "boot");
        assert_eq!(boot.data["record_id"], record.record_id.clone());

        // Inspection links the task to the record.
        let inspection = rt.task_inspect(stale.id).await.unwrap();
        assert_eq!(inspection.task.status, TaskStatus::Interrupted);
        assert!(!inspection.in_process);
        assert_eq!(
            inspection.recovery_record.as_deref(),
            Some(record.record_id.as_str())
        );
    }

    #[tokio::test]
    async fn boot_recovery_clean_boot_records_evidence_without_noise() {
        let store = Arc::new(MemoryStore::new());
        let (rt, sink) = build(store).await;
        let records = rt.recovery_records().await.unwrap();
        assert_eq!(records.len(), 1);
        assert!(records[0].is_clean());
        assert_eq!(records[0].total_findings(), 0);

        // Wait until the dispatcher has demonstrably delivered the boot
        // event stream, then prove no RecoveryStarted was emitted.
        wait_for(&sink, EventKind::RuntimeStarted, 1).await;
        for _ in 0..30 {
            let events = sink.snapshot();
            assert!(
                !events.iter().any(|e| e.kind == EventKind::RecoveryStarted),
                "clean boot must not emit RecoveryStarted"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn live_handle_spawn_cancel_execute_lifecycle() {
        let store = Arc::new(MemoryStore::new());
        let (rt, _sink) = build(store).await;
        let session = rt.create_session("live").await.unwrap();
        let handle = rt.spawn_task(session.id, "please cancel me").await.unwrap();
        let id = handle.task_id();

        // Pre-execute: the task is persisted and registered in-process.
        let inspection = rt.task_inspect(id).await.unwrap();
        assert_eq!(inspection.task.status, TaskStatus::Created);
        assert!(inspection.in_process);
        assert!(!inspection.workflow_backed);
        assert_eq!(rt.task_status(id).await.unwrap(), TaskStatus::Created);

        // Cancel through the runtime handle authority (trips the token).
        let cancelled = rt.task_cancel(id).await.unwrap();
        assert_eq!(cancelled.id, id);

        // Execute honours the pre-tripped token: terminal Cancelled.
        let done = handle.execute().await.unwrap();
        assert_eq!(done.status, TaskStatus::Cancelled);
        // Handle dropped → deregistered.
        let inspection = rt.task_inspect(id).await.unwrap();
        assert!(!inspection.in_process);
        assert_eq!(rt.task_status(id).await.unwrap(), TaskStatus::Cancelled);
    }

    #[tokio::test]
    async fn cancel_refuses_stale_active_terminate_records_it() {
        let store = Arc::new(MemoryStore::new());
        let (rt, _sink) = build(store.clone()).await;
        let session = rt.create_session("stale").await.unwrap();
        let stale = running_task(session.id, "phantom run");
        store.save_task(&stale).await.unwrap();

        // Active with no live handle here → fail-closed refusal.
        let err = rt.task_cancel(stale.id).await.unwrap_err();
        assert!(matches!(
            err,
            Error::Task(openrt_core::TaskError::StaleActive(_))
        ));

        // Terminate is the operator's force path: durable Cancelled.
        let terminated = rt.task_terminate(stale.id).await.unwrap();
        assert_eq!(terminated.status, TaskStatus::Cancelled);
        let persisted = rt.task_status(stale.id).await.unwrap();
        assert_eq!(persisted, TaskStatus::Cancelled);
        // Terminal terminate is idempotent.
        let again = rt.task_terminate(stale.id).await.unwrap();
        assert_eq!(again.status, TaskStatus::Cancelled);
    }

    #[tokio::test]
    async fn task_recover_re_runs_interrupted_task_to_completion() {
        let store = Arc::new(MemoryStore::new());
        let (rt, _sink) = build(store.clone()).await;
        let session = rt.create_session("recover").await.unwrap();
        let mut task = running_task(session.id, "resume this");
        task.transition(TaskStatus::Interrupted).unwrap();
        assert!(task.completed_at.is_none());
        store.save_task(&task).await.unwrap();

        let done = rt.task_recover(task.id).await.unwrap();
        assert_eq!(done.status, TaskStatus::Completed);
        assert!(done.completed_at.is_some());
        let persisted = rt.task_status(task.id).await.unwrap();
        assert_eq!(persisted, TaskStatus::Completed);
        // Registry cleaned up after the recovered run.
        let inspection = rt.task_inspect(task.id).await.unwrap();
        assert!(!inspection.in_process);
        // Turn history was recorded by the pipeline.
        let logs = rt.task_logs(task.id).await.unwrap();
        assert!(!logs.is_empty());
    }

    #[tokio::test]
    async fn task_recover_refuses_live_terminal_and_stale_active() {
        let store = Arc::new(MemoryStore::new());
        let (rt, _sink) = build(store.clone()).await;
        let session = rt.create_session("refuse").await.unwrap();

        // Live in this process.
        let handle = rt.spawn_task(session.id, "live").await.unwrap();
        let spawned_id = handle.task_id();
        let err = rt.task_recover(spawned_id).await.unwrap_err();
        assert!(matches!(
            err,
            Error::Task(openrt_core::TaskError::AlreadyRunning(_))
        ));
        drop(handle);

        // Dropped handle: task still Created (active), no token → stale.
        let err = rt.task_recover(spawned_id).await.unwrap_err();
        assert!(matches!(
            err,
            Error::Task(openrt_core::TaskError::StaleActive(_))
        ));

        // Terminal.
        let finished = rt.run_task(session.id, "all done").await.unwrap();
        let err = rt.task_recover(finished.id).await.unwrap_err();
        assert!(matches!(
            err,
            Error::Task(openrt_core::TaskError::IllegalTransition { .. })
        ));
    }

    #[tokio::test]
    async fn task_recover_refuses_workflow_backed_task() {
        let store = Arc::new(MemoryStore::new());
        let (rt, _sink) = build(store.clone()).await;
        let session = rt.create_session("wf-backed").await.unwrap();
        let mut task = running_task(session.id, "workflow task");
        task.transition(TaskStatus::Interrupted).unwrap();
        store.save_task(&task).await.unwrap();

        // A checkpoint linked to the task makes its chain the recovery
        // authority — task_recover must refuse (no blind re-execution).
        let cp = openrt_checkpoint::Checkpoint {
            schema_version: openrt_checkpoint::CHECKPOINT_SCHEMA_VERSION,
            checkpoint_id: CheckpointId::new(),
            session_id: session.id,
            task_id: Some(task.id),
            workflow_id: "wf-linked".into(),
            created_at: now(),
            kind: openrt_checkpoint::CheckpointKind::Initial,
            status: CheckpointStatus::Running,
            node_states: vec![openrt_checkpoint::NodeStatus {
                step: "plan".into(),
                state: openrt_checkpoint::NodeState::Pending,
                attempts: 0,
                artifact_key: None,
                error: None,
                failure_category: String::new(),
                effect: openrt_checkpoint::NodeEffect::Unknown,
            }],
            completed: vec![],
            pending: vec!["plan".into()],
            failed: vec![],
            uncertain: vec![],
            current_node: None,
            dependency_state: openrt_checkpoint::DependencyState::default(),
            retry_counters: HashMap::new(),
            blackboard: Default::default(),
            artifact_refs: vec![],
            execution: openrt_checkpoint::CheckpointExecution::default(),
            refs: openrt_checkpoint::RunRefs::default(),
            digest: None,
        };
        let manager = openrt_checkpoint::CheckpointManager::new(store.clone());
        manager.create(&cp).await.unwrap();

        let inspection = rt.task_inspect(task.id).await.unwrap();
        assert!(inspection.workflow_backed);
        let err = rt.task_recover(task.id).await.unwrap_err();
        match err {
            Error::Checkpoint(CheckpointError::Refused(msg)) => {
                assert!(msg.contains("workflow-backed"), "{msg}");
            }
            other => panic!("expected workflow-backed refusal, got {other}"),
        }
    }

    #[tokio::test]
    async fn observe_returns_terminal_and_times_out_on_active() {
        let store = Arc::new(MemoryStore::new());
        let (rt, _sink) = build(store.clone()).await;
        let session = rt.create_session("observe").await.unwrap();

        let finished = rt.run_task(session.id, "observe me").await.unwrap();
        let seen = rt
            .task_observe(finished.id, std::time::Duration::from_millis(500))
            .await
            .unwrap();
        assert_eq!(seen.id, finished.id);
        assert!(seen.status.is_terminal());

        let active = running_task(session.id, "never finishes here");
        store.save_task(&active).await.unwrap();
        let err = rt
            .task_observe(active.id, std::time::Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Task(openrt_core::TaskError::Timeout)));
    }

    #[tokio::test]
    async fn logs_and_artifacts_for_plain_task() {
        let store = Arc::new(MemoryStore::new());
        let (rt, _sink) = build(store).await;
        let session = rt.create_session("logs").await.unwrap();
        let task = rt.run_task(session.id, "produce output").await.unwrap();

        let logs = rt.task_logs(task.id).await.unwrap();
        assert_eq!(logs.len(), task.turns.len());
        assert!(!logs.is_empty());

        let artifacts = rt.task_artifacts(task.id).await.unwrap();
        let final_output = artifacts
            .iter()
            .find(|a| a.kind == "final_output")
            .expect("plain task exposes its result as a final_output artifact");
        let value = final_output.value.as_ref().unwrap();
        assert_eq!(
            value,
            &serde_json::Value::String(task.result.clone().unwrap_or_default())
        );
    }

    #[tokio::test]
    async fn task_pause_persists_and_task_resume_re_runs_to_completion() {
        let store = Arc::new(MemoryStore::new());
        let (rt, sink) = build(store).await;
        let session = rt.create_session("pause").await.unwrap();

        // Pre-engage the pause signal through the runtime's live-handle
        // authority before executing: the task starts, then stops at the
        // first turn boundary with a durable Paused status (never terminal).
        let handle = rt.spawn_task(session.id, "pause me").await.unwrap();
        let id = handle.task_id();
        let signalled = rt.task_pause(id).await.unwrap();
        assert_eq!(signalled.id, id);
        let done = handle.execute().await.unwrap();
        assert_eq!(done.status, TaskStatus::Paused);
        assert!(done.completed_at.is_none());
        assert_eq!(rt.task_status(id).await.unwrap(), TaskStatus::Paused);
        // The input turn was folded into the persisted session so the resume
        // re-runs the same prompt without duplicating the instruction.
        let persisted = rt.task_inspect(id).await.unwrap();
        assert!(!persisted.in_process);
        wait_for(&sink, EventKind::TaskPaused, 1).await;

        // Resume through the recovery path: Paused is recoverable, the task
        // re-runs to a real completion.
        let resumed = rt.task_resume(id).await.unwrap();
        assert_eq!(resumed.status, TaskStatus::Completed);
        assert!(resumed.completed_at.is_some());
        assert!(
            resumed
                .result
                .as_deref()
                .is_some_and(|r| !r.trim().is_empty()),
            "the resumed task produced a final answer"
        );
        assert_eq!(rt.task_status(id).await.unwrap(), TaskStatus::Completed);
        let events = wait_for(&sink, EventKind::TaskResumed, 1).await;
        assert!(
            events.iter().any(|e| e.kind == EventKind::TaskCompleted),
            "resume completes, it does not recreate the task"
        );
    }

    #[tokio::test]
    async fn task_resume_refuses_terminal_task() {
        let store = Arc::new(MemoryStore::new());
        let (rt, _sink) = build(store).await;
        let session = rt.create_session("terminal").await.unwrap();
        let finished = rt.run_task(session.id, "all done").await.unwrap();
        let err = rt.task_resume(finished.id).await.unwrap_err();
        assert!(matches!(
            err,
            Error::Task(openrt_core::TaskError::IllegalTransition { .. })
        ));
    }

    #[tokio::test]
    async fn handle_resume_clears_live_pause_signal() {
        let store = Arc::new(MemoryStore::new());
        let (rt, _sink) = build(store).await;
        let session = rt.create_session("live-resume").await.unwrap();
        let handle = rt.spawn_task(session.id, "live").await.unwrap();
        handle.pause();
        // Clear the pre-engaged signal on the same in-flight handle; the run
        // must then proceed to a real completion (no Paused stop).
        handle.resume();
        let done = handle.execute().await.unwrap();
        assert_eq!(done.status, TaskStatus::Completed);
    }
}
