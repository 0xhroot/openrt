//! Phase 9 (M9.3) — visual-language pipeline over the model-provider
//! infrastructure.
//!
//! Honesty rules baked into this crate:
//! * The images transport exists only where it really exists: the Ollama
//!   endpoint path carries base64 images on a user message; providers with no
//!   image transport reject image requests instead of dropping them.
//! * [`probe_vision`] reports one of exactly three honest states —
//!   `AVAILABLE`, `UNAVAILABLE` (endpoint unreachable / feature not built),
//!   or `MODEL_MISSING` (endpoint alive, requested model absent) — with a
//!   concrete reason. Nothing here fabricates a vision answer.
//! * The engineering-drawing route ([`drawing`]) always produces real OCR
//!   regions/labels when a backend exists; the VLM part is attempted only
//!   when configured and reported structurally when it fails.

use std::fmt;
use std::time::Duration;

/// Default vision model probed/used when no `--model` override is given.
pub const DEFAULT_VLM_MODEL: &str = "moondream";

/// Default endpoint (matches the workspace's default Ollama provider).
pub const DEFAULT_VLM_ENDPOINT: &str = "http://127.0.0.1:11434";

/// Ask used for plain image description.
pub const DESCRIBE_PROMPT: &str = "Describe this image in one concise sentence.";

/// Cap on listing probe time (a hung endpoint is `UNAVAILABLE`).
#[cfg_attr(not(feature = "ollama"), allow(dead_code))] // used by the images transport only
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on a single vision generation call.
#[cfg_attr(not(feature = "ollama"), allow(dead_code))] // used by the images transport only
const GENERATE_TIMEOUT: Duration = Duration::from_secs(180);

/// Output bound for vision answers (resource limit: a small local model can
/// otherwise loop forever on structured prompts).
#[cfg_attr(not(feature = "ollama"), allow(dead_code))] // used by the images transport only
const VLM_MAX_TOKENS: u32 = 256;

/// Sampling seeds rotated when a completion comes back empty (EOS-first
/// outputs are seed-dependent on small local models). The model, prompt, and
/// image never change between attempts — only the sampling seed.
#[cfg_attr(not(feature = "ollama"), allow(dead_code))] // used by the images transport only
const SEED_ROTATION: [i64; 4] = [42, 1, 7, 1234];

/// Sampling temperature requested explicitly. Some published modelfiles pin
/// a degenerate sampling setup; an explicit request-side temperature keeps
/// completions non-empty (verified against the reference local model).
#[cfg_attr(not(feature = "ollama"), allow(dead_code))] // used by the images transport only
const VLM_TEMPERATURE: f32 = 0.8;

/// Structured vision failure. Every variant carries enough context to be
/// reported verbatim (no bare "failed").
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VlmError {
    #[error("vision backend unavailable: {reason}")]
    Unavailable { reason: String },
    #[error("vision model `{model}` missing: {reason}")]
    ModelMissing {
        model: String,
        reason: String,
        available: Vec<String>,
    },
    #[error("endpoint returned HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("invalid vision response: {0}")]
    InvalidResponse(String),
    #[error("vision transport error: {0}")]
    Transport(String),
}

/// Honest availability state of a vision model on an endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisionStatus {
    /// Endpoint reachable and the requested model is present.
    Available { endpoint: String, model: String },
    /// Endpoint unreachable, probe failed, or this build has no transport.
    Unavailable { reason: String },
    /// Endpoint reachable but the requested model is not installed.
    ModelMissing {
        model: String,
        reason: String,
        available: Vec<String>,
    },
}

impl VisionStatus {
    /// Stable status word for matrices/CLI output.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Available { .. } => "AVAILABLE",
            Self::Unavailable { .. } => "UNAVAILABLE",
            Self::ModelMissing { .. } => "MODEL_MISSING",
        }
    }

    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available { .. })
    }

    /// The concrete human-readable reason (empty only when available).
    pub fn reason(&self) -> String {
        match self {
            Self::Available { endpoint, model } => format!("{model} at {endpoint}"),
            Self::Unavailable { reason } => reason.clone(),
            Self::ModelMissing { reason, .. } => reason.clone(),
        }
    }
}

impl fmt::Display for VisionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Available { endpoint, model } => {
                write!(f, "AVAILABLE — {model} at {endpoint}")
            }
            Self::Unavailable { reason } => write!(f, "UNAVAILABLE — {reason}"),
            Self::ModelMissing { model, reason, .. } => {
                write!(f, "MODEL_MISSING — {model}: {reason}")
            }
        }
    }
}

