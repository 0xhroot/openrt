//! Integration: persistence failure semantics (SEC-06 / PC-Q8) — required-
//! durable records fail the call, while event-log write failures are surfaced
//! (runtime health + `doctor`), never silently swallowed.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use openrt_core::{
    DomainEvent, ExecutionId, ExecutionRecord, Session, SessionId, Task, TaskId, TaskStatus,
};
use openrt_models::mock::MockProvider;
use openrt_persistence::{ListOptions, MemoryStore, PersistenceError, PersistenceStore};
use openrt_runtime::DoctorStatus;

/// Which operation the store double should fail.
#[derive(Debug, Clone, Copy)]
enum FailOp {
    AppendEvent,
    SaveTask,
}

/// A store that delegates to [`MemoryStore`] but injects failures at one
/// operation, to prove required-durable writes propagate while telemetry
/// writes are surfaced without breaking the run.
struct FailingStore {
    inner: MemoryStore,
    fail: FailOp,
}

impl FailingStore {
    fn io_error() -> PersistenceError {
        PersistenceError::Io {
            path: PathBuf::from("failpoint"),
            source: std::io::Error::other("injected failure"),
        }
    }
}

#[async_trait]
impl PersistenceStore for FailingStore {
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
        if matches!(self.fail, FailOp::SaveTask) {
            return Err(Self::io_error());
        }
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
        if matches!(self.fail, FailOp::AppendEvent) {
            return Err(Self::io_error());
        }
        self.inner.append_event(event).await
    }

    async fn list_events(&self, opts: ListOptions) -> Result<Vec<DomainEvent>, PersistenceError> {
        self.inner.list_events(opts).await
    }

    async fn clear(&self) -> Result<(), PersistenceError> {
        self.inner.clear().await
    }

    async fn put_record(
        &self,
        namespace: &str,
        key: &str,
        value: &serde_json::Value,
    ) -> Result<(), PersistenceError> {
        self.inner.put_record(namespace, key, value).await
    }

    async fn get_record(
        &self,
        namespace: &str,
        key: &str,
    ) -> Result<Option<serde_json::Value>, PersistenceError> {
        self.inner.get_record(namespace, key).await
    }

    async fn list_records(
        &self,
        namespace: &str,
    ) -> Result<Vec<(String, serde_json::Value)>, PersistenceError> {
        self.inner.list_records(namespace).await
    }

    async fn delete_record(&self, namespace: &str, key: &str) -> Result<(), PersistenceError> {
        self.inner.delete_record(namespace, key).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_log_failures_are_surfaced_not_silent() {
    let store = Arc::new(FailingStore {
        inner: MemoryStore::new(),
        fail: FailOp::AppendEvent,
    });
    let rt = openrt_runtime::RuntimeBuilder::new()
        .store(store)
        .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
        .build()
        .await
        .unwrap();

    // Records are required-durable: the task still completes.
    let session = rt.create_session("event-fail").await.unwrap();
    let task = rt.run_task(session.id, "echo works").await.unwrap();
    assert_eq!(task.status, TaskStatus::Completed, "{task:?}");
    let sessions = rt.store.list_sessions().await.unwrap();
    // user + assistant tool-call turn + tool result + final answer
    assert_eq!(sessions[0].content.len(), 4, "transcript still folded");

    // The failure is surfaced on the runtime health handle (dispatcher writes
    // are async, so poll until the first sink write lands).
    let mut snap = rt.event_log_health.snapshot();
    for _ in 0..200 {
        if snap.total > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        snap = rt.event_log_health.snapshot();
    }
    assert!(snap.total > 0, "no event writes were attempted: {snap:?}");
    assert_eq!(snap.failures, snap.total, "all writes failing: {snap:?}");
    assert!(snap.last_error.is_some(), "last error must be recorded");

    // ...and surfaced by `openrt doctor`.
    let report = rt.doctor().await;
    let check = report
        .checks
        .iter()
        .find(|c| c.name == "event-log")
        .expect("event-log check must be present");
    assert_eq!(check.status, DoctorStatus::Warn, "{check:?}");
    assert!(check.detail.contains("failed"), "{check:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_write_failure_fails_the_call() {
    let store = Arc::new(FailingStore {
        inner: MemoryStore::new(),
        fail: FailOp::SaveTask,
    });
    let rt = openrt_runtime::RuntimeBuilder::new()
        .store(store)
        .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
        .build()
        .await
        .unwrap();

    let session = rt.create_session("record-fail").await.unwrap();
    let err = rt.run_task(session.id, "echo x").await.unwrap_err();
    assert!(
        err.to_string().contains("persistence"),
        "expected a persistence failure to propagate, got {err}"
    );
}
