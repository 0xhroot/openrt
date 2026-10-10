//! Integration (P2.6): checkpoint + recovery end-to-end through the real
//! runtime, its checkpoint chain (`Initial → AfterNode… → Final`), and the
//! recovery planner.
//!
//! Scenarios:
//! 1. restart-resume: a failed run is resumed from its Final checkpoint by a
//!    *fresh* runtime over the same store; the chain grows and cumulative
//!    attempt budgets carry across the restart.
//! 2. node-failure → bounded retry: a node exhausting its run-internal
//!    budget is re-attempted on resume, respecting the policy cap
//!    (`RetryExhausted` then `RetryScheduled`).
//! 3. uncertain non-idempotent outcome → operator intervention, never a
//!    blind replay (the checkpoint uncertain-outcome safety invariant).
//! 4. ADR-008: a halt blocks resume; `clear_halt` restores recovery.
//! 5. a store that fails while writing an AfterNode checkpoint aborts the
//!    run and preserves the prior checkpoint chain.
//! 6. dry-run produces a report with zero side effects on the store.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use openrt_audit::AuditQuery;
use openrt_blackboard::{Blackboard, BlackboardSnapshot, BLACKBOARD_NS};
use openrt_checkpoint::{
    plan_recovery, Checkpoint, CheckpointExecution, CheckpointManager, CheckpointStatus,
    DependencyState, NodeEffect, NodeState, NodeStatus, PolicyCompat, RecoveryPolicy,
    RecoveryScope, RollbackAuthorization, RunRefs, CHECKPOINT_SCHEMA_VERSION,
};
use openrt_core::{now, EventKind, ExecutionId, ExecutionRecord, Session, SessionId, Task, TaskId};
use openrt_models::error::ModelError;
use openrt_models::mock::{MockMode, MockProvider};
use openrt_models::provider::{ModelCapabilities, ModelProvider};
use openrt_models::types::{ModelInfo, ModelRequest, ModelResponse};
use openrt_orch::{SubAgentKind, Workflow, WorkflowStatus, WorkflowStep, RUNS_NS};
use openrt_persistence::{LocalFileStore, MemoryStore, PersistenceError, PersistenceStore};
use openrt_telemetry::{ChannelSink, DomainEvent};

fn tmp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "openrt-p2_6-{prefix}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Poll the sink until `n` events of `kind` are visible (the bus drains
/// asynchronously). Returns the full snapshot.
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

/// Fails the first `n` model invocations with a *retryable* transport error,
/// then delegates to an echo provider. Deterministic and network-free.
struct FlakyProvider {
    inner: MockProvider,
    fail_remaining: AtomicUsize,
}

impl FlakyProvider {
    fn new(fail_times: usize) -> Self {
        Self {
            inner: MockProvider::echo("flaky"),
            fail_remaining: AtomicUsize::new(fail_times),
        }
    }
}

#[async_trait::async_trait]
impl ModelProvider for FlakyProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn info(&self) -> ModelInfo {
        self.inner.info()
    }
    async fn generate(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        if self.fail_remaining.load(Ordering::SeqCst) > 0 {
            self.fail_remaining.fetch_sub(1, Ordering::SeqCst);
            return Err(ModelError::Transport("flaky network".into()));
        }
        self.inner.generate(req).await
    }
    fn capabilities(&self) -> ModelCapabilities {
        self.inner.capabilities()
    }
}

/// A store that delegates everything to a `MemoryStore` but fails writes to
/// the `checkpoints` namespace after the very first one — so the Initial
/// checkpoint survives but the first AfterNode write fails.
struct FailCheckpointStore {
    inner: Arc<MemoryStore>,
    cps: AtomicUsize,
}

