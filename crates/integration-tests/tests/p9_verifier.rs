//! Phase 9 (M9.8 / TEST 8) — the hallucination firewall at a production
//! answer path (`Runtime::verify_claim` / `Runtime::verify_claims`):
//! * an unsupported claim is BLOCKED before any model can bless it
//!   (deterministic evidence gate runs first and never spends a model call);
//! * a supported claim PASSES — deterministically, or on a real/canned model
//!   verdict, with the basis recorded honestly;
//! * model verdicts may only DEMOTE (a model cannot lift an evidence gap);
//! * cross-model `INDEPENDENT` requires two genuinely distinct models — the
//!   same model twice is reported as a degradation;
//! * limits (time, evidence size, batch size) are enforced and recorded;
//! * every decision is persisted under `verify.verdicts` first.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use openrt_knowledge::{persist_chunk, Citation, PersistedChunk};
use openrt_models::{
    MockProvider, ModelCapabilities, ModelError, ModelInfo, ModelProvider, ModelRequest,
    ModelResponse, ProviderKind,
};
use openrt_persistence::MemoryStore;
use openrt_runtime::RuntimeBuilder;
use openrt_verifier::{
    list_verdicts, FirewallAction, Independence, Verdict, VerifyInput, VerifyLimits, VerifyOptions,
    VERDICTS_NS,
};

const KB: &str = "kb-v8";
const TEXT: &str = "the launch window is dawn";
const ENDPOINT: &str = "http://127.0.0.1:11434";

fn citation() -> Citation {
    Citation {
        kb_id: KB.into(),
        document_id: "d1".into(),
        document_version: 1,
        chunk_id: "c1".into(),
        block_keys: vec!["b/c1".into()],
        page: Some(1),
        text_sha256: openrt_core::sha256_hex(TEXT.as_bytes()),
    }
}

fn input(claim: &str) -> VerifyInput {
    VerifyInput {
        kb_id: KB.into(),
        claim: claim.into(),
        citations: vec![citation()],
        evidence_texts: vec![TEXT.into()],
    }
}

/// Build the production runtime (with its real store) and persist the one
/// grounded chunk the fixtures cite.
async fn runtime_with_evidence() -> Arc<openrt_runtime::Runtime> {
    let rt = RuntimeBuilder::new()
        .store(Arc::new(MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::echo("mock")))
        .build()
        .await
        .unwrap();
    let chunk = PersistedChunk {
        chunk_id: "c1".into(),
        document_id: "d1".into(),
        document_version: 1,
        block_keys: vec!["b/c1".into()],
        page: Some(1),
        text_sha256: openrt_core::sha256_hex(TEXT.as_bytes()),
        text: Some(TEXT.into()),
    };
    persist_chunk(&*rt.store, KB, &chunk).await.unwrap();
    rt
}

/// Test-only canned backend (same discipline as the rerank fixtures): a
/// fixed strict-JSON body per call — never a production path, and never an
/// echo of the claim.
struct Canned {
    id: String,
    body: String,
    calls: std::sync::Mutex<usize>,
}

impl Canned {
    fn new(id: &str, body: &str) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            body: body.into(),
            calls: std::sync::Mutex::new(0),
        })
    }
    fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

#[async_trait]
impl ModelProvider for Canned {
    fn id(&self) -> &str {
        &self.id
    }
    fn info(&self) -> ModelInfo {
        ModelInfo::new(self.id.clone(), ProviderKind::Mock, None)
    }
    async fn generate(&self, _req: ModelRequest) -> Result<ModelResponse, ModelError> {
        *self.calls.lock().unwrap() += 1;
        Ok(ModelResponse::with_text("canned", self.body.clone()))
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            supports_tools: false,
            supports_streaming: false,
            context_window: None,
        }
    }
}

