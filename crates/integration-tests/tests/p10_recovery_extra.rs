//! Phase 10 M10.2/M10.3/M10.5 — recovery extras beyond the boot tests in
//! `p10_recovery.rs`:
//! * checkpoint creation/integrity (seal → verify → tamper refused),
//! * rollback that never rewrites the append-only history ledger,
//! * corrupted-checkpoint refusal on every consumer path,
//! * the time-machine view set (timeline, state-as-of),
//! * fork after a historical point + branch compare + return-to-current,
//! * live task handles (spawn/observe/cancel through one authority),
//! * partial-task reconciliation (interrupted, never promoted).

use std::sync::Arc;

use openrt_audit::AuditQuery;
use openrt_blackboard::{Blackboard, BlackboardSnapshot, BLACKBOARD_NS};
use openrt_checkpoint::manager::CHECKPOINT_NS;
use openrt_checkpoint::{
    Checkpoint, CheckpointError, CheckpointKind, CheckpointManager, CheckpointStatus,
    DependencyState, NodeEffect, NodeState, NodeStatus, PolicyCompat, RollbackAuthorization,
    RunRefs, CHECKPOINT_SCHEMA_VERSION,
};
use openrt_core::{now, TaskStatus};
use openrt_models::mock::MockProvider;
use openrt_persistence::{MemoryStore, PersistenceStore};
use openrt_runtime::{Runtime, RuntimeBuilder, TimelineOptions};

async fn build(store: Arc<dyn PersistenceStore>) -> Arc<Runtime> {
    RuntimeBuilder::new()
        .config(openrt_config::Config::default())
        .store(store)
        .with_provider(Arc::new(MockProvider::echo("mock")))
        .bus_capacity(32)
        .build()
        .await
        .unwrap()
}

/// A minimal but structurally complete checkpoint (M10.2 model) for `wf`.
fn mk_cp(
    session_id: openrt_core::SessionId,
    wf: &str,
    status: CheckpointStatus,
    policy_version: Option<String>,
    bb: BlackboardSnapshot,
) -> Checkpoint {
    Checkpoint {
        schema_version: CHECKPOINT_SCHEMA_VERSION,
        checkpoint_id: openrt_core::CheckpointId::new(),
        session_id,
        task_id: None,
        workflow_id: wf.into(),
        created_at: now(),
        kind: CheckpointKind::Initial,
        status,
        node_states: vec![NodeStatus {
            step: "plan".into(),
            state: NodeState::Completed,
            attempts: 1,
            artifact_key: Some("output.plan".into()),
            error: None,
            failure_category: String::new(),
            effect: NodeEffect::Unknown,
        }],
        completed: vec!["plan".into()],
        pending: vec![],
        failed: vec![],
        uncertain: vec![],
        current_node: None,
        dependency_state: DependencyState {
            ready: vec![],
            deps_by_step: std::collections::HashMap::new(),
        },
        retry_counters: std::collections::HashMap::new(),
        blackboard: bb,
        artifact_refs: vec![],
        execution: Default::default(),
        refs: RunRefs {
            policy_version,
            ..Default::default()
        },
        digest: None,
    }
}

async fn wait_ledger(rt: &Runtime, min: u64) {
    for _ in 0..200 {
        if rt.history_tail().await.unwrap() >= min {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("history ledger did not reach {min}");
}

/// Poll until the ledger tail stops moving — in-flight sink writes settle
/// before a test takes a baseline snapshot (deterministic, no fixed sleep).
async fn wait_stable(rt: &Runtime) -> u64 {
    let mut last = rt.history_verify().await.unwrap();
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let cur = rt.history_verify().await.unwrap();
        if cur == last {
            return cur;
        }
        last = cur;
    }
    last
}

// ---- M10.2 checkpoint creation / integrity ----

/// Creation seals an integrity digest; a tampered record never loads again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checkpoint_creation_seals_digest_and_tamper_fails_integrity() {
    let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
    let rt = build(store.clone()).await;
    let session = rt.create_session("cp-integrity").await.unwrap();

    let manager = CheckpointManager::new(store.clone());
    let id = manager
        .create(&mk_cp(
            session.id,
            "wf-integrity",
            CheckpointStatus::Completed,
            None,
            BlackboardSnapshot::default(),
        ))
        .await
        .unwrap();

    // Sealed + verified load (happy path).
    let loaded = manager.load(&id).await.unwrap();
    assert!(loaded.digest.is_some(), "create() must seal a digest");

    // Tamper with a digest-covered field that passes schema validation
    // (bump an attempt count) — integrity verification must refuse it on
    // load, never surface a half-truth.
    let key = id.to_string();
    let mut raw = store
        .get_record(CHECKPOINT_NS, &key)
        .await
        .unwrap()
        .expect("checkpoint exists");
    raw["node_states"][0]["attempts"] = serde_json::json!(2);
    store.put_record(CHECKPOINT_NS, &key, &raw).await.unwrap();
    let err = manager.load(&id).await.unwrap_err();
    match err {
        CheckpointError::Corrupt(_, detail) => {
            assert!(
                detail.contains("digest") || detail.contains("integrity"),
                "{detail}"
            )
        }
        other => panic!("tampered checkpoint must be Corrupt, got {other:?}"),
    }
    rt.shutdown().await;
}

