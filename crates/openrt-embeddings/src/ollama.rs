//! Local neural embeddings over an Ollama-compatible HTTP endpoint
//! (M9.5, SEC-07 feature `ollama`). Every vector is produced by a real
//! embedding model run; nothing here fabricates or approximates vectors.
//!
//! Honest status model (mirrors `openrt-vlm`):
//! * `AVAILABLE` — a real probe embedding succeeded and its dimensionality
//!   was *measured from the response*;
//! * `UNAVAILABLE` — the endpoint cannot be reached;
//! * `MODEL_MISSING` — the endpoint is up but does not serve the model.

use std::time::{Duration, Instant};

use crate::{embed_id, validate_embedding, EmbedError, Embedding, EmbeddingStatus};

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const EMBED_TIMEOUT: Duration = Duration::from_secs(60);

/// Probe the endpoint by *actually embedding* a probe text — dimensionality
/// and availability are measured, never assumed. Runs the HTTP call on a
/// worker thread so it is safe from any caller context.
pub fn probe_embedding(endpoint: &str, model: &str) -> EmbeddingStatus {
    let endpoint = endpoint.trim_end_matches('/').to_string();
    let model = model.to_string();
    on_worker(move || async move {
        let started = Instant::now();
        match post_embed(
            &endpoint,
            &model,
            &["openrt embedding probe"],
            PROBE_TIMEOUT,
        )
        .await
        {
            RawOutcome::Vectors(v) => match v.into_iter().next() {
                Some(first) if !first.is_empty() => EmbeddingStatus::Available {
                    model,
                    dims: first.len(),
                    elapsed_ms: started.elapsed().as_millis(),
                },
                _ => EmbeddingStatus::Unavailable {
                    reason: "endpoint returned an empty probe embedding".into(),
                },
            },
            RawOutcome::ModelNotFound(reason) => EmbeddingStatus::ModelMissing { model, reason },
            RawOutcome::Unavailable(reason) => EmbeddingStatus::Unavailable { reason },
        }
    })
    .unwrap_or_else(|e| EmbeddingStatus::Unavailable {
        reason: e.to_string(),
    })
}

/// Real neural embedder over Ollama's embedding route. Implements the sync
/// [`crate::EmbeddingProvider`] trait by running each batch on a worker
/// thread (blocking HTTP never executes on a caller's async executor).
#[derive(Debug, Clone)]
pub struct OllamaEmbedder {
    pub endpoint: String,
    pub model: String,
    dims: usize,
}

impl OllamaEmbedder {
    /// `dims` MUST be the model's real dimensionality (take it from
    /// [`probe_embedding`]'s measured value); every response is validated
    /// against it, so a wrong value fails closed instead of corrupting an
    /// index.
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>, dims: usize) -> Self {
        assert!(dims > 0, "dims must be measured (> 0)");
        Self {
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            model: model.into(),
            dims,
        }
    }
}

impl crate::EmbeddingProvider for OllamaEmbedder {
    fn dims(&self) -> usize {
        self.dims
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Embedding>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        if texts.iter().any(|t| t.is_empty()) {
            return Err(EmbedError::EmptyInput);
        }
        let endpoint = self.endpoint.clone();
        let model = self.model.clone();
        let dims = self.dims;
        let owned: Vec<String> = texts.iter().map(|s| s.to_string()).collect();
        on_worker(move || async move { embed_async(&endpoint, &model, dims, &owned).await })
            .flatten()
    }

    fn describe(&self) -> &'static str {
        "ollama neural embedding model (semantic; local endpoint)"
    }
}

async fn embed_async(
    endpoint: &str,
    model: &str,
    dims: usize,
    texts: &[String],
) -> Result<Vec<Embedding>, EmbedError> {
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let vectors = match post_embed(endpoint, model, &refs, EMBED_TIMEOUT).await {
        RawOutcome::Vectors(v) => v,
        RawOutcome::ModelNotFound(reason) => return Err(EmbedError::ModelMissing { reason }),
        RawOutcome::Unavailable(reason) => return Err(EmbedError::Unavailable { reason }),
    };
    if vectors.len() != texts.len() {
        return Err(EmbedError::Http(format!(
            "endpoint returned {} embeddings for {} inputs",
            vectors.len(),
            texts.len()
        )));
    }
    let mut out = Vec::with_capacity(texts.len());
    for (text, raw) in texts.iter().zip(vectors) {
        let emb = build_embedding(text, raw, dims)?;
        validate_embedding(&emb, dims)?;
        out.push(emb);
    }
    Ok(out)
}

