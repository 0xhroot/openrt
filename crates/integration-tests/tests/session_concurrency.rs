//! Integration: concurrent task runs on the same session must not lose the
//! transcript fold (SEC-08), while different sessions stay independent.

use std::sync::Arc;

use openrt_core::TaskStatus;
use openrt_models::mock::MockProvider;
use openrt_persistence::MemoryStore;

async fn runtime() -> Arc<openrt_runtime::Runtime> {
    let rt = openrt_runtime::RuntimeBuilder::new()
        .store(Arc::new(MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
        .bus_capacity(128)
        .build()
        .await
        .unwrap();
    rt
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_runs_on_same_session_keep_every_fold() {
    let rt = runtime().await;
    let session = rt.create_session("concurrent").await.unwrap();

    // 8 runs on the SAME session, all in flight at once. Without per-session
    // serialization some folds would be overwritten (last-writer-wins).
    let mut handles = Vec::new();
    for i in 0..8 {
        let rt = rt.clone();
        let sid = session.id;
        handles.push(tokio::spawn(async move {
            rt.run_task(sid, &format!("echo c{i}")).await.unwrap()
        }));
    }
    for h in handles {
        let task = h.await.unwrap();
        assert_eq!(task.status, TaskStatus::Completed, "{task:?}");
    }

    let final_session = rt.load_session(session.id).await.unwrap();
    // Serialized per session (SEC-08): the first fold persists a full tool
    // round (user + assistant tool-call + tool result + final = 4); the
    // other seven inherit a Tool message in history, so the mock answers
    // directly (user + final = 2 each) => 4 + 7*2 = 18.
    assert_eq!(
        final_session.content.len(),
        18,
        "all 8 folds (prompt + tool round + result each) must survive: {:?}",
        final_session
            .content
            .iter()
            .map(|m| &m.content)
            .collect::<Vec<_>>()
    );
    for i in 0..8 {
        assert!(
            final_session
                .content
                .iter()
                .any(|m| m.content.contains(&format!("c{i}"))),
            "result for c{i} was lost"
        );
    }
    let tasks = rt.store.list_tasks(Some(session.id)).await.unwrap();
    assert_eq!(tasks.len(), 8, "all tasks must be durable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn different_sessions_stay_independent() {
    let rt = runtime().await;
    let s1 = rt.create_session("s1").await.unwrap();
    let s2 = rt.create_session("s2").await.unwrap();

    let (t1, t2) = tokio::join!(
        rt.run_task(s1.id, "echo one"),
        rt.run_task(s2.id, "echo two")
    );
    assert_eq!(t1.unwrap().status, TaskStatus::Completed);
    assert_eq!(t2.unwrap().status, TaskStatus::Completed);

    for sid in [s1.id, s2.id] {
        let s = rt.load_session(sid).await.unwrap();
        assert_eq!(s.content.len(), 4, "each session keeps only its own fold");
    }
}
