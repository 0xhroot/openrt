//! End-to-end: a denied tool must never reach the executor, even through the
//! full runtime pipeline (gate → agent → store → events).

use std::sync::Arc;

use openrt_config::{Config, PermissionsConfig};
use openrt_core::{EventKind, TaskStatus};
use openrt_models::mock::MockProvider;
use openrt_persistence::MemoryStore;
use openrt_telemetry::ChannelSink;

/// Sink writes are async; wait for a terminal event before snapshotting
/// (same pattern as `event_ordering.rs`).
async fn wait_for(channel: &ChannelSink, kind: EventKind) {
    for _ in 0..200 {
        if channel.snapshot().iter().any(|e| e.kind == kind) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("{kind:?} never arrived");
}

async fn build(config: Config) -> (Arc<openrt_runtime::Runtime>, Arc<ChannelSink>) {
    let sink = Arc::new(ChannelSink::new());
    let rt = openrt_runtime::RuntimeBuilder::new()
        .config(config)
        .store(Arc::new(MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
        .sink(sink.clone())
        .build()
        .await
        .unwrap();
    (rt, sink)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn denied_echo_never_executes() {
    let config = Config {
        permissions: PermissionsConfig { rules: Vec::new() }, // deny everything
        ..Config::default()
    };
    let (rt, channel) = build(config).await;

    let session = rt.create_session("deny").await.unwrap();
    let task = rt.run_task(session.id, "use echo").await.unwrap();

    // Denied → the denial comes back to the model as structured, visible
    // feedback; the model reports the obstacle and the task completes with
    // the denial in its result (no silent failure, no crash).
    assert_eq!(task.status, TaskStatus::Completed, "{task:?}");
    assert!(
        task.result
            .as_deref()
            .unwrap_or_default()
            .contains("permission denied"),
        "{task:?}"
    );

    // Permission request + denial recorded; the tool must never run.
    wait_for(&channel, EventKind::PermissionDenied).await;
    let events = channel.snapshot();
    assert!(
        events
            .iter()
            .any(|e| e.kind == EventKind::PermissionRequested),
        "expected PermissionRequested, got {:?}",
        event_kinds(&events)
    );
    assert!(
        events.iter().any(|e| e.kind == EventKind::PermissionDenied),
        "expected PermissionDenied, got {:?}",
        event_kinds(&events)
    );
    assert!(
        !events
            .iter()
            .any(|e| e.kind == EventKind::ToolInvocationStarted),
        "denied tool was executed: {:?}",
        event_kinds(&events)
    );
    assert!(
        !events
            .iter()
            .any(|e| e.kind == EventKind::ToolInvocationCompleted),
        "denied tool completed: {:?}",
        event_kinds(&events)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn allowed_tool_gets_completed_events() {
    let config = Config::default(); // echo/system.info/filesystem.read allowed
    let (rt, channel) = build(config).await;

    let session = rt.create_session("allow").await.unwrap();
    let task = rt.run_task(session.id, "echo ok").await.unwrap();
    assert_eq!(task.status, TaskStatus::Completed, "{task:?}");

    // High-water mark: the completed tool invocation.
    wait_for(&channel, EventKind::ToolInvocationCompleted).await;
    let events = channel.snapshot();
    assert!(
        events
            .iter()
            .any(|e| e.kind == EventKind::ToolInvocationStarted),
        "tool never started: {:?}",
        event_kinds(&events)
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind == EventKind::ToolInvocationCompleted),
        "tool never completed: {:?}",
        event_kinds(&events)
    );
    assert!(!events.iter().any(|e| e.kind == EventKind::PermissionDenied));
}

fn event_kinds(events: &[openrt_core::DomainEvent]) -> Vec<String> {
    events.iter().map(|e| format!("{:?}", e.kind)).collect()
}
