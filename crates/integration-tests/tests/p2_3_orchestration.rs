//! Integration (P2.3): a workflow DAG runs end-to-end through the runtime,
//! emits orchestration events, writes step artifacts to the blackboard, and
//! persists the task/execution records and the DAG + run report in the store.

use std::sync::Arc;

use openrt_core::{EventKind, TaskStatus};
use openrt_models::mock::MockProvider;
use openrt_orch::{Workflow, WorkflowStatus, WorkflowStep};
use openrt_persistence::MemoryStore;
use openrt_telemetry::ChannelSink;

async fn build() -> (Arc<openrt_runtime::Runtime>, Arc<ChannelSink>) {
    let channel = Arc::new(ChannelSink::new());
    let rt = openrt_runtime::RuntimeBuilder::new()
        .store(Arc::new(MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::echo("mock")))
        .sink(channel.clone())
        .build()
        .await
        .unwrap();
    (rt, channel)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_dag_runs_end_to_end() {
    let (rt, channel) = build().await;
    let session = rt.create_session("orchestrate").await.unwrap();

    let wf = Workflow::new("wf-e2e")
        .named("plan-code-confirm")
        .step(WorkflowStep::new(
            "plan",
            openrt_orch::SubAgentKind::Planner,
            "layout the change",
        ))
        .step(
            WorkflowStep::new("code", openrt_orch::SubAgentKind::Coder, "implement it").dep("plan"),
        )
        .step(
            WorkflowStep::new(
                "verify",
                openrt_orch::SubAgentKind::Verifier,
                "check the result",
            )
            .dep("code"),
        );

    let report = rt.run_workflow(session.id, wf.clone()).await.unwrap();
    assert_eq!(report.status, WorkflowStatus::Completed);
    assert_eq!(report.steps.len(), 3);
    let keys: Vec<_> = report.steps.iter().map(|s| s.step.as_str()).collect();
    assert_eq!(keys, vec!["plan", "code", "verify"]);

    // Execution artifacts on the blackboard.
    let bb = rt
        .blackboard()
        .await
        .expect("blackboard available over the runtime store");
    assert!(bb.get("output.plan").unwrap().is_some());
    assert!(bb.get("output.code").unwrap().is_some());
    assert!(bb.get("output.verify").unwrap().is_some());

    // Task + execution records persisted durably (task visible via the typed
    // listing; the execution record is written alongside).
    let tasks = rt.store.list_tasks(Some(session.id)).await.unwrap();
    assert_eq!(tasks.len(), 1);
    let task = &tasks[0];
    assert_eq!(task.status, TaskStatus::Completed);
    assert_eq!(task.agent_id, Some(rt.orchestrator));

    // DAG + run report persisted under their namespaces (ADR-006 single store).
    assert!(rt
        .store
        .get_record("dag", "wf-e2e")
        .await
        .unwrap()
        .is_some());
    assert!(rt
        .store
        .get_record("runs", "wf-e2e")
        .await
        .unwrap()
        .is_some());

    // Orchestration lifecycle events emitted in order.
    let mut events = Vec::new();
    for _ in 0..200 {
        events = channel.snapshot();
        if events
            .iter()
            .any(|e| e.kind == EventKind::WorkflowCompleted)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let kinds: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
    let pos = |k: EventKind| {
        kinds
            .iter()
            .position(|e| *e == k)
            .unwrap_or_else(|| panic!("{k:?} missing from {kinds:?}"))
    };
    assert!(pos(EventKind::WorkflowStarted) < pos(EventKind::WorkflowStepStarted));
    assert!(pos(EventKind::WorkflowStepStarted) < pos(EventKind::WorkflowStepCompleted));
    assert!(pos(EventKind::WorkflowStepCompleted) < pos(EventKind::WorkflowCompleted));
    // Orchestration events are correlated to the task.
    let wc = events
        .iter()
        .find(|e| e.kind == EventKind::WorkflowCompleted)
        .unwrap();
    assert_eq!(wc.task_id, Some(task.id));
    assert_eq!(wc.agent_id, Some(rt.orchestrator));
    assert_eq!(wc.data["steps_completed"], 3);

    // Session transcript carries the summary fold.
    let session = rt.load_session(session.id).await.unwrap();
    assert!(session
        .content
        .iter()
        .any(|m| m.content.contains("workflow wf-e2e")));
    rt.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failing_role_fails_the_workflow_and_task_records() {
    let channel = Arc::new(ChannelSink::new());
    let rt = openrt_runtime::RuntimeBuilder::new()
        .store(Arc::new(MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::new(
            "mock",
            openrt_models::mock::MockMode::Fail,
        )))
        .sink(channel.clone())
        .build()
        .await
        .unwrap();
    let session = rt.create_session("orchestrate-fail").await.unwrap();

    let wf = Workflow::new("wf-fail-e2e")
        .step(
            WorkflowStep::new(
                "research",
                openrt_orch::SubAgentKind::Researcher,
                "gather facts",
            )
            .retries(1),
        )
        .step(
            WorkflowStep::new(
                "critique",
                openrt_orch::SubAgentKind::Critic,
                "challenge the plan",
            )
            .dep("research"),
        );

    let report = rt.run_workflow(session.id, wf).await.unwrap();
    assert_eq!(report.status, WorkflowStatus::Failed);
    // Only steps that ran are reported; the dependent step is never
    // scheduled once its dependency failed (blocked, absent from reports).
    assert_eq!(report.steps.len(), 1);
    assert_eq!(report.steps[0].step, "research");
    assert_eq!(report.steps[0].status, WorkflowStatus::Failed);
    assert_eq!(report.steps[0].attempts, 2); // 1 attempt + 1 retry

    let tasks = rt.store.list_tasks(Some(session.id)).await.unwrap();
    let task = &tasks[0];
    assert_eq!(task.status, TaskStatus::Failed);
    assert!(task.error.as_deref().unwrap().contains("research"));

    // Both the failure event and TaskFailed appear in the sink.
    let mut events = Vec::new();
    for _ in 0..200 {
        events = channel.snapshot();
        if events.iter().any(|e| e.kind == EventKind::TaskFailed) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(events
        .iter()
        .any(|e| e.kind == EventKind::WorkflowStepFailed));
    assert!(events
        .iter()
        .any(|e| e.kind == EventKind::WorkflowCompleted));
    let wc = events
        .iter()
        .find(|e| e.kind == EventKind::WorkflowCompleted)
        .unwrap();
    assert_eq!(wc.data["status"], "failed");
}
