//! Phase 5 end-to-end: workspace assets -> approval-gated self-improvement ->
//! local telemetry/learning -> localhost web UI, wired cross-crate (M5.9).
//!
//! Covers both the data-layer path (direct `ImproveStore`/`AssetStore`) and
//! the *production runtime* path (`Runtime::{phase5::*}` with `StoreApplyTarget`
//! and shared ModeManager/permission policy). The UI approval gate uses the
//! server-issued bearer token (no client-asserted operator identity).

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use openrt_persistence::{MemoryStore, PersistenceStore};
use openrt_selfimprove::{
    ApplyTarget, Baseline, ImproveError, ImproveStore, ImprovementProposal, LearningEngine,
    MetricDirection, OperatorGrant, ProposalKind, ProposalState, Provenance, RunSummary,
    TargetKind,
};
use openrt_telemetry::{
    ChannelSink, DomainEvent, EventBusBuilder, EventKind, MetricsRegistry, METRICS_SCHEMA_VERSION,
};
use openrt_webui::{serve, CommandPalette, WebUiConfig};
use openrt_workspace_assets::{
    ActivationState, AssetStore, CanonicalForm, Skill, SkillId, SkillVersion,
};

fn prov() -> Provenance {
    Provenance::new("run.p5-e2e", "src.p5_integration")
}

fn grant() -> OperatorGrant {
    OperatorGrant::new("alice").unwrap()
}

struct FakeRouter;

impl ApplyTarget for FakeRouter {
    fn kind(&self) -> TargetKind {
        TargetKind::Router
    }

    fn id(&self) -> &str {
        "router.main"
    }

    fn apply(&self, _proposal: &ImprovementProposal) -> Result<(), ImproveError> {
        Ok(())
    }
}

struct FakePolicy;

impl ApplyTarget for FakePolicy {
    fn kind(&self) -> TargetKind {
        TargetKind::PermissionPolicy
    }

    fn id(&self) -> &str {
        "policy.main"
    }

    fn apply(&self, _proposal: &ImprovementProposal) -> Result<(), ImproveError> {
        Ok(())
    }
}

fn skill() -> Skill {
    Skill::new(
        SkillId("skill.p5".into()),
        SkillVersion(1),
        "phase5 e2e skill",
        vec!["fs.read".to_string()],
        vec![],
        prov(),
    )
    .unwrap()
}

fn evaluated_improve_store() -> ImproveStore {
    let mut s = ImproveStore::new(&["alice".to_string()], false);
    s.record_baseline(
        Baseline::new(
            "baseline.p5",
            "latency_ms",
            100.0,
            MetricDirection::LowerIsBetter,
            "run.p5-base",
            prov(),
        )
        .unwrap(),
    )
    .unwrap();
    s.propose(
        ImprovementProposal::new(
            "prop.p5",
            ProposalKind::SelfOptimize,
            "route echo-class prompts to the fast path",
            TargetKind::Router,
            "router.main",
            Some("baseline.p5".into()),
            Some(75.0),
            None,
            prov(),
        )
        .unwrap(),
    )
    .unwrap();
    s.evaluate("prop.p5").unwrap();
    s
}

