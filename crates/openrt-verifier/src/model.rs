//! Model-backed claim verification (M9.8): one real backend, explicit caps,
//! strict JSON — garbage is a structured error, never a verdict.

use openrt_core::Message;
use openrt_models::{ModelProvider, ModelRequest};
use openrt_retrieval::context::wrap_untrusted;
use serde::{Deserialize, Serialize};

use crate::{types::Verdict, VerifyError, VerifyLimits};

/// A model backend that may take part in verification. `model` is always
/// explicit so reports (and independence) name exactly what ran.
#[derive(Clone)]
pub struct Backend<'a> {
    pub provider: &'a dyn ModelProvider,
    pub model: String,
}

impl<'a> Backend<'a> {
    pub fn new(provider: &'a dyn ModelProvider, model: impl Into<String>) -> Self {
        Self {
            provider,
            model: model.into(),
        }
    }

    /// `provider/model` label used in reports and independence lists.
    pub fn label(&self) -> String {
        format!("{}/{}", self.provider.id(), self.model)
    }
}

/// One backend's model verdict on one claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelVerdict {
    pub provider: String,
    pub model: String,
    pub verdict: Verdict,
    /// The model's own stated reason (verbatim, trimmed) — evidence of what
    /// the model said, not a summary of it.
    pub reason: String,
}

impl ModelVerdict {
    pub fn backend_label(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }
}

/// A backend that did not produce a verdict. Recorded, never swallowed: a
/// failure degrades the report instead of being presented as a verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendFailure {
    pub provider: String,
    pub model: String,
    pub error: String,
}

impl BackendFailure {
    pub fn backend_label(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }
}

/// Ask ONE real model whether the claim is supported by the (already
/// clipped) evidence, bounded by `limits`:
///
/// * wall-clock cap → [`VerifyError::Timeout`];
/// * transport/config failure → [`VerifyError::Unavailable`];
/// * anything that is not strict `{"verdict":…,"reason":…}` JSON with a
///   known verdict → [`VerifyError::InvalidResponse`].
///
/// An `Err` is never converted into a verdict by the caller.
pub async fn model_verify(
    backend: &Backend<'_>,
    claim: &str,
    evidence: &str,
    limits: &VerifyLimits,
) -> Result<ModelVerdict, VerifyError> {
    let system = "You are a claim verifier. Decide whether the CLAIM is \
supported by the EVIDENCE. Respond with ONLY a JSON object of the form \
{\"verdict\":\"SUPPORTED\"|\"UNSUPPORTED\"|\"INSUFFICIENT\",\"reason\":\"short \
explanation\"}. No prose, no code fences.";
    // UNTRUSTED framing: evidence is quoted data behind explicit markers —
    // the same one framing model the context builder uses (M9.9).
    let user = format!(
        "Claim: {claim}\n\nEvidence:\n{}\n\nRespond with JSON only.",
        wrap_untrusted(evidence)
    );
    let mut req = ModelRequest::new(vec![Message::system(system), Message::user(user)]);
    req.model_id = Some(backend.model.clone());

    let resp = tokio::time::timeout(limits.timeout, backend.provider.generate(req))
        .await
        .map_err(|_| VerifyError::Timeout(limits.timeout))?
        .map_err(|e| VerifyError::Unavailable(e.to_string()))?;

    let (verdict, reason) = parse_verdict(&resp.text)?;
    Ok(ModelVerdict {
        provider: backend.provider.id().to_string(),
        model: backend.model.clone(),
        verdict,
        reason,
    })
}

/// Parse `{"verdict":"…","reason":"…"}` out of a model response. Tolerates
/// surrounding prose (first balanced JSON object wins) but accepts nothing
/// else: unknown/missing verdicts, unbalanced JSON and non-JSON are honest
/// [`VerifyError::InvalidResponse`]s.
pub fn parse_verdict(text: &str) -> Result<(Verdict, String), VerifyError> {
    let start = text
        .find('{')
        .ok_or_else(|| VerifyError::InvalidResponse("no JSON object in response".into()))?;
    let mut depth = 0usize;
    let mut end = None;
    for (i, ch) in text[start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    end = Some(start + i + ch.len_utf8());
                    break;
                }
            }
            _ => {}
        }
    }
    let end = end.ok_or_else(|| VerifyError::InvalidResponse("unbalanced JSON object".into()))?;
    let raw: RawVerdict = serde_json::from_str(&text[start..end])
        .map_err(|e| VerifyError::InvalidResponse(format!("bad JSON: {e}")))?;
    let verdict = match raw.verdict.trim().to_ascii_uppercase().as_str() {
        "SUPPORTED" => Verdict::Supported,
        "UNSUPPORTED" => Verdict::Unsupported,
        "INSUFFICIENT" => Verdict::Insufficient,
        other => {
            return Err(VerifyError::InvalidResponse(format!(
                "unknown verdict `{other}` (expected SUPPORTED|UNSUPPORTED|INSUFFICIENT)"
            )))
        }
    };
    Ok((verdict, raw.reason.unwrap_or_default().trim().to_string()))
}

