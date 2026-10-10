//! Rerankers (M9.6): a deterministic, always-available reranker plus a
//! real model-backed reranker over the model-provider infrastructure.
//!
//! Honesty rules:
//! * the deterministic reranker is pure token-coverage — it never claims a
//!   model was involved;
//! * the model reranker either returns a model-produced permutation or a
//!   structured [`RerankError`] — on any failure the caller keeps the fused
//!   order and records the failure (no fabricated rerank scores);
//! * `rerank_score` for model reranking is *positional* (the model ranked
//!   slots, it did not emit numeric relevance) — documented, not disguised.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use openrt_core::Message;
use openrt_models::{ModelProvider, ModelRequest};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::bm25::tokenize;
use crate::hybrid::{HybridHit, RerankKind};

/// Which reranker a hybrid search should apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RerankMode {
    /// Keep fused order; `rerank_score` stays `None`.
    Off,
    /// Deterministic token-coverage reranking (pure function, no backend).
    Deterministic,
    /// Real model-backed reranking through a `ModelProvider`.
    Model,
}

/// Structured model-rerank failure — callers degrade to fused order and
/// surface the reason; they never invent a rerank.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RerankError {
    #[error("model rerank unavailable: {0}")]
    Unavailable(String),
    #[error("model rerank timed out after {0:?}")]
    Timeout(Duration),
    #[error("model rerank returned an invalid response: {0}")]
    InvalidResponse(String),
}

/// Deterministic reranker: re-scores hits by query-token coverage over the
/// candidate text (unique tokens matched / unique query tokens), keeping the
/// fused order inside ties. Pure, synchronous, backend-free.
pub fn deterministic_rerank(
    query: &str,
    mut hits: Vec<HybridHit>,
    texts: &HashMap<(String, String), String>,
) -> Vec<HybridHit> {
    let qtokens: HashSet<String> = tokenize(query).into_iter().collect();
    let qn = qtokens.len() as f32;
    for h in hits.iter_mut() {
        let coverage = if qn == 0.0 {
            0.0
        } else {
            let text = texts
                .get(&(h.document_id.clone(), h.chunk_id.clone()))
                .map(String::as_str)
                .unwrap_or("");
            let ttokens: HashSet<String> = tokenize(text).into_iter().collect();
            qtokens.intersection(&ttokens).count() as f32 / qn
        };
        h.rerank_score = Some(coverage);
        h.rerank_kind = Some(RerankKind::Deterministic);
    }
    hits.sort_by(|a, b| {
        b.rerank_score
            .partial_cmp(&a.rerank_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                b.fused_score
                    .partial_cmp(&a.fused_score)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(a.document_id.cmp(&b.document_id))
            .then(a.chunk_id.cmp(&b.chunk_id))
    });
    hits
}

/// Model-rerank resource bounds (M9.6: bounded, never unbounded context).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRerankOptions {
    /// Maximum passages handed to the model (top of the fused list).
    pub max_passages: usize,
    /// Per-passage character cap in the prompt.
    pub text_cap: usize,
    /// Wall-clock cap for the model call.
    pub timeout: Duration,
}

impl Default for ModelRerankOptions {
    fn default() -> Self {
        Self {
            max_passages: 20,
            text_cap: 1500,
            timeout: Duration::from_secs(30),
        }
    }
}