#[test]
fn p5_asset_then_optimization_loop() {
    let mut assets = AssetStore::new(&["alice".to_string()], &["fs.read".to_string()], false);
    assets.register_skill(skill()).unwrap();
    assets.validate_skill(&SkillId("skill.p5".into())).unwrap();
    assets
        .activate_skill(&SkillId("skill.p5".into()), Some(&grant()))
        .unwrap();
    assert_eq!(
        assets
            .get_skill(&SkillId("skill.p5".into()))
            .unwrap()
            .activation,
        ActivationState::Active
    );
    let mallory = OperatorGrant::new("mallory").unwrap();
    let mut assets2 = AssetStore::new(&["alice".to_string()], &["fs.read".to_string()], false);
    assets2.register_skill(skill()).unwrap();
    assets2.validate_skill(&SkillId("skill.p5".into())).unwrap();
    assert!(
        assets2
            .activate_skill(&SkillId("skill.p5".into()), Some(&mallory))
            .is_err(),
        "activation gate holds across the stack"
    );

    let mut improve = evaluated_improve_store();
    improve.approve("prop.p5", Some(&grant())).unwrap();
    let applied = improve
        .apply("prop.p5", &FakeRouter, Some(&grant()))
        .unwrap();
    assert_eq!(applied.target_id, "router.main");
    assert_eq!(
        improve.get_proposal("prop.p5").unwrap().state,
        ProposalState::Applied
    );

    let mut regressed = ImproveStore::new(&["alice".to_string()], false);
    regressed
        .record_baseline(
            Baseline::new(
                "baseline.p5b",
                "latency_ms",
                100.0,
                MetricDirection::LowerIsBetter,
                "run.p5-base",
                prov(),
            )
            .unwrap(),
        )
        .unwrap();
    regressed
        .propose(
            ImprovementProposal::new(
                "prop.p5bad",
                ProposalKind::SelfOptimize,
                "slower path",
                TargetKind::Router,
                "router.main",
                Some("baseline.p5b".into()),
                Some(140.0),
                None,
                prov(),
            )
            .unwrap(),
        )
        .unwrap();
    let err = regressed.evaluate("prop.p5bad").unwrap_err();
    assert!(matches!(err, ImproveError::RegressionDetected { .. }));
    assert_eq!(
        regressed.get_proposal("prop.p5bad").unwrap().state,
        ProposalState::Rejected
    );

    let mut protected = ImproveStore::new(&["alice".to_string()], false);
    protected
        .propose(
            ImprovementProposal::new(
                "prop.p5policy",
                ProposalKind::ResearchChange,
                "attempt policy change",
                TargetKind::PermissionPolicy,
                "policy.main",
                None,
                None,
                None,
                prov(),
            )
            .unwrap(),
        )
        .unwrap();
    protected.evaluate("prop.p5policy").unwrap();
    protected.approve("prop.p5policy", Some(&grant())).unwrap();
    let err = protected
        .apply("prop.p5policy", &FakePolicy, Some(&grant()))
        .unwrap_err();
    assert!(
        matches!(err, ImproveError::ProtectedTarget { .. }),
        "self-improvement must never apply policy changes, got {err:?}"
    );
}

#[tokio::test]
async fn p5_learning_and_local_telemetry_emit() {
    let summary = RunSummary::new("run.p5-hist")
        .with_success(true)
        .with_tool("fs.read", 6)
        .with_tool("fs.list", 2);
    let records = LearningEngine::derive(&summary, prov()).unwrap();
    assert!(
        records.len() >= 2,
        "successful multi-tool history derives workflow + instinct"
    );

    let mut improve = ImproveStore::new(&["alice".to_string()], false);
    let stored = improve.derive_learnings(&summary, prov()).unwrap();
    assert_eq!(stored.len(), records.len());

    let sink = std::sync::Arc::new(ChannelSink::new());
    let (bus, _handle) = EventBusBuilder::new().sink(sink.clone()).start();
    let mut metrics = MetricsRegistry::new();
    metrics.record("p5.latency_ms", 75.0, "ms").unwrap();
    let sent = metrics.emit_all(&bus).await;
    assert_eq!(sent, 1);
    assert_eq!(metrics.schema_version(), METRICS_SCHEMA_VERSION);
    bus.emit(
        DomainEvent::new(EventKind::ImprovementApproved)
            .with_data(serde_json::json!({"proposal_id": "prop.p5", "operator": "alice"})),
    )
    .await;
    bus.emit(
        DomainEvent::new(EventKind::LearningDerived).with_data(serde_json::json!({
            "learning_id": records[0].id,
            "learning_kind": records[0].kind.as_str(),
            "run_id": records[0].run_id,
        })),
    )
    .await;
    drop(bus);
    tokio::task::yield_now().await;
    let events = sink.snapshot();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].kind, EventKind::MetricRecorded);
    assert_eq!(events[0].data["schema_version"], 1u64);
    assert_eq!(events[1].kind, EventKind::ImprovementApproved);
    assert_eq!(events[2].kind, EventKind::LearningDerived);
    for ev in &events {
        let round: DomainEvent = serde_json::from_value(serde_json::to_value(ev).unwrap()).unwrap();
        assert_eq!(round.kind, ev.kind);
    }
}

