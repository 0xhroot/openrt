use std::cell::Cell;

use openrt_persistence::{MemoryStore, PersistenceStore};
use openrt_selfimprove::{
    ApplyTarget, Baseline, DebugReport, EvalResult, ImproveError, ImproveStore,
    ImprovementProposal, LearningEngine, LearningKind, MetricDirection, OperatorGrant,
    ProposalKind, ProposalState, Provenance, ResearchNote, RunSummary, TargetKind,
};
use openrt_workspace_assets::{AssetEnvelope, CanonicalForm};

fn prov() -> Provenance {
    Provenance::new("run.test-1", "src.test")
}

fn grant() -> OperatorGrant {
    OperatorGrant::new("alice").unwrap()
}

fn store(sovereign: bool) -> ImproveStore {
    ImproveStore::new(&["alice".to_string()], sovereign)
}

fn baseline(id: &str, value: f64, direction: MetricDirection) -> Baseline {
    Baseline::new(id, "latency_ms", value, direction, "run.base-1", prov()).unwrap()
}

fn optimize_proposal(
    id: &str,
    baseline_id: &str,
    expected: f64,
    target_kind: TargetKind,
    target_id: &str,
) -> ImprovementProposal {
    ImprovementProposal::new(
        id,
        ProposalKind::SelfOptimize,
        "route small prompts to the faster model",
        target_kind,
        target_id,
        Some(baseline_id.to_string()),
        Some(expected),
        None,
        prov(),
    )
    .unwrap()
}

struct FakeTarget {
    kind: TargetKind,
    id: String,
    called: Cell<bool>,
}

impl FakeTarget {
    fn new(kind: TargetKind, id: &str) -> Self {
        Self {
            kind,
            id: id.to_string(),
            called: Cell::new(false),
        }
    }
}

