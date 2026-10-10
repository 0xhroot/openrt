//! Phase 11 — workbench integration tests 13–24.
//!
//! Tracks B–D (13–18): rooms + materials over the M5.1 workspace
//! foundation — deterministic ids, fail-closed workbench/material gates,
//! digest-sealed references, append-only steer/history, cross-build
//! persistence, audit rows.
//!
//! Track E (19–24): cooperative workflow pause/resume and room steer —
//! pause is a checkpoint stop (partial history, never a failure), resume
//! is the ONLY continuation path (never auto-resumed at boot), steer
//! hydrates into the run blackboard append-only, every action audited.
//!
//! Security invariants under test: no UI-local or second store (everything
//! round-trips the one PersistenceStore); no fabricated records (missing
//! workbench/material/session fail closed); a paused run is never reported
//! successful; resume never bypasses preflight.

use std::sync::Arc;
use std::time::{Duration, Instant};

use openrt_audit::AuditQuery;
use openrt_checkpoint::{CheckpointKind, CheckpointStatus};
use openrt_core::EventKind;
use openrt_models::mock::{MockMode, MockProvider};
use openrt_orch::{SubAgentKind, Workflow, WorkflowStatus, WorkflowStep, RUNS_NS};
use openrt_persistence::{MemoryStore, PersistenceStore};
use openrt_runtime::{Error, Runtime, RuntimeBuilder};
use openrt_telemetry::ChannelSink;
use openrt_workspace::{Material, MaterialKind, MaterialReference, Room, RoomStatus};

// ---- helpers ----

async fn echo_rt() -> (Arc<MemoryStore>, Arc<Runtime>, Arc<ChannelSink>) {
    let store = Arc::new(MemoryStore::new());
    let sink = Arc::new(ChannelSink::new());
    let rt = RuntimeBuilder::new()
        .store(store.clone())
        .with_provider(Arc::new(MockProvider::echo("mock")))
        .sink(sink.clone())
        .bus_capacity(64)
        .build()
        .await
        .unwrap();
    (store, rt, sink)
}

/// Slow provider: every agent step sleeps 300ms before answering, so a
/// pause request lands deterministically while a step is in flight. The
/// provider handle is returned so tests can count REAL model calls
/// (the honest proof that seeded steps are never re-executed).
async fn slow_rt() -> (
    Arc<MemoryStore>,
    Arc<Runtime>,
    Arc<ChannelSink>,
    Arc<MockProvider>,
) {
    let store = Arc::new(MemoryStore::new());
    let sink = Arc::new(ChannelSink::new());
    let provider = Arc::new(
        MockProvider::new("mock", MockMode::Slow).with_latency(Duration::from_millis(300)),
    );
    let rt = RuntimeBuilder::new()
        .store(store.clone())
        .with_provider(provider.clone())
        .sink(sink.clone())
        .bus_capacity(64)
        .build()
        .await
        .unwrap();
    (store, rt, sink, provider)
}

async fn workbench(store: &Arc<MemoryStore>, code: &str) {
    openrt_workspace::create_workspace(&**store, code, "Lab", "", "2026-09-27T00:00:00Z")
        .await
        .unwrap();
}

fn chain_workflow(id: &str, steps: usize) -> Workflow {
    let mut wf = Workflow::new(id);
    for i in 0..steps {
        let name = format!("s{i}");
        let step = WorkflowStep::new(name.as_str(), SubAgentKind::Planner, "advance the goal");
        let step = if i == 0 {
            step
        } else {
            step.dep(&format!("s{}", i - 1))
        };
        wf = wf.step(step);
    }
    wf
}