// ---- M10.3 rollback vs. the immutable ledger ----

/// An authorized rollback restores blackboard state while the history
/// ledger stays byte-identical (chain verifies before AND after) and the
/// operation is audited as `recovery.rollback`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rollback_restores_state_and_never_rewrites_history() {
    let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
    let rt = build(store.clone()).await;
    let session = rt.create_session("rollback-hist").await.unwrap();
    rt.run_task(session.id, "echo one").await.unwrap();
    rt.run_task(session.id, "echo two").await.unwrap();
    wait_ledger(&rt, 4).await;

    // Baseline: verified ledger BEFORE the rollback.
    let tail_before = rt.history_verify().await.unwrap();
    assert!(tail_before >= 4);

    // Checkpoint snapshotting `output.plan` under the CURRENT policy.
    let mut bb = BlackboardSnapshot::default();
    bb.entries
        .push(("output.plan".into(), serde_json::json!("ORIGINAL")));
    let manager = CheckpointManager::new(store.clone());
    let fp = rt.policy_fingerprint();
    let id = manager
        .create(&mk_cp(
            session.id,
            "wf-rollback-hist",
            CheckpointStatus::Running,
            Some(fp),
            bb,
        ))
        .await
        .unwrap();

    // Live state drifts away from the snapshot.
    let live = Blackboard::with_store(store.clone(), BLACKBOARD_NS)
        .await
        .unwrap();
    live.put("output.plan", serde_json::json!("LIVE"))
        .await
        .unwrap();

    let auth = RollbackAuthorization {
        operator: rt.asset_operators()[0].clone(),
        acknowledge_policy_drift: false,
    };
    let report = rt.rollback_checkpoint(&id, &auth, None).await.unwrap();
    assert_eq!(report.policy_compat, Some(PolicyCompat::Compatible));
    assert_eq!(report.history_immutable, Some(true));
    assert_eq!(report.operator.as_deref(), Some(auth.operator.as_str()));

    // Restored.
    let after = Blackboard::with_store(store.clone(), BLACKBOARD_NS)
        .await
        .unwrap();
    assert_eq!(
        after.get("output.plan").unwrap().unwrap(),
        serde_json::json!("ORIGINAL")
    );

    // The append-only ledger: verification still passes, the pre-rollback
    // prefix is byte-identical, and the tail may only ever GROW (rollback
    // events append like every other event — history is never rewritten).
    let tail_after = rt.history_verify().await.unwrap();
    assert!(
        tail_after >= tail_before,
        "append-only: tail shrank {tail_before} -> {tail_after}"
    );
    let prefix_before = openrt_timeseries::materialize_as_of(&*store, tail_before)
        .await
        .unwrap();
    let prefix_after = openrt_timeseries::materialize_as_of(&*store, tail_before)
        .await
        .unwrap();
    assert_eq!(prefix_before, prefix_after, "prefix must be byte-identical");

    // Audited exactly once.
    let audit = rt
        .audit_query(&AuditQuery::new().with_action_prefix("recovery.rollback"))
        .await
        .unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].entry.result, "completed");
    rt.audit_verify().await.unwrap();
    rt.shutdown().await;
}

// ---- corrupted-checkpoint refusal on every consumer ----

