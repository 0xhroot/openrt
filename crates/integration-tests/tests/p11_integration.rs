//! Phase 11 — cross-track integration tests 37–48.
//!
//! Track U closes the Phase 11 surface with chains that cross subsystem
//! boundaries through the production APIs over ONE durable store:
//!
//! * 37 — all mandated M11.10 doctor rows exist and never `Fail`;
//! * 38 — workbench/room/material reconstruction from authoritative records
//!   across a runtime restart (no UI-local state, no copies);
//! * 39 — room roles map onto existing `SubAgentKind` identities and audit
//!   `room.agent.link` (unknown roles fail closed before any write);
//! * 40 — MCP trust NEVER bypasses the permission gate (trusted ≠ allowed);
//! * 41 — marketplace doctor row is honest under Sovereign (local GREEN,
//!   remote UNAVAILABLE/BLOCKED — never a fabricated listing);
//! * 42 — kill scope `computer` blocks `computer.use` but never discovery;
//! * 43 — WebUI palette Phase 11 command set dispatches through the ONE gate
//!   (denial renders as denial, never as UI-local execution);
//! * 44 — TUI computer panel is a view over the shared runtime snapshot;
//! * 45 — one audit chain spans every Phase 11 namespace and verifies;
//! * 46 — a paused run survives a restart and is NEVER auto-resumed;
//! * 47 — every action carries the structured-tool advisory (GUI is the
//!   fallback, typed/clipboard payloads stay length-redacted);
//! * 48 — the cross-surface mega scenario: palette + runtime + doctor +
//!   audit over one store, one gate.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use openrt_audit::AuditQuery;
use openrt_config::{Config, McpConfig, McpServerConfig, McpTrust, RuleConfig, RuleEffect};
use openrt_core::EventKind;
use openrt_models::mock::{MockMode, MockProvider};
use openrt_orch::{SubAgentKind, Workflow, WorkflowStatus, WorkflowStep, RUNS_NS};
use openrt_persistence::{MemoryStore, PersistenceStore};
use openrt_runtime::{DoctorStatus, Runtime, RuntimeBuilder};
use openrt_telemetry::ChannelSink;
use openrt_webui::{CommandPalette, DispatchRequest, PaletteOutcome};
use openrt_workspace::{MaterialKind, MaterialReference, RoomStatus};

// ---- helpers ----

fn tmp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "openrt-p11i-{prefix}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn allow(caps: &[&str]) -> Vec<RuleConfig> {
    caps.iter()
        .flat_map(|c| {
            [
                RuleConfig {
                    effect: RuleEffect::Allow,
                    capability: (*c).to_string(),
                    resource: None,
                },
                RuleConfig {
                    effect: RuleEffect::Allow,
                    capability: format!("{c}.all"),
                    resource: None,
                },
            ]
        })
        .collect()
}

/// Deterministic runtime over the sim backends: keeps the default allow
/// rules (echo, discovery, …) and ADDS `caps`.
async fn rt_on(store: Arc<MemoryStore>, caps: &[&str]) -> Arc<Runtime> {
    let mut config = Config::default();
    config.runtime.screen_backend = Some("sim".into());
    config.runtime.computer_backend = Some("sim".into());
    // Rules are first-match-wins: prepend so the explicit allows match
    // BEFORE the built-in developer profile's Ask rules.
    let mut rules = allow(caps);
    rules.append(&mut config.permissions.rules);
    config.permissions.rules = rules;
    RuntimeBuilder::new()
        .config(config)
        .store(store)
        .with_provider(Arc::new(MockProvider::echo("mock")))
        .build()
        .await
        .unwrap()
}

async fn create_workbench(store: &MemoryStore, code: &str) {
    openrt_workspace::create_workspace(store, code, "Lab", "", "2026-09-27T00:00:00Z")
        .await
        .unwrap();
}

async fn audit_count(rt: &Runtime, prefix: &str) -> usize {
    rt.audit_query(&AuditQuery::new().with_action_prefix(prefix))
        .await
        .unwrap()
        .len()
}

