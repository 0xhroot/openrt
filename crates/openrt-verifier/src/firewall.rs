//! The hallucination firewall (M9.8): the single decision layer applied at
//! production answer paths.
//!
//! Order of operations:
//!
//! 1. **clip** the supplied (UNTRUSTED) evidence to the size cap;
//! 2. **deterministic evidence checks** — citations re-resolve against
//!   persisted `corpus.chunks`, the claim is lexically grounded (this layer
//!   can only fail a claim);
//! 3. **model verification** (optional, bounded by time/size caps) — real
//!   backends may only *demote* the verdict, never lift it past the
//!   deterministic floor;
//! 4. **cross-model** honesty — `Independent` only for ≥2 distinct models;
//! 5. **persist** the verdict under `verify.verdicts` before returning (a
//!   decision without its record is an error, not a silent pass).

use std::sync::Arc;

use openrt_core::{canonical_pair_digest, sha256_hex};
use openrt_knowledge::Citation;
use openrt_models::ModelProvider;
use openrt_persistence::PersistenceStore;
use serde::{Deserialize, Serialize};

use crate::cross::{cross_model_verify, CrossReport};
use crate::deterministic::{deterministic_check, DeterministicReport, Grounding};
use crate::model::{Backend, BackendFailure, ModelVerdict};
use crate::persist::{record_verdict, EvidenceRefRecord, VerdictRecord};
use crate::types::{Basis, FirewallAction, Independence, Verdict, VerifyLimits};
use crate::VerifyError;

/// One claim at a production answer path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyInput {
    pub kb_id: String,
    pub claim: String,
    /// Cited spans that must re-resolve against persisted evidence (the
    /// knowledge layer stays THE citation authority).
    pub citations: Vec<Citation>,
    /// Evidence passage text supplied by the answer path. UNTRUSTED: it is
    /// clipped to `limits.max_evidence_chars` before any layer sees it and
    /// is never interpreted as an instruction.
    pub evidence_texts: Vec<String>,
}

/// A configured verification backend: a real provider plus an explicit
/// model id (independence is decided by the model id, never the provider).
#[derive(Clone)]
pub struct BackendSpec {
    pub provider: Arc<dyn ModelProvider>,
    pub model: String,
}

impl BackendSpec {
    pub fn new(provider: Arc<dyn ModelProvider>, model: impl Into<String>) -> Self {
        Self {
            provider,
            model: model.into(),
        }
    }
}

/// Verification options for one firewall run.
#[derive(Clone, Default)]
pub struct VerifyOptions {
    pub limits: VerifyLimits,
    /// Backends to consult after the deterministic gate. Empty = honest
    /// deterministic-only mode (reported as a degradation, never as a model
    /// verdict).
    pub backends: Vec<BackendSpec>,
}

impl VerifyOptions {
    /// Add a backend (provider + explicit model id).
    pub fn with_backend(
        mut self,
        provider: Arc<dyn ModelProvider>,
        model: impl Into<String>,
    ) -> Self {
        self.backends.push(BackendSpec::new(provider, model));
        self
    }
}

/// The limits a verdict was produced under (recorded with the verdict).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitsSnapshot {
    pub timeout_ms: u64,
    pub max_evidence_chars: usize,
    pub max_claims: usize,
    /// Evidence characters actually considered this run.
    pub evidence_chars_used: usize,
}

/// The firewall's decision plus everything a reviewer needs to re-check it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FirewallReport {
    pub kb_id: String,
    pub claim_sha256: String,
    pub decision: FirewallAction,
    pub verdict: Verdict,
    /// Which layer produced the verdict (a deterministic pass is never
    /// dressed up as a model-backed one).
    pub basis: Basis,
    pub grounding: Grounding,
    pub deterministic: DeterministicReport,
    pub model_verdicts: Vec<ModelVerdict>,
    pub model_failures: Vec<BackendFailure>,
    /// Structural notes (skipped duplicate model, backend gate skipped, …).
    pub notes: Vec<String>,
    /// Human-true narrative: evidence notes, backend failures, rationale.
    pub reasons: Vec<String>,
    pub independence: Independence,
    pub limits: LimitsSnapshot,
    /// `verify.verdicts` record id — persisted BEFORE this report returned.
    pub verdict_id: String,
}

