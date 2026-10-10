use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::time::Duration;

use openrt_selfimprove::{
    Baseline, ImproveStore, ImprovementProposal, MetricDirection, ProposalKind, ProposalState,
    Provenance, TargetKind,
};
use openrt_webui::{
    serve, CommandPalette, DispatchRequest, PaletteCommand, PaletteOutcome, WebUiConfig, WebUiError,
};

fn prov() -> Provenance {
    Provenance::new("run.test-1", "src.test")
}

fn evaluated_store() -> ImproveStore {
    let mut s = ImproveStore::new(&["alice".to_string()], false);
    s.record_baseline(
        Baseline::new(
            "baseline.a",
            "latency_ms",
            100.0,
            MetricDirection::LowerIsBetter,
            "run.base-1",
            prov(),
        )
        .unwrap(),
    )
    .unwrap();
    s.propose(
        ImprovementProposal::new(
            "prop.a",
            ProposalKind::SelfOptimize,
            "route small prompts faster",
            TargetKind::Router,
            "router.main",
            Some("baseline.a".into()),
            Some(80.0),
            None,
            prov(),
        )
        .unwrap(),
    )
    .unwrap();
    s.evaluate("prop.a").unwrap();
    s
}

fn spawn() -> (SocketAddr, openrt_webui::ServerHandle) {
    let mut palette = CommandPalette::new(evaluated_store());
    // Server-issued token authenticating allowlisted operator `alice`.
    let _token = palette.issue_token("alice");
    let handle = serve(WebUiConfig::localhost(0), palette).expect("bind loopback");
    let addr = handle.local_addr();
    (addr, handle)
}

fn spawn_with_token() -> (SocketAddr, openrt_webui::ServerHandle, String) {
    let mut palette = CommandPalette::new(evaluated_store());
    let token = palette.issue_token("alice");
    let handle = serve(WebUiConfig::localhost(0), palette).expect("bind loopback");
    let addr = handle.local_addr();
    (addr, handle, token)
}

fn http_raw(addr: SocketAddr, request: &str) -> String {
    let stream = TcpStream::connect(addr).expect("connect loopback");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut stream = stream;
    stream.write_all(request.as_bytes()).expect("write request");
    let _ = stream.shutdown(Shutdown::Write);
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).expect("read response");
    String::from_utf8_lossy(&buf).to_string()
}

fn status_of(response: &str) -> u16 {
    response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("status line")
}

fn body_of(response: &str) -> String {
    response
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or_default()
        .to_string()
}

fn get(addr: SocketAddr, path: &str) -> String {
    http_raw(
        addr,
        &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n"),
    )
}