impl FailCheckpointStore {
    fn new() -> Self {
        Self {
            inner: Arc::new(MemoryStore::new()),
            cps: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl PersistenceStore for FailCheckpointStore {
    async fn create_session(&self, session: &Session) -> Result<(), PersistenceError> {
        self.inner.create_session(session).await
    }
    async fn update_session(&self, session: &Session) -> Result<(), PersistenceError> {
        self.inner.update_session(session).await
    }
    async fn load_session(&self, id: SessionId) -> Result<Session, PersistenceError> {
        self.inner.load_session(id).await
    }
    async fn list_sessions(&self) -> Result<Vec<Session>, PersistenceError> {
        self.inner.list_sessions().await
    }
    async fn save_task(&self, task: &Task) -> Result<(), PersistenceError> {
        self.inner.save_task(task).await
    }
    async fn load_task(&self, id: TaskId) -> Result<Task, PersistenceError> {
        self.inner.load_task(id).await
    }
    async fn list_tasks(
        &self,
        session_id: Option<SessionId>,
    ) -> Result<Vec<Task>, PersistenceError> {
        self.inner.list_tasks(session_id).await
    }
    async fn save_execution(&self, record: &ExecutionRecord) -> Result<(), PersistenceError> {
        self.inner.save_execution(record).await
    }
    async fn load_execution(&self, id: ExecutionId) -> Result<ExecutionRecord, PersistenceError> {
        self.inner.load_execution(id).await
    }
    async fn append_event(&self, event: &DomainEvent) -> Result<(), PersistenceError> {
        self.inner.append_event(event).await
    }
    async fn list_events(
        &self,
        opts: openrt_persistence::ListOptions,
    ) -> Result<Vec<DomainEvent>, PersistenceError> {
        self.inner.list_events(opts).await
    }
    async fn clear(&self) -> Result<(), PersistenceError> {
        self.inner.clear().await
    }
    async fn delete_record(&self, namespace: &str, key: &str) -> Result<(), PersistenceError> {
        self.inner.delete_record(namespace, key).await
    }
    async fn list_records(
        &self,
        namespace: &str,
    ) -> Result<Vec<(String, serde_json::Value)>, PersistenceError> {
        self.inner.list_records(namespace).await
    }
    async fn get_record(
        &self,
        namespace: &str,
        key: &str,
    ) -> Result<Option<serde_json::Value>, PersistenceError> {
        self.inner.get_record(namespace, key).await
    }
    async fn put_record(
        &self,
        namespace: &str,
        key: &str,
        value: &serde_json::Value,
    ) -> Result<(), PersistenceError> {
        if namespace == "checkpoints" && self.cps.fetch_add(1, Ordering::SeqCst) > 0 {
            return Err(PersistenceError::NotInitialized(
                "injected checkpoint write failure".into(),
            ));
        }
        self.inner.put_record(namespace, key, value).await
    }
}

fn dep_workflow(id: &str) -> Workflow {
    Workflow::new(id)
        .step(WorkflowStep::new(
            "plan",
            SubAgentKind::Planner,
            "layout the change",
        ))
        .step(WorkflowStep::new("code", SubAgentKind::Coder, "implement it").dep("plan"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_resume_from_final_checkpoint_across_runtime() {
    let dir = tmp_dir("restart");
    let store: Arc<dyn PersistenceStore> = Arc::new(LocalFileStore::open(&dir).await.unwrap());

    // Run 1: a fresh runtime that always fails at the provider level.
    let sink_a = Arc::new(ChannelSink::new());
    let rt_a = openrt_runtime::RuntimeBuilder::new()
        .store(store.clone())
        .with_provider(Arc::new(MockProvider::new("fail", MockMode::Fail)))
        .sink(sink_a.clone())
        .build()
        .await
        .unwrap();
    let session = rt_a.create_session("restart-resume").await.unwrap();

    let wf = dep_workflow("wf-restart");
    let first = rt_a.run_workflow(session.id, wf.clone()).await.unwrap();
    assert_eq!(first.status, WorkflowStatus::Failed);

    let cps = rt_a.list_checkpoints(Some("wf-restart")).await.unwrap();
    assert_eq!(
        cps.len(),
        4,
        "Initial + Scheduled(spawn) + AfterNode + Final for the failed run"
    );
    let [initial, scheduled, after, final_] = cps.as_slice() else {
        panic!("expected exactly 4 checkpoints");
    };
    assert_eq!(initial.kind, openrt_checkpoint::CheckpointKind::Initial);
    assert_eq!(scheduled.kind, openrt_checkpoint::CheckpointKind::Scheduled);
    assert_eq!(after.kind, openrt_checkpoint::CheckpointKind::AfterNode);
    assert_eq!(final_.kind, openrt_checkpoint::CheckpointKind::Final);
    assert_eq!(final_.status, CheckpointStatus::Failed);

    // Chain integrity: each checkpoint links to the previous one.
    let scheduled_cp = rt_a
        .load_checkpoint(&scheduled.checkpoint_id)
        .await
        .unwrap();
    let after_cp = rt_a.load_checkpoint(&after.checkpoint_id).await.unwrap();
    let final_cp = rt_a.load_checkpoint(&final_.checkpoint_id).await.unwrap();
    assert_eq!(
        scheduled_cp.execution.previous_checkpoint_id.as_ref(),
        Some(&initial.checkpoint_id)
    );
    assert_eq!(
        after_cp.execution.previous_checkpoint_id.as_ref(),
        Some(&scheduled.checkpoint_id)
    );
    assert_eq!(
        final_cp.execution.previous_checkpoint_id.as_ref(),
        Some(&after.checkpoint_id)
    );
    rt_a.shutdown().await;

    // Run 2: a *fresh* runtime (process-restart equivalent) over the same
    // store with a working provider, resuming from the Final checkpoint.
    let sink_b = Arc::new(ChannelSink::new());
    let rt_b = openrt_runtime::RuntimeBuilder::new()
        .store(store.clone())
        .with_provider(Arc::new(MockProvider::echo("ok")))
        .sink(sink_b.clone())
        .build()
        .await
        .unwrap();

    let resumed = rt_b
        .resume_workflow(
            session.id,
            wf.clone(),
            final_.checkpoint_id,
            openrt_runtime::CheckpointOptions::default(),
            RecoveryPolicy::default(),
        )
        .await
        .unwrap();
    assert_eq!(resumed.status, WorkflowStatus::Completed);
    assert_eq!(resumed.steps.len(), 2);

    // Cumulative attempt budgets cross the restart: `plan` consumed 1 attempt
    // in run 1 (exhausted its single attempt; no extra retries configured) and
    // exactly 1 fresh attempt on resume; `code` had never run and starts at 0.
    let by_step: HashMap<_, _> = resumed.steps.iter().map(|s| (s.step.as_str(), s)).collect();
    assert_eq!(by_step["plan"].status, WorkflowStatus::Completed);
    assert_eq!(by_step["plan"].attempts, 2);
    assert_eq!(by_step["code"].status, WorkflowStatus::Completed);
    assert_eq!(by_step["code"].attempts, 1);

    // The resumed run's blackboard carries the fresh outputs.
    let bb = rt_b.blackboard().await.unwrap();
    assert!(bb.get("output.plan").unwrap().is_some());
    assert!(bb.get("output.code").unwrap().is_some());

    // Recovery events are emitted and correlated to the task.
    let events = wait_for(&sink_b, EventKind::WorkflowCompleted, 1).await;
    let kinds: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
    let pos = |k: EventKind| {
        kinds
            .iter()
            .position(|e| *e == k)
            .unwrap_or_else(|| panic!("{k:?} missing from {kinds:?}"))
    };
    assert!(pos(EventKind::RecoveryStarted) < pos(EventKind::WorkflowResumed));
    assert!(pos(EventKind::WorkflowResumed) < pos(EventKind::RetryScheduled));
    assert!(pos(EventKind::RetryScheduled) < pos(EventKind::WorkflowCompleted));
    let retry = events
        .iter()
        .find(|e| e.kind == EventKind::RetryScheduled)
        .unwrap();
    assert_eq!(retry.data["attempt"], 2, "prior 1 attempt + 1 fresh retry");
    assert_eq!(retry.data["node"], "plan");

    // The checkpoint chain grew: 4 from the failed run (Initial, Scheduled,
    // AfterNode, Final) + 6 from the resumed run (Initial + one Scheduled and
    // one AfterNode per node + Final), all linked.
    let cps = rt_b.list_checkpoints(Some("wf-restart")).await.unwrap();
    assert_eq!(cps.len(), 10);
    let mut prev = None;
    for meta in &cps {
        let cp = rt_b.load_checkpoint(&meta.checkpoint_id).await.unwrap();
        assert_eq!(cp.execution.previous_checkpoint_id, prev);
        prev = Some(meta.checkpoint_id);
    }

    // A second resume of the *completed* run is a skip-only no-op: every step
    // is already completed with its cumulative attempts preserved.
    let cps = rt_b.list_checkpoints(Some("wf-restart")).await.unwrap();
    let latest = cps.last().unwrap().checkpoint_id;
    let again = rt_b
        .resume_workflow(
            session.id,
            wf,
            latest,
            openrt_runtime::CheckpointOptions::default(),
            RecoveryPolicy::default(),
        )
        .await
        .unwrap();
    assert_eq!(again.status, WorkflowStatus::Completed);
    let by_step: HashMap<_, _> = again.steps.iter().map(|s| (s.step.as_str(), s)).collect();
    assert_eq!(by_step["plan"].attempts, 2);
    assert_eq!(by_step["code"].attempts, 1);
    rt_b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn node_failure_bounded_retry_across_runs() {
    let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
    let sink_a = Arc::new(ChannelSink::new());
    let rt_a = openrt_runtime::RuntimeBuilder::new()
        .store(store.clone())
        .with_provider(Arc::new(FlakyProvider::new(2))) // fails exactly 2 calls
        .sink(sink_a.clone())
        .build()
        .await
        .unwrap();
    let session = rt_a.create_session("bounded-retry").await.unwrap();

    let wf = Workflow::new("wf-flaky")
        .step(WorkflowStep::new("research", SubAgentKind::Researcher, "gather facts").retries(1));

    // Run 1: the node exhausts its in-run budget (2 attempts, 1 retry) and
    // the workflow fails; the engine emits RetryExhausted with the
    // cumulative count.
    let first = rt_a.run_workflow(session.id, wf.clone()).await.unwrap();
    assert_eq!(first.status, WorkflowStatus::Failed);
    assert_eq!(first.steps[0].status, WorkflowStatus::Failed);
    assert_eq!(first.steps[0].attempts, 2);
    let events = wait_for(&sink_a, EventKind::RetryExhausted, 1).await;
    let exhausted = events
        .iter()
        .find(|e| e.kind == EventKind::RetryExhausted)
        .unwrap();
    assert_eq!(exhausted.data["node"], "research");
    assert_eq!(exhausted.data["attempts"], 2);

    let cps = rt_a.list_checkpoints(Some("wf-flaky")).await.unwrap();
    let final_id = cps.last().unwrap().checkpoint_id;
    rt_a.shutdown().await;

    // Resume with a policy cap of 3 cumulative attempts: exactly one fresh
    // attempt is allowed, and the (now-healthy) provider completes it.
    let sink_b = Arc::new(ChannelSink::new());
    let rt_b = openrt_runtime::RuntimeBuilder::new()
        .store(store.clone())
        .with_provider(Arc::new(MockProvider::echo("ok")))
        .sink(sink_b.clone())
        .build()
        .await
        .unwrap();
    let resumed = rt_b
        .resume_workflow(
            session.id,
            wf.clone(),
            final_id,
            openrt_runtime::CheckpointOptions::default(),
            RecoveryPolicy::default(), // max_attempts = 3
        )
        .await
        .unwrap();
    assert_eq!(resumed.status, WorkflowStatus::Completed);
    assert_eq!(resumed.steps[0].status, WorkflowStatus::Completed);
    assert_eq!(resumed.steps[0].attempts, 3, "2 prior + 1 fresh retry");

    let events = wait_for(&sink_b, EventKind::WorkflowCompleted, 1).await;
    let retry = events
        .iter()
        .find(|e| e.kind == EventKind::RetryScheduled)
        .unwrap();
    assert_eq!(retry.data["node"], "research");
    assert_eq!(retry.data["attempt"], 3);

    // The checkpointed node state now records 3 cumulative attempts.
    let cps = rt_b.list_checkpoints(Some("wf-flaky")).await.unwrap();
    let last = rt_b
        .load_checkpoint(&cps.last().unwrap().checkpoint_id)
        .await
        .unwrap();
    let node = last
        .node_states
        .iter()
        .find(|n| n.step == "research")
        .unwrap();
    assert_eq!(node.state, NodeState::Completed);
    assert_eq!(node.attempts, 3);

    rt_b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uncertain_non_idempotent_never_blindly_replayed() {
    let sink = Arc::new(ChannelSink::new());
    let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
    let rt = openrt_runtime::RuntimeBuilder::new()
        .store(store.clone())
        .with_provider(Arc::new(MockProvider::echo("ok")))
        .sink(sink.clone())
        .build()
        .await
        .unwrap();
    let session = rt.create_session("uncertain").await.unwrap();

    // Craft a checkpoint whose only node was *running* when it was captured
    // and is classified NonIdempotent — the planner must refuse to auto-replay
    // it (the checkpoint uncertain-outcome safety invariant), regardless of the policy.
    let node = NodeStatus {
        step: "pay".into(),
        state: NodeState::Running,
        attempts: 1,
        artifact_key: None,
        error: None,
        failure_category: String::new(),
        effect: NodeEffect::NonIdempotent,
    };
    let cp = Checkpoint {
        schema_version: CHECKPOINT_SCHEMA_VERSION,
        checkpoint_id: openrt_core::CheckpointId::new(),
        session_id: session.id,
        task_id: None,
        workflow_id: "wf-uncertain".into(),
        created_at: now(),
        kind: openrt_checkpoint::CheckpointKind::AfterNode,
        status: CheckpointStatus::Running,
        node_states: vec![node.clone()],
        completed: vec![],
        pending: vec![],
        failed: vec![],
        uncertain: vec!["pay".into()],
        current_node: Some("pay".into()),
        dependency_state: DependencyState::default(),
        retry_counters: HashMap::new(),
        blackboard: BlackboardSnapshot::default(),
        artifact_refs: vec![],
        execution: CheckpointExecution::default(),
        refs: openrt_checkpoint::RunRefs::default(),
        digest: None,
    };
    CheckpointManager::new(store.clone())
        .create(&cp)
        .await
        .unwrap();

    let wf = Workflow::new("wf-uncertain").step(WorkflowStep::new(
        "pay",
        SubAgentKind::Coder,
        "make payment",
    ));
    let err = rt
        .resume_workflow(
            session.id,
            wf,
            cp.checkpoint_id,
            openrt_runtime::CheckpointOptions::default(),
            RecoveryPolicy::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        openrt_runtime::Error::Checkpoint(openrt_checkpoint::CheckpointError::NeedsIntervention(_))
    ));

    // The refusal is audited on the bus.
    let events = wait_for(&sink, EventKind::RecoveryNeedsIntervention, 1).await;
    let needs = events
        .iter()
        .find(|e| e.kind == EventKind::RecoveryNeedsIntervention)
        .unwrap();
    assert_eq!(needs.data["node"], "pay");

    rt.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn halt_blocks_resume_until_cleared() {
    // Same shape as scenario 2: fail exactly 2 calls, step has 1 retry, so
    // run 1 leaves a failed node with 2 cumulative attempts.
    let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
    let sink_a = Arc::new(ChannelSink::new());
    let rt_a = openrt_runtime::RuntimeBuilder::new()
        .store(store.clone())
        .with_provider(Arc::new(FlakyProvider::new(2)))
        .sink(sink_a.clone())
        .build()
        .await
        .unwrap();
    let session = rt_a.create_session("halt-gate").await.unwrap();
    let wf = Workflow::new("wf-halt")
        .step(WorkflowStep::new("research", SubAgentKind::Researcher, "gather facts").retries(1));
    let first = rt_a.run_workflow(session.id, wf.clone()).await.unwrap();
    assert_eq!(first.status, WorkflowStatus::Failed);
    let cps = rt_a.list_checkpoints(Some("wf-halt")).await.unwrap();
    let final_id = cps.last().unwrap().checkpoint_id;
    rt_a.shutdown().await;

    let sink_b = Arc::new(ChannelSink::new());
    let rt_b = openrt_runtime::RuntimeBuilder::new()
        .store(store.clone())
        .with_provider(Arc::new(FlakyProvider::new(0)))
        .sink(sink_b.clone())
        .build()
        .await
        .unwrap();

    // ADR-008: recovery never auto-clears the kill switch; a halted engine
    // refuses to resume.
    assert!(rt_b.halt("post-incident review").await.unwrap());
    let err = rt_b
        .resume_workflow(
            session.id,
            wf.clone(),
            final_id,
            openrt_runtime::CheckpointOptions::default(),
            RecoveryPolicy::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        openrt_runtime::Error::Checkpoint(openrt_checkpoint::CheckpointError::Refused(_))
    ));
    let events = wait_for(&sink_b, EventKind::SafetyHalt, 1).await;
    assert!(events.iter().any(|e| e.kind == EventKind::SafetyHalt));

    // Operator clears the halt; resume now proceeds and completes.
    rt_b.clear_halt();
    assert!(rt_b.halted().is_none());
    let resumed = rt_b
        .resume_workflow(
            session.id,
            wf.clone(),
            final_id,
            openrt_runtime::CheckpointOptions::default(),
            RecoveryPolicy::default(),
        )
        .await
        .unwrap();
    assert_eq!(resumed.status, WorkflowStatus::Completed);
    assert_eq!(resumed.steps[0].attempts, 3);

    rt_b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failing_store_preserves_prior_checkpoint_chain() {
    let sink = Arc::new(ChannelSink::new());
    let store: Arc<dyn PersistenceStore> = Arc::new(FailCheckpointStore::new());
    let rt = openrt_runtime::RuntimeBuilder::new()
        .store(store.clone())
        .with_provider(Arc::new(MockProvider::echo("ok")))
        .sink(sink.clone())
        .build()
        .await
        .unwrap();
    let session = rt.create_session("failing-store").await.unwrap();

    let wf = dep_workflow("wf-store");
    let err = rt.run_workflow(session.id, wf).await.unwrap_err();
    // The hook aborts the run via a fail-closed checkpoint error; a run that
    // started cleanly surfaces the injected store failure.
    assert!(matches!(
        err,
        openrt_runtime::Error::Checkpoint(openrt_checkpoint::CheckpointError::Invalid(_))
    ));

    // The chain is preserved: only the Initial checkpoint survives, and it is
    // still loadable and schema-valid.
    let cps = rt.list_checkpoints(Some("wf-store")).await.unwrap();
    assert_eq!(cps.len(), 1);
    assert_eq!(cps[0].kind, openrt_checkpoint::CheckpointKind::Initial);
    let survivor = rt.load_checkpoint(&cps[0].checkpoint_id).await.unwrap();
    assert_eq!(survivor.workflow_id, "wf-store");

    rt.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dry_run_has_zero_side_effects() {
    let sink = Arc::new(ChannelSink::new());
    let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
    let rt = openrt_runtime::RuntimeBuilder::new()
        .store(store.clone())
        .with_provider(Arc::new(FlakyProvider::new(2)))
        .sink(sink.clone())
        .build()
        .await
        .unwrap();
    let session = rt.create_session("dry-run").await.unwrap();
    let wf = Workflow::new("wf-dry")
        .step(WorkflowStep::new("research", SubAgentKind::Researcher, "gather facts").retries(1));

    let report = rt.run_workflow(session.id, wf.clone()).await.unwrap();
    assert_eq!(report.status, WorkflowStatus::Failed);
    let cps = rt.list_checkpoints(Some("wf-dry")).await.unwrap();
    let final_id = cps.last().unwrap().checkpoint_id;

    // Snapshot the store before the dry run.
    let tasks_before = rt.store.list_tasks(Some(session.id)).await.unwrap().len();
    let cps_before = rt.list_checkpoints(Some("wf-dry")).await.unwrap().len();
    let runs_before = rt.store.get_record("runs", "wf-dry").await.unwrap();
    let bb_before = rt.blackboard().await.unwrap().snapshot();

    let dry = rt
        .resume_dry_run(session.id, wf, final_id, RecoveryPolicy::default())
        .await
        .unwrap();
    assert_eq!(
        dry.overall,
        openrt_checkpoint::DryRunConclusion::WouldComplete
    );
    assert!(dry
        .nodes
        .iter()
        .any(|n| n.step == "research" && n.action == openrt_checkpoint::DryRunAction::Retried));

    // The report is persisted in the dryrun namespace, keyed by checkpoint.
    assert!(rt
        .store
        .get_record(openrt_checkpoint::DRYRUN_NS, &final_id.to_string())
        .await
        .unwrap()
        .is_some());

    // Zero side effects: no tasks, checkpoints, run reports, or blackboard
    // writes.
    assert_eq!(
        rt.store.list_tasks(Some(session.id)).await.unwrap().len(),
        tasks_before
    );
    assert_eq!(
        rt.list_checkpoints(Some("wf-dry")).await.unwrap().len(),
        cps_before
    );
    let runs_after = rt.store.get_record("runs", "wf-dry").await.unwrap();
    assert_eq!(runs_before, runs_after);
    let bb_after = rt.blackboard().await.unwrap().snapshot();
    assert_eq!(bb_before.entries, bb_after.entries);

    rt.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checkpoint_list_and_delete_round_trip() {
    let sink = Arc::new(ChannelSink::new());
    let rt = openrt_runtime::RuntimeBuilder::new()
        .store(Arc::new(MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::echo("ok")))
        .sink(sink.clone())
        .build()
        .await
        .unwrap();
    let session = rt.create_session("ckpt-admin").await.unwrap();
    let wf = dep_workflow("wf-admin");
    let report = rt.run_workflow(session.id, wf).await.unwrap();
    assert_eq!(report.status, WorkflowStatus::Completed);

    let cps = rt.list_checkpoints(Some("wf-admin")).await.unwrap();
    // Initial + Scheduled + 2× AfterNode + Scheduled + Final (dep_workflow's
    // two nodes spawn in separate waves; each spawn is durably marked first).
    assert_eq!(cps.len(), 6);
    let id = cps[0].checkpoint_id;
    assert!(rt.load_checkpoint(&id).await.is_ok());

    rt.delete_checkpoint(&id).await.unwrap();
    let events = wait_for(&sink, EventKind::CheckpointDeleted, 1).await;
    assert!(rt.load_checkpoint(&id).await.is_err());
    assert_eq!(
        rt.list_checkpoints(Some("wf-admin")).await.unwrap().len(),
        5
    );
    assert!(events
        .iter()
        .any(|e| e.kind == EventKind::CheckpointDeleted));

    rt.shutdown().await;
}

/// Phase 10 production path: the spawn-time `Scheduled` checkpoint durably
/// records the in-flight node as Running/uncertain (never a stale Pending
/// that could be silently replayed), and the default recovery policy refuses
/// to auto-resume unknown-outcome work — intervention, not blind execution.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scheduled_checkpoint_marks_in_flight_node_uncertain() {
    let sink = Arc::new(ChannelSink::new());
    let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
    let rt = openrt_runtime::RuntimeBuilder::new()
        .store(store.clone())
        .with_provider(Arc::new(MockProvider::echo("ok")))
        .sink(sink.clone())
        .build()
        .await
        .unwrap();
    let session = rt.create_session("sched-uncertain").await.unwrap();
    let wf = dep_workflow("wf-scheduled");
    let report = rt.run_workflow(session.id, wf.clone()).await.unwrap();
    assert_eq!(report.status, WorkflowStatus::Completed);

    let cps = rt.list_checkpoints(Some("wf-scheduled")).await.unwrap();
    let sched = cps
        .iter()
        .find(|c| c.kind == openrt_checkpoint::CheckpointKind::Scheduled)
        .expect("spawn-time Scheduled checkpoint persisted in production");
    let cp = rt.load_checkpoint(&sched.checkpoint_id).await.unwrap();
    // Coherence: Running ⇔ uncertain list (validate enforces both directions).
    cp.validate().unwrap();
    let running = cp
        .node_states
        .iter()
        .find(|n| n.state == NodeState::Running)
        .expect("in-flight node recorded Running in the durable checkpoint");
    assert_eq!(running.step, "plan");
    assert!(cp.uncertain.contains(&running.step.to_string()));
    // The still-unstarted `code` node (deps unsatisfied) never masquerades
    // as Running or Completed — it is simply absent from this spawn-time
    // snapshot, and recovery treats unknown-as-pending from the step list.
    assert!(cp
        .node_states
        .iter()
        .all(|n| n.step != "code" || n.state == NodeState::Pending));

    // Default policy (AutoWhenSafe) + Unknown effect → intervention: recovery
    // never auto-replays a node whose outcome it does not know.
    let steps: Vec<String> = wf.steps.iter().map(|s| s.id.clone()).collect();
    let deps: HashMap<String, Vec<String>> = wf
        .steps
        .iter()
        .map(|s| (s.id.clone(), s.depends_on.clone()))
        .collect();
    let plan = plan_recovery(
        &cp,
        &steps,
        &deps,
        &RecoveryPolicy::default(),
        RecoveryScope::Resume,
    )
    .unwrap();
    assert!(
        plan.intervention_nodes.iter().any(|n| n == "plan"),
        "Running node with Unknown effect must require intervention"
    );
    assert!(plan.is_blocked());

    rt.shutdown().await;
}

/// M10.3 production rollback path (happy sequence): integrity validation →
/// policy compatibility → plan → operator authorization → restore → verify →
/// `recovery.rollback` audit → continuation record, with history-ledger
/// immutability asserted against real ledger rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rollback_production_path_full_sequence() {
    let dir = tmp_dir("rbws");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/app.txt"), "content-A").unwrap();

    let sink = Arc::new(ChannelSink::new());
    let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
    let rt = openrt_runtime::RuntimeBuilder::new()
        .store(store.clone())
        .with_provider(Arc::new(MockProvider::echo("ok")))
        .sink(sink.clone())
        .build()
        .await
        .unwrap();
    let session = rt.create_session("rb-m103").await.unwrap();
    let wf = dep_workflow("wf-rollback");

    // Run once so `runs/wf-rollback` exists (run-metadata rollback target).
    rt.run_workflow(session.id, wf.clone()).await.unwrap();

    // Resume with options that snapshot the workspace file: the resumed
    // run's checkpoints carry file_snapshot + run-metadata + output.* state.
    let cps = rt.list_checkpoints(Some("wf-rollback")).await.unwrap();
    let latest = cps.last().unwrap().checkpoint_id;
    let opts = openrt_runtime::CheckpointOptions {
        workspace_root: Some(dir.clone()),
        snapshot_files: vec![PathBuf::from("src/app.txt")],
        ..Default::default()
    };
    let resumed = rt
        .resume_workflow(
            session.id,
            wf.clone(),
            latest,
            opts,
            RecoveryPolicy::default(),
        )
        .await
        .unwrap();
    assert_eq!(resumed.status, WorkflowStatus::Completed);

    let cps = rt.list_checkpoints(Some("wf-rollback")).await.unwrap();
    let target = cps.last().unwrap().checkpoint_id;
    let cp = rt.load_checkpoint(&target).await.unwrap();
    assert!(cp.execution.run_snapshot.is_some(), "run-metadata snapshot");
    assert!(cp
        .blackboard
        .entries
        .iter()
        .any(|(k, _)| k.starts_with("output.")));
    let snap_plan = cp
        .blackboard
        .entries
        .iter()
        .find(|(k, _)| k == "output.plan")
        .map(|(_, v)| v.clone())
        .expect("output.plan in snapshot");

    // A real history-ledger row, so the immutability assertion is not vacuous.
    let tail_before = openrt_timeseries::tail_seq(&*store).await.unwrap();
    let seq = openrt_timeseries::append_event(
        &*store,
        "rollback-baseline",
        "node-1",
        "ok",
        "TaskCompleted",
        "pre-rollback baseline",
    )
    .await
    .unwrap();
    assert_eq!(seq, tail_before + 1);

    // Tamper with live state so the rollback has real work to do.
    let bb = Blackboard::with_store(store.clone(), BLACKBOARD_NS)
        .await
        .unwrap();
    bb.put("output.plan", serde_json::json!("TAMPERED"))
        .await
        .unwrap();
    store
        .put_record(
            RUNS_NS,
            "wf-rollback",
            &serde_json::json!({ "tampered": true }),
        )
        .await
        .unwrap();

    // Step 2+4: compatible policy, trusted operator, authorized rollback.
    let auth = RollbackAuthorization {
        operator: rt.asset_operators()[0].clone(),
        acknowledge_policy_drift: false,
    };
    let report = rt
        .rollback_checkpoint(&target, &auth, Some(dir.clone()))
        .await
        .unwrap();

    // Steps 2/6/7/9 outcomes.
    assert_eq!(report.policy_compat, Some(PolicyCompat::Compatible));
    assert_eq!(report.operator.as_deref(), Some(auth.operator.as_str()));
    let v = report.verification.as_ref().expect("verification recorded");
    assert_eq!(v.checked, 3, "file + blackboard + run metadata");
    assert!(v.mismatches.is_empty(), "mismatches: {:?}", v.mismatches);
    assert_eq!(report.history_immutable, Some(true));
    let cont = report.continuation.as_ref().expect("continuation record");
    assert_eq!(cont.resume_from, target.to_string());
    assert_eq!(cont.workflow_id, "wf-rollback");
    assert_eq!(cont.next_action, "resume");

    // Restored state matches the checkpoint snapshot (fresh reads).
    let bb2 = Blackboard::with_store(store.clone(), BLACKBOARD_NS)
        .await
        .unwrap();
    let got_plan = bb2.get("output.plan").unwrap().expect("plan key");
    assert_eq!(got_plan, snap_plan);
    assert_ne!(got_plan, serde_json::json!("TAMPERED"));
    let runs = store
        .get_record(RUNS_NS, "wf-rollback")
        .await
        .unwrap()
        .expect("run record restored");
    assert_eq!(runs, cp.execution.run_snapshot.unwrap());
    let file_after = std::fs::read_to_string(dir.join("src/app.txt")).unwrap();
    assert_eq!(file_after, "content-A");

    // Step 8: exactly one `recovery.rollback` audit record, result
    // `completed`, actor = the authorizing operator; chain still verifies.
    let records = rt
        .audit_query(&AuditQuery::new().with_action_prefix("recovery.rollback"))
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].entry.action, "recovery.rollback");
    assert_eq!(records[0].entry.result, "completed");
    assert_eq!(records[0].entry.actor, auth.operator);
    assert_eq!(records[0].entry.subject, target.to_string());
    rt.audit_verify().await.unwrap();

    // Ledger immutability: rollback events are APPENDED as auditable rows
    // but existing rows are never rewritten (the in-path prefix equality
    // already asserted `history_immutable`) and the chain still verifies.
    let tail_after = openrt_timeseries::tail_seq(&*store).await.unwrap();
    assert!(
        tail_after >= tail_before,
        "history rows must never disappear ({tail_before} -> {tail_after})"
    );
    openrt_timeseries::verify_chain(&*store).await.unwrap();

    rt.shutdown().await;
}

/// M10.3 gates: an untrusted operator and unacknowledged policy drift both
/// fail closed BEFORE any state is touched (no restore, no audit record);
/// explicit drift acknowledgement then authorizes the mismatched rollback.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rollback_refuses_untrusted_operator_and_policy_drift() {
    let sink = Arc::new(ChannelSink::new());
    let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
    let rt = openrt_runtime::RuntimeBuilder::new()
        .store(store.clone())
        .with_provider(Arc::new(MockProvider::echo("ok")))
        .sink(sink.clone())
        .build()
        .await
        .unwrap();
    let session = rt.create_session("rb-gates").await.unwrap();

    // A restorable checkpoint: one completed node with a snapshot value.
    let mk_cp = |workflow_id: &str, policy_version: Option<String>| Checkpoint {
        schema_version: CHECKPOINT_SCHEMA_VERSION,
        checkpoint_id: openrt_core::CheckpointId::new(),
        session_id: session.id,
        task_id: None,
        workflow_id: workflow_id.into(),
        created_at: now(),
        kind: openrt_checkpoint::CheckpointKind::AfterNode,
        status: CheckpointStatus::Running,
        node_states: vec![NodeStatus {
            step: "plan".into(),
            state: NodeState::Completed,
            attempts: 1,
            artifact_key: None,
            error: None,
            failure_category: String::new(),
            effect: NodeEffect::ReadOnly,
        }],
        completed: vec!["plan".into()],
        pending: vec![],
        failed: vec![],
        uncertain: vec![],
        current_node: None,
        dependency_state: DependencyState::default(),
        retry_counters: HashMap::new(),
        blackboard: BlackboardSnapshot {
            namespace: BLACKBOARD_NS.into(),
            entries: vec![("output.plan".to_string(), serde_json::json!("ORIGINAL"))],
        },
        artifact_refs: vec![],
        execution: CheckpointExecution::default(),
        refs: RunRefs {
            policy_version,
            ..Default::default()
        },
        digest: None,
    };
    let manager = CheckpointManager::new(store.clone());
    let drifted = manager
        .create(&mk_cp(
            "wf-drift",
            Some("sha256:0000000000000000".to_string()),
        ))
        .await
        .unwrap();

    let bb = Blackboard::with_store(store.clone(), BLACKBOARD_NS)
        .await
        .unwrap();
    bb.put("output.plan", serde_json::json!("LIVE"))
        .await
        .unwrap();

    // 1. Untrusted operator → refused, nothing touched, nothing audited.
    let mallory = RollbackAuthorization {
        operator: "mallory".to_string(),
        acknowledge_policy_drift: true,
    };
    let err = rt
        .rollback_checkpoint(&drifted, &mallory, None)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            openrt_runtime::Error::Improve(
                openrt_selfimprove::ImproveError::UnauthorizedOperator { .. }
            )
        ),
        "unexpected error: {err}"
    );

    // 2. Trusted operator but unacknowledged policy drift → refused too.
    let trusted = rt.asset_operators()[0].clone();
    let no_ack = RollbackAuthorization {
        operator: trusted.clone(),
        acknowledge_policy_drift: false,
    };
    let err = rt
        .rollback_checkpoint(&drifted, &no_ack, None)
        .await
        .unwrap_err();
    match err {
        openrt_runtime::Error::Checkpoint(openrt_checkpoint::CheckpointError::Refused(msg)) => {
            assert!(
                msg.contains("acknowledge policy drift"),
                "refusal must name the missing acknowledgement: {msg}"
            );
        }
        other => panic!("expected Refused, got {other}"),
    }

    // Both refusals left live state untouched and audited nothing.
    assert_eq!(
        bb.get("output.plan").unwrap().unwrap(),
        serde_json::json!("LIVE")
    );
    let records = rt
        .audit_query(&AuditQuery::new().with_action_prefix("recovery.rollback"))
        .await
        .unwrap();
    assert!(records.is_empty(), "refused rollbacks must not be audited");

    // 3. Explicit drift acknowledgement authorizes the mismatched rollback.
    let ack = RollbackAuthorization {
        operator: trusted,
        acknowledge_policy_drift: true,
    };
    let report = rt.rollback_checkpoint(&drifted, &ack, None).await.unwrap();
    assert_eq!(report.policy_compat, Some(PolicyCompat::Mismatched));
    let v = report.verification.as_ref().unwrap();
    assert_eq!(v.checked, 1);
    assert!(v.mismatches.is_empty());
    let bb2 = Blackboard::with_store(store.clone(), BLACKBOARD_NS)
        .await
        .unwrap();
    assert_eq!(
        bb2.get("output.plan").unwrap().unwrap(),
        serde_json::json!("ORIGINAL")
    );

    let records = rt
        .audit_query(&AuditQuery::new().with_action_prefix("recovery.rollback"))
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].entry.result, "completed");
    assert_eq!(
        records[0].entry.ext["acknowledged_policy_drift"],
        serde_json::json!(true)
    );
    assert_eq!(
        records[0].entry.ext["policy_compat"],
        serde_json::json!("mismatched")
    );
    rt.audit_verify().await.unwrap();

    // 4. A legacy checkpoint (no policy fingerprint → Unknown) also requires
    //    explicit acknowledgement — never a silent downgrade.
    let legacy = manager.create(&mk_cp("wf-legacy", None)).await.unwrap();
    let err = rt
        .rollback_checkpoint(&legacy, &no_ack, None)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            openrt_runtime::Error::Checkpoint(openrt_checkpoint::CheckpointError::Refused(_))
        ),
        "legacy checkpoint without fingerprint must require acknowledgement: {err}"
    );

    rt.shutdown().await;
}