/// Clip the supplied evidence texts to `budget` characters. Returns the
/// clipped text and whether any input characters were dropped (the caller
/// records the clip; it is never a silent truncation).
pub fn clip_evidence(texts: &[String], budget: usize) -> (String, bool) {
    let mut out = String::new();
    let mut clipped = false;
    for (i, t) in texts.iter().enumerate() {
        if i > 0 {
            if out.chars().count() >= budget {
                clipped = true;
                break;
            }
            out.push(' ');
        }
        let room = budget - out.chars().count();
        let take = t.chars().count().min(room);
        out.extend(t.chars().take(take));
        if take < t.chars().count() {
            clipped = true;
            break;
        }
    }
    (out, clipped)
}

/// Run the hallucination firewall over ONE claim. Deterministic evidence
/// checks always run; model backends (when configured) may only demote the
/// verdict; the verdict record is persisted before `Ok` is returned.
pub async fn verify_claim(
    store: &dyn PersistenceStore,
    input: VerifyInput,
    opts: &VerifyOptions,
) -> Result<FirewallReport, VerifyError> {
    if input.kb_id.trim().is_empty() {
        return Err(VerifyError::Invalid("kb_id is empty".into()));
    }
    if input.claim.trim().is_empty() {
        return Err(VerifyError::Invalid("claim is empty".into()));
    }

    // 1. size cap: UNTRUSTED evidence is clipped before any layer sees it.
    let supplied_chars: usize = input.evidence_texts.iter().map(|t| t.chars().count()).sum();
    let (evidence, clipped) = clip_evidence(&input.evidence_texts, opts.limits.max_evidence_chars);

    // 2. deterministic evidence checks (always).
    let mut det = deterministic_check(store, &input.claim, &input.citations, &evidence).await?;
    let mut notes: Vec<String> = Vec::new();
    let mut reasons: Vec<String> = det.notes.clone();
    if clipped {
        det.notes.push(format!(
            "evidence clipped from {supplied_chars} to {} characters",
            opts.limits.max_evidence_chars
        ));
        reasons.push(format!(
            "evidence clipped from {supplied_chars} to {} characters",
            opts.limits.max_evidence_chars
        ));
        notes.push("evidence size cap applied".to_string());
    }

    // 3. model layer — only after a non-failed deterministic gate, and only
    //    when a backend is actually configured.
    let skip_reason = if det.grounding == Grounding::Ungrounded {
        Some("model verification skipped: deterministic evidence gate failed".to_string())
    } else if opts.backends.is_empty() {
        Some("no model verifier configured — deterministic evidence checks only".to_string())
    } else {
        None
    };
    let cross: Option<CrossReport> = match &skip_reason {
        None => {
            let backends: Vec<Backend<'_>> = opts
                .backends
                .iter()
                .map(|s| Backend::new(&*s.provider, s.model.clone()))
                .collect();
            Some(cross_model_verify(&backends, &input.claim, &evidence, &opts.limits).await?)
        }
        Some(_) => None,
    };
    if let Some(reason) = &skip_reason {
        notes.push(reason.clone());
    }

    // 4. combine: the deterministic floor first, model verdicts may only
    //    demote (never lift a claim past its evidence).
    let (decision, verdict, basis) = combine(&det, cross.as_ref(), &mut reasons);
    if let Some(c) = &cross {
        notes.extend(c.notes.iter().cloned());
    }

    let independence = match &cross {
        Some(c) => c.independence.clone(),
        None => Independence::Degraded {
            reason: skip_reason.unwrap_or_else(|| "model verification did not run".to_string()),
        },
    };
    let mut model_verdicts: Vec<ModelVerdict> = cross
        .as_ref()
        .map(|c| c.verdicts.clone())
        .unwrap_or_default();
    model_verdicts.sort_by(|a, b| a.backend_label().cmp(&b.backend_label()));
    let model_failures: Vec<BackendFailure> = cross
        .as_ref()
        .map(|c| c.failures.clone())
        .unwrap_or_default();

    // 5. persist the verdict (fail closed: no report without its record).
    let claim_sha256 = sha256_hex(input.claim.as_bytes());
    let created_at = openrt_core::now().to_rfc3339();
    let mut backends_ran: Vec<String> = model_verdicts.iter().map(|v| v.backend_label()).collect();
    backends_ran.sort();
    backends_ran.dedup();
    let verdict_id = canonical_pair_digest(&[
        ("kb_id".into(), input.kb_id.clone()),
        ("claim_sha256".into(), claim_sha256.clone()),
        (
            "decision".into(),
            serde_json::to_string(&decision).unwrap_or_default(),
        ),
        (
            "verdict".into(),
            serde_json::to_string(&verdict).unwrap_or_default(),
        ),
        (
            "basis".into(),
            serde_json::to_string(&basis).unwrap_or_default(),
        ),
        (
            "grounding".into(),
            serde_json::to_string(&det.grounding).unwrap_or_default(),
        ),
        ("backends".into(), backends_ran.join(",")),
        (
            "independence".into(),
            serde_json::to_string(&independence).unwrap_or_default(),
        ),
        ("created_at".into(), created_at.clone()),
    ]);
    let record = VerdictRecord {
        verdict_id: verdict_id.clone(),
        kb_id: input.kb_id.clone(),
        claim: input.claim.clone(),
        claim_sha256: claim_sha256.clone(),
        decision,
        verdict,
        basis,
        grounding: det.grounding,
        evidence_refs: det
            .citations
            .iter()
            .map(|c| EvidenceRefRecord {
                document_id: c.document_id.clone(),
                chunk_id: c.chunk_id.clone(),
                resolved: c.resolved,
            })
            .collect(),
        backends: backends_ran,
        independence: independence.clone(),
        limits: LimitsSnapshot {
            timeout_ms: opts.limits.timeout.as_millis() as u64,
            max_evidence_chars: opts.limits.max_evidence_chars,
            max_claims: opts.limits.max_claims,
            evidence_chars_used: det.evidence_chars,
        },
        reasons: reasons.clone(),
        created_at,
    };
    record_verdict(store, &record).await?;

    Ok(FirewallReport {
        kb_id: input.kb_id,
        claim_sha256,
        decision,
        verdict,
        basis,
        grounding: det.grounding,
        deterministic: det,
        model_verdicts,
        model_failures,
        notes,
        reasons,
        independence,
        limits: LimitsSnapshot {
            timeout_ms: opts.limits.timeout.as_millis() as u64,
            max_evidence_chars: opts.limits.max_evidence_chars,
            max_claims: opts.limits.max_claims,
            evidence_chars_used: evidence.chars().count(),
        },
        verdict_id,
    })
}