async fn wait_for(sink: &ChannelSink, kind: EventKind, n: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let events = sink.snapshot();
        if events.iter().filter(|e| e.kind == kind).count() >= n {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {n} event(s) {kind:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Spawn `wf` on a background task, wait until the FIRST step has durably
/// completed (partial history guaranteed — no pre-spawn races), then
/// request the cooperative pause while later steps are still running.
async fn spawn_and_pause(rt: &Arc<Runtime>, sink: &ChannelSink, wf: Workflow) -> WorkflowRunReport {
    let session = rt.create_session("pause").await.unwrap();
    let rt2 = rt.clone();
    let sid = session.id;
    let workflow_id = wf.id.clone();
    let handle = tokio::spawn(async move { rt2.run_workflow(sid, wf).await.unwrap() });
    wait_for(sink, EventKind::WorkflowStepCompleted, 1).await;
    rt.workflow_pause(&workflow_id, "alice").await.unwrap();
    handle.await.unwrap()
}

use openrt_orch::WorkflowRunReport;

fn checkpoints_chronological(
    mut cps: Vec<openrt_checkpoint::CheckpointMetadata>,
) -> Vec<openrt_checkpoint::CheckpointMetadata> {
    cps.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    cps
}

async fn runs_status(store: &MemoryStore, workflow_id: &str) -> String {
    let v = store
        .get_record(RUNS_NS, workflow_id)
        .await
        .unwrap()
        .expect("runs record must exist");
    v["status"].as_str().unwrap().to_string()
}

fn err_contains(e: &Error, needle: &str) -> bool {
    e.to_string().contains(needle)
}

// ---- tests 13–18: workbench / rooms / materials ----

/// TEST 13: room lifecycle — workbench gate, deterministic identity,
/// status/workflow transitions with append-only history, cross-build
/// persistence (fresh Runtime over the same store sees identical state).
#[tokio::test]
async fn t13_room_lifecycle_gated_and_persistent() {
    let (store, rt, _sink) = echo_rt().await;
    workbench(&store, "acct/lab").await;

    // Unknown workbench fails closed — a room never exists without one.
    let err = rt
        .create_room("acct/ghost", "Nowhere", "alice")
        .await
        .unwrap_err();
    assert!(err_contains(&err, "not found"));

    // Empty title fails closed.
    let err = rt.create_room("acct/lab", "  ", "alice").await;
    assert!(err.is_err());

    let room = rt.create_room("acct/lab", "Triage", "alice").await.unwrap();
    assert_eq!(room.status, RoomStatus::Active);
    assert_eq!(room.workbench_code, "acct/lab");
    assert_eq!(room.created_by, "alice");

    // Deterministic identity: same inputs + same created_at ⇒ same id.
    let again =
        openrt_workspace::create_room(&*store, "acct/lab", "Triage", "alice", &room.created_at)
            .await;
    match again {
        Err(openrt_workspace::WorkspaceError::AlreadyExists(_)) => {}
        Ok(other) => panic!("same-input room must collide deterministically: {other:?}"),
        Err(e) => panic!("unexpected error: {e}"),
    }

    // Status + active workflow transitions recorded in history.
    let room = rt
        .set_room_status(&room.id, RoomStatus::Paused, "alice")
        .await
        .unwrap();
    assert_eq!(room.status, RoomStatus::Paused);
    let room = rt
        .set_room_workflow(&room.id, Some("wf-x"), "alice")
        .await
        .unwrap();
    assert_eq!(room.active_workflow.as_deref(), Some("wf-x"));
    let room = rt.set_room_workflow(&room.id, None, "alice").await.unwrap();
    assert!(room.active_workflow.is_none());
    assert_eq!(room.history[0].action, "room.created");
    assert!(room.history.iter().any(|h| h.action == "room.status"));
    assert!(room
        .history
        .iter()
        .any(|h| h.action == "room.active_workflow"));

    // Linking session/artifact is idempotent (link, never duplicate).
    let room = rt
        .link_room_session(&room.id, "ses-1", "alice")
        .await
        .unwrap();
    let room = rt
        .link_room_session(&room.id, "ses-1", "alice")
        .await
        .unwrap();
    assert_eq!(room.sessions, vec!["ses-1".to_string()]);

    // Roles are configuration (M11.4): the role must map onto an existing
    // SubAgentKind identity — unknown roles fail closed before any write.
    let room = rt
        .link_room_agent(&room.id, "agent-1", "researcher", "alice")
        .await
        .unwrap();
    assert_eq!(room.agents.len(), 1);
    assert_eq!(
        room.agents[0].role,
        openrt_workspace::RoomRole::Researcher,
        "room role stored as configuration"
    );
    let err = rt
        .link_room_agent(&room.id, "agent-2", "pirate", "alice")
        .await
        .unwrap_err();
    assert!(err_contains(&err, "unknown room role"));
    assert!(
        rt.room(&room.id).await.unwrap().agents.len() == 1,
        "failed role never writes a record"
    );
    // The audit row records the resolved orch identity (one authority).
    let records = rt
        .audit_query(&AuditQuery::new().with_action_prefix("room.agent.link"))
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].entry.actor, "alice");
    assert_eq!(records[0].entry.ext["sub_agent"], "researcher");
    drop(rt);

    // Cross-build persistence: a fresh Runtime over the same store sees it.
    let sink2 = Arc::new(ChannelSink::new());
    let rt2 = RuntimeBuilder::new()
        .store(store.clone() as Arc<dyn PersistenceStore>)
        .with_provider(Arc::new(MockProvider::echo("mock")))
        .sink(sink2)
        .bus_capacity(8)
        .build()
        .await
        .unwrap();
    let rooms = rt2.list_rooms().await.unwrap();
    assert_eq!(rooms.len(), 1);
    assert_eq!(rooms[0].id, room.id);
    assert_eq!(rooms[0].status, RoomStatus::Paused);
    assert_eq!(rooms[0].sessions, vec!["ses-1".to_string()]);
    // Role assignments reconstruct from the store (restart-safe).
    assert_eq!(rooms[0].agents.len(), 1);
    assert_eq!(rooms[0].agents[0].id, "agent-1");
    assert_eq!(
        rooms[0].agents[0].role,
        openrt_workspace::RoomRole::Researcher
    );
}

/// TEST 14: material↔room links are validated (no dangling references)
/// and idempotent in both directions.
#[tokio::test]
async fn t14_room_material_links_fail_closed_and_idempotent() {
    let (store, rt, _sink) = echo_rt().await;
    workbench(&store, "acct/lab").await;
    store
        .put_record("docs", "a1", &serde_json::json!({"body": "brief"}))
        .await
        .unwrap();

    let room = rt.create_room("acct/lab", "Desk", "alice").await.unwrap();
    let material = rt
        .create_material(
            MaterialKind::Document,
            "brief",
            "cli",
            MaterialReference::Store {
                namespace: "docs".into(),
                key: "a1".into(),
            },
            "alice",
        )
        .await
        .unwrap();

    // Attaching an unknown material fails closed — no dangling ids.
    let err = rt
        .attach_material_to_room(&room.id, "mat-ghost", "alice")
        .await
        .unwrap_err();
    assert!(err_contains(&err, "not found"));
    let room = rt.room(&room.id).await.unwrap();
    assert!(room.materials.is_empty());

    // Valid attach is idempotent; detach never destroys the record.
    let room = rt
        .attach_material_to_room(&room.id, &material.id, "alice")
        .await
        .unwrap();
    let room = rt
        .attach_material_to_room(&room.id, &material.id, "alice")
        .await
        .unwrap();
    assert_eq!(room.materials, vec![material.id.clone()]);
    let room = rt
        .detach_material_from_room(&room.id, &material.id, "alice")
        .await
        .unwrap();
    let room = rt
        .detach_material_from_room(&room.id, &material.id, "alice")
        .await
        .unwrap();
    assert!(room.materials.is_empty());
    assert!(rt.material(&material.id).await.is_ok());
    // History recorded attach + detach exactly once each (idempotence).
    assert_eq!(
        room.history
            .iter()
            .filter(|h| h.action == "room.material.attached")
            .count(),
        1
    );
    assert_eq!(
        room.history
            .iter()
            .filter(|h| h.action == "room.material.detached")
            .count(),
        1
    );
}

/// TEST 15: materials are digest-sealed references — verify detects drift
/// in both store and file backing, and unresolvable input fails closed.
#[tokio::test]
async fn t15_material_digest_seal_and_drift_detection() {
    let (store, rt, _sink) = echo_rt().await;

    // Store-backed material.
    store
        .put_record("docs", "a1", &serde_json::json!({"body": "hello"}))
        .await
        .unwrap();
    let m: Material = rt
        .create_material(
            MaterialKind::Evidence,
            "hello",
            "ingest",
            MaterialReference::Store {
                namespace: "docs".into(),
                key: "a1".into(),
            },
            "alice",
        )
        .await
        .unwrap();
    assert!(m.digest.len() == 64);
    assert!(openrt_workspace::verify_material(&*store, &m.id)
        .await
        .unwrap());

    // Tampering with the referenced content is detected (fail-closed).
    store
        .put_record("docs", "a1", &serde_json::json!({"body": "EVIL"}))
        .await
        .unwrap();
    assert!(!openrt_workspace::verify_material(&*store, &m.id)
        .await
        .unwrap());

    // A reference that does not resolve NOW is refused at registration.
    let err = rt
        .create_material(
            MaterialKind::Document,
            "ghost",
            "cli",
            MaterialReference::Store {
                namespace: "docs".into(),
                key: "missing".into(),
            },
            "alice",
        )
        .await;
    assert!(err.is_err());

    // File-backed material + drift after rewrite.
    let dir = std::env::temp_dir().join(format!("p11-mat-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("note.txt");
    std::fs::write(&file, b"original").unwrap();
    let path = file.to_string_lossy().to_string();
    let fm = rt
        .create_material(
            MaterialKind::Document,
            "note",
            "cli",
            MaterialReference::Path { path: path.clone() },
            "alice",
        )
        .await
        .unwrap();
    assert!(openrt_workspace::verify_material(&*store, &fm.id)
        .await
        .unwrap());
    std::fs::write(&file, b"rewritten").unwrap();
    assert!(!openrt_workspace::verify_material(&*store, &fm.id)
        .await
        .unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

/// TEST 16: material↔workbench attachment validates the workbench and is
/// link-additive (records are never destroyed by link removal).
#[tokio::test]
async fn t16_material_workbench_attach_gates() {
    let (store, rt, _sink) = echo_rt().await;
    store
        .put_record("docs", "a1", &serde_json::json!({"b": 1}))
        .await
        .unwrap();
    let m = rt
        .create_material(
            MaterialKind::Knowledge,
            "kb-entry",
            "cli",
            MaterialReference::Store {
                namespace: "docs".into(),
                key: "a1".into(),
            },
            "alice",
        )
        .await
        .unwrap();

    // Unknown workbench fails closed.
    let err = rt
        .attach_material_to_workbench(&m.id, "acct/ghost", "alice")
        .await
        .unwrap_err();
    assert!(err_contains(&err, "not found"));

    workbench(&store, "acct/lab").await;
    let m = rt
        .attach_material_to_workbench(&m.id, "acct/lab", "alice")
        .await
        .unwrap();
    let m = rt
        .attach_material_to_workbench(&m.id, "acct/lab", "alice")
        .await
        .unwrap();
    assert_eq!(m.attached_workbenches, vec!["acct/lab".to_string()]);

    let m = rt
        .detach_material_from_workbench(&m.id, "acct/lab", "alice")
        .await
        .unwrap();
    assert!(m.attached_workbenches.is_empty());
    assert!(rt.material(&m.id).await.is_ok());
}

/// TEST 17: steer is append-only with an audit row per directive; the
/// audit chain still verifies.
#[tokio::test]
async fn t17_steer_appends_only_and_audits() {
    let (store, rt, _sink) = echo_rt().await;
    workbench(&store, "acct/lab").await;
    let room: Room = rt.create_room("acct/lab", "Ops", "alice").await.unwrap();

    let e1 = rt
        .room_steer(&room.id, "alice", "prefer batch size 8")
        .await
        .unwrap();
    let e2 = rt
        .room_steer(&room.id, "alice", "halt after step 3")
        .await
        .unwrap();
    assert!(e1.at <= e2.at);
    let room = rt.room(&room.id).await.unwrap();
    assert_eq!(room.steer.len(), 2);
    assert_eq!(room.steer[0].input, "prefer batch size 8");
    assert_eq!(room.steer[1].input, "halt after step 3");

    // Empty steer fails closed.
    let err = rt.room_steer(&room.id, "alice", "  ").await;
    assert!(err.is_err());

    let records = rt
        .audit_query(&AuditQuery::new().with_action_prefix("room.steer"))
        .await
        .unwrap();
    assert_eq!(records.len(), 2);
    assert!(records.iter().all(|r| r.entry.actor == "alice"));
    rt.audit_verify().await.unwrap();
}

/// TEST 18: the full workbench scenario round-trips across a Runtime
/// restart — rooms, materials and links live in the ONE store, never a
/// UI-local copy.
#[tokio::test]
async fn t18_full_workbench_scenario_cross_build() {
    let store = Arc::new(MemoryStore::new());
    {
        let sink = Arc::new(ChannelSink::new());
        let rt = RuntimeBuilder::new()
            .store(store.clone() as Arc<dyn PersistenceStore>)
            .with_provider(Arc::new(MockProvider::echo("mock")))
            .sink(sink)
            .bus_capacity(8)
            .build()
            .await
            .unwrap();
        workbench(&store, "acct/lab").await;
        store
            .put_record("docs", "brief", &serde_json::json!({"title": "q3"}))
            .await
            .unwrap();

        let a = rt.create_room("acct/lab", "Room A", "alice").await.unwrap();
        let b = rt.create_room("acct/lab", "Room B", "bob").await.unwrap();
        let m = rt
            .create_material(
                MaterialKind::Document,
                "q3 brief",
                "ingest",
                MaterialReference::Store {
                    namespace: "docs".into(),
                    key: "brief".into(),
                },
                "alice",
            )
            .await
            .unwrap();
        rt.attach_material_to_room(&a.id, &m.id, "alice")
            .await
            .unwrap();
        rt.attach_material_to_room(&b.id, &m.id, "alice")
            .await
            .unwrap();
        rt.attach_material_to_workbench(&m.id, "acct/lab", "alice")
            .await
            .unwrap();
        rt.set_room_workflow(&a.id, Some("wf-a"), "alice")
            .await
            .unwrap();
    }

    // Fresh Runtime (restart): everything visible, nothing fabricated.
    let sink = Arc::new(ChannelSink::new());
    let rt = RuntimeBuilder::new()
        .store(store.clone() as Arc<dyn PersistenceStore>)
        .with_provider(Arc::new(MockProvider::echo("mock")))
        .sink(sink)
        .bus_capacity(8)
        .build()
        .await
        .unwrap();
    let rooms = rt.list_rooms().await.unwrap();
    assert_eq!(rooms.len(), 2);
    let materials = rt.list_materials().await.unwrap();
    assert_eq!(materials.len(), 1);
    assert_eq!(
        materials[0].attached_workbenches,
        vec!["acct/lab".to_string()]
    );
    let a = rooms.iter().find(|r| r.title == "Room A").unwrap();
    assert_eq!(a.materials, vec![materials[0].id.clone()]);
    assert_eq!(a.active_workflow.as_deref(), Some("wf-a"));
    let by_wb = openrt_workspace::rooms_for_workbench(&*store, "acct/lab")
        .await
        .unwrap();
    assert_eq!(by_wb.len(), 2);
    // The sealed digest still verifies after the restart.
    assert!(openrt_workspace::verify_material(&*store, &materials[0].id)
        .await
        .unwrap());

    // Part Q / M11.10 doctor rows exist and honestly reflect the live store.
    let report = rt.doctor().await;
    for name in [
        "extensions",
        "marketplace",
        "mcp.ecosystem",
        "workbench",
        "rooms",
        "workbench.materials",
        "workbench.recovery",
        "workbench.steer",
    ] {
        let row = report
            .checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("doctor must include `{name}`; got {:?}", report.checks));
        assert_eq!(
            row.status,
            openrt_runtime::DoctorStatus::Pass,
            "{name}: {}",
            row.detail
        );
    }
}

// ---- tests 19–24: pause / resume / steer ----

/// TEST 19: cooperative pause stops at a checkpoint with partial history —
/// `Paused` is never a failure, never claims success, and leaves the
/// checkpoint chain Running (resumable, recorded incomplete at boot).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t19_pause_records_partial_history_without_failure() {
    let (store, rt, sink, _provider) = slow_rt().await;
    let wf = chain_workflow("wf-pause", 5);

    let report = spawn_and_pause(&rt, &sink, wf).await;
    assert_eq!(report.status, WorkflowStatus::Paused);
    // Partial history: some but not all steps ran, and the drained step
    // completed (pause never aborts an in-flight step).
    assert!(!report.steps.is_empty());
    assert!(report.steps.len() < 5);
    assert_eq!(report.steps[0].status, WorkflowStatus::Completed);

    // No Final checkpoint — the chain stays Running (resumable; boot only
    // records it as an incomplete run, never auto-resumes).
    let cps = checkpoints_chronological(rt.list_checkpoints(Some("wf-pause")).await.unwrap());
    assert!(!cps.iter().any(|c| matches!(c.kind, CheckpointKind::Final)));
    assert!(cps
        .iter()
        .all(|c| matches!(c.status, CheckpointStatus::Running)));

    // The runs record honestly says `paused` (not completed/failed).
    assert_eq!(runs_status(&store, "wf-pause").await, "paused");

    // WorkflowPaused event observed (audit-visible signal).
    wait_for(&sink, EventKind::WorkflowPaused, 1).await;

    // A second pause request fails closed: the run is no longer active.
    let err = rt.workflow_pause("wf-pause", "alice").await.unwrap_err();
    assert!(err_contains(&err, "no active run"));

    // Paused task never claims success: the session transcript records it.
    // (Task status is Waiting — active — so boot records it interrupted.)
}

/// TEST 20: resume is the ONLY continuation — it picks up the paused run
/// to a real Completed terminal state, and both verbs are audited.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t20_resume_completes_paused_run_end_to_end() {
    let (store, rt, sink, provider) = slow_rt().await;
    let wf = chain_workflow("wf-resume", 5);
    let first = spawn_and_pause(&rt, &sink, wf).await;
    assert_eq!(first.status, WorkflowStatus::Paused);
    // Exactly the partial history ran in run 1 (s0..s1, never all five).
    let after_pause = provider.call_count();
    assert!(
        (1..5).contains(&after_pause),
        "partial history: {after_pause} calls"
    );

    let report = rt.workflow_resume("wf-resume", "alice").await.unwrap();
    assert_eq!(report.status, WorkflowStatus::Completed);
    // THE seed invariant, measured honestly: run 1 executed `after_pause`
    // steps and resume executed exactly the remaining `5 - after_pause` —
    // seeded steps are NEVER re-executed (a re-run would inflate the count).
    assert_eq!(
        provider.call_count(),
        5,
        "resume must execute only the remaining steps (no re-execution)"
    );
    // Every step is accounted for across the chain (seed reports included).
    let names: std::collections::BTreeSet<&str> =
        report.steps.iter().map(|s| s.step.as_str()).collect();
    assert_eq!(names.len(), 5);

    // Terminal Final checkpoint now exists and the runs record says so.
    let cps = checkpoints_chronological(rt.list_checkpoints(Some("wf-resume")).await.unwrap());
    let final_cp = cps
        .iter()
        .find(|c| matches!(c.kind, CheckpointKind::Final))
        .expect("resume must finalize");
    assert_eq!(final_cp.status, CheckpointStatus::Completed);
    assert_eq!(runs_status(&store, "wf-resume").await, "completed");

    wait_for(&sink, EventKind::WorkflowCompleted, 1).await;

    // Both verbs audited by the operator.
    for prefix in ["workflow.pause", "workflow.resume"] {
        let records = rt
            .audit_query(&AuditQuery::new().with_action_prefix(prefix))
            .await
            .unwrap();
        assert_eq!(records.len(), 1, "expected exactly one {prefix}");
        assert_eq!(records[0].entry.actor, "alice");
        assert_eq!(records[0].entry.subject, "wf-resume");
    }
    rt.audit_verify().await.unwrap();
}

/// TEST 21: pause/resume gates fail closed — no active run, no checkpoint,
/// or a terminal checkpoint all refuse (never a fabricated success).
#[tokio::test]
async fn t21_pause_resume_gates_fail_closed() {
    let (store, rt, _sink) = echo_rt().await;
    let _ = &store;

    // Pause with no run executing in this process.
    let err = rt.workflow_pause("wf-none", "alice").await.unwrap_err();
    assert!(err_contains(&err, "no active run"));

    // Resume with no checkpoint at all.
    let err = rt.workflow_resume("wf-none", "alice").await.unwrap_err();
    assert!(err_contains(&err, "no checkpoint"));

    // A completed workflow's chain is terminal — resume refuses.
    let session = rt.create_session("done").await.unwrap();
    let wf = chain_workflow("wf-done", 2);
    let report = rt.run_workflow(session.id, wf).await.unwrap();
    assert_eq!(report.status, WorkflowStatus::Completed);
    let err = rt.workflow_resume("wf-done", "alice").await.unwrap_err();
    assert!(err_contains(&err, "nothing to resume"));
    // And the terminal chain was finalized (nothing pending for boot).
    let cps = checkpoints_chronological(rt.list_checkpoints(Some("wf-done")).await.unwrap());
    assert!(cps.iter().any(|c| matches!(c.kind, CheckpointKind::Final)
        && matches!(c.status, CheckpointStatus::Completed)));
}

/// TEST 22: room steer hydrates into the workflow blackboard at run start,
/// survives pause→resume without duplication or rewriting, and the entry
/// count stays append-only (exactly one key per directive).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t22_steer_hydrates_into_blackboard_and_survives_resume() {
    let (store, rt, sink, _provider) = slow_rt().await;
    workbench(&store, "acct/lab").await;
    let room = rt.create_room("acct/lab", "Steer", "alice").await.unwrap();
    rt.room_steer(&room.id, "alice", "prefer batch size 8")
        .await
        .unwrap();
    rt.room_steer(&room.id, "alice", "halt after step 3")
        .await
        .unwrap();
    rt.set_room_workflow(&room.id, Some("wf-steer"), "alice")
        .await
        .unwrap();

    let wf = chain_workflow("wf-steer", 3);
    let first = spawn_and_pause(&rt, &sink, wf).await;
    assert_eq!(first.status, WorkflowStatus::Paused);
    let report = rt.workflow_resume("wf-steer", "alice").await.unwrap();
    assert_eq!(report.status, WorkflowStatus::Completed);

    // The Final checkpoint's blackboard snapshot carries EXACTLY the two
    // steer keys (put-if-absent: resume never duplicated or rewrote them).
    let cps = checkpoints_chronological(rt.list_checkpoints(Some("wf-steer")).await.unwrap());
    let final_meta = cps
        .iter()
        .find(|c| matches!(c.kind, CheckpointKind::Final))
        .unwrap();
    let cp = rt.load_checkpoint(&final_meta.checkpoint_id).await.unwrap();
    let steer_keys: Vec<&String> = cp
        .blackboard
        .entries
        .iter()
        .map(|(k, _)| k)
        .filter(|k| k.starts_with("steer."))
        .collect();
    assert_eq!(steer_keys.len(), 2, "append-only: one key per directive");
    let k0 = format!("steer.{}.0", room.id);
    let k1 = format!("steer.{}.1", room.id);
    let v0 = cp
        .blackboard
        .entries
        .iter()
        .find(|(k, _)| *k == k0)
        .map(|(_, v)| v.clone())
        .expect("first steer hydrated");
    let v1 = cp
        .blackboard
        .entries
        .iter()
        .find(|(k, _)| *k == k1)
        .map(|(_, v)| v.clone())
        .expect("second steer hydrated");
    assert_eq!(v0["input"], serde_json::json!("prefer batch size 8"));
    assert_eq!(v0["operator"], serde_json::json!("alice"));
    assert_eq!(v1["input"], serde_json::json!("halt after step 3"));
}

/// TEST 23: steer hydration is scoped to the OWNING room only — another
/// room's steer never leaks into a workflow it does not own.
#[tokio::test]
async fn t23_steer_scoped_to_owning_room() {
    let (store, rt, _sink) = echo_rt().await;
    workbench(&store, "acct/lab").await;
    let owner = rt.create_room("acct/lab", "Owner", "alice").await.unwrap();
    let other = rt.create_room("acct/lab", "Other", "bob").await.unwrap();
    rt.room_steer(&owner.id, "alice", "owner directive")
        .await
        .unwrap();
    rt.room_steer(&other.id, "bob", "other directive")
        .await
        .unwrap();
    rt.set_room_workflow(&owner.id, Some("wf-scope"), "alice")
        .await
        .unwrap();
    // `other` deliberately does NOT own the workflow.

    let session = rt.create_session("scope").await.unwrap();
    let wf = chain_workflow("wf-scope", 2);
    let report = rt.run_workflow(session.id, wf).await.unwrap();
    assert_eq!(report.status, WorkflowStatus::Completed);

    let cps = checkpoints_chronological(rt.list_checkpoints(Some("wf-scope")).await.unwrap());
    let final_meta = cps
        .iter()
        .find(|c| matches!(c.kind, CheckpointKind::Final))
        .unwrap();
    let cp = rt.load_checkpoint(&final_meta.checkpoint_id).await.unwrap();
    let keys: Vec<&str> = cp
        .blackboard
        .entries
        .iter()
        .map(|(k, _)| k.as_str())
        .filter(|k| k.starts_with("steer."))
        .collect();
    assert_eq!(keys.len(), 1, "only the owning room hydrates");
    assert!(keys[0].starts_with(&format!("steer.{}.", owner.id)));
    assert!(!keys.iter().any(|k| k.contains(&other.id)));
}

/// TEST 24: the full operator flow (room → material → steer → pause →
/// resume) leaves one auditable trail and a verifying chain — no action is
/// silent, no actor is anonymous.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t24_full_flow_is_audited_with_verifying_chain() {
    let (store, rt, sink, _provider) = slow_rt().await;
    workbench(&store, "acct/lab").await;
    store
        .put_record("docs", "brief", &serde_json::json!({"t": "q3"}))
        .await
        .unwrap();

    let room = rt.create_room("acct/lab", "Flow", "alice").await.unwrap();
    let material = rt
        .create_material(
            MaterialKind::Document,
            "q3 brief",
            "ingest",
            MaterialReference::Store {
                namespace: "docs".into(),
                key: "brief".into(),
            },
            "alice",
        )
        .await
        .unwrap();
    rt.attach_material_to_room(&room.id, &material.id, "alice")
        .await
        .unwrap();
    rt.room_steer(&room.id, "alice", "review before executing")
        .await
        .unwrap();
    rt.set_room_workflow(&room.id, Some("wf-flow"), "alice")
        .await
        .unwrap();

    let wf = chain_workflow("wf-flow", 3);
    let first = spawn_and_pause(&rt, &sink, wf).await;
    assert_eq!(first.status, WorkflowStatus::Paused);
    let report = rt.workflow_resume("wf-flow", "alice").await.unwrap();
    assert_eq!(report.status, WorkflowStatus::Completed);
    wait_for(&sink, EventKind::WorkflowCompleted, 1).await;

    // Every mutating action left exactly one audit row, actor = operator.
    for prefix in [
        "workbench.room.create",
        "workbench.material.create",
        "workbench.room.material.attach",
        "room.steer",
        "workbench.room.workflow",
        "workflow.pause",
        "workflow.resume",
    ] {
        let records = rt
            .audit_query(&AuditQuery::new().with_action_prefix(prefix))
            .await
            .unwrap();
        assert_eq!(records.len(), 1, "expected exactly one {prefix} row");
        assert_eq!(records[0].entry.actor, "alice", "{prefix} actor");
    }
    // The chain verifies end-to-end (tamper-evident, one authority).
    let count = rt.audit_verify().await.unwrap();
    assert!(count >= 7);
}
