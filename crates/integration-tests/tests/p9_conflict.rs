//! Phase 9 (TEST 10) — conflicting evidence is resolved honestly:
//! * when a verifier adjudicates the conflict, the claim is REJECTED
//!   (Block) — never passed on lexical grounding alone;
//! * when independent verifiers disagree, the outcome is INCONCLUSIVE
//!   (Flag) — never averaged into a fabricated consensus;
//! * with no model available, the deterministic floor's semantic blindness
//!   is reported as an explicit degradation, never as silent confidence;
//! * both conflicting passages are always presented and re-resolved through
//!   the knowledge citation authority — the conflict is on the record.

use std::sync::Arc;

use async_trait::async_trait;
use openrt_knowledge::{load_chunk, persist_chunk, Citation, PersistedChunk};
use openrt_models::{
    MockProvider, ModelCapabilities, ModelError, ModelInfo, ModelProvider, ModelRequest,
    ModelResponse, ProviderKind,
};
use openrt_persistence::MemoryStore;
use openrt_runtime::RuntimeBuilder;
use openrt_verifier::{
    list_verdicts, Basis, FirewallAction, Grounding, Independence, Verdict, VerifyInput,
    VerifyOptions, MIN_GROUNDING,
};

const KB: &str = "kb-conflict";
const CLAIM: &str = "the launch window is dawn";
const SUPPORT_TEXT: &str = "telemetry confirms the launch window is dawn for the campaign";
const CONTRA_TEXT: &str =
    "telemetry confirms the launch window was scrubbed after the ascent anomaly";

/// (document_id, chunk_id, text)
const FIXTURES: [(&str, &str, &str); 2] = [
    ("doc-support", "c-support", SUPPORT_TEXT),
    ("doc-contradict", "c-contradict", CONTRA_TEXT),
];

/// Build the production runtime and persist BOTH sides of the conflict.
async fn runtime_with_conflict() -> Arc<openrt_runtime::Runtime> {
    let rt = RuntimeBuilder::new()
        .store(Arc::new(MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::echo("mock")))
        .build()
        .await
        .unwrap();
    for (doc, chunk, text) in FIXTURES {
        persist_chunk(
            &*rt.store,
            KB,
            &PersistedChunk {
                chunk_id: chunk.into(),
                document_id: doc.into(),
                document_version: 1,
                block_keys: vec![format!("b/{chunk}")],
                page: Some(1),
                text_sha256: openrt_core::sha256_hex(text.as_bytes()),
                text: Some(text.into()),
            },
        )
        .await
        .unwrap();
    }
    rt
}

/// Assemble the answer-path input the production way: every citation and
/// its text reloaded through the knowledge store (the citation authority).
async fn input(rt: &openrt_runtime::Runtime) -> VerifyInput {
    let mut citations = Vec::new();
    let mut evidence_texts = Vec::new();
    for (doc, chunk, _) in FIXTURES {
        let pc = load_chunk(&*rt.store, KB, doc, chunk)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("persisted chunk missing: {doc}/{chunk}"));
        citations.push(Citation {
            kb_id: KB.into(),
            document_id: doc.into(),
            document_version: pc.document_version,
            chunk_id: chunk.into(),
            block_keys: pc.block_keys.clone(),
            page: pc.page,
            text_sha256: pc.text_sha256.clone(),
        });
        evidence_texts.push(pc.text.expect("fixture chunks persist their text"));
    }
    VerifyInput {
        kb_id: KB.into(),
        claim: CLAIM.into(),
        citations,
        evidence_texts,
    }
}

/// Test-only canned backend (same discipline as `p9_verifier`): a fixed
/// strict-JSON body per call — never a production path.
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

const SUPPORTED: &str =
    r#"{"verdict":"SUPPORTED","reason":"the dawn window line confirms the claim"}"#;
const UNSUPPORTED: &str =
    r#"{"verdict":"UNSUPPORTED","reason":"a later scrub report contradicts the window"}"#;

