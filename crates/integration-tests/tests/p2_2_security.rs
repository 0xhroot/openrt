//! P2.2 security boundary, end-to-end through the real runtime:
//! * SEC-01 filesystem jail: unscoped default denies; `workspace_dir` scopes.
//! * ASK boundary: `Ask` rules fail closed without a resolver and are
//!   released only by an explicit resolver allowance.

use std::path::PathBuf;
use std::sync::Arc;

use openrt_config::{Config, PermissionsConfig, RuleConfig, RuleEffect};
use openrt_core::{EventKind, SessionId, TaskId, TaskStatus};
use openrt_models::mock::MockProvider;
use openrt_telemetry::ChannelSink;
use openrt_tools::ToolError;

fn tmp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "openrt-p2_2-{prefix}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn build(config: Config) -> (Arc<openrt_runtime::Runtime>, Arc<ChannelSink>) {
    let sink = Arc::new(ChannelSink::new());
    let rt = openrt_runtime::RuntimeBuilder::new()
        .config(config)
        .store(Arc::new(openrt_persistence::MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
        .sink(sink.clone())
        .build()
        .await
        .unwrap();
    (rt, sink)
}

fn ctx(session: SessionId, task: TaskId) -> openrt_tools::ToolExecutionContext {
    openrt_tools::ToolExecutionContext {
        session_id: Some(session.to_string()),
        task_id: Some(task.to_string()),
        channel: None,
    }
}

/// Sink writes are async; wait for the decisive event before snapshotting
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

/// SEC-01: with no workspace configured, `filesystem.read` is registered but
/// jailed — every read is a scope violation even though policy allows it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_runtime_jails_filesystem_read() {
    let (rt, _channel) = build(Config::default()).await;
    let session = rt.create_session("jail").await.unwrap();
    let task = openrt_core::Task::new(session.id, "read");
    let err = rt
        .executor
        .execute(
            "filesystem.read",
            serde_json::json!({ "path": "/etc/hostname" }),
            &ctx(session.id, task.id),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, ToolError::ScopeViolation { .. }),
        "expected ScopeViolation, got {err:?}"
    );
    rt.shutdown().await;
}

/// SEC-01: a configured workspace confines reads to that tree; outside paths
/// are rejected even though they exist and policy allows the capability.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_scoped_filesystem_read() {
    let ws = tmp_dir("ws");
    std::fs::write(ws.join("notes.txt"), b"inside").unwrap();

    let config = Config {
        runtime: openrt_config::RuntimeConfig {
            workspace_dir: Some(ws.clone()),
            ..openrt_config::RuntimeConfig::default()
        },
        ..Config::default()
    };
    let (rt, _channel) = build(config).await;
    let session = rt.create_session("scoped").await.unwrap();
    let task = openrt_core::Task::new(session.id, "read");

    // Inside the workspace: allowed.
    let out = rt
        .executor
        .execute(
            "filesystem.read",
            serde_json::json!({ "path": ws.join("notes.txt") }),
            &ctx(session.id, task.id),
        )
        .await
        .unwrap();
    assert_eq!(out.render(), "inside");

    // Outside the workspace: scope violation (policy still allows).
    let outside = std::env::temp_dir(); // sibling to (not under) the workspace
    let err = rt
        .executor
        .execute(
            "filesystem.read",
            serde_json::json!({ "path": outside.join("openrt-readme-should-not-exist") }),
            &ctx(session.id, task.id),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, ToolError::ScopeViolation { .. }),
        "expected ScopeViolation for out-of-workspace path, got {err:?}"
    );

    rt.shutdown().await;
    let _ = std::fs::remove_dir_all(&ws);
}

/// ASK boundary: an `Ask` rule without a resolver never runs the tool — the
/// agent sees the denial as structured feedback and the task completes with
/// the fail-closed error reported, tool never started.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_without_resolver_fails_closed() {
    let config = Config {
        permissions: PermissionsConfig {
            rules: vec![RuleConfig {
                effect: RuleEffect::Ask,
                capability: "echo".into(),
                resource: None,
            }],
        },
        ..Config::default()
    };
    let (rt, channel) = build(config).await;
    let session = rt.create_session("ask-deny").await.unwrap();
    let task = rt.run_task(session.id, "echo hi").await.unwrap();

    assert_eq!(task.status, TaskStatus::Completed, "{task:?}");
    assert!(
        task.result
            .as_deref()
            .unwrap_or_default()
            .contains("permission denied"),
        "{task:?}"
    );
    wait_for(&channel, EventKind::PermissionDenied).await;
    let events = channel.snapshot();
    assert!(
        events.iter().any(|e| e.kind == EventKind::PermissionDenied),
        "expected PermissionDenied, got {:?}",
        event_kinds(&events)
    );
    assert!(
        !events
            .iter()
            .any(|e| e.kind == EventKind::ToolInvocationStarted),
        "Ask tool ran despite no resolver: {:?}",
        event_kinds(&events)
    );
    rt.shutdown().await;
}

/// ASK boundary: with a resolver attached and an `Ask` rule, an explicit
/// allowance runs the tool and records `PermissionApproved`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_with_resolver_approved_runs_tool() {
    let config = Config {
        permissions: PermissionsConfig {
            rules: vec![RuleConfig {
                effect: RuleEffect::Ask,
                capability: "echo".into(),
                resource: None,
            }],
        },
        ..Config::default()
    };
    let sink = Arc::new(ChannelSink::new());
    let rt = openrt_runtime::RuntimeBuilder::new()
        .config(config)
        .store(Arc::new(openrt_persistence::MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
        .sink(sink.clone())
        .permission_resolver(Arc::new(openrt_permissions::MockResolver::allow_only(&[
            "echo",
        ])))
        .build()
        .await
        .unwrap();
    let session = rt.create_session("ask-approve").await.unwrap();
    let task = rt.run_task(session.id, "echo hi").await.unwrap();

    assert_eq!(task.status, TaskStatus::Completed, "{task:?}");
    // ToolInvocationCompleted is emitted last of the three, so once it is
    // dispatched both PermissionApproved and ToolInvocationStarted are present.
    wait_for(&sink, EventKind::ToolInvocationCompleted).await;
    let events = sink.snapshot();
    assert!(
        events
            .iter()
            .any(|e| e.kind == EventKind::PermissionApproved),
        "expected PermissionApproved, got {:?}",
        event_kinds(&events)
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind == EventKind::ToolInvocationStarted),
        "approved tool never ran: {:?}",
        event_kinds(&events)
    );
    rt.shutdown().await;
}

fn event_kinds(events: &[openrt_core::DomainEvent]) -> Vec<String> {
    events.iter().map(|e| format!("{:?}", e.kind)).collect()
}