/// Outcome of one successful vision generation. Carries provenance: the model
/// that actually answered, the endpoint, and the real token usage when the
/// backend reported it.
#[derive(Debug, Clone, PartialEq)]
pub struct VlmAnswer {
    pub text: String,
    pub model: String,
    pub endpoint: String,
    pub prompt: String,
    pub elapsed_ms: u128,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// Match a requested model against an endpoint's installed-model listing.
/// Accepts an exact name (`moondream:latest`) or a bare name matched against
/// any tagged form (`moondream` → `moondream:latest`). Case-insensitive
/// because Ollama model names are lowercase.
pub fn match_model<'a>(names: &'a [String], want: &str) -> Option<&'a String> {
    let want = want.to_lowercase();
    names
        .iter()
        .find(|n| {
            let n = n.to_lowercase();
            n == want || n.starts_with(&format!("{want}:"))
        })
        .or_else(|| {
            // Requested `name:tag` against a listing entry `name` without tag.
            names.iter().find(|n| n.eq_ignore_ascii_case(&want))
        })
}

/// Classify a listing probe result into an honest [`VisionStatus`].
/// `listing` is `Ok(names)` when the endpoint answered within the probe
/// timeout, `Err(reason)` otherwise.
pub fn classify_probe(
    endpoint: &str,
    model: &str,
    listing: Result<Vec<String>, String>,
) -> VisionStatus {
    match listing {
        Err(reason) => VisionStatus::Unavailable { reason },
        Ok(names) => match match_model(&names, model) {
            Some(found) => VisionStatus::Available {
                endpoint: endpoint.trim_end_matches('/').to_string(),
                model: found.clone(),
            },
            None => VisionStatus::ModelMissing {
                model: model.to_string(),
                reason: format!(
                    "not in the endpoint's model list — pull it (`ollama pull {model}`) \
                     or pass a different model"
                ),
                available: names,
            },
        },
    }
}

/// Probe an endpoint for a vision model without invoking it (doctor /
/// capability matrix). Never fabricates availability: an unreachable
/// endpoint or a build without the `ollama` transport is `UNAVAILABLE`.
pub async fn probe_vision(endpoint: &str, model: &str) -> VisionStatus {
    probe_impl(endpoint, model).await
}

/// Run one vision generation (fail-closed: the endpoint is probed first, so
/// a missing model or dead endpoint surfaces as the structured
/// [`VlmError`] instead of a fabricated answer).
pub async fn ask_vision(
    endpoint: &str,
    model: &str,
    image: &[u8],
    prompt: &str,
) -> Result<VlmAnswer, VlmError> {
    ask_impl(endpoint, model, image, prompt).await
}

#[cfg(feature = "ollama")]
mod transport {
    use super::*;
    use base64::Engine;
    use openrt_core::Message;
    use openrt_models::ollama::OllamaProvider;
    use openrt_models::{ModelError, ModelProvider, ModelRequest};

    /// Vision client over one Ollama-compatible endpoint. The images
    /// transport rides the existing model-provider infrastructure
    /// ([`OllamaProvider`]/`ModelRequest::images`).
    pub struct VlmClient {
        endpoint: String,
        model: String,
        provider: OllamaProvider,
    }

    impl VlmClient {
        pub fn new(endpoint: &str, model: &str) -> Result<Self, VlmError> {
            let provider = OllamaProvider::new("vision", endpoint, Some(model.to_string()))
                .map_err(|e| VlmError::Unavailable {
                    reason: format!("invalid endpoint `{endpoint}`: {e}"),
                })?;
            Ok(Self {
                endpoint: endpoint.trim_end_matches('/').to_string(),
                model: model.to_string(),
                provider,
            })
        }

        pub fn endpoint(&self) -> &str {
            &self.endpoint
        }

        pub fn model(&self) -> &str {
            &self.model
        }

        /// Probe the endpoint's model listing (bounded by STATUS_TIMEOUT).
        pub async fn status(&self) -> VisionStatus {
            let listing =
                match tokio::time::timeout(STATUS_TIMEOUT, self.provider.list_models()).await {
                    Err(_) => Err(format!(
                        "model listing timed out after {}s",
                        STATUS_TIMEOUT.as_secs()
                    )),
                    Ok(Err(e)) => Err(e.to_string()),
                    Ok(Ok(names)) => Ok(names),
                };
            classify_probe(&self.endpoint, &self.model, listing)
        }