/// Build a validated [`Embedding`] from one backend vector: finite checks,
/// measured dimensionality check against `expected`, honest unit-length
/// normalization (math, not invention).
fn build_embedding(text: &str, raw: Vec<f32>, expected: usize) -> Result<Embedding, EmbedError> {
    if raw.len() != expected {
        return Err(EmbedError::WrongDims {
            id: embed_id(text),
            got: raw.len(),
            expected,
        });
    }
    if raw.iter().any(|v| !v.is_finite()) {
        return Err(EmbedError::NotFinite { id: embed_id(text) });
    }
    let norm = raw
        .iter()
        .map(|v| (*v as f64) * (*v as f64))
        .sum::<f64>()
        .sqrt();
    let values: Vec<f32> = if norm < 1e-12 {
        raw // zero vector: leave it; `validate_embedding` fails it honestly.
    } else {
        raw.into_iter().map(|v| (v as f64 / norm) as f32).collect()
    };
    let sha = crate::sha256_hex(text.as_bytes());
    Ok(Embedding {
        id: embed_id(text),
        source: text.to_string(),
        dims: values.len(),
        values,
        sha256: sha,
    })
}

/// Outcome of one embedding request (never fabricated content).
enum RawOutcome {
    Vectors(Vec<Vec<f32>>),
    /// Endpoint reachable, model not served (HTTP 404 `model … not found`).
    ModelNotFound(String),
    /// Endpoint unreachable / unusable.
    Unavailable(String),
}

/// POST `{endpoint}/api/embed` with a batch input, falling back to the
/// legacy per-text `/api/embeddings` route when the batch route does not
/// exist (older servers). 404s are classified strictly from the server's
/// JSON `error` field — a plain 404 page is a route miss, not a model miss.
async fn post_embed(endpoint: &str, model: &str, inputs: &[&str], timeout: Duration) -> RawOutcome {
    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(c) => c,
        Err(e) => return RawOutcome::Unavailable(format!("build http client: {e}")),
    };
    let url = format!("{endpoint}/api/embed");
    let body = serde_json::json!({ "model": model, "input": inputs });
    let resp = match client.post(&url).json(&body).send().await {
        Ok(r) => r,
        Err(e) => return RawOutcome::Unavailable(format!("cannot reach {endpoint}: {e}")),
    };
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.as_u16() == 404 {
        return match classify_404(&text) {
            Class404::Model(reason) => RawOutcome::ModelNotFound(reason),
            Class404::Route => match legacy_embed(&client, endpoint, model, inputs).await {
                Ok(v) => RawOutcome::Vectors(v),
                Err(o) => o,
            },
            Class404::Other(reason) => RawOutcome::Unavailable(reason),
        };
    }
    if !status.is_success() {
        return RawOutcome::Unavailable(format!("HTTP {status}: {}", truncate(&text)));
    }
    match parse_embeddings(&text) {
        Ok(v) => RawOutcome::Vectors(v),
        Err(e) => RawOutcome::Unavailable(e),
    }
}

/// Legacy `/api/embeddings` route: one prompt per request.
async fn legacy_embed(
    client: &reqwest::Client,
    endpoint: &str,
    model: &str,
    inputs: &[&str],
) -> Result<Vec<Vec<f32>>, RawOutcome> {
    let url = format!("{endpoint}/api/embeddings");
    let mut out = Vec::with_capacity(inputs.len());
    for prompt in inputs {
        let body = serde_json::json!({ "model": model, "prompt": prompt });
        let resp = match client.post(&url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                return Err(RawOutcome::Unavailable(format!(
                    "cannot reach {endpoint}: {e}"
                )))
            }
        };
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.as_u16() == 404 {
            return match classify_404(&text) {
                Class404::Model(reason) => Err(RawOutcome::ModelNotFound(reason)),
                Class404::Route => Err(RawOutcome::Unavailable(
                    "endpoint exposes no embedding route (/api/embed, /api/embeddings)".into(),
                )),
                Class404::Other(reason) => Err(RawOutcome::Unavailable(reason)),
            };
        }
        if !status.is_success() {
            return Err(RawOutcome::Unavailable(format!(
                "HTTP {status}: {}",
                truncate(&text)
            )));
        }
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            RawOutcome::Unavailable(format!("legacy embed response is not JSON: {e}"))
        })?;
        let embedding = v
            .get("embedding")
            .and_then(|e| e.as_array())
            .ok_or_else(|| {
                RawOutcome::Unavailable("legacy embed response lacks `embedding`".into())
            })?;
        out.push(parse_f32_array(embedding).map_err(RawOutcome::Unavailable)?);
    }
    Ok(out)
}

enum Class404 {
    /// The server's JSON error says the *model* is missing.
    Model(String),
    /// The route itself does not exist on this server.
    Route,
    /// Any other structured 404 error.
    Other(String),
}

fn classify_404(body: &str) -> Class404 {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(v) => match v.get("error").and_then(|e| e.as_str()) {
            Some(err) => {
                let lower = err.to_ascii_lowercase();
                if lower.contains("not found") {
                    Class404::Model(err.to_string())
                } else if lower.contains("unknown endpoint") || lower.contains("route") {
                    Class404::Route
                } else {
                    Class404::Other(err.to_string())
                }
            }
            None => Class404::Route,
        },
        Err(_) => Class404::Route, // plain 404 page: route miss, not a model claim
    }
}