/// Verify a bounded batch of claims (`limits.max_claims` enforced — never
/// silently truncated). Sequential and bounded: each claim's model calls are
/// individually capped by `limits.timeout`.
pub async fn verify_claims(
    store: &dyn PersistenceStore,
    inputs: Vec<VerifyInput>,
    opts: &VerifyOptions,
) -> Result<Vec<FirewallReport>, VerifyError> {
    if inputs.len() > opts.limits.max_claims {
        return Err(VerifyError::Limits(format!(
            "{} claims exceed max_claims {}",
            inputs.len(),
            opts.limits.max_claims
        )));
    }
    let mut out = Vec::with_capacity(inputs.len());
    for input in inputs {
        out.push(verify_claim(store, input, opts).await?);
    }
    Ok(out)
}

/// Combine the deterministic floor with the model layer. Pure: mutates
/// `reasons` with the human-true rationale of the chosen decision.
fn combine(
    det: &DeterministicReport,
    cross: Option<&CrossReport>,
    reasons: &mut Vec<String>,
) -> (FirewallAction, Verdict, Basis) {
    if det.grounding == Grounding::Ungrounded {
        reasons.push("blocked: deterministic evidence gate failed".to_string());
        return (
            FirewallAction::Block,
            Verdict::Unsupported,
            Basis::Deterministic,
        );
    }
    let Some(c) = cross else {
        return match det.grounding {
            Grounding::Grounded => {
                reasons.push(
                    "passed on deterministic evidence checks alone; no model verifier ran"
                        .to_string(),
                );
                (
                    FirewallAction::Pass,
                    Verdict::Supported,
                    Basis::Deterministic,
                )
            }
            _ => {
                reasons
                    .push("flagged: evidence gap not confirmed by any model verifier".to_string());
                (
                    FirewallAction::Flag,
                    Verdict::Insufficient,
                    Basis::Deterministic,
                )
            }
        };
    };
    for f in &c.failures {
        reasons.push(format!("backend {} failed: {}", f.backend_label(), f.error));
    }
    if c.verdicts.is_empty() {
        // Every configured backend failed: honest degradation, no verdict
        // is invented — the deterministic floor decides.
        return match det.grounding {
            Grounding::Grounded => {
                reasons.push(
                    "model verification unavailable; passed on deterministic evidence checks"
                        .to_string(),
                );
                (
                    FirewallAction::Pass,
                    Verdict::Supported,
                    Basis::Deterministic,
                )
            }
            _ => {
                reasons.push("model verification unavailable; evidence gap remains".to_string());
                (
                    FirewallAction::Flag,
                    Verdict::Insufficient,
                    Basis::Deterministic,
                )
            }
        };
    }
    let model_basis = if c.independence.is_independent() {
        Basis::CrossModel
    } else {
        Basis::Model
    };
    match c.combined {
        Verdict::Unsupported => {
            reasons.push("model verification contradicts the claim".to_string());
            (FirewallAction::Block, Verdict::Unsupported, model_basis)
        }
        Verdict::Insufficient => {
            if c.verdicts.len() >= 2 && !c.agreement {
                reasons.push("cross-model disagreement — inconclusive".to_string());
            } else {
                reasons.push("model verification is insufficient".to_string());
            }
            (FirewallAction::Flag, Verdict::Insufficient, model_basis)
        }
        Verdict::Supported => match det.grounding {
            Grounding::Grounded => {
                reasons.push(
                    "claim grounded in resolving evidence and supported by model verification"
                        .to_string(),
                );
                (FirewallAction::Pass, Verdict::Supported, model_basis)
            }
            _ => {
                reasons.push(
                    "model support cannot lift an evidence gap; flagged for review".to_string(),
                );
                (
                    FirewallAction::Flag,
                    Verdict::Insufficient,
                    Basis::Deterministic,
                )
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::list_verdicts;
    use crate::types::VerifyLimits;
    use async_trait::async_trait;
    use openrt_knowledge::{persist_chunk, PersistedChunk};
    use openrt_models::{
        ModelCapabilities, ModelError, ModelInfo, ModelProvider, ModelRequest, ModelResponse,
        ProviderKind,
    };
    use openrt_persistence::MemoryStore;
    use std::sync::Mutex;

    const KB: &str = "kb-fw";
    const TEXT: &str = "the launch window is dawn";

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

    async fn store() -> std::sync::Arc<dyn PersistenceStore> {
        let s = std::sync::Arc::new(MemoryStore::new()) as std::sync::Arc<dyn PersistenceStore>;
        let chunk = PersistedChunk {
            chunk_id: "c1".into(),
            document_id: "d1".into(),
            document_version: 1,
            block_keys: vec!["b/c1".into()],
            page: Some(1),
            text_sha256: openrt_core::sha256_hex(TEXT.as_bytes()),
            text: Some(TEXT.into()),
        };
        persist_chunk(&*s, KB, &chunk).await.unwrap();
        s
    }

    struct Canned {
        id: String,
        body: String,
        calls: Mutex<usize>,
    }

    impl Canned {
        fn new(id: &str, body: &str) -> Arc<Self> {
            Arc::new(Self {
                id: id.into(),
                body: body.into(),
                calls: Mutex::new(0),
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

    const SUPPORTED: &str = r#"{"verdict":"SUPPORTED","reason":"grounded"}"#;
    const UNSUPPORTED: &str = r#"{"verdict":"UNSUPPORTED","reason":"not in evidence"}"#;

    fn input(claim: &str) -> VerifyInput {
        VerifyInput {
            kb_id: KB.into(),
            claim: claim.into(),
            citations: vec![citation()],
            evidence_texts: vec![TEXT.into()],
        }
    }

    #[test]
    fn clip_respects_the_budget_and_reports_the_clip() {
        let texts = vec!["abc".to_string(), "defgh".to_string()];
        let (out, clipped) = clip_evidence(&texts, 100);
        assert_eq!(out, "abc defgh");
        assert!(!clipped);
        let (out, clipped) = clip_evidence(&texts, 4);
        assert_eq!(out, "abc ");
        assert!(clipped, "{out:?}");
        let (out, clipped) = clip_evidence(&texts, 3);
        assert_eq!(out, "abc");
        assert!(clipped, "{out:?}");
        let (out, clipped) = clip_evidence(&[], 10);
        assert!(out.is_empty() && !clipped);
    }

    #[tokio::test]
    async fn unsupported_claim_is_blocked_without_any_model() {
        let s = store().await;
        let opts = VerifyOptions::default();
        let r = verify_claim(&*s, input("the moon is made of cheese"), &opts)
            .await
            .unwrap();
        assert_eq!(r.decision, FirewallAction::Block, "{r:?}");
        assert_eq!(r.verdict, Verdict::Unsupported);
        assert_eq!(r.basis, Basis::Deterministic);
        assert!(r.model_verdicts.is_empty());
        assert!(matches!(
            &r.independence,
            Independence::Degraded { reason } if reason.contains("deterministic evidence gate")
        ));
        let records = list_verdicts(&*s, KB).await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].decision, FirewallAction::Block);
        assert_eq!(records[0].claim, "the moon is made of cheese");
        assert_eq!(records[0].verdict_id, r.verdict_id);
    }

    #[tokio::test]
    async fn grounded_claim_passes_deterministic_only_with_honest_degradation() {
        let s = store().await;
        let r = verify_claim(&*s, input(TEXT), &VerifyOptions::default())
            .await
            .unwrap();
        assert_eq!(r.decision, FirewallAction::Pass, "{r:?}");
        assert_eq!(r.verdict, Verdict::Supported);
        assert_eq!(r.basis, Basis::Deterministic);
        assert!(matches!(
            &r.independence,
            Independence::Degraded { reason } if reason.contains("no model verifier")
        ));
    }

    #[tokio::test]
    async fn model_can_demote_a_grounded_claim_to_blocked() {
        let s = store().await;
        let canned = Canned::new("p1", UNSUPPORTED);
        let opts = VerifyOptions::default().with_backend(canned.clone(), "model-a");
        let r = verify_claim(&*s, input(TEXT), &opts).await.unwrap();
        assert_eq!(r.decision, FirewallAction::Block, "{r:?}");
        assert_eq!(r.verdict, Verdict::Unsupported);
        assert_eq!(r.basis, Basis::Model);
        assert_eq!(r.model_verdicts.len(), 1);
        assert_eq!(canned.calls(), 1);
    }

    #[tokio::test]
    async fn model_cannot_lift_an_evidence_gap() {
        let s = store().await;
        // Partial lexical overlap: Weak floor, but the model claims support.
        let claim = "the dawn launch report was delayed by weather";
        let canned = Canned::new("p1", SUPPORTED);
        let opts = VerifyOptions::default().with_backend(canned.clone(), "model-a");
        let r = verify_claim(&*s, input(claim), &opts).await.unwrap();
        assert_eq!(r.decision, FirewallAction::Flag, "{r:?}");
        assert_eq!(r.verdict, Verdict::Insufficient);
        assert_eq!(r.basis, Basis::Deterministic);
        assert_eq!(r.model_verdicts.len(), 1, "model did run");
        assert!(r
            .reasons
            .iter()
            .any(|x| x.contains("cannot lift an evidence gap")));
    }

    #[tokio::test]
    async fn ungrounded_gate_never_spends_a_model_call() {
        let s = store().await;
        let canned = Canned::new("p1", SUPPORTED);
        let opts = VerifyOptions::default().with_backend(canned.clone(), "model-a");
        let r = verify_claim(&*s, input("the moon is made of cheese"), &opts)
            .await
            .unwrap();
        assert_eq!(r.decision, FirewallAction::Block);
        assert_eq!(
            canned.calls(),
            0,
            "failed deterministic gate must not call a model"
        );
    }

    #[tokio::test]
    async fn batch_claim_limit_is_enforced_not_truncated() {
        let s = store().await;
        let opts = VerifyOptions {
            limits: VerifyLimits {
                max_claims: 2,
                ..VerifyLimits::default()
            },
            backends: Vec::new(),
        };
        let err = verify_claims(&*s, vec![input(TEXT); 3], &opts)
            .await
            .unwrap_err();
        assert!(matches!(err, VerifyError::Limits(_)), "{err:?}");
        // Within the limit it runs.
        let ok = verify_claims(&*s, vec![input(TEXT); 2], &opts)
            .await
            .unwrap();
        assert_eq!(ok.len(), 2);
    }

    #[tokio::test]
    async fn empty_claim_and_empty_kb_are_invalid_requests() {
        let s = store().await;
        let mut i = input(" ");
        let err = verify_claim(&*s, i.clone(), &VerifyOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, VerifyError::Invalid(_)), "{err:?}");
        i.kb_id = " ".into();
        i.claim = TEXT.into();
        let err = verify_claim(&*s, i, &VerifyOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, VerifyError::Invalid(_)), "{err:?}");
    }

    #[tokio::test]
    async fn two_distinct_models_agreeing_pass_as_cross_model() {
        let s = store().await;
        let a = Canned::new("p1", SUPPORTED);
        let b = Canned::new("p2", SUPPORTED);
        let opts = VerifyOptions::default()
            .with_backend(a, "model-a")
            .with_backend(b, "model-b");
        let r = verify_claim(&*s, input(TEXT), &opts).await.unwrap();
        assert_eq!(r.decision, FirewallAction::Pass, "{r:?}");
        assert_eq!(r.basis, Basis::CrossModel);
        assert!(r.independence.is_independent());
        let records = list_verdicts(&*s, KB).await.unwrap();
        assert_eq!(records[0].backends.len(), 2);
    }

    #[tokio::test]
    async fn cross_model_disagreement_is_flagged_not_resolved() {
        let s = store().await;
        let a = Canned::new("p1", SUPPORTED);
        let b = Canned::new("p2", UNSUPPORTED);
        let opts = VerifyOptions::default()
            .with_backend(a, "model-a")
            .with_backend(b, "model-b");
        let r = verify_claim(&*s, input(TEXT), &opts).await.unwrap();
        assert_eq!(r.decision, FirewallAction::Flag, "{r:?}");
        assert_eq!(r.verdict, Verdict::Insufficient);
        assert!(r.independence.is_independent());
        assert!(r.reasons.iter().any(|x| x.contains("disagreement")));
    }

    #[tokio::test]
    async fn timeouts_are_structured_and_recorded_not_swallowed() {
        let s = store().await;
        let slow = openrt_models::MockProvider::new("slow", openrt_models::mock::MockMode::Slow)
            .with_latency(std::time::Duration::from_secs(30));
        let opts = VerifyOptions {
            limits: VerifyLimits {
                timeout: std::time::Duration::from_millis(50),
                ..VerifyLimits::default()
            },
            backends: Vec::new(),
        }
        .with_backend(Arc::new(slow), "mock://default");
        let start = std::time::Instant::now();
        let r = verify_claim(&*s, input(TEXT), &opts).await.unwrap();
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "timeout not enforced: {:?}",
            start.elapsed()
        );
        // Deterministic floor still decided; the failure is recorded.
        assert_eq!(r.decision, FirewallAction::Pass, "{r:?}");
        assert_eq!(r.model_failures.len(), 1);
        assert!(
            r.model_failures[0].error.contains("timed out"),
            "{:?}",
            r.model_failures
        );
        assert!(matches!(
            &r.independence,
            Independence::Degraded { reason } if reason.contains("no model verdict produced")
        ));
    }

    #[tokio::test]
    async fn evidence_size_cap_is_applied_and_recorded() {
        let s = store().await;
        let long = format!("{TEXT} {}", "filler ".repeat(500));
        let mut i = input(TEXT);
        i.evidence_texts = vec![long];
        let opts = VerifyOptions {
            limits: VerifyLimits {
                max_evidence_chars: 64,
                ..VerifyLimits::default()
            },
            backends: Vec::new(),
        };
        let r = verify_claim(&*s, i, &opts).await.unwrap();
        assert!(r.limits.evidence_chars_used <= 64, "{:?}", r.limits);
        assert!(r.deterministic.evidence_chars <= 64);
        assert!(r.reasons.iter().any(|x| x.contains("evidence clipped")));
        let records = list_verdicts(&*s, KB).await.unwrap();
        assert!(records[0]
            .reasons
            .iter()
            .any(|x| x.contains("evidence clipped")));
    }
}
