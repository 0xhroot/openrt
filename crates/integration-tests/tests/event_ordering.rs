//! Integration: event ordering — a single task run must emit its lifecycle
//! events to every sink in total order (the bus dispatcher serializes writes).

use std::sync::Arc;

use openrt_core::{EventKind, TaskStatus};
use openrt_models::mock::MockProvider;
use openrt_persistence::MemoryStore;
use openrt_telemetry::ChannelSink;

async fn build() -> (Arc<openrt_runtime::Runtime>, Arc<ChannelSink>) {
    let channel = Arc::new(ChannelSink::new());
    let rt = openrt_runtime::RuntimeBuilder::new()
        .store(Arc::new(MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
        .sink(channel.clone())
        .build()
        .await
        .unwrap();
    (rt, channel)
}

fn assert_relative_order(events: &[openrt_core::DomainEvent], seq: &[EventKind]) {
    let kinds: Vec<&openrt_core::EventKind> = events.iter().map(|e| &e.kind).collect();
    let mut last = None;
    for want in seq {
        let idx = kinds
            .iter()
            .position(|k| *k == want)
            .unwrap_or_else(|| panic!("kind {want:?} missing"));
        if let Some(prev) = last {
            assert!(prev < idx, "{want:?} not ordered after {seq:?}: {kinds:?}");
        }
        last = Some(idx);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_events_are_totally_ordered() {
    let (rt, channel) = build().await;

    let session = rt.create_session("order").await.unwrap();
    let task = rt.run_task(session.id, "echo ordered").await.unwrap();
    assert_eq!(task.status, TaskStatus::Completed);

    // Sink writes are async; wait for the TaskCompleted bookend.
    let mut events = Vec::new();
    for _ in 0..200 {
        events = channel.snapshot();
        if events.iter().any(|e| e.kind == EventKind::TaskCompleted) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        events.iter().any(|e| e.kind == EventKind::TaskCompleted),
        "TaskCompleted never arrived: {events:?}"
    );

    // Strict total order for one scripted echo run.
    assert_relative_order(
        &events,
        &[
            EventKind::RuntimeStarted,
            EventKind::SessionCreated,
            EventKind::TaskCreated,
            EventKind::TaskStarted,
            EventKind::AgentStarted,
            EventKind::PermissionRequested,
            EventKind::PermissionGranted,
            EventKind::ResourceCheck,
            EventKind::ToolInvocationStarted,
            EventKind::ToolInvocationCompleted,
            EventKind::AgentCompleted,
            EventKind::TaskCompleted,
        ],
    );

    // The task bookends are correlated to the same session + task (agent_id is
    // carried in event data by the agent bookends, not the correlation field).
    let tc = events
        .iter()
        .find(|e| e.kind == EventKind::TaskCompleted)
        .unwrap();
    assert_eq!(tc.session_id, Some(session.id));
    assert_eq!(tc.task_id, Some(task.id));
    assert_eq!(tc.agent_id, None);
}