async fn runs_status(store: &MemoryStore, workflow_id: &str) -> String {
    let v = store
        .get_record(RUNS_NS, workflow_id)
        .await
        .unwrap()
        .expect("runs record must exist");
    v["status"].as_str().unwrap().to_string()
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

/// Spawn `wf`, wait until the first step durably completed, then pause.
async fn spawn_and_pause(
    rt: &Arc<Runtime>,
    sink: &ChannelSink,
    wf: Workflow,
) -> openrt_orch::WorkflowRunReport {
    let session = rt.create_session("pause").await.unwrap();
    let rt2 = rt.clone();
    let sid = session.id;
    let workflow_id = wf.id.clone();
    let handle = tokio::spawn(async move { rt2.run_workflow(sid, wf).await.unwrap() });
    wait_for(sink, EventKind::WorkflowStepCompleted, 1).await;
    rt.workflow_pause(&workflow_id, "alice").await.unwrap();
    handle.await.unwrap()
}

fn fake_server_path() -> String {
    format!(
        "{}/../openrt-mcp/tests/fixtures/fake_mcp.py",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// MCP server fixture over the real subprocess server (stdio only).
async fn mcp_rt(ws: &Path, caps: &[&str], trust: McpTrust) -> Arc<Runtime> {
    let mut config = Config::default();
    config.runtime.workspace_dir = Some(ws.to_path_buf());
    config.runtime.screen_backend = Some("sim".into());
    config.runtime.computer_backend = Some("sim".into());
    config.permissions.rules.extend(allow(caps));
    config.fabric.mcp = McpConfig {
        servers: vec![McpServerConfig {
            name: "fake".into(),
            command: "python3".into(),
            args: vec![fake_server_path()],
            env: BTreeMap::new(),
            trust,
            declared_capabilities: vec![],
        }],
    };
    RuntimeBuilder::new()
        .config(config)
        .store(Arc::new(MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::echo("mock")))
        .build()
        .await
        .unwrap()
}

// ---- tests 37–42 ----

/// TEST 37: every M11.10 doctor row exists on one report and none of them
/// is ever a `Fail` (absence/honesty surface as PASS/WARN/UNAVAILABLE/
/// BLOCKED, never a fabricated green and never a hard failure).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t37_doctor_phase11_rows_complete_and_never_fail() {
    let rt = rt_on(Arc::new(MemoryStore::new()), &[]).await;
    let report = rt.doctor().await;
    for name in [
        "extensions",
        "marketplace",
        "mcp.ecosystem",
        "workbench",
        "rooms",
        "computer.vision",
        "computer.grounding",
        "computer.twin",
        "computer.verify",
        "workbench.recovery",
    ] {
        let check = report
            .checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("missing mandated doctor row `{name}`"));
        assert_ne!(
            check.status,
            DoctorStatus::Fail,
            "doctor row `{name}` must never Fail: {}",
            check.detail
        );
        assert!(!check.detail.is_empty(), "doctor row `{name}` needs detail");
    }
}

/// TEST 38: reconstruction across a runtime restart — a fresh Runtime over
/// the SAME store sees the identical workbench/room/material graph
/// (authoritative records, references + digests, never copies).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t38_workbench_reconstruction_across_restart() {
    let store = Arc::new(MemoryStore::new());
    let rt1 = rt_on(store.clone(), &[]).await;
    create_workbench(&store, "acct/restart").await;
    store
        .put_record("docs", "brief", &serde_json::json!({"t": "q3"}))
        .await
        .unwrap();
    let room = rt1
        .create_room("acct/restart", "Flow", "alice")
        .await
        .unwrap();
    let material = rt1
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
    rt1.attach_material_to_room(&room.id, &material.id, "alice")
        .await
        .unwrap();
    rt1.attach_material_to_workbench(&material.id, "acct/restart", "alice")
        .await
        .unwrap();
    drop(rt1);

    // Restart: brand-new Runtime, same store — no UI-local authoritative state.
    let rt2 = rt_on(store.clone(), &[]).await;
    let rooms = rt2.list_rooms().await.unwrap();
    assert_eq!(rooms.len(), 1);
    assert_eq!(rooms[0].id, room.id);
    assert_eq!(rooms[0].status, RoomStatus::Active);
    assert_eq!(rooms[0].materials, vec![material.id.clone()]);

    let materials = rt2.list_materials().await.unwrap();
    assert_eq!(materials.len(), 1, "references never become copies");
    assert_eq!(
        materials[0].attached_workbenches,
        vec!["acct/restart".to_string()]
    );
    // The digest still verifies after restart (content was never copied).
    let intact = openrt_workspace::verify_material(&*store, &material.id)
        .await
        .expect("digest must still verify across restart");
    assert!(intact, "digest drifted after restart");

    let report = rt2.doctor().await;
    let wb = report
        .checks
        .iter()
        .find(|c| c.name == "workbench")
        .expect("workbench row");
    assert_eq!(wb.status, DoctorStatus::Pass, "{}", wb.detail);
    assert!(wb.detail.contains("1 room(s)"), "{}", wb.detail);
}