        /// One vision generation with the images transport.
        pub async fn ask(&self, image: &[u8], prompt: &str) -> Result<VlmAnswer, VlmError> {
            match self.status().await {
                VisionStatus::Available { .. } => {}
                VisionStatus::Unavailable { reason } => {
                    return Err(VlmError::Unavailable { reason })
                }
                VisionStatus::ModelMissing {
                    model,
                    reason,
                    available,
                } => {
                    return Err(VlmError::ModelMissing {
                        model,
                        reason,
                        available,
                    })
                }
            }
            let b64 = base64::engine::general_purpose::STANDARD.encode(image);
            let started = std::time::Instant::now();
            // Bounded empty-answer retry: small local models sometimes emit
            // an EOS-first completion depending on the sampling seed. Rotate
            // seeds (same model, same prompt — nothing else changes) and
            // only report failure when every attempt came back empty.
            let mut empty_err = VlmError::InvalidResponse("no attempt made".into());
            for seed in SEED_ROTATION {
                let mut req = ModelRequest::new(vec![Message::user(prompt)])
                    .with_model(self.model.clone())
                    .with_image(b64.clone());
                req.parameters.max_tokens = Some(VLM_MAX_TOKENS);
                req.parameters.temperature = Some(VLM_TEMPERATURE);
                req.parameters.seed = Some(seed);
                let outcome =
                    tokio::time::timeout(GENERATE_TIMEOUT, self.provider.generate(req)).await;
                let resp = match outcome {
                    Err(_) => {
                        return Err(VlmError::Unavailable {
                            reason: format!(
                                "vision model timed out after {}s",
                                GENERATE_TIMEOUT.as_secs()
                            ),
                        })
                    }
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => return Err(map_model_error(e)),
                };
                if resp.text.trim().is_empty() {
                    empty_err = VlmError::InvalidResponse(format!(
                        "model returned an empty answer (seed {seed})"
                    ));
                    continue;
                }
                let model = if resp.model.is_empty() {
                    self.model.clone()
                } else {
                    resp.model
                };
                return Ok(VlmAnswer {
                    text: resp.text,
                    model,
                    endpoint: self.endpoint.clone(),
                    prompt: prompt.to_string(),
                    elapsed_ms: started.elapsed().as_millis(),
                    input_tokens: resp.usage.input_tokens,
                    output_tokens: resp.usage.output_tokens,
                });
            }
            Err(empty_err)
        }
    }

    fn map_model_error(e: ModelError) -> VlmError {
        match e {
            ModelError::ModelNotFound(m) => VlmError::ModelMissing {
                model: m,
                reason: "endpoint rejected the model at generate time".into(),
                available: Vec::new(),
            },
            ModelError::Transport(t) => VlmError::Transport(t),
            ModelError::Timeout(s) => VlmError::Unavailable {
                reason: format!("request timed out after {s}s"),
            },
            ModelError::NotConfigured(e) => VlmError::Unavailable {
                reason: format!("endpoint not configured: {e}"),
            },
            ModelError::Http { status, body, .. } => VlmError::Http { status, body },
            ModelError::InvalidResponse(e) => VlmError::InvalidResponse(e),
            other => VlmError::Transport(other.to_string()),
        }
    }

    pub async fn probe_impl(endpoint: &str, model: &str) -> VisionStatus {
        match VlmClient::new(endpoint, model) {
            Ok(c) => c.status().await,
            Err(e) => VisionStatus::Unavailable {
                reason: e.to_string(),
            },
        }
    }

    pub async fn ask_impl(
        endpoint: &str,
        model: &str,
        image: &[u8],
        prompt: &str,
    ) -> Result<VlmAnswer, VlmError> {
        let client = VlmClient::new(endpoint, model)?;
        client.ask(image, prompt).await
    }
}

#[cfg(feature = "ollama")]
pub use transport::VlmClient;

#[cfg(feature = "ollama")]
async fn probe_impl(endpoint: &str, model: &str) -> VisionStatus {
    transport::probe_impl(endpoint, model).await
}

#[cfg(feature = "ollama")]
async fn ask_impl(
    endpoint: &str,
    model: &str,
    image: &[u8],
    prompt: &str,
) -> Result<VlmAnswer, VlmError> {
    transport::ask_impl(endpoint, model, image, prompt).await
}

#[cfg(not(feature = "ollama"))]
async fn probe_impl(_endpoint: &str, _model: &str) -> VisionStatus {
    VisionStatus::Unavailable {
        reason: "built without the `ollama` feature — vision model calls unavailable".into(),
    }
}

