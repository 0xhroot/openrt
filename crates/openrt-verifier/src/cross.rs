//! Cross-model verification (M9.8): run the same claim through genuinely
//! distinct backends and report independence *honestly*.
//!
//! * the same model id is never run twice — duplicates are skipped with a
//!   note, so `same-model-twice` can never masquerade as cross-model;
//! * [`Independence::Independent`] requires ≥2 **distinct model ids** that
//!   each produced a verdict;
//! * disagreement is inconclusive ([`Verdict::Insufficient`]), never
//!   averaged into a fake consensus.

use serde::{Deserialize, Serialize};

use crate::model::{model_verify, Backend, BackendFailure, ModelVerdict};
use crate::types::{Independence, Verdict};
use crate::{VerifyError, VerifyLimits};

/// Outcome of a (possibly empty, possibly degraded) cross-model run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrossReport {
    /// Verdicts that were actually produced, in backend order.
    pub verdicts: Vec<ModelVerdict>,
    /// Backends that failed (timeout, transport, invalid response).
    pub failures: Vec<BackendFailure>,
    /// Honest notes (skipped duplicates, …).
    pub notes: Vec<String>,
    pub independence: Independence,
    /// Agreement of all produced verdicts, or `Insufficient` when they
    /// disagree or nothing ran.
    pub combined: Verdict,
    /// True only when ≥2 verdicts exist and every one of them agrees.
    pub agreement: bool,
}