/// TEST 39: room roles are configuration mapped onto EXISTING sub-agent
/// identities — audited with the resolved identity; an unknown role fails
/// closed before any record is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t39_room_roles_map_to_existing_identities_and_audit() {
    let store = Arc::new(MemoryStore::new());
    let rt = rt_on(store.clone(), &[]).await;
    create_workbench(&store, "acct/roles").await;
    let room = rt
        .create_room("acct/roles", "Panel", "alice")
        .await
        .unwrap();

    let room = rt
        .link_room_agent(&room.id, "agent-1", "researcher", "alice")
        .await
        .unwrap();
    assert_eq!(room.agents.len(), 1);
    assert_eq!(room.agents[0].id, "agent-1");
    assert_eq!(room.agents[0].role.as_str(), "researcher");

    // The audit row carries the resolved EXISTING identity, not a new
    // authority.
    let records = rt
        .audit_query(&AuditQuery::new().with_action_prefix("room.agent.link"))
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    let ext = &records[0].entry.ext;
    assert_eq!(ext["agent"], "agent-1");
    assert_eq!(ext["role"], "researcher");
    let sub = ext["sub_agent"].as_str().unwrap_or("");
    assert!(!sub.is_empty(), "sub_agent identity must be resolved");
    assert_eq!(
        openrt_orch::room_role_sub_agent("researcher").map(|k| k.as_str().to_string()),
        Some(sub.to_string()),
        "audit identity must equal the one mapping"
    );
    assert_eq!(records[0].entry.actor, "alice");

    // Unknown role: fail closed BEFORE any write, no audit row.
    let err = rt
        .link_room_agent(&room.id, "agent-2", "wizard", "alice")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unknown room role"), "{err}");
    assert_eq!(rt.room(&room.id).await.unwrap().agents.len(), 1);
    assert_eq!(audit_count(&rt, "room.agent.link").await, 1);

    // Every configured role resolves onto the frozen SubAgentKind set.
    for role in ["researcher", "analyst", "coder", "reviewer", "verifier"] {
        assert!(
            openrt_orch::room_role_sub_agent(role).is_some(),
            "role `{role}` must map onto an existing identity"
        );
    }
    let _ = SubAgentKind::Planner; // identity set is frozen, not extended
}

/// TEST 40: MCP trust is advisory metadata, NEVER authority — a `trusted`
/// server without the required capabilities is still denied by the gate,
/// and with them it executes through the same registry tool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t40_mcp_trust_never_bypasses_the_permission_gate() {
    let ws = tmp_dir("trust");

    // Trusted in config, tool id allowed, capabilities missing → denied.
    let rt_deny = mcp_rt(&ws, &["mcp.fake.echo"], McpTrust::Trusted).await;
    let err = rt_deny
        .executor
        .execute(
            "mcp.fake.echo",
            serde_json::json!({ "message": "x" }),
            &openrt_tools::ToolExecutionContext {
                session_id: None,
                task_id: None,
                channel: None,
            },
        )
        .await
        .expect_err("trusted without caps must still be denied");
    assert!(
        matches!(err, openrt_tools::ToolError::PermissionDenied { .. }),
        "expected PermissionDenied, got {err:?}"
    );
    drop(rt_deny);

    // Same trust + BOTH capabilities → the identical call goes through.
    let rt_allow = mcp_rt(
        &ws,
        &[
            "mcp.fake.echo",
            "mcp.fake.add",
            "mcp.invoke.all",
            "mcp.fake.invoke",
        ],
        McpTrust::Trusted,
    )
    .await;
    let out = rt_allow
        .executor
        .execute(
            "mcp.fake.echo",
            serde_json::json!({ "message": "via-gate" }),
            &openrt_tools::ToolExecutionContext {
                session_id: None,
                task_id: None,
                channel: None,
            },
        )
        .await
        .expect("trusted + fully granted executes");
    match out {
        openrt_tools::ToolOutput::Json(v) => {
            assert_eq!(v["text"], serde_json::json!("echo: via-gate"), "{v}");
        }
        other => panic!("expected JSON echo, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&ws);
}

/// TEST 41: marketplace honesty under Sovereign — local catalog served from
/// the store, remote fetch reported UNAVAILABLE/BLOCKED in the SAME row.
/// Never a fabricated remote listing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t41_marketplace_doctor_is_honest_under_sovereign() {
    let rt = rt_on(Arc::new(MemoryStore::new()), &[]).await;
    let record = rt.modes.current().await.unwrap();
    assert_eq!(
        record.mode,
        openrt_modes::ExecutionMode::Sovereign,
        "test must run under Sovereign"
    );
    // Local catalog reads work (contents may be empty — that is honest too).
    let local = rt.skill_marketplace_catalog().await.unwrap();
    assert!(local.len() < 100, "local catalog is store-backed");

    let report = rt.doctor().await;
    let row = report
        .checks
        .iter()
        .find(|c| c.name == "marketplace")
        .expect("marketplace doctor row");
    assert_eq!(row.status, DoctorStatus::Pass, "{}", row.detail);
    assert!(row.detail.contains("local catalog"), "{}", row.detail);
    assert!(
        row.detail.contains("UNAVAILABLE") || row.detail.contains("BLOCKED"),
        "remote fetch must be reported honestly: {}",
        row.detail
    );
}