#[cfg(not(feature = "ollama"))]
async fn ask_impl(
    _endpoint: &str,
    _model: &str,
    _image: &[u8],
    _prompt: &str,
) -> Result<VlmAnswer, VlmError> {
    Err(VlmError::Unavailable {
        reason: "built without the `ollama` feature — vision model calls unavailable".into(),
    })
}

/// Engineering-drawing route (M9.3): real OCR regions/labels from the
/// drawing image, plus semantic part descriptions from a vision model when
/// one is configured and actually available.
pub mod drawing {
    use super::{
        ask_vision, VisionStatus, VlmAnswer, VlmError, DEFAULT_VLM_ENDPOINT, DEFAULT_VLM_MODEL,
    };
    use openrt_ingest::{OcrError, OcrRegion, Psm, Tesseract};

    /// Prompt for the semantic part of the drawing route: literal inventory
    /// of visible parts/labels, no interpretation.
    pub const DRAWING_PROMPT: &str = "List the parts, numbers, and labels visible in this \
         engineering drawing. Only state what is literally visible.";

    /// VLM outcome embedded in a drawing report.
    #[derive(Debug, PartialEq)]
    pub enum VlmPart {
        /// No vision model was requested for this analysis.
        NotRequested,
        /// The model answered (provenance in the answer).
        Answered(VlmAnswer),
        /// The model could not answer — the structured reason is preserved.
        Failed { error: VlmError },
    }

    /// One drawing report: real OCR geometry first, vision second.
    #[derive(Debug, PartialEq)]
    pub struct DrawingReport {
        pub width: Option<u32>,
        pub height: Option<u32>,
        /// Honest `name|version` of the OCR backend (regions are its output).
        pub ocr_backend: String,
        /// Real label regions with bbox + confidence.
        pub regions: Vec<OcrRegion>,
        pub vlm: VlmPart,
    }

    /// Why the drawing route could not run.
    #[derive(Debug, thiserror::Error)]
    pub enum DrawingError {
        #[error("input is not a supported drawing image: {0}")]
        Unsupported(String),
        #[error("no OCR backend: {reason}")]
        OcrMissing { reason: String },
        #[error(transparent)]
        Ocr(#[from] OcrError),
    }

    /// Analyze a drawing image. `vision = Some((endpoint, model))` requests
    /// the semantic part; a failing model never fails the report — the
    /// structured error is embedded instead (OCR regions are still real).
    pub async fn analyze(
        bytes: &[u8],
        name: Option<&str>,
        vision: Option<(&str, &str)>,
    ) -> Result<DrawingReport, DrawingError> {
        let det = openrt_ingest::detect(bytes, name);
        if !det.status.is_supported() {
            return Err(DrawingError::Unsupported(
                det.status
                    .reason()
                    .unwrap_or("unsupported input")
                    .to_string(),
            ));
        }
        let ext = match det.kind {
            openrt_ingest::FormatKind::Png => "png",
            openrt_ingest::FormatKind::Jpeg => "jpg",
            openrt_ingest::FormatKind::Tiff => "tif",
            other => {
                return Err(DrawingError::Unsupported(format!(
                    "`{}` is not a raster image the drawing route reads",
                    other.label()
                )))
            }
        };
        let t = Tesseract::detect().ok_or_else(|| DrawingError::OcrMissing {
            reason: "no `tesseract` binary on PATH — drawing labels/regions unavailable".into(),
        })?;
        let page = t.recognize_bytes(bytes, ext, Psm::Auto)?;
        let regions = page.regions();
        let (width, height) = openrt_ingest::image_dims(bytes).unzip();
        let vlm = match vision {
            None => VlmPart::NotRequested,
            Some((endpoint, model)) => {
                // The client rotates sampling seeds internally on empty
                // completions; a failure here is the structured reason
                // (never a fabricated part list).
                match ask_vision(endpoint, model, bytes, DRAWING_PROMPT).await {
                    Ok(answer) => VlmPart::Answered(answer),
                    Err(error) => VlmPart::Failed { error },
                }
            }
        };
        Ok(DrawingReport {
            width,
            height,
            ocr_backend: t.label(),
            regions,
            vlm,
        })
    }