const SUPPORTED: &str = r#"{"verdict":"SUPPORTED","reason":"claim is in the evidence"}"#;
const UNSUPPORTED: &str = r#"{"verdict":"UNSUPPORTED","reason":"evidence does not say this"}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_claim_is_blocked_at_the_production_answer_path() {
    let rt = runtime_with_evidence().await;
    let report = rt
        .verify_claim(input("the moon is made of cheese"), &rt.verify_options())
        .await
        .unwrap();

    assert_eq!(report.decision, FirewallAction::Block, "{report:?}");
    assert_eq!(report.verdict, Verdict::Unsupported);
    assert_eq!(report.basis, openrt_verifier::Basis::Deterministic);
    assert!(
        report.model_verdicts.is_empty(),
        "the deterministic gate must block BEFORE any model is consulted: {report:?}"
    );
    assert!(matches!(
        &report.independence,
        Independence::Degraded { reason } if reason.contains("deterministic evidence gate")
    ));

    // The decision is persisted under `verify.verdicts` (fail closed: the
    // report only exists because its record round-tripped).
    let raw = rt.store.list_records(VERDICTS_NS).await.unwrap();
    assert_eq!(raw.len(), 1, "verdict must live in {VERDICTS_NS}");
    let records = list_verdicts(&*rt.store, KB).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].decision, FirewallAction::Block);
    assert_eq!(records[0].verdict, Verdict::Unsupported);
    assert_eq!(records[0].claim, "the moon is made of cheese");
    assert_eq!(records[0].verdict_id, report.verdict_id);
    assert!(records[0].evidence_refs.iter().all(|r| r.resolved));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supported_claim_passes_at_the_production_answer_path() {
    let rt = runtime_with_evidence().await;

    // Deterministic-only mode: fully grounded in resolving evidence, no
    // model configured — passes WITH an honest degradation, never as a
    // fabricated model verdict.
    let det = rt
        .verify_claim(input(TEXT), &VerifyOptions::default())
        .await
        .unwrap();
    assert_eq!(det.decision, FirewallAction::Pass, "{det:?}");
    assert_eq!(det.verdict, Verdict::Supported);
    assert_eq!(det.basis, openrt_verifier::Basis::Deterministic);
    assert!(matches!(
        &det.independence,
        Independence::Degraded { reason } if reason.contains("no model verifier")
    ));

    // With a real (canned, test-only) model backend: model-backed pass.
    let canned = Canned::new("p1", SUPPORTED);
    let opts = VerifyOptions::default().with_backend(canned.clone(), "model-a");
    let mod_report = rt.verify_claim(input(TEXT), &opts).await.unwrap();
    assert_eq!(mod_report.decision, FirewallAction::Pass, "{mod_report:?}");
    assert_eq!(mod_report.verdict, Verdict::Supported);
    assert_eq!(mod_report.basis, openrt_verifier::Basis::Model);
    assert_eq!(mod_report.model_verdicts.len(), 1);
    assert_eq!(canned.calls(), 1);

    // Both decisions are on the record.
    let records = list_verdicts(&*rt.store, KB).await.unwrap();
    assert_eq!(records.len(), 2);
    assert!(records.iter().all(|r| r.decision == FirewallAction::Pass));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_verdict_can_only_demote_never_lift() {
    let rt = runtime_with_evidence().await;

    // Grounded claim + a model that contradicts it → blocked.
    let uns = Canned::new("p1", UNSUPPORTED);
    let opts = VerifyOptions::default().with_backend(uns.clone(), "model-a");
    let report = rt.verify_claim(input(TEXT), &opts).await.unwrap();
    assert_eq!(report.decision, FirewallAction::Block, "{report:?}");
    assert_eq!(report.verdict, Verdict::Unsupported);
    assert_eq!(report.basis, openrt_verifier::Basis::Model);

    // Evidence gap + a model that claims support → still flagged (a model
    // cannot conjure evidence that is not there).
    let gap = "the dawn launch report was delayed by weather";
    let sup = Canned::new("p2", SUPPORTED);
    let opts = VerifyOptions::default().with_backend(sup.clone(), "model-b");
    let lifted = rt.verify_claim(input(gap), &opts).await.unwrap();
    assert_eq!(lifted.decision, FirewallAction::Flag, "{lifted:?}");
    assert_eq!(lifted.verdict, Verdict::Insufficient);
    assert_eq!(lifted.basis, openrt_verifier::Basis::Deterministic);
    assert_eq!(sup.calls(), 1, "the model did run — it just cannot lift");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_model_independence_is_reported_honestly() {
    let rt = runtime_with_evidence().await;

    // The SAME model id through two providers: deduplicated, never
    // independent (anti-pattern: same-model-twice-as-cross-model).
    let a = Canned::new("p1", SUPPORTED);
    let b = Canned::new("p2", SUPPORTED);
    let opts = VerifyOptions::default()
        .with_backend(a.clone(), "shared-model")
        .with_backend(b.clone(), "shared-model");
    let same = rt.verify_claim(input(TEXT), &opts).await.unwrap();
    assert_eq!(a.calls(), 1, "first backend runs");
    assert_eq!(b.calls(), 0, "the same model must never run twice");
    assert_eq!(same.model_verdicts.len(), 1);
    assert!(matches!(
        &same.independence,
        Independence::Degraded { reason } if reason.contains("only one distinct model")
    ));
    assert!(!same.independence.is_independent());

    // Two genuinely distinct models that agree → INDEPENDENT.
    let c = Canned::new("p1", SUPPORTED);
    let d = Canned::new("p2", SUPPORTED);
    let opts = VerifyOptions::default()
        .with_backend(c, "model-a")
        .with_backend(d, "model-b");
    let indep = rt.verify_claim(input(TEXT), &opts).await.unwrap();
    assert_eq!(indep.decision, FirewallAction::Pass, "{indep:?}");
    match &indep.independence {
        Independence::Independent { backends } => {
            assert_eq!(backends.len(), 2, "{backends:?}");
            assert!(backends.iter().any(|l| l.ends_with("/model-a")));
            assert!(backends.iter().any(|l| l.ends_with("/model-b")));
        }
        other => panic!("expected Independent, got {other:?}"),
    }
    assert_eq!(indep.basis, openrt_verifier::Basis::CrossModel);

    // Disagreement stays inconclusive — never averaged into a consensus.
    let e = Canned::new("p1", SUPPORTED);
    let f = Canned::new("p2", UNSUPPORTED);
    let opts = VerifyOptions::default()
        .with_backend(e, "model-a")
        .with_backend(f, "model-b");
    let disagree = rt.verify_claim(input(TEXT), &opts).await.unwrap();
    assert_eq!(disagree.decision, FirewallAction::Flag, "{disagree:?}");
    assert_eq!(disagree.verdict, Verdict::Insufficient);
    assert!(disagree.independence.is_independent());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn limits_are_enforced_at_the_production_answer_path() {
    let rt = runtime_with_evidence().await;

    // Batch cap: an over-limit batch is an ERROR, never a silent truncation.
    let capped = VerifyOptions {
        limits: VerifyLimits {
            max_claims: 2,
            ..VerifyLimits::default()
        },
        backends: Vec::new(),
    };
    let err = rt
        .verify_claims(vec![input(TEXT); 3], &capped)
        .await
        .unwrap_err();
    assert!(
        matches!(err, openrt_runtime::Error::Verifier(_)),
        "expected a structured limit error: {err:?}"
    );
    let ok = rt
        .verify_claims(vec![input(TEXT); 2], &capped)
        .await
        .unwrap();
    assert_eq!(ok.len(), 2);

    // Evidence size cap: applied before anything reads the evidence, and
    // recorded on the report AND the persisted verdict.
    let size_capped = VerifyOptions {
        limits: VerifyLimits {
            max_evidence_chars: 32,
            ..VerifyLimits::default()
        },
        backends: Vec::new(),
    };
    let mut big = input(TEXT);
    big.evidence_texts = vec![format!("{TEXT} {}", "filler ".repeat(400))];
    let clipped = rt.verify_claim(big, &size_capped).await.unwrap();
    assert!(clipped.limits.evidence_chars_used <= 32, "{clipped:?}");
    assert!(clipped.deterministic.evidence_chars <= 32);
    assert!(clipped
        .reasons
        .iter()
        .any(|r| r.contains("evidence clipped")));

    // Time cap: a slow backend is cut off at the deadline — bounded wall
    // time, a structured failure, and an honest degradation on the record.
    let slow = MockProvider::new("slow", openrt_models::mock::MockMode::Slow)
        .with_latency(Duration::from_secs(60));
    let timed = VerifyOptions {
        limits: VerifyLimits {
            timeout: Duration::from_millis(100),
            ..VerifyLimits::default()
        },
        backends: Vec::new(),
    }
    .with_backend(Arc::new(slow), "mock://default");
    let start = Instant::now();
    let report = rt.verify_claim(input(TEXT), &timed).await.unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "time cap not enforced: {:?}",
        start.elapsed()
    );
    assert_eq!(report.model_failures.len(), 1);
    assert!(report.model_failures[0].error.contains("timed out"));
    assert!(matches!(
        &report.independence,
        Independence::Degraded { reason } if reason.contains("no model verdict produced")
    ));
    // The deterministic floor still decided; nothing was invented.
    assert_eq!(report.decision, FirewallAction::Pass, "{report:?}");
    assert_eq!(report.basis, openrt_verifier::Basis::Deterministic);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_cross_model_verification_reports_independence_honestly() {
    let rt = runtime_with_evidence().await;

    let p1 = match openrt_models::ollama::OllamaProvider::new(
        "verify-a",
        ENDPOINT,
        Some("qwen2.5:0.5b".to_string()),
    ) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            eprintln!("SKIP: cannot construct Ollama verifier provider A: {e}");
            return;
        }
    };
    let p2 = match openrt_models::ollama::OllamaProvider::new(
        "verify-b",
        ENDPOINT,
        Some("llama3.1:8b".to_string()),
    ) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            eprintln!("SKIP: cannot construct Ollama verifier provider B: {e}");
            return;
        }
    };
    let opts = VerifyOptions {
        limits: VerifyLimits {
            timeout: Duration::from_secs(120),
            ..VerifyLimits::default()
        },
        backends: Vec::new(),
    }
    .with_backend(p1, "qwen2.5:0.5b")
    .with_backend(p2, "llama3.1:8b");

    let report = rt.verify_claim(input(TEXT), &opts).await.unwrap();

    // Whatever the models said, the record exists and the decision never
    // claims more than the verdict.
    let records = list_verdicts(&*rt.store, KB).await.unwrap();
    assert_eq!(records.len(), 1, "verdict must be persisted");
    assert_eq!(records[0].verdict_id, report.verdict_id);
    match report.decision {
        FirewallAction::Pass => assert_eq!(report.verdict, Verdict::Supported),
        FirewallAction::Flag => assert_ne!(report.verdict, Verdict::Supported),
        FirewallAction::Block => assert_eq!(report.verdict, Verdict::Unsupported),
    }

    // Independence is reported exactly as honestly as what ran.
    match &report.independence {
        Independence::Independent { backends } => {
            assert_eq!(backends.len(), 2, "{backends:?}");
            assert!(report.model_verdicts.len() >= 2, "{report:?}");
            eprintln!(
                "cross-model verdicts: {:?}",
                report
                    .model_verdicts
                    .iter()
                    .map(|v| (v.backend_label(), v.verdict))
                    .collect::<Vec<_>>()
            );
            // Both genuinely distinct models agreed SUPPORTED → the
            // production path passes the supported claim.
            if report
                .model_verdicts
                .iter()
                .all(|v| v.verdict == Verdict::Supported)
            {
                assert_eq!(report.decision, FirewallAction::Pass, "{report:?}");
            }
        }
        Independence::Degraded { reason } => {
            eprintln!("SKIP: real cross-model verification degraded: {reason}");
            for f in &report.model_failures {
                eprintln!("  backend {} failed: {}", f.backend_label(), f.error);
            }
        }
    }
}