/// TEST 42: kill scope `computer` — tripping the switch blocks the
/// computer-scoped `computer.use` while discovery/echo stay available
/// (the scope never widens into a global outage).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t42_kill_scope_computer_blocks_actions_not_discovery() {
    let rt = rt_on(
        Arc::new(MemoryStore::new()),
        &["computer.use", "tools.list", "tools.discovery.all"],
    )
    .await;

    // Before the trip: the allowed action executes.
    let out = rt
        .executor
        .execute(
            "computer.use",
            serde_json::json!({"action": "click", "x": 9, "y": 9}),
            &openrt_tools::ToolExecutionContext {
                session_id: None,
                task_id: None,
                channel: None,
            },
        )
        .await
        .expect("allow rule permits the pre-trip call");
    assert!(matches!(out, openrt_tools::ToolOutput::Json(_)));

    assert!(rt.kill_switch.trip("t42 kill scope test"));
    let err = rt
        .executor
        .execute(
            "computer.use",
            serde_json::json!({"action": "click", "x": 9, "y": 9}),
            &openrt_tools::ToolExecutionContext {
                session_id: None,
                task_id: None,
                channel: None,
            },
        )
        .await
        .expect_err("computer scope must be blocked after the trip");
    match err {
        openrt_tools::ToolError::KillSwitchEngaged { tool, .. } => {
            assert_eq!(tool, "computer.use");
        }
        other => panic!("expected KillSwitchEngaged, got {other:?}"),
    }

    // Discovery stays available — the scope is `computer`, not `*`.
    let list = rt
        .executor
        .execute(
            "tools.list",
            serde_json::json!({}),
            &openrt_tools::ToolExecutionContext {
                session_id: None,
                task_id: None,
                channel: None,
            },
        )
        .await;
    assert!(
        list.is_ok(),
        "tools.list must survive the computer kill: {list:?}"
    );
    let echo = rt
        .executor
        .execute(
            "echo",
            serde_json::json!({"text": "alive"}),
            &openrt_tools::ToolExecutionContext {
                session_id: None,
                task_id: None,
                channel: None,
            },
        )
        .await;
    assert!(echo.is_ok(), "echo must survive: {echo:?}");
}

// ---- tests 43–44: cross-surface views over the one runtime ----

