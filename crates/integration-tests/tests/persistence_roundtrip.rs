//! Integration: JSON-file persistence survives a full runtime restart — the
//! session transcript, task history and event log are reloadable.

use std::sync::Arc;

use openrt_core::{TaskStatus, Timestamp};
use openrt_models::mock::MockProvider;
use openrt_persistence::LocalFileStore;

fn temp_dir() -> std::path::PathBuf {
    let uniq = uuid::Uuid::new_v4();
    std::env::temp_dir().join(format!("openrt-rt-{uniq}"))
}

async fn persisted_runtime(dir: &std::path::Path) -> Arc<openrt_runtime::Runtime> {
    openrt_runtime::RuntimeBuilder::new()
        .store(Arc::new(LocalFileStore::open(dir).await.unwrap()))
        .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
        .build()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_store_survives_restart() {
    let dir = temp_dir();

    // First incarnation: write a session + completed task.
    {
        let rt = persisted_runtime(&dir).await;
        let session = rt.create_session("roundtrip").await.unwrap();
        let task = rt.run_task(session.id, "echo persisted").await.unwrap();
        assert_eq!(task.status, TaskStatus::Completed);
        assert!(task
            .result
            .as_deref()
            .unwrap_or_default()
            .contains("persisted"));
    }

    // Second incarnation: same directory, fresh runtime.
    {
        let rt = persisted_runtime(&dir).await;

        let sessions = rt.store.list_sessions().await.unwrap();
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.title, "roundtrip");
        // user prompt + assistant tool-call turn + tool result + final answer
        assert_eq!(session.content.len(), 4);
        assert!(
            session.content[3].content.contains("persisted"),
            "{:?}",
            session.content
        );

        let tasks = rt.store.list_tasks(Some(session.id)).await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, TaskStatus::Completed);
        assert!(tasks[0].completed_at.is_some());

        // Event log persisted: at least the lifecycle bookends for a task run.
        let events = rt
            .store
            .list_events(openrt_persistence::ListOptions {
                limit: None,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(!events.is_empty(), "expected persisted events");
        assert!(events.iter().any(|e| e.session_id == Some(session.id)));
        assert!(
            events.len() >= 6,
            "expected lifecycle events, got {}",
            events.len()
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timestamps_are_durable() {
    let dir = temp_dir();
    let created_at: Timestamp;
    {
        let rt = persisted_runtime(&dir).await;
        let session = rt.create_session("ts").await.unwrap();
        let task = rt.run_task(session.id, "echo ts").await.unwrap();
        created_at = task.created_at;
        assert_eq!(task.status, TaskStatus::Completed);
    }
    {
        let rt = persisted_runtime(&dir).await;
        let sessions = rt.store.list_sessions().await.unwrap();
        let tasks = rt.store.list_tasks(Some(sessions[0].id)).await.unwrap();
        assert_eq!(tasks[0].created_at, created_at);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