/// M9.7 / TEST 12 — the production answer path writes the evidence graph:
/// every adjudicated claim is recorded, linked to exactly its RESOLVED
/// citations (idempotent, append-only, integrity intact) — an unresolved
/// citation never becomes an edge, however the operator spells it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verified_claims_land_in_the_evidence_graph() {
    let rt = runtime_with_evidence().await;
    let report = rt
        .verify_claim(input(TEXT), &VerifyOptions::default())
        .await
        .unwrap();
    assert_eq!(report.decision, FirewallAction::Pass, "{report:?}");

    // The claim and its grounding edges are on the graph.
    let claims = openrt_evidence::list_claims(&*rt.store, KB).await.unwrap();
    assert_eq!(claims.len(), 1, "{claims:?}");
    assert_eq!(claims[0].text, TEXT);
    let edges = openrt_evidence::list_edges(&*rt.store, KB).await.unwrap();
    assert!(
        edges
            .iter()
            .any(|e| e.kind == openrt_evidence::EdgeKind::ClaimGroundedIn),
        "{edges:?}"
    );
    assert!(
        edges
            .iter()
            .any(|e| e.kind == openrt_evidence::EdgeKind::PassageExtractedFrom),
        "{edges:?}"
    );
    let integrity = openrt_evidence::verify_integrity(&*rt.store, KB)
        .await
        .unwrap();
    assert!(integrity.intact(), "{integrity:?}");

    // A second run is idempotent (append-only, never duplicated) …
    let again = rt
        .verify_claim(input(TEXT), &VerifyOptions::default())
        .await
        .unwrap();
    assert_eq!(again.decision, FirewallAction::Pass);
    let edges_after = openrt_evidence::list_edges(&*rt.store, KB).await.unwrap();
    assert_eq!(
        edges_after.len(),
        edges.len(),
        "re-verify must not duplicate"
    );

    // … and an UNRESOLVED citation is never linked into the graph.
    let mut with_bad = input(TEXT);
    with_bad.citations.push(Citation {
        kb_id: KB.into(),
        chunk_id: "c-missing".into(),
        document_id: "d1".into(),
        document_version: 1,
        block_keys: vec!["b/c-missing".into()],
        page: Some(1),
        text_sha256: openrt_core::sha256_hex(b"absent"),
    });
    let _report = rt
        .verify_claim(with_bad, &VerifyOptions::default())
        .await
        .unwrap();
    let edges_final = openrt_evidence::list_edges(&*rt.store, KB).await.unwrap();
    assert_eq!(
        edges_final.len(),
        edges.len(),
        "unresolved citation must not add edges: {edges_final:?}"
    );
    let integrity = openrt_evidence::verify_integrity(&*rt.store, KB)
        .await
        .unwrap();
    assert!(integrity.intact(), "{integrity:?}");
}