/// Run `backends` sequentially (each call bounded by `limits.timeout`) and
/// combine the results. Backends sharing a model id are run at most once.
pub async fn cross_model_verify(
    backends: &[Backend<'_>],
    claim: &str,
    evidence: &str,
    limits: &VerifyLimits,
) -> Result<CrossReport, VerifyError> {
    let mut verdicts = Vec::new();
    let mut failures = Vec::new();
    let mut notes = Vec::new();
    let mut seen_models: Vec<String> = Vec::new();

    for b in backends {
        if seen_models.iter().any(|m| m == &b.model) {
            notes.push(format!(
                "model `{}` already covered by an earlier backend; never run twice \
                 (the same model twice is never independent)",
                b.model
            ));
            continue;
        }
        seen_models.push(b.model.clone());
        match model_verify(b, claim, evidence, limits).await {
            Ok(v) => verdicts.push(v),
            Err(e) => failures.push(BackendFailure {
                provider: b.provider.id().to_string(),
                model: b.model.clone(),
                error: e.to_string(),
            }),
        }
    }

    let mut distinct: Vec<String> = verdicts.iter().map(|v| v.model.clone()).collect();
    distinct.sort();
    distinct.dedup();
    let independence = if distinct.len() >= 2 {
        let mut labels: Vec<String> = verdicts.iter().map(|v| v.backend_label()).collect();
        labels.sort();
        labels.dedup();
        Independence::Independent { backends: labels }
    } else if verdicts.is_empty() && backends.is_empty() {
        Independence::Degraded {
            reason: "no model backend configured".to_string(),
        }
    } else if verdicts.is_empty() {
        let errs: Vec<String> = failures
            .iter()
            .map(|f| format!("{}: {}", f.backend_label(), f.error))
            .collect();
        let reason = if errs.is_empty() {
            "no model backend configured".to_string()
        } else {
            format!("no model verdict produced ({})", errs.join("; "))
        };
        Independence::Degraded { reason }
    } else {
        Independence::Degraded {
            reason: format!(
                "only one distinct model produced a verdict (`{}`); the same model \
                 twice is never independent",
                distinct[0]
            ),
        }
    };

    let agreement =
        verdicts.len() >= 2 && verdicts.windows(2).all(|w| w[0].verdict == w[1].verdict);
    let combined = match verdicts.first() {
        None => Verdict::Insufficient,
        Some(first) => {
            if verdicts.iter().all(|v| v.verdict == first.verdict) {
                first.verdict
            } else {
                Verdict::Insufficient
            }
        }
    };

    Ok(CrossReport {
        verdicts,
        failures,
        notes,
        independence,
        combined,
        agreement,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use openrt_models::{
        ModelCapabilities, ModelError, ModelInfo, ModelProvider, ModelRequest, ModelResponse,
        ProviderKind,
    };
    use std::sync::Mutex;

    struct Canned {
        id: String,
        body: String,
        calls: Mutex<usize>,
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

    impl Canned {
        fn new(id: &str, body: &str) -> Self {
            Self {
                id: id.into(),
                body: body.into(),
                calls: Mutex::new(0),
            }
        }
        fn calls(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    const SUPPORTED: &str = r#"{"verdict":"SUPPORTED","reason":"grounded"}"#;
    const UNSUPPORTED: &str = r#"{"verdict":"UNSUPPORTED","reason":"not in evidence"}"#;

    fn limits() -> VerifyLimits {
        VerifyLimits {
            timeout: std::time::Duration::from_millis(500),
            ..VerifyLimits::default()
        }
    }

    #[tokio::test]
    async fn no_backends_is_an_honest_degradation() {
        let r = cross_model_verify(&[], "claim", "evidence", &limits())
            .await
            .unwrap();
        assert_eq!(r.combined, Verdict::Insufficient);
        assert!(!r.agreement);
        assert!(matches!(
            &r.independence,
            Independence::Degraded { reason } if reason.contains("no model backend")
        ));
    }

    #[tokio::test]
    async fn two_distinct_models_agreeing_are_independent() {
        let a = Canned::new("p1", SUPPORTED);
        let b = Canned::new("p2", SUPPORTED);
        let backends = vec![Backend::new(&a, "model-a"), Backend::new(&b, "model-b")];
        let r = cross_model_verify(&backends, "claim", "evidence", &limits())
            .await
            .unwrap();
        assert_eq!(r.combined, Verdict::Supported);
        assert!(r.agreement);
        match &r.independence {
            Independence::Independent { backends } => {
                assert_eq!(backends.len(), 2, "{backends:?}");
                assert!(backends.iter().any(|l| l.ends_with("/model-a")));
                assert!(backends.iter().any(|l| l.ends_with("/model-b")));
            }
            other => panic!("expected Independent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn disagreement_is_inconclusive_never_averaged() {
        let a = Canned::new("p1", SUPPORTED);
        let b = Canned::new("p2", UNSUPPORTED);
        let backends = vec![Backend::new(&a, "model-a"), Backend::new(&b, "model-b")];
        let r = cross_model_verify(&backends, "claim", "evidence", &limits())
            .await
            .unwrap();
        assert_eq!(r.verdicts.len(), 2);
        assert!(!r.agreement);
        assert_eq!(r.combined, Verdict::Insufficient);
        assert!(r.independence.is_independent());
    }

    #[tokio::test]
    async fn same_model_twice_is_deduplicated_and_degraded() {
        let a = Canned::new("p1", SUPPORTED);
        let b = Canned::new("p2", SUPPORTED);
        let backends = vec![
            Backend::new(&a, "shared-model"),
            Backend::new(&b, "shared-model"),
        ];
        let r = cross_model_verify(&backends, "claim", "evidence", &limits())
            .await
            .unwrap();
        assert_eq!(a.calls(), 1, "first backend must run");
        assert_eq!(b.calls(), 0, "the same model must never run twice");
        assert_eq!(r.verdicts.len(), 1);
        assert!(r.notes.iter().any(|n| n.contains("never run twice")));
        assert!(!r.agreement);
        match &r.independence {
            Independence::Degraded { reason } => {
                assert!(reason.contains("only one distinct model"), "{reason}");
            }
            other => panic!("same model twice must be Degraded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn total_backend_failure_is_recorded_not_presented_as_verdict() {
        let p = openrt_models::MockProvider::new("fail", openrt_models::mock::MockMode::Fail);
        let backends = vec![Backend::new(&p, "model-a")];
        let r = cross_model_verify(&backends, "claim", "evidence", &limits())
            .await
            .unwrap();
        assert!(r.verdicts.is_empty());
        assert_eq!(r.failures.len(), 1);
        assert_eq!(r.combined, Verdict::Insufficient);
        assert!(matches!(
            &r.independence,
            Independence::Degraded { reason } if reason.contains("no model verdict produced")
        ));
    }
}