/// Parse `{"embeddings": [[..], ..]}` (batch route).
fn parse_embeddings(body: &str) -> Result<Vec<Vec<f32>>, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("response is not JSON: {e}"))?;
    let arr = v
        .get("embeddings")
        .and_then(|e| e.as_array())
        .ok_or_else(|| "response lacks `embeddings`".to_string())?;
    arr.iter()
        .map(|inner| {
            let inner = inner
                .as_array()
                .ok_or_else(|| "`embeddings` entry is not an array".to_string())?;
            parse_f32_array(inner)
        })
        .collect()
}

fn parse_f32_array(arr: &[serde_json::Value]) -> Result<Vec<f32>, String> {
    arr.iter()
        .map(|x| {
            x.as_f64()
                .map(|f| f as f32)
                .ok_or_else(|| "embedding component is not a number".to_string())
        })
        .collect()
}

fn truncate(s: &str) -> String {
    s.chars().take(300).collect()
}

/// Run the HTTP future on a fresh worker thread with its own current-thread
/// runtime: safe from any caller context (including async executors), and a
/// panic inside the worker surfaces as a structured error instead of
/// unwinding the caller.
fn on_worker<T, Fut, F>(f: F) -> Result<T, EmbedError>
where
    T: Send + 'static,
    Fut: std::future::Future<Output = T> + Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
{
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        match rt {
            Ok(rt) => rt.block_on(f()),
            Err(e) => panic!("worker runtime build failed: {e}"),
        }
    })
    .join()
    .map_err(|_| EmbedError::Unavailable {
        reason: "embedding worker thread panicked".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EmbeddingProvider;

    const EMBEDDINGS_JSON: &str =
        r#"{"model":"all-minilm:latest","embeddings":[[0.1,0.2,0.3],[0.4,0.5,0.6]]}"#;

    #[test]
    fn batch_response_parsed() {
        let v = parse_embeddings(EMBEDDINGS_JSON).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0], vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn model_missing_404_is_classified_from_json_error() {
        let body = r#"{"error":"model \"nope:zz\" not found, try pulling it first"}"#;
        assert!(matches!(classify_404(body), Class404::Model(m) if m.contains("nope:zz")));
        // A plain 404 page is a ROUTE miss — never misreported as a model miss.
        assert!(matches!(
            classify_404("<html>Not Found</html>"),
            Class404::Route
        ));
        assert!(matches!(
            classify_404(r#"{"error":"unknown endpoint /x"}"#),
            Class404::Route
        ));
    }

    #[test]
    fn build_embedding_measures_and_normalizes() {
        let emb = build_embedding("hello", vec![3.0, 4.0], 2).unwrap();
        assert_eq!(emb.dims, 2);
        let norm: f32 = emb.values.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "unit length: {norm}");
        assert_eq!(emb.id, embed_id("hello"));
        // Wrong measured dims fail closed — never a silent reshape.
        assert!(matches!(
            build_embedding("hello", vec![1.0, 2.0, 3.0], 2),
            Err(EmbedError::WrongDims {
                got: 3,
                expected: 2,
                ..
            })
        ));
        // Non-finite backend output fails closed.
        assert!(matches!(
            build_embedding("x", vec![f32::NAN, 1.0], 2),
            Err(EmbedError::NotFinite { .. })
        ));
    }

    #[test]
    fn dead_endpoint_is_structured_unavailable() {
        // Port 1 on localhost: connection refused quickly, offline-honest.
        let status = probe_embedding("http://127.0.0.1:1", "all-minilm");
        assert!(!status.is_available());
        assert_eq!(status.label(), "UNAVAILABLE");
        assert!(status.dims().is_none());
    }

    #[test]
    fn status_labels_and_display() {
        let a = EmbeddingStatus::Available {
            model: "all-minilm".into(),
            dims: 384,
            elapsed_ms: 3,
        };
        assert_eq!(a.label(), "AVAILABLE");
        assert!(a.to_string().contains("384"));
        let m = EmbeddingStatus::ModelMissing {
            model: "nope".into(),
            reason: "model \"nope\" not found".into(),
        };
        assert_eq!(m.label(), "MODEL_MISSING");
        assert!(m.to_string().contains("nope"));
    }

    #[test]
    fn sync_embed_over_dead_endpoint_is_structured() {
        let e = OllamaEmbedder::new("http://127.0.0.1:1", "all-minilm", 384);
        let err = e.embed(&["hello"]).unwrap_err();
        assert!(matches!(err, EmbedError::Unavailable { .. }), "{err:?}");
    }

    #[test]
    fn empty_inputs_are_fail_closed() {
        let e = OllamaEmbedder::new("http://127.0.0.1:1", "all-minilm", 384);
        assert!(matches!(e.embed(&[""]), Err(EmbedError::EmptyInput)));
        assert_eq!(e.embed(&[]).unwrap(), Vec::<Embedding>::new());
    }
}