fn post_json(addr: SocketAddr, path: &str, body: &str) -> String {
    http_raw(
        addr,
        &format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
}

fn post_json_bearer(addr: SocketAddr, path: &str, body: &str, token: &str) -> String {
    http_raw(
        addr,
        &format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
}

// ---------- binding: localhost only ----------

#[test]
fn m58_neg_non_local_bind_refused() {
    let err = WebUiConfig::new("0.0.0.0".parse().unwrap(), 7788).unwrap_err();
    assert!(matches!(err, WebUiError::NonLocalBind { .. }));
    let err2 = WebUiConfig::new("192.168.1.10".parse().unwrap(), 0).unwrap_err();
    assert!(matches!(err2, WebUiError::NonLocalBind { .. }));
    let rogue = WebUiConfig {
        bind: "0.0.0.0".parse().unwrap(),
        port: 0,
    };
    let err3 = serve(rogue, CommandPalette::new(evaluated_store())).unwrap_err();
    assert!(matches!(err3, WebUiError::NonLocalBind { .. }));
}

#[test]
fn m58_index_served_on_loopback() {
    let (addr, _handle) = spawn();
    assert!(
        addr.ip().is_loopback(),
        "server must be loopback, got {addr}"
    );
    let resp = get(addr, "/");
    assert_eq!(status_of(&resp), 200);
    let body = body_of(&resp);
    assert!(
        body.contains("OpenRT Workbench"),
        "index must ship the palette UI"
    );
    assert!(body.contains("/api/execute"));
}

#[test]
fn m58_health_endpoint() {
    let (addr, _handle) = spawn();
    let resp = get(addr, "/api/health");
    assert_eq!(status_of(&resp), 200);
    let body = body_of(&resp);
    assert!(body.contains("\"status\":\"ok\""));
    // Real bound address, not a hardcoded placeholder.
    assert!(
        body.contains("127.0.0.1:") || body.contains("[::1]:"),
        "health must report the real bind addr: {body}"
    );
    assert!(body.contains("\"loopback\":true"));
}

#[test]
fn m58_commands_list_contains_approval_gate_commands() {
    let (addr, _handle) = spawn();
    let resp = get(addr, "/api/commands");
    assert_eq!(status_of(&resp), 200);
    let body = body_of(&resp);
    assert!(body.contains("improve.approve"));
    assert!(body.contains("improve.reject"));
    assert!(body.contains("improve.proposals"));
    assert!(body.contains("session.info"));
}

#[test]
fn m58_unknown_route_404() {
    let (addr, _handle) = spawn();
    let resp = get(addr, "/definitely-missing");
    assert_eq!(status_of(&resp), 404);
}

// ---------- approval gate enforced in the UI (same as headless) ----------

#[test]
fn m58_neg_execute_approve_without_token_is_403() {
    let (addr, _handle) = spawn();
    // No Authorization header → 403 even if a body claims an operator.
    let resp = post_json(
        addr,
        "/api/execute",
        r#"{"command_id":"improve.approve","operator":"alice","args":{"proposal_id":"prop.a"}}"#,
    );
    assert_eq!(status_of(&resp), 403, "body: {}", body_of(&resp));
    let body = body_of(&resp);
    assert!(
        body.contains("operator token required") || body.contains("operator"),
        "body operator must not authorize: {body}"
    );
    let list = post_json(
        addr,
        "/api/execute",
        r#"{"command_id":"improve.proposals","args":{}}"#,
    );
    assert_eq!(status_of(&list), 200);
    let detail = body_of(&list);
    assert!(
        detail.contains("eval_passed"),
        "state must not advance without token: {detail}"
    );
}

#[test]
fn m58_neg_execute_approve_with_body_operator_only_is_403() {
    let (addr, _handle) = spawn();
    // Body `operator` is never authoritative — only the bearer token is.
    let resp = post_json(
        addr,
        "/api/execute",
        r#"{"command_id":"improve.approve","operator":"alice","args":{"proposal_id":"prop.a"}}"#,
    );
    assert_eq!(status_of(&resp), 403, "body: {}", body_of(&resp));
    let body = body_of(&resp);
    assert!(body.contains("denied") || body.contains("operator"));
    let list = post_json(
        addr,
        "/api/execute",
        r#"{"command_id":"improve.proposals","args":{}}"#,
    );
    assert!(body_of(&list).contains("eval_passed"));
}

#[test]
fn m58_neg_execute_approve_with_unknown_token_is_403() {
    let (addr, _handle) = spawn();
    let resp = post_json_bearer(
        addr,
        "/api/execute",
        r#"{"command_id":"improve.approve","args":{"proposal_id":"prop.a"}}"#,
        "not-a-real-token",
    );
    assert_eq!(status_of(&resp), 403, "body: {}", body_of(&resp));
    let body = body_of(&resp);
    assert!(body.contains("operator") || body.contains("denied"));
    let list = post_json(
        addr,
        "/api/execute",
        r#"{"command_id":"improve.proposals","args":{}}"#,
    );
    assert!(body_of(&list).contains("eval_passed"));
}

#[test]
fn m58_execute_approve_with_server_issued_token_200() {
    let (addr, _handle, token) = spawn_with_token();
    let resp = post_json_bearer(
        addr,
        "/api/execute",
        r#"{"command_id":"improve.approve","args":{"proposal_id":"prop.a"}}"#,
        &token,
    );
    assert_eq!(status_of(&resp), 200, "body: {}", body_of(&resp));
    let body = body_of(&resp);
    assert!(body.contains("\"ok\":true"));
    assert!(body.contains("approved"));
    let list = post_json(
        addr,
        "/api/execute",
        r#"{"command_id":"improve.proposals","args":{}}"#,
    );
    assert_eq!(status_of(&list), 200);
    let detail = body_of(&list);
    assert!(
        detail.contains("approved"),
        "UI approval must land in the shared store: {detail}"
    );
    assert!(!detail.contains("eval_passed"));
}

#[test]
fn m58_neg_execute_unknown_command_404() {
    let (addr, _handle) = spawn();
    let resp = post_json(
        addr,
        "/api/execute",
        r#"{"command_id":"does.not.exist","args":{}}"#,
    );
    assert_eq!(status_of(&resp), 404);
}

#[test]
fn m58_neg_execute_bad_json_400() {
    let (addr, _handle) = spawn();
    let resp = post_json(addr, "/api/execute", "not-json");
    assert_eq!(status_of(&resp), 400);
}

#[test]
fn m58_neg_execute_missing_command_id_400() {
    let (addr, _handle) = spawn();
    let resp = post_json(addr, "/api/execute", r#"{"args":{}}"#);
    assert_eq!(status_of(&resp), 400);
}

// ---------- palette dispatch (headless unit surface) ----------

#[test]
fn m58_palette_lists_commands_sorted() {
    let palette = CommandPalette::new(evaluated_store());
    let ids: Vec<String> = palette.commands().into_iter().map(|c| c.id).collect();
    assert_eq!(
        ids,
        vec![
            "assets.skills",
            "computer.env",
            "computer.observe",
            "computer.use",
            "extension.inspect",
            "extension.list",
            "history.compare",
            "history.fork",
            "history.return",
            "history.state",
            "history.tail",
            "history.timeline",
            "improve.approve",
            "improve.proposals",
            "improve.reject",
            "improve.states",
            "industrial.assets",
            "learn.episodes",
            "learn.promotions",
            "material.list",
            "material.verify",
            "mcp.disable",
            "mcp.enable",
            "mcp.list",
            "mcp.trust",
            "memory.working.has",
            "phase12.info",
            "recovery.list",
            "room.agent",
            "room.create",
            "room.list",
            "room.show",
            "room.status",
            "room.steer",
            "session.info",
            "session.list",
            "shield.findings",
            "sih.info",
            "skill.inspect",
            "task.list",
            "trading.datasets",
            "trading.execute",
            "workbench.info",
            "workflow.compile",
            "workflow.inspect",
            "workflow.list",
            "workflow.validate",
            "workspace.info",
        ]
    );
}

#[test]
fn m58_palette_session_info_reports_real_bind() {
    let mut palette = CommandPalette::new(evaluated_store());
    palette.set_bind("127.0.0.1:7788");
    let out = palette
        .dispatch(&DispatchRequest::new("session.info"))
        .unwrap();
    match out {
        PaletteOutcome::Completed { id, detail } => {
            assert_eq!(id, "session.info");
            assert!(
                detail.contains("127.0.0.1:7788"),
                "session.info must report the real bind: {detail}"
            );
            assert!(
                !detail.contains("\"bind\":\"loopback\""),
                "placeholder bind must be replaced: {detail}"
            );
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn m58_neg_palette_unknown_command() {
    let mut palette = CommandPalette::new(evaluated_store());
    let err = palette
        .dispatch(&DispatchRequest::new("nope.command"))
        .unwrap_err();
    assert!(matches!(err, WebUiError::CommandNotFound { .. }));
}

#[test]
fn m58_neg_palette_approve_requires_token() {
    let mut palette = CommandPalette::new(evaluated_store());
    // Mint a token but do not attach it — still unauthorized.
    let _tok = palette.issue_token("alice");
    let err = palette
        .dispatch(&DispatchRequest::new("improve.approve").with_arg("proposal_id", "prop.a"))
        .unwrap_err();
    assert!(matches!(err, WebUiError::UnauthorizedCommand { .. }));
}

#[test]
fn m58_palette_register_custom_command() {
    let mut palette = CommandPalette::new(evaluated_store());
    palette
        .register(PaletteCommand {
            id: "custom.hello".into(),
            title: "Hello".into(),
            description: "custom".into(),
        })
        .unwrap();
    let err = palette
        .register(PaletteCommand {
            id: "custom.hello".into(),
            title: "Dup".into(),
            description: "dup".into(),
        })
        .unwrap_err();
    assert!(matches!(err, WebUiError::BadRequest(_)));
}

#[test]
fn m58_palette_reject_path() {
    let mut palette = CommandPalette::new(evaluated_store());
    let token = palette.issue_token("alice");
    let out = palette
        .dispatch(
            &DispatchRequest::new("improve.reject")
                .with_arg("proposal_id", "prop.a")
                .with_auth_token(&token),
        )
        .unwrap();
    assert!(matches!(out, PaletteOutcome::Completed { .. }));
    assert_eq!(
        palette.store().get_proposal("prop.a").unwrap().state,
        ProposalState::Rejected
    );
}

#[test]
fn m58_neg_palette_reject_with_wrong_token_denied() {
    let mut palette = CommandPalette::new(evaluated_store());
    // Token minted for a non-allowlisted operator: OperatorGrant is valid
    // but the store's allowlist rejects bob → PaletteOutcome::Denied.
    let bob = palette.issue_token("bob");
    let out = palette
        .dispatch(
            &DispatchRequest::new("improve.reject")
                .with_arg("proposal_id", "prop.a")
                .with_auth_token(&bob),
        )
        .unwrap();
    match out {
        PaletteOutcome::Denied { id, reason } => {
            assert_eq!(id, "improve.reject");
            assert!(
                reason.contains("bob") || reason.contains("not authorized"),
                "{reason}"
            );
        }
        other => panic!("non-allowlisted operator must be denied, got {other:?}"),
    }
    assert_eq!(
        palette.store().get_proposal("prop.a").unwrap().state,
        ProposalState::EvalPassed,
        "state must not change for non-allowlisted operator"
    );
}

#[test]
fn m58_palette_missing_proposal_arg_is_bad_request() {
    let mut palette = CommandPalette::new(evaluated_store());
    let token = palette.issue_token("alice");
    let err = palette
        .dispatch(&DispatchRequest::new("improve.approve").with_auth_token(&token))
        .unwrap_err();
    assert!(matches!(err, WebUiError::BadRequest(_)));
}

#[test]
fn m58_palette_empty_args_map_builds_cleanly() {
    let req = DispatchRequest {
        command_id: "session.info".into(),
        args: BTreeMap::new(),
        auth_token: None,
    };
    assert_eq!(req.command_id, "session.info");
    assert!(req.args.is_empty());
    assert!(req.auth_token.is_none());
}

/// M5.8 security: `issue_token` must use the OS CSPRNG — two tokens for the
/// same operator are never equal (uuid v4, non-deterministic across calls).
#[test]
fn m58_issue_token_is_csprng_nondeterministic() {
    let mut palette = CommandPalette::new(evaluated_store());
    let t1 = palette.issue_token("alice");
    let t2 = palette.issue_token("alice");
    assert!(t1.starts_with("oprt-"), "t1={t1}");
    assert!(t2.starts_with("oprt-"), "t2={t2}");
    assert_ne!(t1, t2, "CSPRNG tokens must not collide");
}

// ---------- Phase 10 palette views (M10.2 recovery / M10.5 timeline /
// M10.6 skills / M10.9 shield) ----------

/// A palette wired to a REAL runtime (same construction the production
/// server uses) plus the runtime handle needed to seed data.
fn p10_env() -> (
    CommandPalette,
    std::sync::Arc<openrt_runtime::Runtime>,
    tokio::runtime::Runtime,
) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let app = rt.block_on(async {
        openrt_runtime::RuntimeBuilder::new()
            .store(std::sync::Arc::new(openrt_persistence::MemoryStore::new()))
            .with_provider(std::sync::Arc::new(
                openrt_models::mock::MockProvider::echo("mock"),
            ))
            .bus_capacity(32)
            .build()
            .await
            .unwrap()
    });
    let handle = rt.handle().clone();
    let palette = CommandPalette::with_runtime(evaluated_store(), app.clone(), handle);
    (palette, app, rt)
}

fn completed_detail(out: PaletteOutcome) -> serde_json::Value {
    match out {
        PaletteOutcome::Completed { detail, .. } => {
            serde_json::from_str(&detail).expect("detail is JSON")
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

/// M10.5: the palette timeline returns REAL ledger rows for the session
/// (verified chain — seq/tail/status come from the store, never invented),
/// and every runtime-gated command fails closed without a runtime.
#[test]
fn p10_palette_history_timeline_returns_real_rows() {
    // Runtime-gated commands are honest when no runtime is attached.
    let mut offline = CommandPalette::new(evaluated_store());
    assert!(matches!(
        offline.dispatch(&DispatchRequest::new("history.timeline")),
        Err(WebUiError::BadRequest(_))
    ));

    let (mut palette, app, rt) = p10_env();
    let sid = rt.block_on(async {
        let s = app.create_session("p10 timeline").await.unwrap();
        let task = app.run_task(s.id, "echo p10 timeline").await.unwrap();
        assert_eq!(task.status, openrt_core::TaskStatus::Completed);
        // HistorySink lands rows asynchronously — poll like phase5 does.
        for _ in 0..100 {
            if app.history_tail().await.unwrap() >= 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        s.id.to_string()
    });

    let out = palette
        .dispatch(
            &DispatchRequest::new("history.timeline")
                .with_arg("session", &sid)
                .with_arg("limit", "5"),
        )
        .expect("timeline dispatch");
    let v = completed_detail(out);
    assert!(
        v["count"].as_u64().unwrap() >= 1,
        "session-scoped timeline must show real rows: {v}"
    );
    assert!(v["tail_seq"].as_u64().unwrap() >= 1, "{v}");
    let first = &v["records"][0];
    assert!(first["seq"].as_u64().is_some(), "{first}");
    assert!(
        !first["event_kind"].as_str().unwrap_or("").is_empty(),
        "{first}"
    );

    // Unknown args fail closed (bad limit), never silently defaulted.
    assert!(palette
        .dispatch(&DispatchRequest::new("history.timeline").with_arg("limit", "lots"))
        .is_err());
}

/// M10.9: shield.findings reads the SAME store the gate records to —
/// seeded finding visible with real severity/status; filters work; a
/// status typo fails closed.
#[test]
fn p10_palette_shield_findings_lists_recorded() {
    let (mut palette, app, rt) = p10_env();
    let finding = openrt_shield::SecurityFinding::new(
        openrt_shield::FindingCategory::CredentialAccess,
        openrt_shield::Severity::Critical,
        "webui.p10.test",
        "tool.read",
        "seeded credential access for the palette view",
        serde_json::json!({"seed": true}),
    )
    .with_action(openrt_shield::ShieldAction::Deny, "test:deny->DENY");
    let report =
        openrt_shield::ShieldReport::from_findings("webui.p10.test", "tool.read", vec![finding]);
    let store = app.store.clone();
    rt.block_on(async {
        openrt_shield::store::record_enforced(
            &*store,
            &report,
            openrt_shield::FindingStatus::Denied,
        )
        .await
        .unwrap();
    });

    let v = completed_detail(
        palette
            .dispatch(&DispatchRequest::new("shield.findings"))
            .unwrap(),
    );
    assert_eq!(v["count"].as_u64(), Some(1), "{v}");
    assert_eq!(v["findings"][0]["severity"], "CRITICAL");
    assert_eq!(v["findings"][0]["status"], "denied");
    assert_eq!(v["findings"][0]["category"], "credential_access");

    // Lifecycle filter works both ways…
    let denied = completed_detail(
        palette
            .dispatch(&DispatchRequest::new("shield.findings").with_arg("status", "denied"))
            .unwrap(),
    );
    assert_eq!(denied["count"].as_u64(), Some(1));
    let halted = completed_detail(
        palette
            .dispatch(&DispatchRequest::new("shield.findings").with_arg("status", "halted"))
            .unwrap(),
    );
    assert_eq!(halted["count"].as_u64(), Some(0));

    // …and a typo never silently lists everything.
    assert!(palette
        .dispatch(&DispatchRequest::new("shield.findings").with_arg("status", "denIed"))
        .is_err());
}

/// M10.2: every boot writes a recovery record — the palette lists the real
/// record id and its honest finding counts (clean boot = zero counts).
#[test]
fn p10_palette_recovery_list_shows_boot_record() {
    let (mut palette, _app, _rt) = p10_env();
    let v = completed_detail(
        palette
            .dispatch(&DispatchRequest::new("recovery.list"))
            .unwrap(),
    );
    assert!(
        v["count"].as_u64().unwrap() >= 1,
        "every boot records evidence: {v}"
    );
    let rec = &v["records"][0];
    assert!(
        rec["record_id"].as_str().unwrap().starts_with("boot-"),
        "{rec}"
    );
    assert_eq!(rec["clean"], serde_json::json!(true));
    assert_eq!(rec["interrupted"], serde_json::json!(0));
}

/// M10.6: skill.inspect requires an id (fail-closed BadRequest) and an
/// unknown skill NEVER fabricates a report — it errors out.
#[test]
fn p10_neg_palette_skill_inspect_fails_closed() {
    let (mut palette, _app, _rt) = p10_env();
    assert!(matches!(
        palette.dispatch(&DispatchRequest::new("skill.inspect")),
        Err(WebUiError::BadRequest(_))
    ));
    match palette.dispatch(&DispatchRequest::new("skill.inspect").with_arg("id", "no.such")) {
        Err(WebUiError::BadRequest(_)) | Ok(PaletteOutcome::Denied { .. }) => {}
        other => panic!("unknown skill must fail closed, got {other:?}"),
    }
}

/// Phase 11 M11.6: workbench/room/material/extension palette commands read
/// and mutate through the ONE Runtime (same methods as CLI/TUI); mutations
/// require the server-issued token, args fail closed, verify reports drift
/// honestly (Denied — never a fake pass).
#[test]
fn p11_palette_workbench_room_material_extension_commands() {
    let (mut palette, app, rt) = p10_env();
    let token = palette.issue_token("alice");

    // Seed a workbench + a material reference through the shared store.
    rt.block_on(async {
        openrt_workspace::create_workspace(
            &*app.store,
            "acct/pal",
            "Palette",
            "",
            "2026-09-27T00:00:00Z",
        )
        .await
        .unwrap();
        app.store
            .put_record("docs", "brief", &serde_json::json!({"t": "p11"}))
            .await
            .unwrap();
    });

    // Missing args fail closed (no token → unauthorized; token → BadRequest).
    assert!(matches!(
        palette.dispatch(&DispatchRequest::new("room.create")),
        Err(WebUiError::UnauthorizedCommand { .. }) | Err(WebUiError::BadRequest(_))
    ));
    // …and a mutation without the server-issued token is never authorized
    // (client-asserted operator identity does not exist).
    assert!(matches!(
        palette.dispatch(
            &DispatchRequest::new("room.create")
                .with_arg("workbench", "acct/pal")
                .with_arg("title", "Standup")
        ),
        Err(WebUiError::UnauthorizedCommand { .. })
    ));

    // Unknown workbench fails closed (room never exists without one).
    assert!(matches!(
        palette.dispatch(
            &DispatchRequest::new("room.create")
                .with_arg("workbench", "acct/ghost")
                .with_arg("title", "Nope")
                .with_auth_token(&token)
        ),
        Err(WebUiError::BadRequest(_))
    ));

    let out = palette
        .dispatch(
            &DispatchRequest::new("room.create")
                .with_arg("workbench", "acct/pal")
                .with_arg("title", "Standup")
                .with_auth_token(&token),
        )
        .expect("room.create");
    let room_id = completed_detail(out)["id"].as_str().unwrap().to_string();

    // workbench.info reflects the created room (store-derived totals).
    let v = completed_detail(
        palette
            .dispatch(&DispatchRequest::new("workbench.info"))
            .unwrap(),
    );
    assert_eq!(v["workspaces"], 1, "{v}");
    assert_eq!(v["rooms"], 1, "{v}");
    assert_eq!(v["steer"], 0, "{v}");

    // room.list / room.show expose the same record the CLI writes.
    let v = completed_detail(
        palette
            .dispatch(&DispatchRequest::new("room.list"))
            .unwrap(),
    );
    assert_eq!(v[0]["id"], room_id.as_str());
    assert_eq!(v[0]["status"], "active");
    let v = completed_detail(
        palette
            .dispatch(&DispatchRequest::new("room.show").with_arg("id", &room_id))
            .unwrap(),
    );
    assert_eq!(v["title"], "Standup");

    // room.status: valid transition works; a typo fails closed.
    let v = completed_detail(
        palette
            .dispatch(
                &DispatchRequest::new("room.status")
                    .with_arg("id", &room_id)
                    .with_arg("status", "paused")
                    .with_auth_token(&token),
            )
            .unwrap(),
    );
    assert_eq!(v["status"], "paused");
    assert!(matches!(
        palette.dispatch(
            &DispatchRequest::new("room.status")
                .with_arg("id", &room_id)
                .with_arg("status", "sleeping")
                .with_auth_token(&token)
        ),
        Err(WebUiError::BadRequest(_))
    ));

    // room.agent: the role must map onto an existing SubAgentKind (M11.4).
    let v = completed_detail(
        palette
            .dispatch(
                &DispatchRequest::new("room.agent")
                    .with_arg("id", &room_id)
                    .with_arg("agent", "a1")
                    .with_arg("role", "researcher")
                    .with_auth_token(&token),
            )
            .unwrap(),
    );
    assert_eq!(v["agents"][0]["role"], "researcher");
    assert!(matches!(
        palette.dispatch(
            &DispatchRequest::new("room.agent")
                .with_arg("id", &room_id)
                .with_arg("agent", "a2")
                .with_arg("role", "pirate")
                .with_auth_token(&token)
        ),
        Err(WebUiError::BadRequest(_))
    ));

    // room.steer: appends exactly once; empty input fails closed.
    let v = completed_detail(
        palette
            .dispatch(
                &DispatchRequest::new("room.steer")
                    .with_arg("id", &room_id)
                    .with_arg("input", "keep it short")
                    .with_auth_token(&token),
            )
            .unwrap(),
    );
    assert_eq!(v["appended"], true);
    assert!(matches!(
        palette.dispatch(
            &DispatchRequest::new("room.steer")
                .with_arg("id", &room_id)
                .with_arg("input", "   ")
                .with_auth_token(&token)
        ),
        Err(WebUiError::BadRequest(_))
    ));
    let v = completed_detail(
        palette
            .dispatch(&DispatchRequest::new("room.show").with_arg("id", &room_id))
            .unwrap(),
    );
    assert_eq!(v["steer"].as_array().map(Vec::len), Some(1), "{v}");

    // extension.list is honest on a fresh store (empty array, no fakes).
    let v = completed_detail(
        palette
            .dispatch(&DispatchRequest::new("extension.list"))
            .unwrap(),
    );
    assert_eq!(v.as_array().map(Vec::len), Some(0), "{v}");

    // material.list / material.verify over the sealed digest.
    let material = rt
        .block_on(async {
            app.create_material(
                openrt_workspace::MaterialKind::Document,
                "q3 brief",
                "ingest",
                openrt_workspace::MaterialReference::Store {
                    namespace: "docs".into(),
                    key: "brief".into(),
                },
                "alice",
            )
            .await
        })
        .unwrap();
    let v = completed_detail(
        palette
            .dispatch(&DispatchRequest::new("material.list"))
            .unwrap(),
    );
    assert_eq!(v.as_array().map(Vec::len), Some(1), "{v}");
    let v = completed_detail(
        palette
            .dispatch(&DispatchRequest::new("material.verify").with_arg("id", &material.id))
            .unwrap(),
    );
    assert_eq!(v["intact"], true, "{v}");

    // Drift the referenced content — verify must report it, never pass.
    rt.block_on(async {
        app.store
            .put_record("docs", "brief", &serde_json::json!({"t": "tampered"}))
            .await
            .unwrap();
    });
    match palette.dispatch(&DispatchRequest::new("material.verify").with_arg("id", &material.id)) {
        Ok(PaletteOutcome::Denied { reason, .. }) => assert!(reason.contains("DRIFTED")),
        other => panic!("drift must be Denied, got {other:?}"),
    }
}

/// Phase 11 M11.2: mcp.* palette commands — reads work without a token,
/// trust changes require one, and an unknown server never fabricates.
#[test]
fn p11_palette_mcp_commands_token_gated() {
    let (mut palette, _app, _rt) = p10_env();

    // Read-only list works and reports the real registry summary shape.
    let v = completed_detail(palette.dispatch(&DispatchRequest::new("mcp.list")).unwrap());
    assert!(v.get("summary").is_some(), "{v}");

    // Trust mutation without a token is unauthorized.
    assert!(matches!(
        palette.dispatch(
            &DispatchRequest::new("mcp.trust")
                .with_arg("name", "ghost")
                .with_arg("trust", "trusted")
        ),
        Err(WebUiError::UnauthorizedCommand { .. })
    ));

    // With a token, an unknown server fails closed (no fabricated success).
    let token = palette.issue_token("alice");
    match palette.dispatch(
        &DispatchRequest::new("mcp.trust")
            .with_arg("name", "ghost")
            .with_arg("trust", "trusted")
            .with_auth_token(&token),
    ) {
        Err(WebUiError::BadRequest(_)) | Ok(PaletteOutcome::Denied { .. }) => {}
        other => panic!("unknown mcp server must fail closed, got {other:?}"),
    }
}

// ---------- Phase 13: SIH palette command + read-only routes ----------

/// Phase 13: `sih.info` fails closed without a runtime, returns the real
/// CyberSaarthi dashboard with a runtime, and `GET /sih` + `GET /api/sih`
/// serve the demo page/data with no operator token (read-only surface).
#[test]
fn p13_sih_info_dispatch_and_routes() {
    let mut offline = CommandPalette::new(evaluated_store());
    assert!(matches!(
        offline.dispatch(&DispatchRequest::new("sih.info")),
        Err(WebUiError::BadRequest(_))
    ));

    let (mut palette, _app, rt) = p10_env();
    let out = palette
        .dispatch(&DispatchRequest::new("sih.info"))
        .expect("sih.info dispatches");
    let v = completed_detail(out);
    assert_eq!(v["case"]["case_id"], "case.sih-demo", "{v}");
    assert_eq!(v["case"]["synthetic"], true, "{v}");
    for key in ["shield_findings", "halted", "egress_grants", "secrets"] {
        assert!(v["security"].get(key).is_some(), "security.{key}: {v}");
    }
    assert!(v["audit"].get("chain_verified").is_some(), "{v}");

    let handle = serve(WebUiConfig::localhost(0), palette).expect("bind loopback");
    let addr = handle.local_addr();

    let api = get(addr, "/api/sih");
    assert_eq!(status_of(&api), 200);
    let body = body_of(&api);
    assert!(body.contains("case.sih-demo"), "dashboard body: {body}");

    let page = get(addr, "/sih");
    assert_eq!(status_of(&page), 200);
    let html = body_of(&page);
    assert!(html.contains("CyberSaarthi"), "page: {html}");

    let cmds = get(addr, "/api/commands");
    assert!(body_of(&cmds).contains("sih.info"));

    drop(rt);
}