/// TEST 43: the WebUI palette exposes the full Phase 11 command set and
/// dispatches `computer.*` through the ONE gate — permission denial renders
/// as `PaletteOutcome::Denied`, never as UI-local execution.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t43_palette_phase11_commands_dispatch_through_one_gate() {
    let rt_deny = rt_on(Arc::new(MemoryStore::new()), &[]).await;
    let rt_allow = rt_on(Arc::new(MemoryStore::new()), &["computer.use"]).await;
    let op = rt_allow.asset_operators()[0].clone();
    let improve_deny = rt_deny.load_improve().await.unwrap();
    let improve_allow = rt_allow.load_improve().await.unwrap();
    let handle = tokio::runtime::Handle::current();

    let (ids, env, denied, allowed) = tokio::task::spawn_blocking(move || {
        let mut p_deny = CommandPalette::with_runtime(improve_deny, rt_deny, handle.clone());
        let mut p_allow = CommandPalette::with_runtime(improve_allow, rt_allow, handle);

        let ids: Vec<String> = p_allow.commands().into_iter().map(|c| c.id).collect();

        // Read-only computer.env needs no operator token.
        let env = p_allow
            .dispatch(&DispatchRequest::new("computer.env"))
            .expect("computer.env dispatches");

        // No-allow runtime + valid token → gate denies → Denied outcome.
        let token_deny = p_deny.issue_token(&op);
        let denied = p_deny
            .dispatch(
                &DispatchRequest::new("computer.use")
                    .with_arg("action", "click")
                    .with_arg("x", "7")
                    .with_arg("y", "8")
                    .with_auth_token(&token_deny),
            )
            .expect("dispatch itself succeeds");

        // Allow runtime + valid token → executes through the gate.
        let token_allow = p_allow.issue_token(&op);
        let allowed = p_allow
            .dispatch(
                &DispatchRequest::new("computer.use")
                    .with_arg("action", "click")
                    .with_arg("x", "7")
                    .with_arg("y", "8")
                    .with_auth_token(&token_allow),
            )
            .expect("dispatch itself succeeds");

        (ids, env, denied, allowed)
    })
    .await
    .unwrap();

    // Full Phase 11 command set (M11.6/M11.7).
    for id in [
        "workbench.info",
        "room.list",
        "room.create",
        "room.show",
        "room.status",
        "room.steer",
        "room.agent",
        "material.list",
        "material.verify",
        "extension.list",
        "extension.inspect",
        "mcp.list",
        "mcp.trust",
        "mcp.enable",
        "mcp.disable",
        "computer.env",
        "computer.observe",
        "computer.use",
    ] {
        assert!(
            ids.iter().any(|i| i == id),
            "palette missing `{id}`: {ids:?}"
        );
    }

    match env {
        PaletteOutcome::Completed { detail, .. } => {
            assert!(detail.contains("capabilities"), "{detail}");
            assert!(detail.contains("\"sim\""), "{detail}");
        }
        other => panic!("computer.env must complete: {other:?}"),
    }
    match denied {
        PaletteOutcome::Denied { reason, .. } => {
            // No allow rule + no resolver: the profile's Ask fails closed
            // (denied, never executed) — the gate is still the authority.
            assert!(
                reason.contains("policy") || reason.contains("no resolver configured"),
                "gate denial reason: {reason}"
            );
        }
        other => panic!("computer.use without allow must render Denied: {other:?}"),
    }
    match allowed {
        PaletteOutcome::Completed { detail, .. } => {
            assert!(detail.contains("\"ok\":true"), "{detail}");
            assert!(detail.contains("\"verified\":true"), "{detail}");
        }
        other => panic!("computer.use with allow must complete: {other:?}"),
    }
}

/// TEST 44: the TUI computer panel is a VIEW over the shared runtime
/// snapshot — identical rows, no UI-local authoritative state, and the
/// Ops tab ring includes the computer sub-view.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t44_tui_computer_panel_reads_the_shared_runtime_snapshot() {
    use openrt_tui::{App, OpsTab};

    let rt = rt_on(Arc::new(MemoryStore::new()), &[]).await;
    let snapshot = rt.computer_env_snapshot();
    assert_eq!(snapshot["screen_backend"], serde_json::json!("sim"));

    let mut app = App::new(rt.clone()).await.expect("tui app builds");
    assert!(
        !app.ops_computer.is_empty(),
        "capability matrix rows must be loaded"
    );
    let caps = snapshot["capabilities"]
        .as_array()
        .expect("capabilities array");
    assert_eq!(app.ops_computer.len(), caps.len(), "view row count");
    for (row, cap) in app.ops_computer.iter().zip(caps.iter()) {
        assert_eq!(row.capability, cap["capability"].as_str().unwrap());
        assert_eq!(row.status, cap["status"].as_str().unwrap());
        assert_eq!(row.detail, cap["reason"].as_str().unwrap_or(""));
    }

    // The computer sub-view is part of the Ops ring (key 7 / index 6).
    app.set_ops_tab(OpsTab::Computer);
    assert_eq!(app.ops_tab, OpsTab::Computer);
    assert_eq!(app.ops_tab.title(), "Ops · Computer");
    assert_eq!(OpsTab::Computer.index(), 6);
    assert_eq!(OpsTab::from_index(6), OpsTab::Computer);
    // Phase 12 appended `Intel` at index 7 — the ring still wraps after
    // the last tab (now index 8 → back to Timeline).
    assert_eq!(OpsTab::from_index(7), OpsTab::Intel);
    assert_eq!(OpsTab::from_index(8), OpsTab::Timeline, "ring wraps");

    // Refresh re-reads the same runtime snapshot (still identical).
    app.refresh_ops().await.unwrap();
    assert_eq!(app.ops_computer.len(), caps.len());
}