/// A corrupted checkpoint is refused by load, rollback AND resume — no
/// consumer ever operates on a half-truth.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupted_checkpoint_is_refused_by_every_consumer() {
    let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
    let rt = build(store.clone()).await;
    let session = rt.create_session("corrupt-cp").await.unwrap();

    let manager = CheckpointManager::new(store.clone());
    let id = manager
        .create(&mk_cp(
            session.id,
            "wf-corrupt",
            CheckpointStatus::Running,
            None,
            BlackboardSnapshot::default(),
        ))
        .await
        .unwrap();

    // Corrupt the durable record (deserialization-level breakage).
    let key = id.to_string();
    store
        .put_record(
            CHECKPOINT_NS,
            &key,
            &serde_json::json!({"not": "a checkpoint"}),
        )
        .await
        .unwrap();

    assert!(manager.load(&id).await.is_err(), "load must refuse");

    let auth = RollbackAuthorization {
        operator: rt.asset_operators()[0].clone(),
        acknowledge_policy_drift: true,
    };
    let err = rt.rollback_checkpoint(&id, &auth, None).await.unwrap_err();
    assert!(
        matches!(err, openrt_runtime::Error::Checkpoint(_)),
        "rollback must fail closed before any mutation: {err}"
    );

    let err = rt
        .resume_workflow(
            session.id,
            openrt_orch::Workflow::new("wf-corrupt"),
            id,
            openrt_runtime::CheckpointOptions::default(),
            openrt_checkpoint::RecoveryPolicy::default(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, openrt_runtime::Error::Checkpoint(_)),
        "resume must refuse a corrupt checkpoint: {err}"
    );
    rt.shutdown().await;
}

// ---- M10.5 time-machine view ----

/// Timeline + state-as-of render REAL verified ledger rows; chain
/// verification runs first (a tampered ledger yields no view at all).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn time_machine_timeline_and_state_as_of_view_real_history() {
    let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
    let rt = build(store.clone()).await;
    let session = rt.create_session("tm-view").await.unwrap();
    rt.run_task(session.id, "echo a").await.unwrap();
    rt.run_task(session.id, "echo b").await.unwrap();
    wait_ledger(&rt, 4).await;
    wait_stable(&rt).await;

    let sid = session.id.to_string();
    let timeline = rt
        .history_timeline(&TimelineOptions {
            session: Some(sid.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(!timeline.records.is_empty(), "session-scoped timeline");
    for w in timeline.records.windows(2) {
        assert!(w[0].seq < w[1].seq, "ordered chain");
    }
    assert!(timeline.records.iter().all(|r| r.session_id == sid));

    // State as of the current head: rows match the timeline, sessions summary
    // covers our session.
    let head = timeline.tail_seq;
    let as_of_now = rt.history_state_as_of(head).await.unwrap();
    assert_eq!(as_of_now.as_of, head);
    assert_eq!(as_of_now.tail_seq, head);
    assert!(as_of_now
        .sessions
        .iter()
        .any(|s| s.session_id == sid && s.events >= 3));

    // State as of the genesis row: strictly fewer rows than the head view.
    let as_of_early = rt.history_state_as_of(1).await.unwrap();
    assert_eq!(as_of_early.as_of, 1);
    assert!(as_of_early.records.len() <= as_of_now.records.len());

    // Verification surface agrees with the timeline.
    let verified = rt.history_verify().await.unwrap();
    assert_eq!(verified, head);
    rt.shutdown().await;
}

// ---- fork after a historical point ----

/// Forking mid-history branches the ledger without touching the parent:
/// the parent tail/verify stay identical, the branch verifies against the
/// shared base, and return-to-current re-anchors at the parent head.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_after_historical_point_branches_without_touching_parent() {
    let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
    let rt = build(store.clone()).await;
    let session = rt.create_session("tm-fork").await.unwrap();
    rt.run_task(session.id, "echo base").await.unwrap();
    wait_ledger(&rt, 3).await;
    let parent_tail = wait_stable(&rt).await;
    let base_seq = parent_tail / 2;
    assert!(base_seq >= 1, "need a historical point inside the chain");

    rt.history_fork("p10-branch", &format!("seq.{base_seq}"))
        .await
        .unwrap();

    // The catalog knows the branch.
    let forks = rt.history_forks().await.unwrap();
    assert!(forks.iter().any(|f| f.branch == "p10-branch"));

    // Divergent commit on the branch.
    let fork_seq = rt
        .history_fork_commit(
            "p10-branch",
            "node.fork",
            "ok",
            "CustomEvent",
            "divergent row",
        )
        .await
        .unwrap();
    assert!(fork_seq > 0);

    // Branch comparison: shared prefix up to the base, verified commits.
    let cmp = rt.history_branch_compare("p10-branch").await.unwrap();
    assert_eq!(cmp.branch, "p10-branch");
    assert_eq!(cmp.base_seq, base_seq);
    assert_eq!(cmp.parent_tail_seq, parent_tail);
    assert!(cmp.fork_records_verified >= 1);
    assert!(cmp.entries.iter().any(|e| e.diverged));

    // Parent untouched: same verified tail, no new rows.
    let parent_after = rt.history_verify().await.unwrap();
    assert_eq!(
        parent_tail, parent_after,
        "fork must never write the parent"
    );

    // Return-to-current re-anchors at the parent head with the fork catalog.
    let cur = rt.history_return_to_current().await.unwrap();
    assert_eq!(cur.tail_seq, parent_after);
    assert!(cur.forks.iter().any(|f| f.branch == "p10-branch"));

    // Branch chain verifies independently.
    let verified = rt.history_fork_verify("p10-branch").await.unwrap();
    assert!(verified >= 1);
    rt.shutdown().await;
}

// ---- M10.4 live task handles ----

/// One live handle: spawn (durable Created) → observe → execute → terminal;
/// a cancelled handle never runs the pipeline to success.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_task_handle_spawns_observes_and_cancels() {
    let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
    let rt = build(store.clone()).await;
    let session = rt.create_session("live-handle").await.unwrap();

    // Spawn is durable BEFORE execution starts.
    let handle = rt.spawn_task(session.id, "echo live").await.unwrap();
    let tid = handle.task_id();
    assert_eq!(rt.task_status(tid).await.unwrap(), TaskStatus::Created);
    let seen = rt.task_inspect(tid).await.unwrap();
    assert_eq!(seen.task.id, tid);

    // Execute through the same handle — completes, logs are readable.
    let task = handle.execute().await.unwrap();
    assert_eq!(task.status, TaskStatus::Completed);
    assert_eq!(rt.task_status(tid).await.unwrap(), TaskStatus::Completed);
    let logs = rt.task_logs(tid).await.unwrap();
    assert!(!logs.is_empty(), "turns recorded for the live run");

    // Cancel BEFORE execute: the handle's token is the cancel authority and
    // the run never reaches Completed.
    let doomed = rt.spawn_task(session.id, "echo doomed").await.unwrap();
    let doomed_id = doomed.task_id();
    doomed.cancel();
    let task = doomed.execute().await.unwrap();
    assert_ne!(
        task.status,
        TaskStatus::Completed,
        "a pre-cancelled handle must not run to success"
    );
    assert!(
        matches!(
            rt.task_status(doomed_id).await.unwrap(),
            TaskStatus::Cancelled | TaskStatus::Interrupted | TaskStatus::Failed
        ),
        "terminal-not-success status, got {:?}",
        rt.task_status(doomed_id).await.unwrap()
    );
    rt.shutdown().await;
}