fn http(addr: std::net::SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).expect("loopback connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let _ = stream.shutdown(Shutdown::Write);
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).unwrap();
    String::from_utf8_lossy(&buf).to_string()
}

fn status_of(response: &str) -> u16 {
    response
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .expect("status")
}

fn body_of(response: &str) -> String {
    response
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or_default()
        .to_string()
}

#[test]
fn p5_webui_approval_gate_end_to_end() {
    let mut palette = CommandPalette::new(evaluated_improve_store());
    let token = palette.issue_token("alice");
    let handle = serve(WebUiConfig::localhost(0), palette).expect("loopback bind");
    let addr = handle.local_addr();
    assert!(addr.ip().is_loopback());

    let index = http(addr, "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(status_of(&index), 200);
    assert!(body_of(&index).contains("OpenRT Workbench"));

    // No token → 403 (body operator is never authoritative).
    let denied_body =
        r#"{"command_id":"improve.approve","operator":"alice","args":{"proposal_id":"prop.p5"}}"#;
    let denied = http(
        addr,
        &format!(
            "POST /api/execute HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{denied_body}",
            denied_body.len()
        ),
    );
    assert_eq!(status_of(&denied), 403, "{}", body_of(&denied));

    // Server-issued bearer token → 200.
    let body = r#"{"command_id":"improve.approve","args":{"proposal_id":"prop.p5"}}"#;
    let ok = http(
        addr,
        &format!(
            "POST /api/execute HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    );
    assert_eq!(status_of(&ok), 200, "{}", body_of(&ok));
    assert!(body_of(&ok).contains("approved"));

    let list_body = r#"{"command_id":"improve.proposals","args":{}}"#;
    let list = http(
        addr,
        &format!(
            "POST /api/execute HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{list_body}",
            list_body.len()
        ),
    );
    assert_eq!(status_of(&list), 200);
    assert!(body_of(&list).contains("approved"));
    handle.stop();
}

#[tokio::test]
async fn p5_cross_crate_persistence_roundtrip() {
    let mut assets = AssetStore::new(&["alice".to_string()], &["fs.read".to_string()], false);
    assets.register_skill(skill()).unwrap();
    assets.validate_skill(&SkillId("skill.p5".into())).unwrap();
    assets
        .activate_skill(&SkillId("skill.p5".into()), Some(&grant()))
        .unwrap();

    let mut improve = evaluated_improve_store();
    improve.approve("prop.p5", Some(&grant())).unwrap();
    improve
        .derive_learnings(
            &RunSummary::new("run.p5-hist")
                .with_success(true)
                .with_tool("fs.read", 4),
            prov(),
        )
        .unwrap();

    let mem = MemoryStore::default();
    assets.save(&mem).await.unwrap();
    improve.save(&mem).await.unwrap();

    let assets2 = AssetStore::load(&mem).await.unwrap();
    assert_eq!(
        assets2
            .get_skill(&SkillId("skill.p5".into()))
            .unwrap()
            .digest(),
        assets
            .get_skill(&SkillId("skill.p5".into()))
            .unwrap()
            .digest()
    );
    assert_eq!(
        assets2
            .get_skill(&SkillId("skill.p5".into()))
            .unwrap()
            .activation,
        ActivationState::Active
    );

    let improve2 = ImproveStore::load(&mem).await.unwrap();
    assert_eq!(
        improve2.get_proposal("prop.p5").unwrap().state,
        ProposalState::Approved
    );
    assert_eq!(
        improve2.get_proposal("prop.p5").unwrap().digest(),
        improve.get_proposal("prop.p5").unwrap().digest()
    );
    assert_eq!(improve2.meta(), improve.meta());
}

/// M5.9 full e2e through the *production* Runtime path (no fakes): a real
/// session/task → history-derived skill candidate → asset activate → baseline
/// → propose → evaluate → approve → apply via `StoreApplyTarget` → learning
/// from the ledger → metrics on the Phase-1 bus. All state lives in one
/// MemoryStore (ADR-006).
#[tokio::test]
async fn p5_runtime_production_end_to_end() {
    use openrt_runtime::RuntimeBuilder;

    let store = std::sync::Arc::new(MemoryStore::new());
    let rt = RuntimeBuilder::new()
        .config(openrt_config::Config::default())
        .store(store.clone())
        .with_provider(std::sync::Arc::new(
            openrt_models::MockProvider::tool_sequence("mock"),
        ))
        .bus_capacity(64)
        .build()
        .await
        .unwrap();

    // Real run on the ledger (hooks fire on SessionCreated).
    let session = rt.create_session("p5e2e").await.unwrap();
    let task = rt
        .run_task(session.id, "echo phase5 production path")
        .await
        .unwrap();
    assert_eq!(task.status, openrt_core::TaskStatus::Completed);
    for _ in 0..100 {
        if rt.history_tail().await.unwrap() >= 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    // History-derived skill candidate (generate ≠ activate).
    let cand_id = rt
        .skill_submit_from_history(
            "req.p5e2e",
            "derived from real runs",
            vec!["fs.read".into()],
            &session.id.to_string(),
        )
        .await
        .unwrap();
    assert!(cand_id.starts_with("cand."));

    // Register + activate a skill under the process operator allowlist.
    let op = std::env::var("USER").unwrap_or_else(|_| "openrt".into());
    rt.assets_register_skill("skill.p5e2e", "production skill", vec!["fs.read".into()])
        .await
        .unwrap();
    rt.assets_validate_skill("skill.p5e2e").await.unwrap();
    rt.assets_activate_skill("skill.p5e2e", &op).await.unwrap();
    let states = rt.assets_skill_states().await.unwrap();
    assert!(states
        .iter()
        .any(|(id, s)| id == "skill.p5e2e" && s == "active"));

    // Package round-trip through the production store.
    let pkg = rt.assets_export_package().await.unwrap();
    assert_eq!(pkg["schema"], 1);

    // Approval-gated self-optimization through StoreApplyTarget.
    rt.improve_record_baseline(
        "baseline.p5e2e",
        "latency_ms",
        100.0,
        openrt_selfimprove::MetricDirection::LowerIsBetter,
        "run.p5e2e",
    )
    .await
    .unwrap();
    rt.improve_propose(
        "prop.p5e2e",
        openrt_selfimprove::ProposalKind::SelfOptimize,
        "fast path",
        openrt_selfimprove::TargetKind::Router,
        "router.main",
        Some("baseline.p5e2e".into()),
        Some(75.0),
    )
    .await
    .unwrap();
    let eval = rt.improve_evaluate("prop.p5e2e").await.unwrap();
    assert!(eval.contains("passed"), "eval={eval}");

    // Non-operator must fail closed.
    assert!(rt.improve_approve("prop.p5e2e", "mallory").await.is_err());

    rt.improve_approve("prop.p5e2e", &op).await.unwrap();
    let change = rt.improve_apply("prop.p5e2e", &op).await.unwrap();
    assert_eq!(change.target_id, "router.main");
    let applied = store
        .get_record("improve.applied", "prop.p5e2e")
        .await
        .unwrap()
        .expect("applied change must be durable");
    assert_eq!(applied["proposal_id"], "prop.p5e2e");

    // Protected target refuses apply even after approve.
    rt.improve_propose(
        "prop.p5e2epol",
        openrt_selfimprove::ProposalKind::ResearchChange,
        "policy edit",
        openrt_selfimprove::TargetKind::PermissionPolicy,
        "policy.main",
        None,
        None,
    )
    .await
    .unwrap();
    rt.improve_evaluate("prop.p5e2epol").await.unwrap();
    rt.improve_approve("prop.p5e2epol", &op).await.unwrap();
    let prot = rt.improve_apply("prop.p5e2epol", &op).await;
    assert!(prot.is_err(), "protected target must refuse: {prot:?}");

    // Learning from the real ledger into the durable store.
    let learnings = rt
        .learn_from_history("run.p5e2e", &session.id.to_string())
        .await
        .unwrap();
    assert!(
        !learnings.is_empty(),
        "a real completed run must derive learnings"
    );
    let improve = rt.load_improve().await.unwrap();
    for r in &learnings {
        assert!(improve.get_learning(&r.id).is_ok());
    }

    // Metrics ride the Phase-1 bus.
    use openrt_telemetry::ChannelSink;
    let sink = Arc::new(ChannelSink::new());
    let rt2 = RuntimeBuilder::new()
        .config(openrt_config::Config::default())
        .store(store.clone())
        .with_provider(Arc::new(openrt_models::MockProvider::tool_sequence("mock")))
        .sink(sink.clone())
        .bus_capacity(32)
        .build()
        .await
        .unwrap();
    rt2.metrics_record("task.duration_ms", 42.0, "ms")
        .await
        .unwrap();
    let sent = rt2.metrics_emit().await.unwrap();
    assert_eq!(sent, 1);
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert!(
        sink.snapshot()
            .iter()
            .any(|e| e.kind == openrt_telemetry::EventKind::MetricRecorded),
        "MetricRecorded must land on the bus"
    );
}

/// M5.8 durable approval through the *production* Runtime: `with_runtime`
/// palette dispatch → `improve_approve_with_note` → shared MemoryStore →
/// a fresh `load_improve` still sees `Approved` + the operator note.
/// Runs outside the async context (palette uses `Handle::block_on`).
#[test]
fn p5_webui_durable_approval_persists_via_runtime() {
    use openrt_runtime::RuntimeBuilder;
    use openrt_webui::{DispatchRequest, PaletteOutcome};

    let outer = tokio::runtime::Runtime::new().expect("outer runtime");
    let store = Arc::new(MemoryStore::new());
    let rt = outer.block_on(async {
        let rt = RuntimeBuilder::new()
            .config(openrt_config::Config::default())
            .store(store.clone())
            .with_provider(Arc::new(openrt_models::MockProvider::tool_sequence("mock")))
            .bus_capacity(32)
            .build()
            .await
            .unwrap();
        rt.improve_record_baseline(
            "baseline.p5ui",
            "latency_ms",
            100.0,
            MetricDirection::LowerIsBetter,
            "run.p5ui",
        )
        .await
        .unwrap();
        rt.improve_propose(
            "prop.p5ui",
            ProposalKind::SelfOptimize,
            "durable UI approval path",
            TargetKind::Router,
            "router.main",
            Some("baseline.p5ui".into()),
            Some(75.0),
        )
        .await
        .unwrap();
        rt.improve_evaluate("prop.p5ui").await.unwrap();
        rt
    });
    let improve = outer.block_on(rt.load_improve()).unwrap();
    assert_eq!(
        improve.get_proposal("prop.p5ui").unwrap().state,
        ProposalState::EvalPassed
    );

    let mut palette = CommandPalette::with_runtime(improve, rt.clone(), outer.handle().clone());
    let op = std::env::var("USER").unwrap_or_else(|_| "openrt".into());
    let token = palette.issue_token(&op);
    let out = palette
        .dispatch(
            &DispatchRequest::new("improve.approve")
                .with_arg("proposal_id", "prop.p5ui")
                .with_arg("note", "durable ok")
                .with_auth_token(&token),
        )
        .expect("durable approve dispatch");
    match out {
        PaletteOutcome::Completed { id, detail } => {
            assert_eq!(id, "improve.approve");
            assert!(detail.contains("\"durable\":true"), "detail={detail}");
            assert!(detail.contains("\"state\":\"approved\""), "detail={detail}");
        }
        other => panic!("expected durable Completed, got {other:?}"),
    }

    // Reload from the shared store (not the palette-local clone).
    let reloaded = outer.block_on(rt.load_improve()).unwrap();
    let p = reloaded.get_proposal("prop.p5ui").unwrap();
    assert_eq!(p.state, ProposalState::Approved, "state must be durable");
    assert_eq!(
        p.operator_decision.as_deref(),
        Some("durable ok"),
        "operator note must be durable"
    );
}