// ---- tests 45–48: chains, recovery honesty, advisory, mega scenario ----

/// TEST 45: one audit chain spans every Phase 11 namespace (MCP registry,
/// workbench rooms/materials, steer, the computer gate) and the whole
/// chain verifies end-to-end over the one tamper-evident log.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t45_audit_chain_spans_every_phase11_namespace() {
    let store = Arc::new(MemoryStore::new());
    let rt = rt_on(store.clone(), &["computer.use"]).await;
    create_workbench(&store, "acct/audit").await;
    store
        .put_record("docs", "brief", &serde_json::json!({"t": "q3"}))
        .await
        .unwrap();

    rt.mcp_register(
        "chainmcp",
        "python3",
        vec!["-c".into(), "pass".into()],
        vec![],
        vec![],
        "alice",
    )
    .await
    .unwrap();
    let room = rt
        .create_room("acct/audit", "Chain", "alice")
        .await
        .unwrap();
    rt.link_room_agent(&room.id, "agent-1", "analyst", "alice")
        .await
        .unwrap();
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
    rt.room_steer(&room.id, "alice", "cross-check sources")
        .await
        .unwrap();
    rt.executor
        .execute(
            "computer.use",
            serde_json::json!({"action": "click", "x": 4, "y": 4}),
            &openrt_tools::ToolExecutionContext {
                session_id: None,
                task_id: None,
                channel: Some("cli".into()),
            },
        )
        .await
        .expect("gated computer.use executes");

    for prefix in [
        "mcp.register",
        "workbench.room.create",
        "room.agent.link",
        "workbench.material.create",
        "workbench.room.material.attach",
        "room.steer",
        "permission.decision",
    ] {
        let n = audit_count(&rt, prefix).await;
        assert!(n >= 1, "expected at least one `{prefix}` row, saw {n}");
    }
    // The computer decision is recorded with the tool as subject.
    let perms = rt
        .audit_query(&AuditQuery::new().with_action_prefix("permission.decision"))
        .await
        .unwrap();
    assert!(
        perms
            .iter()
            .any(|r| r.entry.subject == "computer.use" && r.entry.result == "allowed"),
        "computer.use allow must be audited"
    );
    let count = rt.audit_verify().await.unwrap();
    assert!(count >= 7, "chain verifies with every row: {count}");
}

