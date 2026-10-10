//! Phase 10 M10.4 — crash recovery (boot reconciliation) integration tests.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use openrt_blackboard::BLACKBOARD_NS;
use openrt_checkpoint::{
    Checkpoint, CheckpointKind, CheckpointManager, CheckpointStatus, DependencyState, NodeEffect,
    NodeState, NodeStatus, RecoveryPolicy, RunRefs, CHECKPOINT_SCHEMA_VERSION,
};
use openrt_core::{now, EventKind, Task, TaskStatus};
use openrt_models::error::ModelError;
use openrt_models::mock::MockProvider;
use openrt_models::provider::{ModelCapabilities, ModelProvider};
use openrt_models::types::{ModelInfo, ModelRequest, ModelResponse};
use openrt_orch::{SubAgentKind, Workflow, WorkflowStatus, WorkflowStep, RUNS_NS};
use openrt_persistence::{MemoryStore, PersistenceStore};
use openrt_runtime::RuntimeBuilder;
use openrt_sandbox::{
    IsolationLevel, NetworkMode, SandboxId, SandboxInstanceRecord, SandboxState, SANDBOX_NS,
};
use openrt_telemetry::{ChannelSink, DomainEvent};

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

fn dep_workflow(id: &str) -> Workflow {
    Workflow::new(id)
        .step(WorkflowStep::new(
            "plan",
            SubAgentKind::Planner,
            "layout the change",
        ))
        .step(WorkflowStep::new("code", SubAgentKind::Coder, "implement it").dep("plan"))
}

/// Echo provider that counts every model call — proves *which* steps
/// executed (a re-executed `plan` would bump the count).
struct CountingProvider {
    inner: MockProvider,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ModelProvider for CountingProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn info(&self) -> ModelInfo {
        self.inner.info()
    }
    async fn generate(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.generate(req).await
    }
    fn capabilities(&self) -> ModelCapabilities {
        self.inner.capabilities()
    }
}