impl ApplyTarget for FakeTarget {
    fn kind(&self) -> TargetKind {
        self.kind
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn apply(&self, _proposal: &ImprovementProposal) -> Result<(), ImproveError> {
        self.called.set(true);
        Ok(())
    }
}

// ---------- M5.6 positive: full approval-gated pipeline ----------

#[test]
fn m56_full_pipeline_eval_approve_apply() {
    let mut s = store(false);
    s.record_baseline(baseline(
        "baseline.a",
        100.0,
        MetricDirection::LowerIsBetter,
    ))
    .unwrap();
    let p = optimize_proposal(
        "prop.a",
        "baseline.a",
        80.0,
        TargetKind::Router,
        "router.main",
    );
    s.propose(p).unwrap();
    assert_eq!(
        s.get_proposal("prop.a").unwrap().state,
        ProposalState::Proposed
    );
    let eval = s.evaluate("prop.a").unwrap();
    match eval {
        EvalResult::Compared(outcome) => {
            assert!(outcome.beats_baseline);
            assert_eq!(outcome.baseline_value, 100.0);
            assert_eq!(outcome.candidate_value, 80.0);
        }
        EvalResult::NoBaseline => panic!("optimization must be compared to its baseline"),
    }
    assert_eq!(
        s.get_proposal("prop.a").unwrap().state,
        ProposalState::EvalPassed
    );
    s.approve("prop.a", Some(&grant())).unwrap();
    assert_eq!(
        s.get_proposal("prop.a").unwrap().state,
        ProposalState::Approved
    );
    let target = FakeTarget::new(TargetKind::Router, "router.main");
    let applied = s.apply("prop.a", &target, Some(&grant())).unwrap();
    assert!(target.called.get(), "approved apply must reach the target");
    assert_eq!(applied.proposal_id, "prop.a");
    assert_eq!(applied.target_id, "router.main");
    assert_eq!(
        s.get_proposal("prop.a").unwrap().state,
        ProposalState::Applied
    );
}

#[test]
fn m56_regression_that_does_not_beat_baseline_is_rejected() {
    let mut s = store(false);
    s.record_baseline(baseline(
        "baseline.a",
        100.0,
        MetricDirection::LowerIsBetter,
    ))
    .unwrap();
    let p = optimize_proposal(
        "prop.a",
        "baseline.a",
        150.0,
        TargetKind::Router,
        "router.main",
    );
    s.propose(p).unwrap();
    let err = s.evaluate("prop.a").unwrap_err();
    match err {
        ImproveError::RegressionDetected {
            baseline,
            candidate,
            ..
        } => {
            assert_eq!(baseline, 100.0);
            assert_eq!(candidate, 150.0);
        }
        other => panic!("expected RegressionDetected, got {other:?}"),
    }
    assert_eq!(
        s.get_proposal("prop.a").unwrap().state,
        ProposalState::Rejected,
        "non-improving optimization must end rejected"
    );
    let err2 = s.approve("prop.a", Some(&grant())).unwrap_err();
    assert!(matches!(err2, ImproveError::InvalidState { .. }));
    let target = FakeTarget::new(TargetKind::Router, "router.main");
    let err3 = s.apply("prop.a", &target, Some(&grant())).unwrap_err();
    assert!(matches!(err3, ImproveError::InvalidState { .. }));
    assert!(!target.called.get(), "rejected proposal must never apply");
}

#[test]
fn m56_higher_is_better_direction() {
    let mut s = store(false);
    let b = Baseline::new(
        "baseline.acc",
        "accuracy",
        0.80,
        MetricDirection::HigherIsBetter,
        "run.base-2",
        prov(),
    )
    .unwrap();
    s.record_baseline(b).unwrap();
    let p = ImprovementProposal::new(
        "prop.acc",
        ProposalKind::SelfOptimize,
        "swap retrieval threshold",
        TargetKind::Plan,
        "plan.rag",
        Some("baseline.acc".into()),
        Some(0.85),
        None,
        prov(),
    )
    .unwrap();
    s.propose(p).unwrap();
    let eval = s.evaluate("prop.acc").unwrap();
    assert!(matches!(eval, EvalResult::Compared(_)));
    s.approve("prop.acc", Some(&grant())).unwrap();
    let target = FakeTarget::new(TargetKind::Plan, "plan.rag");
    s.apply("prop.acc", &target, Some(&grant())).unwrap();
    assert!(target.called.get());
}

#[test]
fn m56_research_review_loop() {
    let mut s = store(false);
    let note = ResearchNote::new(
        "research.router",
        "why do short prompts overshoot the latency budget",
        vec![
            "overshoot correlates with 7b model on one-token echoes".into(),
            "70b adds 40ms p50 on echo class".into(),
        ],
        Some("prop.res".into()),
        prov(),
    )
    .unwrap();
    s.record_research(note).unwrap();
    s.record_baseline(baseline(
        "baseline.b",
        100.0,
        MetricDirection::LowerIsBetter,
    ))
    .unwrap();
    let mut p = optimize_proposal(
        "prop.res",
        "baseline.b",
        70.0,
        TargetKind::Router,
        "router.main",
    );
    p.research_id = Some("research.router".into());
    let digest_before = p.digest();
    s.propose(p).unwrap();
    assert_eq!(
        s.get_proposal("prop.res").unwrap().digest(),
        digest_before,
        "research link must be part of the sealed identity"
    );
    s.evaluate("prop.res").unwrap();
    s.approve("prop.res", Some(&grant()))
        .unwrap_or_else(|e| panic!("operator review gate failed: {e}"));
    let target = FakeTarget::new(TargetKind::Router, "router.main");
    s.apply("prop.res", &target, Some(&grant())).unwrap();
    assert_eq!(
        s.get_research("research.router").unwrap().proposal_id,
        Some("prop.res".into())
    );
    assert!(target.called.get());
}

#[test]
fn m56_self_debug_loop_without_baseline() {
    let mut s = store(false);
    let report = DebugReport::new(
        "debug.run1",
        "run.1",
        "sig.timeout-8f3a",
        vec![
            "replay run.1 step 3 with fixture input".into(),
            "observe tool timeout at step 3".into(),
        ],
        "tool fs.read retried on a deleted path",
        Some("prop.debug".into()),
        prov(),
    )
    .unwrap();
    s.record_debug(report).unwrap();
    let p = ImprovementProposal::new(
        "prop.debug",
        ProposalKind::SelfDebug,
        "skip deleted paths during replay",
        TargetKind::Tool,
        "fs.read",
        None,
        None,
        Some("debug.run1".into()),
        prov(),
    )
    .unwrap();
    s.propose(p).unwrap();
    let eval = s.evaluate("prop.debug").unwrap();
    assert_eq!(eval, EvalResult::NoBaseline);
    s.approve("prop.debug", Some(&grant())).unwrap();
    let target = FakeTarget::new(TargetKind::Tool, "fs.read");
    s.apply("prop.debug", &target, Some(&grant())).unwrap();
    assert!(target.called.get());
    assert_eq!(
        s.get_debug("debug.run1").unwrap().proposal_id,
        Some("prop.debug".into())
    );
}

// ---------- M5.6 negative / approval gate / protected targets ----------

#[test]
fn m56_neg_approve_without_grant_is_refused() {
    let mut s = store(false);
    s.record_baseline(baseline(
        "baseline.a",
        100.0,
        MetricDirection::LowerIsBetter,
    ))
    .unwrap();
    s.propose(optimize_proposal(
        "prop.a",
        "baseline.a",
        80.0,
        TargetKind::Router,
        "router.main",
    ))
    .unwrap();
    s.evaluate("prop.a").unwrap();
    let err = s.approve("prop.a", None).unwrap_err();
    assert!(matches!(err, ImproveError::UnauthorizedApproval { .. }));
    assert_eq!(
        s.get_proposal("prop.a").unwrap().state,
        ProposalState::EvalPassed,
        "headless approve without grant must not advance state"
    );
}

#[test]
fn m56_neg_approve_with_unknown_operator_is_refused() {
    let mut s = store(false);
    s.record_baseline(baseline(
        "baseline.a",
        100.0,
        MetricDirection::LowerIsBetter,
    ))
    .unwrap();
    s.propose(optimize_proposal(
        "prop.a",
        "baseline.a",
        80.0,
        TargetKind::Router,
        "router.main",
    ))
    .unwrap();
    s.evaluate("prop.a").unwrap();
    let mallory = OperatorGrant::new("mallory").unwrap();
    let err = s.approve("prop.a", Some(&mallory)).unwrap_err();
    assert!(matches!(err, ImproveError::UnauthorizedOperator { .. }));
}

#[test]
fn m56_neg_apply_before_approval_is_refused() {
    let mut s = store(false);
    s.record_baseline(baseline(
        "baseline.a",
        100.0,
        MetricDirection::LowerIsBetter,
    ))
    .unwrap();
    s.propose(optimize_proposal(
        "prop.a",
        "baseline.a",
        80.0,
        TargetKind::Router,
        "router.main",
    ))
    .unwrap();
    s.evaluate("prop.a").unwrap();
    let target = FakeTarget::new(TargetKind::Router, "router.main");
    let err = s.apply("prop.a", &target, Some(&grant())).unwrap_err();
    assert!(matches!(err, ImproveError::NotApproved { .. }));
    assert!(!target.called.get());
}

#[test]
fn m56_neg_approve_before_evaluation_is_refused() {
    let mut s = store(false);
    s.record_baseline(baseline(
        "baseline.a",
        100.0,
        MetricDirection::LowerIsBetter,
    ))
    .unwrap();
    s.propose(optimize_proposal(
        "prop.a",
        "baseline.a",
        80.0,
        TargetKind::Router,
        "router.main",
    ))
    .unwrap();
    let err = s.approve("prop.a", Some(&grant())).unwrap_err();
    assert!(matches!(err, ImproveError::EvalNotRun { .. }));
}

#[test]
fn m56_neg_double_evaluate_is_refused() {
    let mut s = store(false);
    s.record_baseline(baseline(
        "baseline.a",
        100.0,
        MetricDirection::LowerIsBetter,
    ))
    .unwrap();
    s.propose(optimize_proposal(
        "prop.a",
        "baseline.a",
        80.0,
        TargetKind::Router,
        "router.main",
    ))
    .unwrap();
    s.evaluate("prop.a").unwrap();
    let err = s.evaluate("prop.a").unwrap_err();
    assert!(matches!(err, ImproveError::InvalidState { .. }));
}

#[test]
fn m56_neg_self_improvement_cannot_modify_protected_targets() {
    for kind in [
        TargetKind::Policy,
        TargetKind::PermissionPolicy,
        TargetKind::SecurityScan,
        TargetKind::Verification,
    ] {
        let mut s = store(false);
        s.record_baseline(baseline(
            "baseline.a",
            100.0,
            MetricDirection::LowerIsBetter,
        ))
        .unwrap();
        let target_id = format!("target.{}", kind.as_str());
        s.propose(optimize_proposal(
            "prop.protected",
            "baseline.a",
            80.0,
            kind,
            &target_id,
        ))
        .unwrap();
        s.evaluate("prop.protected").unwrap();
        s.approve("prop.protected", Some(&grant())).unwrap();
        let target = FakeTarget::new(kind, &target_id);
        let err = s
            .apply("prop.protected", &target, Some(&grant()))
            .unwrap_err();
        assert!(
            matches!(err, ImproveError::ProtectedTarget { .. }),
            "kind {:?} must be operator-only even with a valid grant, got {err:?}",
            kind
        );
        assert!(!target.called.get());
    }
}

#[test]
fn m56_neg_apply_target_mismatch_is_refused() {
    let mut s = store(false);
    s.record_baseline(baseline(
        "baseline.a",
        100.0,
        MetricDirection::LowerIsBetter,
    ))
    .unwrap();
    s.propose(optimize_proposal(
        "prop.a",
        "baseline.a",
        80.0,
        TargetKind::Router,
        "router.main",
    ))
    .unwrap();
    s.evaluate("prop.a").unwrap();
    s.approve("prop.a", Some(&grant())).unwrap();
    let wrong = FakeTarget::new(TargetKind::Router, "router.other");
    let err = s.apply("prop.a", &wrong, Some(&grant())).unwrap_err();
    assert!(matches!(err, ImproveError::Malformed { .. }));
    assert!(!wrong.called.get());
}

#[test]
fn m56_neg_sovereign_mode_blocks_network_targets() {
    let mut s = store(true);
    let p = ImprovementProposal::new(
        "prop.net",
        ProposalKind::ResearchChange,
        "try hosted model fallback",
        TargetKind::Router,
        "network.egress",
        None,
        None,
        None,
        prov(),
    )
    .unwrap();
    let err = s.propose(p).unwrap_err();
    assert!(matches!(err, ImproveError::SovereignViolation { .. }));
}

#[test]
fn m56_neg_self_optimize_requires_baseline_at_construction() {
    let err = ImprovementProposal::new(
        "prop.bad",
        ProposalKind::SelfOptimize,
        "no baseline recorded",
        TargetKind::Router,
        "router.main",
        None,
        None,
        None,
        prov(),
    )
    .unwrap_err();
    assert!(matches!(err, ImproveError::Malformed { .. }));
}

#[test]
fn m56_neg_invalid_provenance_is_refused() {
    let err = ImprovementProposal::new(
        "prop.bad",
        ProposalKind::ResearchChange,
        "summary",
        TargetKind::Tool,
        "fs.read",
        None,
        None,
        None,
        Provenance::new("", "src"),
    )
    .unwrap_err();
    assert!(matches!(err, ImproveError::InvalidProvenance));
    let p = ImprovementProposal::new(
        "prop.ok",
        ProposalKind::ResearchChange,
        "summary",
        TargetKind::Tool,
        "fs.read",
        None,
        None,
        None,
        Provenance::new("", "src"),
    );
    assert!(p.is_err());
}

#[test]
fn m56_neg_tampered_proposal_envelope_is_rejected() {
    let p = optimize_raw();
    let env = AssetEnvelope::seal(p);
    env.verify().unwrap();
    let mut tampered = env.clone();
    tampered.payload.state = ProposalState::Approved;
    let err = tampered.verify().unwrap_err();
    assert!(matches!(
        err,
        openrt_workspace_assets::AssetError::DigestMismatch { .. }
    ));
    let mut tampered_summary = env.clone();
    tampered_summary.payload.summary = "silently approved".into();
    assert!(matches!(
        tampered_summary.verify().unwrap_err(),
        openrt_workspace_assets::AssetError::DigestMismatch { .. }
    ));
    let _ = CanonicalForm::asset_kind(&env.payload);
}

fn optimize_raw() -> ImprovementProposal {
    ImprovementProposal::new(
        "prop.raw",
        ProposalKind::ResearchChange,
        "tweak plan step order",
        TargetKind::Plan,
        "plan.orch",
        None,
        None,
        None,
        prov(),
    )
    .unwrap()
}

// ---------- M5.7 learning from execution history ----------

fn successful_summary() -> RunSummary {
    RunSummary::new("run.hist-1")
        .with_success(true)
        .with_tool("fs.read", 5)
        .with_tool("fs.list", 2)
        .with_tool("tool.verify", 1)
}

#[test]
fn m57_learning_derivation_is_deterministic_and_local() {
    let a = LearningEngine::derive(&successful_summary(), prov()).unwrap();
    let b = LearningEngine::derive(&successful_summary(), prov()).unwrap();
    assert_eq!(a.len(), b.len());
    assert!(
        !a.is_empty(),
        "successful multi-tool run must derive learnings"
    );
    for (x, y) in a.iter().zip(b.iter()) {
        assert_eq!(x.id, y.id, "same history must derive identical ids");
        assert_eq!(x.digest(), y.digest());
    }
    let kinds: Vec<LearningKind> = a.iter().map(|r| r.kind).collect();
    assert!(
        kinds.contains(&LearningKind::WorkflowPattern),
        "successful run with >=4 tool calls derives a workflow pattern"
    );
    assert!(
        kinds.contains(&LearningKind::InstinctCandidate),
        "dominant tool used >=3 times derives an instinct candidate"
    );
}

#[test]
fn m57_failed_run_derives_failure_learning() {
    let summary = RunSummary::new("run.hist-2")
        .with_success(false)
        .with_tool("fs.read", 3)
        .with_failure_signature("sig.timeout-8f3a");
    let records = LearningEngine::derive(&summary, prov()).unwrap();
    assert!(
        records
            .iter()
            .any(|r| r.kind == LearningKind::SkillSuggestion
                && r.summary.contains("sig.timeout-8f3a")),
        "failure signature must be derived into a skill suggestion"
    );
    assert!(
        !records
            .iter()
            .any(|r| r.kind == LearningKind::WorkflowPattern),
        "failed run must not derive a success workflow pattern"
    );
}

#[test]
fn m57_store_derivation_is_idempotent() {
    let mut s = store(false);
    let first = s.derive_learnings(&successful_summary(), prov()).unwrap();
    let second = s.derive_learnings(&successful_summary(), prov()).unwrap();
    assert_eq!(first.len(), second.len());
    for rec in &first {
        let loaded = s.get_learning(&rec.id).unwrap();
        assert_eq!(loaded.digest(), rec.digest());
    }
}

#[test]
fn m57_neg_learning_invalid_provenance() {
    let err = LearningEngine::derive(&successful_summary(), Provenance::new("", "")).unwrap_err();
    assert!(matches!(err, ImproveError::InvalidProvenance));
}

/// M10.7: every derived learning record is evidence-backed — a non-empty
/// run-summary hash survives into the record (provenance that outlives
/// review; no unevidenced learnings).
#[test]
fn m10_learning_records_are_evidence_backed() {
    let cases = vec![
        LearningEngine::derive(&successful_summary(), prov()).unwrap(),
        LearningEngine::derive(
            &RunSummary::new("run.m10-fail")
                .with_success(false)
                .with_tool("fs.read", 2)
                .with_failure_signature("sig.m10"),
            prov(),
        )
        .unwrap(),
    ];
    let mut seen = 0;
    for records in cases {
        assert!(!records.is_empty());
        for rec in &records {
            assert!(
                !rec.evidence.is_empty(),
                "record {} derived without evidence",
                rec.id
            );
            assert!(rec.evidence.iter().all(|e| !e.trim().is_empty()));
            seen += 1;
        }
    }
    assert!(seen >= 2);
}

// ---------- persistence ----------

#[tokio::test]
async fn m56_persist_reopen_verify() {
    let mut s = store(false);
    s.record_baseline(baseline(
        "baseline.a",
        100.0,
        MetricDirection::LowerIsBetter,
    ))
    .unwrap();
    s.propose(optimize_proposal(
        "prop.a",
        "baseline.a",
        80.0,
        TargetKind::Router,
        "router.main",
    ))
    .unwrap();
    s.evaluate("prop.a").unwrap();
    s.approve("prop.a", Some(&grant())).unwrap();
    let target = FakeTarget::new(TargetKind::Router, "router.main");
    s.apply("prop.a", &target, Some(&grant())).unwrap();
    s.derive_learnings(&successful_summary(), prov()).unwrap();
    let mem = MemoryStore::default();
    s.save(&mem).await.unwrap();
    let loaded = ImproveStore::load(&mem).await.unwrap();
    assert_eq!(loaded.meta(), s.meta());
    assert_eq!(
        loaded.get_proposal("prop.a").unwrap().state,
        ProposalState::Applied
    );
    assert_eq!(
        loaded.get_proposal("prop.a").unwrap().digest(),
        s.get_proposal("prop.a").unwrap().digest()
    );
    assert_eq!(loaded.get_baseline("baseline.a").unwrap().value, 100.0);
    assert_eq!(
        loaded
            .get_learning(&s.derive_learnings(&successful_summary(), prov()).unwrap()[0].id)
            .unwrap()
            .digest(),
        s.derive_learnings(&successful_summary(), prov()).unwrap()[0].digest()
    );
}

#[tokio::test]
async fn m56_neg_persist_tamper_reopen_rejects() {
    let mut s = store(false);
    s.record_baseline(baseline(
        "baseline.a",
        100.0,
        MetricDirection::LowerIsBetter,
    ))
    .unwrap();
    s.propose(optimize_proposal(
        "prop.a",
        "baseline.a",
        80.0,
        TargetKind::Router,
        "router.main",
    ))
    .unwrap();
    let mem = MemoryStore::default();
    s.save(&mem).await.unwrap();
    let record = mem
        .get_record(openrt_selfimprove::PROPOSALS_NS, "prop.a")
        .await
        .unwrap()
        .unwrap();
    let mut tampered = record.clone();
    tampered["payload"]["state"] = serde_json::Value::String("applied".into());
    mem.put_record(openrt_selfimprove::PROPOSALS_NS, "prop.a", &tampered)
        .await
        .unwrap();
    let err = ImproveStore::load(&mem).await.unwrap_err();
    assert!(matches!(err, ImproveError::DigestMismatch { .. }));
}