/// TEST 46: cooperative pause is process-local, the paused state is
/// durable, and a restarted runtime NEVER auto-resumes it — resume stays
/// the only continuation path (M11.5 non-negotiable).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t46_paused_run_survives_restart_and_is_never_auto_resumed() {
    let store = Arc::new(MemoryStore::new());
    let sink = Arc::new(ChannelSink::new());
    let provider = Arc::new(
        MockProvider::new("mock", MockMode::Slow).with_latency(Duration::from_millis(300)),
    );
    let mut config = Config::default();
    config.runtime.screen_backend = Some("sim".into());
    config.runtime.computer_backend = Some("sim".into());
    let rt1 = RuntimeBuilder::new()
        .config(config.clone())
        .store(store.clone())
        .with_provider(provider)
        .sink(sink.clone())
        .bus_capacity(64)
        .build()
        .await
        .unwrap();

    let report = spawn_and_pause(&rt1, &sink, chain_workflow("wf-restart", 3)).await;
    assert_eq!(report.status, WorkflowStatus::Paused);
    assert_eq!(runs_status(&store, "wf-restart").await, "paused");
    drop(rt1);

    // Restart over the SAME store.
    let rt2 = rt_on(store.clone(), &[]).await;
    // Boot recovery must have only RECORDED the run — never resumed it.
    assert_eq!(
        runs_status(&store, "wf-restart").await,
        "paused",
        "boot recovery must never auto-resume a paused run"
    );
    let doctor = rt2.doctor().await;
    let row = doctor
        .checks
        .iter()
        .find(|c| c.name == "workbench.recovery")
        .expect("workbench.recovery row");
    assert_ne!(row.status, DoctorStatus::Fail, "{}", row.detail);
    assert!(
        row.detail.contains("never auto-resumes"),
        "recovery posture must state the rule: {}",
        row.detail
    );

    // Explicit resume is still the only continuation path — and it works
    // from the persisted checkpoints.
    let resumed = rt2.workflow_resume("wf-restart", "alice").await.unwrap();
    assert_eq!(resumed.status, WorkflowStatus::Completed);
    assert_eq!(runs_status(&store, "wf-restart").await, "completed");
}

/// TEST 47: every action carries the structured-tool advisory (the GUI is
/// the fallback, never the first choice), and typed/clipboard payloads are
/// reported by LENGTH only — the advisory never echoes them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t47_advisory_structured_alternative_for_every_action() {
    let screen: Arc<dyn openrt_computer::ScreenBackend> =
        Arc::new(openrt_computer::SimScreen::new(1280, 720, vec![]));
    let control = Arc::new(openrt_platform::ComputerControl::with_profile(
        openrt_platform::EnvironmentProfile::host(),
        Some(openrt_platform::BackendKind::Sim),
    ));
    let e = openrt_computer::ComputerUse::new(screen, control);

    let cases: Vec<(openrt_computer::ComputerAction, Option<&str>)> = vec![
        (
            openrt_computer::ComputerAction::Focus {
                target: "sim-1".into(),
            },
            Some("computer.window"),
        ),
        (
            openrt_computer::ComputerAction::Launch {
                app: "xterm".into(),
            },
            Some("computer.app"),
        ),
        (
            openrt_computer::ComputerAction::Click {
                target: None,
                x: Some(5),
                y: Some(6),
                button: 1,
            },
            Some("input.mouse"),
        ),
        (
            openrt_computer::ComputerAction::Type {
                target: None,
                text: "hunter2-super-secret".into(),
            },
            Some("computer.input"),
        ),
        (
            openrt_computer::ComputerAction::Hotkey {
                key: "ctrl+c".into(),
            },
            Some("computer.input"),
        ),
        (
            openrt_computer::ComputerAction::Scroll {
                target: None,
                x: Some(5),
                y: Some(6),
                delta: 100,
            },
            None,
        ),
        (
            openrt_computer::ComputerAction::Drag {
                from_x: 1,
                from_y: 1,
                to_x: 40,
                to_y: 40,
            },
            Some("input.mouse"),
        ),
        (
            openrt_computer::ComputerAction::Workspace { name: "2".into() },
            None,
        ),
        (
            openrt_computer::ComputerAction::Clipboard {
                set: Some("another-secret".into()),
            },
            Some("computer.clipboard"),
        ),
        (
            openrt_computer::ComputerAction::Close {
                target: "sim-1".into(),
            },
            Some("computer.window"),
        ),
        (
            openrt_computer::ComputerAction::Navigate {
                url: "https://example.com".into(),
            },
            Some("computer.app"),
        ),
    ];

    for (action, expected) in cases {
        let plan = e.plan(&action).await.expect("plan is advisory-only");
        assert_eq!(
            plan.structured_alternative.as_deref(),
            expected,
            "action `{}` structured alternative",
            plan.action
        );
        assert!(
            !plan.predicted_effect.is_empty(),
            "{} needs a predicted effect",
            plan.action
        );
        // Secret-shaped payloads never enter the advisory.
        assert!(
            !plan.predicted_effect.contains("hunter2-super-secret")
                && !plan.predicted_effect.contains("another-secret")
                && !plan
                    .predicted_state
                    .to_string()
                    .contains("hunter2-super-secret")
                && !plan.predicted_state.to_string().contains("another-secret"),
            "advisory leaked a payload: {} / {}",
            plan.predicted_effect,
            plan.predicted_state
        );
    }
}