#[derive(Deserialize)]
struct RawVerdict {
    verdict: String,
    reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use openrt_models::{
        MockProvider, ModelCapabilities, ModelError, ModelInfo, ModelResponse, ProviderKind,
    };
    use std::sync::Arc;
    use std::time::Duration;

    /// Test-only canned provider: answers every request with one fixed body
    /// (like the rerank tests' canned providers — never a production path).
    struct Canned {
        id: String,
        model: String,
        body: String,
        seen_models: Arc<std::sync::Mutex<Vec<Option<String>>>>,
    }

    #[async_trait]
    impl ModelProvider for Canned {
        fn id(&self) -> &str {
            &self.id
        }
        fn info(&self) -> ModelInfo {
            ModelInfo::new(
                self.id.clone(),
                ProviderKind::Mock,
                Some(self.model.clone()),
            )
        }
        async fn generate(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
            self.seen_models.lock().unwrap().push(req.model_id.clone());
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

    fn canned(id: &str, model: &str, body: &str) -> Canned {
        Canned {
            id: id.into(),
            model: model.into(),
            body: body.into(),
            seen_models: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    fn limits(timeout_ms: u64) -> VerifyLimits {
        VerifyLimits {
            timeout: Duration::from_millis(timeout_ms),
            ..VerifyLimits::default()
        }
    }

    #[test]
    fn parses_strict_verdict_json() {
        assert_eq!(
            parse_verdict(r#"{"verdict":"SUPPORTED","reason":"grounded"}"#).unwrap(),
            (Verdict::Supported, "grounded".to_string())
        );
        assert_eq!(
            parse_verdict(r#"{"verdict":"unsupported","reason":"no evidence"}"#).unwrap(),
            (Verdict::Unsupported, "no evidence".to_string())
        );
        assert_eq!(
            parse_verdict(r#"{"verdict":"INSUFFICIENT"}"#).unwrap(),
            (Verdict::Insufficient, String::new())
        );
    }

    #[test]
    fn tolerates_prose_around_the_json_object() {
        let (v, reason) =
            parse_verdict("Sure! {\"verdict\":\"INSUFFICIENT\",\"reason\":\"gap\"} done").unwrap();
        assert_eq!(v, Verdict::Insufficient);
        assert_eq!(reason, "gap");
    }

    #[test]
    fn garbage_is_a_structured_error_never_a_verdict() {
        for bad in [
            "I think that is true.",
            "",
            "{\"verdict\":\"MAYBE\"}",
            "{\"verdict\":\"SUPPORTED\"",
            "{\"nope\":1}",
        ] {
            let err = parse_verdict(bad).unwrap_err();
            assert!(
                matches!(err, VerifyError::InvalidResponse(_)),
                "{bad:?} -> {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn echo_as_verifier_is_never_accepted() {
        // A provider that just echoes the prompt back can never produce a
        // verdict (anti-pattern: echo-as-verifier).
        let p = MockProvider::echo("echo");
        let b = Backend::new(&p, "mock://default");
        let err = model_verify(&b, "any claim", "any evidence", &limits(1_000))
            .await
            .unwrap_err();
        assert!(matches!(err, VerifyError::InvalidResponse(_)), "{err:?}");
    }

    #[tokio::test]
    async fn model_id_is_explicit_on_the_request() {
        let c = canned("p1", "model-a", r#"{"verdict":"SUPPORTED","reason":"ok"}"#);
        let seen = c.seen_models.clone();
        let b = Backend::new(&c, "model-a");
        let v = model_verify(&b, "claim", "evidence", &limits(1_000))
            .await
            .unwrap();
        assert_eq!(v.verdict, Verdict::Supported);
        assert_eq!(v.backend_label(), "p1/model-a");
        assert_eq!(*seen.lock().unwrap(), vec![Some("model-a".to_string())]);
    }

    #[tokio::test]
    async fn slow_backend_hits_the_time_cap_structured() {
        let p = MockProvider::new("slow", openrt_models::mock::MockMode::Slow)
            .with_latency(Duration::from_secs(30));
        let b = Backend::new(&p, "mock://default");
        let start = std::time::Instant::now();
        let err = model_verify(&b, "claim", "evidence", &limits(50))
            .await
            .unwrap_err();
        assert!(matches!(err, VerifyError::Timeout(_)), "{err:?}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timeout not enforced: {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn failing_backend_is_unavailable_not_a_verdict() {
        let p = MockProvider::new("fail", openrt_models::mock::MockMode::Fail);
        let b = Backend::new(&p, "mock://default");
        let err = model_verify(&b, "claim", "evidence", &limits(1_000))
            .await
            .unwrap_err();
        assert!(matches!(err, VerifyError::Unavailable(_)), "{err:?}");
    }
}