/// Model-backed rerank: ask a real model for a permutation of the top fused
/// passages. Returns the reordered hits with positional `rerank_score`s, or
/// a structured error (caller keeps fused order).
pub async fn model_rerank(
    provider: &dyn ModelProvider,
    model: &str,
    query: &str,
    mut hits: Vec<HybridHit>,
    texts: &HashMap<(String, String), String>,
    opts: &ModelRerankOptions,
) -> Result<Vec<HybridHit>, RerankError> {
    if hits.is_empty() {
        return Ok(hits);
    }
    let take = hits.len().min(opts.max_passages);
    let head: Vec<HybridHit> = hits.drain(..take).collect();
    let tail = hits; // beyond the model budget: keeps fused order honestly

    let mut passages = String::new();
    for (i, h) in head.iter().enumerate() {
        let text = texts
            .get(&(h.document_id.clone(), h.chunk_id.clone()))
            .map(String::as_str)
            .unwrap_or("");
        let clipped: String = if text.chars().count() > opts.text_cap {
            let mut clipped: String = text.chars().take(opts.text_cap).collect();
            clipped.push('…');
            clipped
        } else {
            text.to_string()
        };
        passages.push_str(&format!("[{}] {}\n", i + 1, clipped));
    }
    let system = "You are a search relevance reranker. Given a query and \
numbered passages, order the passages from most to least relevant to the \
query. Respond with ONLY a JSON object of the form {\"order\":[n1,n2,...]} \
containing every passage number exactly once. No prose, no code fences.";
    let user = format!("Query: {query}\n\nPassages:\n{passages}\nRespond with JSON only.");
    let mut req = ModelRequest::new(vec![Message::system(system), Message::user(user)]);
    req.model_id = Some(model.to_string());

    let resp = tokio::time::timeout(opts.timeout, provider.generate(req))
        .await
        .map_err(|_| RerankError::Timeout(opts.timeout))?
        .map_err(|e| RerankError::Unavailable(e.to_string()))?;

    let order = parse_order(&resp.text, head.len())?;
    let n = head.len() as f32;
    let mut reordered: Vec<Option<HybridHit>> = Vec::with_capacity(head.len());
    for _ in 0..head.len() {
        reordered.push(None);
    }
    for (pos, &idx1) in order.iter().enumerate() {
        let mut h = head[idx1 - 1].clone();
        // Positional score: the model ranked slots; it emitted no numbers.
        h.rerank_score = Some((n - pos as f32) / n);
        h.rerank_kind = Some(RerankKind::Model);
        reordered[pos] = Some(h);
    }
    let mut out: Vec<HybridHit> = reordered
        .into_iter()
        .map(|h| h.expect("permutation"))
        .collect();
    out.extend(tail);
    Ok(out)
}