    /// Probe-only helper for capability rows that combine OCR + VLM.
    pub async fn vision_status(endpoint: Option<&str>, model: Option<&str>) -> VisionStatus {
        super::probe_vision(
            endpoint.unwrap_or(DEFAULT_VLM_ENDPOINT),
            model.unwrap_or(DEFAULT_VLM_MODEL),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn classify_available_exact_and_tagged() {
        let s = classify_probe(
            "http://127.0.0.1:11434/",
            "moondream:latest",
            Ok(names(&["moondream:latest", "qwen2.5:0.5b"])),
        );
        assert_eq!(s.label(), "AVAILABLE");
        match &s {
            VisionStatus::Available { endpoint, model } => {
                assert_eq!(endpoint, "http://127.0.0.1:11434"); // trailing slash trimmed
                assert_eq!(model, "moondream:latest");
            }
            other => panic!("expected Available, got {other:?}"),
        }

        let s = classify_probe("http://e", "moondream", Ok(names(&["moondream:latest"])));
        assert!(s.is_available(), "{s}");
    }

    #[test]
    fn classify_missing_lists_installed_models() {
        let s = classify_probe(
            "http://127.0.0.1:11434",
            "moondream",
            Ok(names(&["all-minilm:latest", "qwen2.5:0.5b"])),
        );
        assert_eq!(s.label(), "MODEL_MISSING");
        match &s {
            VisionStatus::ModelMissing {
                model,
                reason,
                available,
            } => {
                assert_eq!(model, "moondream");
                assert!(reason.contains("ollama pull moondream"), "{reason}");
                assert_eq!(available.len(), 2);
            }
            other => panic!("expected ModelMissing, got {other:?}"),
        }
    }

    #[test]
    fn classify_unreachable_is_unavailable_with_reason() {
        let s = classify_probe(
            "http://127.0.0.1:1",
            "moondream",
            Err("connection refused".into()),
        );
        assert_eq!(s.label(), "UNAVAILABLE");
        assert!(s.reason().contains("connection refused"));
    }

    #[test]
    fn labels_and_display_are_stable() {
        let a = VisionStatus::Available {
            endpoint: "http://x".into(),
            model: "m".into(),
        };
        assert_eq!(a.to_string(), "AVAILABLE — m at http://x");
        let u = VisionStatus::Unavailable {
            reason: "no transport".into(),
        };
        assert_eq!(u.to_string(), "UNAVAILABLE — no transport");
        let m = VisionStatus::ModelMissing {
            model: "v".into(),
            reason: "absent".into(),
            available: vec![],
        };
        assert_eq!(m.to_string(), "MODEL_MISSING — v: absent");
    }

    #[test]
    fn match_model_is_case_insensitive_and_tag_aware() {
        let list = names(&["Moondream:latest"]);
        assert!(match_model(&list, "moondream").is_some());
        assert!(match_model(&list, "MOONDREAM:LATEST").is_some());
        assert!(match_model(&list, "other").is_none());
    }

    #[test]
    fn error_variants_render_with_context() {
        let e = VlmError::Unavailable {
            reason: "endpoint down".into(),
        };
        assert!(e.to_string().contains("endpoint down"));
        let e = VlmError::ModelMissing {
            model: "v".into(),
            reason: "absent".into(),
            available: vec!["a:latest".into()],
        };
        assert!(e.to_string().contains("`v` missing"));
    }

    /// Feature-off honesty: without the `ollama` transport the facades must
    /// report UNAVAILABLE / the structured error — never a fake answer.
    #[cfg(not(feature = "ollama"))]
    #[tokio::test]
    async fn without_ollama_feature_status_is_unavailable() {
        let s = probe_vision(DEFAULT_VLM_ENDPOINT, DEFAULT_VLM_MODEL).await;
        assert_eq!(s.label(), "UNAVAILABLE");
        assert!(s.reason().contains("`ollama` feature"), "{}", s.reason());
        let e = ask_vision(DEFAULT_VLM_ENDPOINT, DEFAULT_VLM_MODEL, b"x", "hi")
            .await
            .unwrap_err();
        assert!(matches!(e, VlmError::Unavailable { .. }));
    }

    #[cfg(feature = "ollama")]
    #[test]
    fn client_rejects_invalid_endpoint_without_network() {
        let err = match VlmClient::new("not a url", "moondream") {
            Err(e) => e,
            Ok(_) => panic!("invalid endpoint must be rejected"),
        };
        assert!(
            matches!(err, VlmError::Unavailable { ref reason } if reason.contains("invalid endpoint")),
            "{err:?}"
        );
    }

    #[cfg(feature = "ollama")]
    #[tokio::test]
    async fn dead_endpoint_probe_is_unavailable() {
        // Port 1 on localhost refuses instantly — deterministic, no network.
        let s = probe_vision("http://127.0.0.1:1", DEFAULT_VLM_MODEL).await;
        assert_eq!(s.label(), "UNAVAILABLE", "{s}");
        assert!(!s.reason().is_empty());
    }
}