/// The durable state a process death leaves behind: a workflow task stuck
/// `Running` plus an `Initial` checkpoint (status `Running`) for a chain
/// whose first node completed and whose second node never started.
async fn seed_crashed_run(
    store: &Arc<dyn PersistenceStore>,
    session_id: openrt_core::SessionId,
    workflow_id: &str,
) -> (openrt_core::Task, openrt_checkpoint::CheckpointId) {
    let mut task = Task::new(session_id, format!("workflow {workflow_id}"));
    task.transition(TaskStatus::Running).unwrap();
    store.save_task(&task).await.unwrap();

    let checkpoint = Checkpoint {
        schema_version: CHECKPOINT_SCHEMA_VERSION,
        checkpoint_id: openrt_core::CheckpointId::new(),
        session_id,
        task_id: Some(task.id),
        workflow_id: workflow_id.into(),
        created_at: now(),
        kind: CheckpointKind::Initial,
        status: CheckpointStatus::Running,
        node_states: vec![
            NodeStatus {
                step: "plan".into(),
                state: NodeState::Completed,
                attempts: 1,
                artifact_key: Some("output.plan".into()),
                error: None,
                failure_category: String::new(),
                effect: NodeEffect::Unknown,
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
        dependency_state: DependencyState {
            ready: vec!["code".into()],
            deps_by_step: [
                ("plan".to_string(), vec![]),
                ("code".to_string(), vec!["plan".to_string()]),
            ]
            .into_iter()
            .collect(),
        },
        retry_counters: std::collections::HashMap::new(),
        blackboard: Default::default(),
        artifact_refs: vec![],
        execution: Default::default(),
        refs: RunRefs::default(),
        digest: None,
    };
    let manager = CheckpointManager::new(store.clone());
    let id = manager.create(&checkpoint).await.unwrap();
    (task, id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crashed_workflow_boot_records_resume_and_operator_resumes_once() {
    let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());

    // Run 1: the session the crashed run lived in.
    let sink1 = Arc::new(ChannelSink::new());
    let rt1 = RuntimeBuilder::new()
        .store(store.clone())
        .with_provider(Arc::new(MockProvider::echo("mock")))
        .sink(sink1.clone())
        .bus_capacity(16)
        .build()
        .await
        .unwrap();
    let session = rt1.create_session("crash-recovery").await.unwrap();
    rt1.shutdown().await;
    drop(rt1);

    // Crash: durable leftovers of a run that never finalized.
    let (crashed_task, checkpoint_id) = seed_crashed_run(&store, session.id, "wf-crash").await;

    // Run 2: boot recovery over the same store.
    let calls = Arc::new(AtomicUsize::new(0));
    let sink2 = Arc::new(ChannelSink::new());
    let rt2 = RuntimeBuilder::new()
        .store(store.clone())
        .with_provider(Arc::new(CountingProvider {
            inner: MockProvider::echo("mock"),
            calls: calls.clone(),
        }))
        .sink(sink2.clone())
        .bus_capacity(16)
        .build()
        .await
        .unwrap();

    // The interrupted task is reconciled — never marked successful.
    let task_after = store.load_task(crashed_task.id).await.unwrap();
    assert_eq!(task_after.status, TaskStatus::Interrupted);
    assert!(task_after.completed_at.is_none());

    // The recovery record names the run and a safe resume point.
    let records = rt2.recovery_records().await.unwrap();
    let record = records.last().unwrap();
    assert!(!record.is_clean());
    assert_eq!(record.interrupted_tasks.len(), 1);
    assert_eq!(record.interrupted_tasks[0].task_id, crashed_task.id);
    assert_eq!(record.incomplete_runs.len(), 1);
    let run = &record.incomplete_runs[0];
    assert_eq!(run.workflow_id, "wf-crash");
    assert_eq!(run.task_id, Some(crashed_task.id));
    assert_eq!(run.resume.checkpoint_id, checkpoint_id);
    assert_eq!(run.resume.plan_status, "ready");
    assert_eq!(run.resume.resume_nodes, vec!["code"]);
    assert_eq!(run.resume.skip_nodes, vec!["plan"]);

    // Boot event (phase `boot`) — one recovery signal, correlated to nothing
    // because the owning process is gone.
    let events = wait_for(&sink2, EventKind::RecoveryStarted, 1).await;
    let boot = events
        .iter()
        .find(|e| e.kind == EventKind::RecoveryStarted)
        .unwrap();
    assert_eq!(boot.data["phase"], "boot");
    assert_eq!(boot.data["record_id"], record.record_id.clone());
    assert_eq!(boot.data["incomplete_runs"], 1);

    // Discovery only: boot executed nothing.
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let runs = store.get_record(RUNS_NS, "wf-crash").await.ok().flatten();
    assert!(runs.is_none(), "boot must not write run reports");
    let bb = store.list_records(BLACKBOARD_NS).await.unwrap();
    assert!(bb.is_empty(), "boot must not materialize blackboard state");

    // Operator resumes at the safe point: `code` runs exactly once and the
    // completed `plan` is never re-executed (one echo call = one step).
    let report = rt2
        .resume_workflow(
            session.id,
            dep_workflow("wf-crash"),
            checkpoint_id,
            openrt_runtime::CheckpointOptions::default(),
            RecoveryPolicy::default(),
        )
        .await
        .unwrap();
    assert_eq!(report.status, WorkflowStatus::Completed);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "only the pending `code` node may execute on resume"
    );
    let plan_report = report
        .steps
        .iter()
        .find(|s| s.step == "plan")
        .expect("plan appears in the resumed report");
    assert_eq!(plan_report.status, WorkflowStatus::Completed);
    assert_eq!(plan_report.error, None);

    // The crashed attempt stays honestly interrupted — never promoted to
    // successful by the resume that completed under a new task.
    let old_task = store.load_task(crashed_task.id).await.unwrap();
    assert_eq!(old_task.status, TaskStatus::Interrupted);

    // Chain now ends terminal: a further boot finds nothing incomplete.
    let final_cps = rt2.list_checkpoints(Some("wf-crash")).await.unwrap();
    let last = final_cps.last().unwrap();
    assert_eq!(last.status, CheckpointStatus::Completed);
    rt2.shutdown().await;
    drop(rt2);

    let sink3 = Arc::new(ChannelSink::new());
    let rt3 = RuntimeBuilder::new()
        .store(store.clone())
        .with_provider(Arc::new(MockProvider::echo("mock")))
        .sink(sink3.clone())
        .bus_capacity(16)
        .build()
        .await
        .unwrap();
    let records = rt3.recovery_records().await.unwrap();
    assert!(
        records.last().unwrap().is_clean(),
        "post-resume boot is clean"
    );
    // Evidence chain: clean → findings → clean.
    assert_eq!(records.len(), 3);
    assert!(records[0].is_clean(), "first boot was clean");
    assert!(!records[1].is_clean());
    let events = sink3.snapshot();
    assert!(
        !events.iter().any(|e| e.kind == EventKind::RecoveryStarted),
        "clean boot emits no recovery signal"
    );
    rt3.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn boot_records_stale_sandbox_without_touching_it() {
    let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());

    // A sandbox instance left Running by a dead process.
    let record = SandboxInstanceRecord {
        id: SandboxId("sbx-crashed".into()),
        profile: "python".into(),
        backend: "process".into(),
        network: NetworkMode::LocalOnly,
        isolation_level: IsolationLevel::Process,
        policy_digest: "digest".into(),
        state: SandboxState::Running,
        parent: None,
        actor: "test".into(),
        created_at: now().to_string(),
        ended_at: None,
        workspace: None,
        exit_code: None,
        failure: None,
        pid: Some(4242),
        starttime: None,
    };
    store
        .put_record(
            SANDBOX_NS,
            "sbx-crashed",
            &serde_json::to_value(&record).unwrap(),
        )
        .await
        .unwrap();

    let sink = Arc::new(ChannelSink::new());
    let rt = RuntimeBuilder::new()
        .store(store.clone())
        .with_provider(Arc::new(MockProvider::echo("mock")))
        .sink(sink.clone())
        .bus_capacity(16)
        .build()
        .await
        .unwrap();

    let records = rt.recovery_records().await.unwrap();
    let last = records.last().unwrap();
    assert!(!last.is_clean());
    assert_eq!(last.stale_sandboxes.len(), 1);
    let stale = &last.stale_sandboxes[0];
    assert_eq!(stale.sandbox_id, "sbx-crashed");
    assert_eq!(stale.state, SandboxState::Running);
    assert_eq!(stale.pid, Some(4242));

    // Recorded only — boot recovery never destroys processes.
    let instance = store
        .get_record(SANDBOX_NS, "sbx-crashed")
        .await
        .ok()
        .flatten()
        .expect("instance record survives boot");
    let state: SandboxInstanceRecord = serde_json::from_value(instance).unwrap();
    assert_eq!(state.state, SandboxState::Running);
    rt.shutdown().await;
}