/// Parse `{"order":[...]}` from a model response. Every index 1..=n must
/// appear exactly once — anything else is an honest `InvalidResponse`.
fn parse_order(text: &str, n: usize) -> Result<Vec<usize>, RerankError> {
    let start = text
        .find('{')
        .ok_or_else(|| RerankError::InvalidResponse("no JSON object in response".into()))?;
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
    let end = end.ok_or_else(|| RerankError::InvalidResponse("unbalanced JSON object".into()))?;
    let v: serde_json::Value = serde_json::from_str(&text[start..end])
        .map_err(|e| RerankError::InvalidResponse(format!("bad JSON: {e}")))?;
    let arr = v
        .get("order")
        .and_then(|o| o.as_array())
        .ok_or_else(|| RerankError::InvalidResponse("missing `order` array".into()))?;
    let mut order = Vec::with_capacity(arr.len());
    for x in arr {
        let idx = x
            .as_u64()
            .ok_or_else(|| RerankError::InvalidResponse(format!("non-integer index: {x}")))?;
        order.push(idx as usize);
    }
    if order.len() != n {
        return Err(RerankError::InvalidResponse(format!(
            "expected {n} indices, got {}",
            order.len()
        )));
    }
    let mut sorted = order.clone();
    sorted.sort_unstable();
    let want: Vec<usize> = (1..=n).collect();
    if sorted != want {
        return Err(RerankError::InvalidResponse(format!(
            "indices are not a permutation of 1..={n}: {order:?}"
        )));
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bm25::Candidate;
    use crate::hybrid::{fuse, FusionOptions};
    use async_trait::async_trait;
    use openrt_models::{ModelCapabilities, ModelError, ModelInfo, ModelResponse, ProviderKind};

    fn hit(id: &str, fused: f32) -> HybridHit {
        HybridHit {
            document_id: id.to_string(),
            chunk_id: "c1".to_string(),
            page: None,
            source: crate::hybrid::RetrievalSource::Hybrid,
            bm25_score: Some(1.0),
            dense_score: Some(0.5),
            fused_score: fused,
            rerank_score: None,
            rerank_kind: None,
        }
    }

    fn texts(pairs: &[(&str, &str)]) -> HashMap<(String, String), String> {
        pairs
            .iter()
            .map(|(id, t)| ((id.to_string(), "c1".to_string()), t.to_string()))
            .collect()
    }

    /// Fixture where the fused order prefers the dense/lexical-favored doc
    /// with *partial* query coverage, and the full-coverage doc sits second;
    /// deterministic coverage reranking must lift it (TEST 6: rerank lift).
    #[test]
    fn deterministic_reranker_lifts_lexically_relevant_doc() {
        let long = format!("kitten quokka {}", "filler ".repeat(100));
        let candidates = vec![
            Candidate::new("partial", "c1", None, "quokka quokka quokka"),
            Candidate::new("full", "c1", None, long.clone()),
            Candidate::new("other", "c1", None, "filler filler filler"),
        ];
        // Dense (synthetic) also ranks `partial` first.
        let dense = vec![
            crate::hybrid::DenseHit::new("partial", "c1", 0.9),
            crate::hybrid::DenseHit::new("full", "c1", 0.2),
        ];
        let mut fused = fuse(
            &candidates,
            &dense,
            "kitten quokka",
            &FusionOptions::default(),
        );
        assert_eq!(fused.len(), 2, "{fused:?}");
        // Fixture precondition: fused favors the partial-coverage doc.
        assert_eq!(
            fused[0].document_id, "partial",
            "fixture must start with partial coverage: {fused:?}"
        );
        let map = texts(&[("partial", "quokka quokka quokka"), ("full", &long)]);
        let before_len = fused.len();
        fused = deterministic_rerank("kitten quokka", fused, &map);
        assert_eq!(fused.len(), before_len);
        assert_eq!(
            fused[0].document_id, "full",
            "rerank lift failed: {fused:?}"
        );
        assert_eq!(fused[0].rerank_kind, Some(RerankKind::Deterministic));
        assert_eq!(fused[0].rerank_score, Some(1.0));
        assert_eq!(fused[1].rerank_score, Some(0.5));
    }

    #[test]
    fn deterministic_reranker_missing_text_scores_zero_honestly() {
        let mut hits = vec![hit("a", 0.5), hit("b", 0.4)];
        hits = deterministic_rerank("anything", hits, &HashMap::new());
        assert_eq!(hits[0].rerank_score, Some(0.0));
        assert_eq!(hits[1].rerank_score, Some(0.0));
        // tie ⇒ fused order preserved (a before b)
        assert_eq!(hits[0].document_id, "a");
    }

    struct Canned {
        text: String,
    }

    #[async_trait]
    impl ModelProvider for Canned {
        fn id(&self) -> &str {
            "canned"
        }
        fn info(&self) -> ModelInfo {
            ModelInfo::new("canned", ProviderKind::Mock, None)
        }
        async fn generate(&self, _req: ModelRequest) -> Result<ModelResponse, ModelError> {
            Ok(ModelResponse::with_text("canned", self.text.clone()))
        }
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                supports_tools: false,
                supports_streaming: false,
                context_window: None,
            }
        }
    }

    struct Slow {
        delay: Duration,
    }

    #[async_trait]
    impl ModelProvider for Slow {
        fn id(&self) -> &str {
            "slow"
        }
        fn info(&self) -> ModelInfo {
            ModelInfo::new("slow", ProviderKind::Mock, None)
        }
        async fn generate(&self, _req: ModelRequest) -> Result<ModelResponse, ModelError> {
            tokio::time::sleep(self.delay).await;
            Ok(ModelResponse::with_text("slow", "{}"))
        }
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                supports_tools: false,
                supports_streaming: false,
                context_window: None,
            }
        }
    }

    struct Broken;

    #[async_trait]
    impl ModelProvider for Broken {
        fn id(&self) -> &str {
            "broken"
        }
        fn info(&self) -> ModelInfo {
            ModelInfo::new("broken", ProviderKind::Mock, None)
        }
        async fn generate(&self, _req: ModelRequest) -> Result<ModelResponse, ModelError> {
            Err(ModelError::Transport("connection refused".into()))
        }
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                supports_tools: false,
                supports_streaming: false,
                context_window: None,
            }
        }
    }

    fn map() -> HashMap<(String, String), String> {
        texts(&[
            ("a", "first passage about alpha"),
            ("b", "second passage about beta"),
            ("c", "third passage about gamma"),
        ])
    }

    #[tokio::test]
    async fn model_rerank_applies_real_permutation_with_positional_scores() {
        let hits = vec![hit("a", 0.03), hit("b", 0.02), hit("c", 0.01)];
        let provider = Canned {
            text: "Here you go: {\"order\":[3,1,2]} done.".into(),
        };
        let out = model_rerank(
            &provider,
            "m",
            "q",
            hits,
            &map(),
            &ModelRerankOptions::default(),
        )
        .await
        .unwrap();
        let ids: Vec<&str> = out.iter().map(|h| h.document_id.as_str()).collect();
        assert_eq!(ids, vec!["c", "a", "b"]);
        assert_eq!(out[0].rerank_kind, Some(RerankKind::Model));
        assert_eq!(out[0].rerank_score, Some(1.0));
        assert!((out[1].rerank_score.unwrap() - 2.0 / 3.0).abs() < 1e-6);
        assert!((out[2].rerank_score.unwrap() - 1.0 / 3.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn model_rerank_garbage_is_structured_invalid_response() {
        let hits = vec![hit("a", 0.03), hit("b", 0.02)];
        let provider = Canned {
            text: "I think passage one is better overall.".into(),
        };
        let err = model_rerank(
            &provider,
            "m",
            "q",
            hits,
            &map(),
            &ModelRerankOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RerankError::InvalidResponse(_)), "{err}");
    }

    #[tokio::test]
    async fn model_rerank_non_permutation_is_rejected() {
        let hits = vec![hit("a", 0.03), hit("b", 0.02), hit("c", 0.01)];
        for bad in ["{\"order\":[1,1,2]}", "{\"order\":[1,2]}", "{\"other\":[]}"] {
            let provider = Canned { text: bad.into() };
            let err = model_rerank(
                &provider,
                "m",
                "q",
                hits.clone(),
                &map(),
                &ModelRerankOptions::default(),
            )
            .await
            .unwrap_err();
            assert!(
                matches!(err, RerankError::InvalidResponse(_)),
                "{bad} → {err}"
            );
        }
    }

    #[tokio::test]
    async fn model_rerank_provider_failure_is_unavailable() {
        let hits = vec![hit("a", 0.03)];
        let err = model_rerank(
            &Broken,
            "m",
            "q",
            hits,
            &map(),
            &ModelRerankOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RerankError::Unavailable(_)), "{err}");
        assert!(err.to_string().contains("connection refused"));
    }

    #[tokio::test]
    async fn model_rerank_timeout_is_structured() {
        let hits = vec![hit("a", 0.03)];
        let slow = Slow {
            delay: Duration::from_millis(300),
        };
        let opts = ModelRerankOptions {
            timeout: Duration::from_millis(50),
            ..ModelRerankOptions::default()
        };
        let err = model_rerank(&slow, "m", "q", hits, &map(), &opts)
            .await
            .unwrap_err();
        assert!(matches!(err, RerankError::Timeout(_)), "{err}");
    }

    #[tokio::test]
    async fn model_rerank_caps_passages_and_keeps_tail_in_fused_order() {
        let hits: Vec<HybridHit> = (0..5)
            .map(|i| hit(&format!("p{i}"), 0.05 - i as f32 * 0.01))
            .collect();
        let map: HashMap<(String, String), String> = (0..5)
            .map(|i| ((format!("p{i}"), "c1".to_string()), format!("passage {i}")))
            .collect();
        let provider = Canned {
            text: "{\"order\":[2,1]}".into(),
        };
        let opts = ModelRerankOptions {
            max_passages: 2,
            ..ModelRerankOptions::default()
        };
        let out = model_rerank(&provider, "m", "q", hits, &map, &opts)
            .await
            .unwrap();
        assert_eq!(out.len(), 5);
        // top-2 reordered by the model
        assert_eq!(out[0].document_id, "p1");
        assert_eq!(out[1].document_id, "p0");
        assert_eq!(out[0].rerank_kind, Some(RerankKind::Model));
        // tail keeps fused order and NO rerank score (never fabricated)
        let tail: Vec<&str> = out[2..].iter().map(|h| h.document_id.as_str()).collect();
        assert_eq!(tail, vec!["p2", "p3", "p4"]);
        assert!(out[2].rerank_score.is_none());
        assert!(out[2].rerank_kind.is_none());
    }
}