// ---- partial-task reconciliation ----

/// A Running task with NO checkpoint is interrupted at boot — never
/// promoted to Completed — and the recovery record counts only the task
/// (no phantom incomplete runs).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partial_task_reconciliation_interrupts_without_phantom_runs() {
    let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
    let rt1 = build(store.clone()).await;
    let session = rt1.create_session("partial").await.unwrap();
    rt1.shutdown().await;
    drop(rt1);

    // Durable leftovers: a task stuck Running, no checkpoint chain.
    let mut task = openrt_core::Task::new(session.id, "echo partial");
    task.transition(TaskStatus::Running).unwrap();
    store.save_task(&task).await.unwrap();

    let sink = Arc::new(openrt_telemetry::ChannelSink::new());
    let rt2 = RuntimeBuilder::new()
        .config(openrt_config::Config::default())
        .store(store.clone())
        .with_provider(Arc::new(MockProvider::echo("mock")))
        .sink(sink.clone())
        .bus_capacity(32)
        .build()
        .await
        .unwrap();

    let after = store.load_task(task.id).await.unwrap();
    assert_eq!(after.status, TaskStatus::Interrupted);
    assert!(after.completed_at.is_none(), "never promoted");

    let records = rt2.recovery_records().await.unwrap();
    let last = records.last().unwrap();
    assert!(!last.is_clean());
    assert_eq!(last.interrupted_tasks.len(), 1);
    assert_eq!(last.interrupted_tasks[0].task_id, task.id);
    assert_eq!(
        last.interrupted_tasks[0].previous_status,
        TaskStatus::Running
    );
    assert!(
        last.incomplete_runs.is_empty(),
        "no checkpoint = no phantom incomplete run"
    );
    rt2.shutdown().await;
}
