//! Provider-agnostic request/response and metadata types.

use openrt_core::Message;
use serde::{Deserialize, Serialize};

/// Generation steering parameters. All optional — providers apply defaults.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ModelParameters {
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_tokens: Option<u32>,
    pub stop: Vec<String>,
    /// Sampling seed (providers that support deterministic sampling — Ollama
    /// `options.seed`). Used by the VLM client for bounded empty-answer
    /// retries; `None` means the provider's default.
    #[serde(default)]
    pub seed: Option<i64>,
    /// Explicit "no generation cap" (Ollama `options.num_predict = -1`).
    /// `max_tokens = Some(_)` wins when both are set; `false` leaves the
    /// provider default. Ollama-only — an OpenAI-compatible endpoint has no
    /// unbounded flag and ignores this.
    #[serde(default)]
    pub unbounded_output: bool,
    /// Per-request context window (Ollama `options.num_ctx`). None = the
    /// provider's own default. Ollama-only: OpenAI-compatible endpoints have
    /// no `num_ctx` and must not receive it.
    #[serde(default)]
    pub context_window: Option<u32>,
}

/// One event from a streaming generation
/// ([`ModelProvider::generate_stream`](crate::ModelProvider::generate_stream)).
///
/// Text deltas arrive as the backend emits them; the method completes only
/// once the provider's `done` frame (or transport end) is seen. A quiet gap
/// with no tokens is **idle** — never a completion; callers must keep
/// waiting for `Done`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StreamEvent {
    /// A real text delta from the provider (never synthesized by OpenRT).
    Token(String),
    /// The generation finished; the full result (text, tool calls, usage,
    /// finish reason) is returned by the streaming method itself.
    Done,
}

/// One generation request: messages + parameters + the tool definitions
/// available for this turn. `model_id` overrides the provider's configured
/// default model when set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub model_id: Option<String>,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub parameters: ModelParameters,
    /// Base64-encoded input images (images transport, M9.3). Providers that
    /// cannot consume images must reject the request rather than silently
    /// drop them.
    #[serde(default)]
    pub images: Vec<String>,
    /// Tool definitions available to the model for this turn, in the
    /// provider-neutral `ToolSpec::to_provider_tool()` shape
    /// (`{"name","description","parameters"}`). Providers translate this to
    /// their native wire format at the boundary. Informational only — the
    /// schema never grants authority; every call still goes through the
    /// gate. `#[serde(default)]` keeps pre-tool requests deserializing.
    #[serde(default)]
    pub tools: Vec<serde_json::Value>,
}

impl ModelRequest {
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            model_id: None,
            messages,
            parameters: ModelParameters::default(),
            images: Vec::new(),
            tools: Vec::new(),
        }
    }

    pub fn with_model(mut self, model_id: impl Into<String>) -> Self {
        self.model_id = Some(model_id.into());
        self
    }

    /// Attach one base64-encoded input image.
    pub fn with_image(mut self, b64: impl Into<String>) -> Self {
        self.images.push(b64.into());
        self
    }

    /// Attach the tool definitions for this turn (empty = no tools offered).
    pub fn with_tools(mut self, tools: Vec<serde_json::Value>) -> Self {
        self.tools = tools;
        self
    }

    /// Attach explicit generation parameters. Taken verbatim; the effective
    /// `context_window` is bounded upstream by the provider's declared window
    /// when one is advertised.
    pub fn with_parameters(mut self, parameters: ModelParameters) -> Self {
        self.parameters = parameters;
        self
    }
}

/// Why a generation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// Produced a complete response.
    Stop,
    /// Hit the configured token/context limit.
    Length,
    /// Produced one or more tool calls.
    ToolCalls,
    /// Provider-level interruption.
    Interrupted,
}

/// Token usage for a generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ModelUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// A tool call produced by the model (provider-agnostic shape).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// The result of one generation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelResponse {
    /// Model that actually produced this response.
    pub model: String,
    pub text: String,
    pub tool_calls: Vec<ProviderToolCall>,
    pub finish_reason: FinishReason,
    pub usage: ModelUsage,
}

impl ModelResponse {
    pub fn with_text(model: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            text: text.into(),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: ModelUsage::default(),
        }
    }
}

/// Kind of a provider port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Ollama,
    Mock,
    /// llama.cpp `server` binary over the OpenAI-compatible HTTP API.
    Llamacpp,
    /// vLLM OpenAI-compatible server.
    Vllm,
}

impl ProviderKind {
    /// Stable lowercase label for logs/reports.
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::Mock => "mock",
            Self::Llamacpp => "llamacpp",
            Self::Vllm => "vllm",
        }
    }
}

/// Static metadata about a provider port.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub kind: ProviderKind,
    pub display_name: String,
    pub default_model: Option<String>,
}

impl ModelInfo {
    pub fn new(id: impl Into<String>, kind: ProviderKind, default_model: Option<String>) -> Self {
        Self {
            id: id.into(),
            kind,
            display_name: "default".to_string(),
            default_model,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip() {
        let req = ModelRequest::new(vec![Message::user("hi")]).with_model("qwen3:4b");
        let json = serde_json::to_string(&req).unwrap();
        let back: ModelRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.model_id.as_deref(), Some("qwen3:4b"));
    }

    #[test]
    fn legacy_request_without_tools_deserializes() {
        // Requests serialized before the tools contract existed must keep
        // loading (serde default).
        let legacy = r#"{"model_id":null,"messages":[],"parameters":{"temperature":null,"top_p":null,"max_tokens":null,"stop":[],"seed":null},"images":[]}"#;
        let back: ModelRequest = serde_json::from_str(legacy).unwrap();
        assert!(back.tools.is_empty());
    }

    #[test]
    fn tools_round_trip() {
        let tool =
            serde_json::json!({"name":"echo","description":"d","parameters":{"type":"object"}});
        let req = ModelRequest::new(vec![Message::user("hi")]).with_tools(vec![tool.clone()]);
        let back: ModelRequest =
            serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(back.tools, vec![tool]);
    }
}