/// TEST 48: the cross-surface mega scenario — palette creates the room,
/// the palette runs `computer.use` through the gate, runtime attaches
/// material, doctor reports the model, and one audit chain verifies at the
/// end. One store, one runtime, one gate; surfaces are views only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t48_cross_surface_mega_scenario_one_store_one_gate() {
    let store = Arc::new(MemoryStore::new());
    let rt = rt_on(store.clone(), &["computer.use"]).await;
    create_workbench(&store, "acct/mega").await;
    store
        .put_record("docs", "brief", &serde_json::json!({"t": "q3"}))
        .await
        .unwrap();
    let op = rt.asset_operators()[0].clone();
    let improve = rt.load_improve().await.unwrap();
    let handle = tokio::runtime::Handle::current();
    let rt_palette = rt.clone();

    let (room_id, env_detail, use_detail, info_detail) = tokio::task::spawn_blocking(move || {
        let mut palette = CommandPalette::with_runtime(improve, rt_palette, handle);
        let token = palette.issue_token(&op);

        let env = palette
            .dispatch(&DispatchRequest::new("computer.env"))
            .expect("computer.env");
        let PaletteOutcome::Completed {
            detail: env_detail, ..
        } = env
        else {
            panic!("computer.env must complete: {env:?}");
        };

        let created = palette
            .dispatch(
                &DispatchRequest::new("room.create")
                    .with_arg("workbench", "acct/mega")
                    .with_arg("title", "Mega")
                    .with_auth_token(&token),
            )
            .expect("room.create");
        let PaletteOutcome::Completed { detail, .. } = created else {
            panic!("room.create must complete: {created:?}");
        };
        let v: serde_json::Value = serde_json::from_str(&detail).expect("room json");
        let room_id = v["id"].as_str().expect("room id").to_string();

        let used = palette
            .dispatch(
                &DispatchRequest::new("computer.use")
                    .with_arg("action", "click")
                    .with_arg("x", "3")
                    .with_arg("y", "4")
                    .with_auth_token(&token),
            )
            .expect("computer.use");
        let PaletteOutcome::Completed {
            detail: use_detail, ..
        } = used
        else {
            panic!("computer.use must complete through the gate: {used:?}");
        };

        let info = palette
            .dispatch(&DispatchRequest::new("workbench.info"))
            .expect("workbench.info");
        let PaletteOutcome::Completed {
            detail: info_detail,
            ..
        } = info
        else {
            panic!("workbench.info must complete: {info:?}");
        };

        (room_id, env_detail, use_detail, info_detail)
    })
    .await
    .unwrap();

    // Runtime side: material attach + role link over the same records.
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
    rt.attach_material_to_room(&room_id, &material.id, "alice")
        .await
        .unwrap();
    rt.link_room_agent(&room_id, "agent-1", "reviewer", "alice")
        .await
        .unwrap();

    // Palette reads saw the runtime's state; runtime reads see the palette's.
    assert!(use_detail.contains("\"ok\":true"), "{use_detail}");
    assert!(use_detail.contains("\"twin\""), "{use_detail}");
    assert!(env_detail.contains("capabilities"), "{env_detail}");
    assert!(info_detail.contains("\"rooms\":1"), "{info_detail}");

    let room = rt.room(&room_id).await.unwrap();
    assert_eq!(room.materials, vec![material.id.clone()]);
    assert_eq!(room.agents.len(), 1);
    assert_eq!(room.agents[0].role.as_str(), "reviewer");

    // Doctor sees the whole model from the one store.
    let report = rt.doctor().await;
    for name in ["workbench", "rooms", "workbench.recovery"] {
        let row = report
            .checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("missing doctor row {name}"));
        assert_ne!(row.status, DoctorStatus::Fail, "{name}: {}", row.detail);
    }

    // One verifying audit chain covers both surfaces.
    assert!(audit_count(&rt, "permission.decision").await >= 1);
    assert!(audit_count(&rt, "workbench.room.create").await >= 1);
    assert!(audit_count(&rt, "workbench.material.create").await >= 1);
    assert!(audit_count(&rt, "room.agent.link").await >= 1);
    let count = rt.audit_verify().await.unwrap();
    assert!(count >= 4, "audit chain verifies: {count}");
}