/// TEST 10 (deterministic floor) — the conflict is fully presented and both
/// sides resolve, but with no semantic verifier the pass carries an explicit
/// "no model verifier" degradation: the report never pretends the conflict
/// was adjudicated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deterministic_floor_presents_the_conflict_with_an_honest_degradation() {
    let rt = runtime_with_conflict().await;
    let report = rt
        .verify_claim(input(&rt).await, &VerifyOptions::default())
        .await
        .unwrap();

    // Claim tokens are lexically present in the pooled evidence …
    assert_eq!(report.grounding, Grounding::Grounded, "{report:?}");
    assert!(report.deterministic.lexical_coverage >= MIN_GROUNDING);
    assert_eq!(report.deterministic.citations_total, 2);
    assert_eq!(report.deterministic.citations_resolved, 2);
    assert_eq!(
        report.deterministic.unresolved().len(),
        0,
        "the contradictory passage resolves too — it is on the record"
    );

    // … so the deterministic floor passes it — NEVER dressed up as a model
    // verdict, and never silently confident about the semantic conflict.
    assert_eq!(report.decision, FirewallAction::Pass, "{report:?}");
    assert_eq!(report.basis, Basis::Deterministic);
    assert!(report.model_verdicts.is_empty());
    assert!(
        matches!(
            &report.independence,
            Independence::Degraded { reason } if reason.contains("no model verifier")
        ),
        "semantic conflict is undetectable without a model — say so: {:?}",
        report.independence
    );

    // Both conflicting passages are persisted with the verdict record.
    let records = list_verdicts(&*rt.store, KB).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].evidence_refs.len(),
        2,
        "{:?}",
        records[0].evidence_refs
    );
    assert!(records[0]
        .evidence_refs
        .iter()
        .all(|r| r.resolved && !r.chunk_id.is_empty()));
}

/// TEST 10 (adjudicated conflict) — a verifier that reads the corpus and
/// finds the contradiction REJECTS the claim: model verdicts may only demote,
/// so lexical grounding cannot survive the conflict.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adjudicated_conflict_is_rejected() {
    let rt = runtime_with_conflict().await;
    let canned = Canned::new("p1", UNSUPPORTED);
    let opts = VerifyOptions::default().with_backend(canned.clone(), "model-a");
    let report = rt.verify_claim(input(&rt).await, &opts).await.unwrap();

    assert_eq!(report.decision, FirewallAction::Block, "{report:?}");
    assert_eq!(report.verdict, Verdict::Unsupported);
    assert_eq!(report.basis, Basis::Model);
    assert_eq!(report.model_verdicts.len(), 1);
    assert_eq!(report.model_verdicts[0].verdict, Verdict::Unsupported);
    assert!(
        report.model_verdicts[0].reason.contains("contradicts"),
        "the model's own stated reason is kept verbatim: {:?}",
        report.model_verdicts[0]
    );
    assert_eq!(canned.calls(), 1, "the model ran exactly once");

    // The rejection is on the persisted record.
    let records = list_verdicts(&*rt.store, KB).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].decision, FirewallAction::Block);
    assert_eq!(records[0].verdict, Verdict::Unsupported);
}

/// TEST 10 (unresolved conflict) — independent verifiers reading the same
/// conflicting corpus disagree: the outcome stays INCONCLUSIVE (Flag), never
/// averaged into consensus, with independence reported for what it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verifier_disagreement_over_conflict_stays_inconclusive() {
    let rt = runtime_with_conflict().await;
    let a = Canned::new("p1", SUPPORTED);
    let b = Canned::new("p2", UNSUPPORTED);
    let opts = VerifyOptions::default()
        .with_backend(a.clone(), "model-a")
        .with_backend(b.clone(), "model-b");
    let report = rt.verify_claim(input(&rt).await, &opts).await.unwrap();

    assert_eq!(report.decision, FirewallAction::Flag, "{report:?}");
    assert_eq!(report.verdict, Verdict::Insufficient);
    assert_eq!(report.basis, Basis::CrossModel);
    assert_ne!(
        report.verdict,
        Verdict::Supported,
        "never a fabricated consensus"
    );
    assert_eq!(a.calls(), 1);
    assert_eq!(b.calls(), 1);

    // Both genuinely distinct models actually ran.
    assert!(
        report.independence.is_independent(),
        "{:?}",
        report.independence
    );
    match &report.independence {
        Independence::Independent { backends } => {
            assert_eq!(backends.len(), 2, "{backends:?}");
        }
        other => panic!("expected Independent, got {other:?}"),
    }
    assert_eq!(report.model_verdicts.len(), 2);
    assert!(
        report.reasons.iter().any(|r| r.contains("disagree")),
        "the inconclusive reason is on the record: {:?}",
        report.reasons
    );

    let records = list_verdicts(&*rt.store, KB).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].decision, FirewallAction::Flag);
}
