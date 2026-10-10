//! Ollama provider — talks to a local Ollama server over HTTP.
//!
//! Feature-gated (`ollama`); never a hard dependency of the workspace. Uses
//! `/api/chat` (non-streaming) and `/api/tags`.

use std::time::Duration;

use openrt_core::Message;
use reqwest::Url;

use crate::error::ModelError;
use crate::provider::{ContextCapability, ModelCapabilities, ModelProvider};
use crate::toolcall::{canonical_tool_call_id, normalize_arguments};
use crate::types::{
    FinishReason, ModelInfo, ModelRequest, ModelResponse, ModelUsage, ProviderKind,
    ProviderToolCall, StreamEvent,
};

/// Provider for a single Ollama endpoint.
pub struct OllamaProvider {
    id: String,
    base_url: Url,
    default_model: Option<String>,
    client: reqwest::Client,
    timeout: Duration,
    capabilities: ModelCapabilities,
    /// Per-model native tool-support cache filled by [`Self::probe_tool_support`].
    tool_support: std::sync::Mutex<std::collections::HashMap<String, bool>>,
    /// Per-model `capabilities` array from `/api/show` (shared by the tool
    /// and vision probes — one fetch per tag).
    show_caps: std::sync::Mutex<std::collections::HashMap<String, Vec<String>>>,
    /// PART 21: raw body of the last successful chat response (capped) for
    /// the operator `/debug model-response` path.
    last_response: std::sync::Mutex<Option<String>>,
}

impl std::fmt::Debug for OllamaProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OllamaProvider")
            .field("id", &self.id)
            .field("base_url", &self.base_url.as_str())
            .field("default_model", &self.default_model)
            .finish()
    }
}

impl OllamaProvider {
    pub fn new(
        id: impl Into<String>,
        base_url: impl Into<String>,
        default_model: Option<String>,
    ) -> Result<Self, ModelError> {
        let base = Url::parse(&base_url.into())
            .map_err(|e| ModelError::NotConfigured(format!("invalid base_url: {e}")))?;
        let client = reqwest::Client::builder()
            .user_agent("openrt/0.1")
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| ModelError::Transport(e.to_string()))?;
        Ok(Self {
            id: id.into(),
            client,
            base_url: base,
            default_model,
            timeout: Duration::from_secs(90),
            tool_support: std::sync::Mutex::new(std::collections::HashMap::new()),
            show_caps: std::sync::Mutex::new(std::collections::HashMap::new()),
            last_response: std::sync::Mutex::new(None),
            capabilities: ModelCapabilities {
                supports_tools: true,
                supports_streaming: true,
                context_window: Some(8192),
            },
        })
    }

    /// Override the per-request timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn chat_url(&self) -> Url {
        self.base_url
            .join("api/chat")
            .unwrap_or_else(|_| self.base_url.clone())
    }

    fn tags_url(&self) -> Url {
        self.base_url
            .join("api/tags")
            .unwrap_or_else(|_| self.base_url.clone())
    }

    /// List locally available models (names) via `/api/tags`.
    pub async fn list_models(&self) -> Result<Vec<String>, ModelError> {
        let resp = self
            .client
            .get(self.tags_url())
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| ModelError::Transport(e.to_string()))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(self.http_error("tags", status.as_u16(), &body));
        }
        let parsed: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| ModelError::InvalidResponse(format!("tags: {e}")))?;
        let mut names = Vec::new();
        if let Some(models) = parsed.get("models").and_then(|m| m.as_array()) {
            for m in models {
                if let Some(name) = m.get("name").and_then(|n| n.as_str()) {
                    names.push(name.to_string());
                }
            }
        }
        Ok(names)
    }

    /// Simple liveness probe (PASS/WARN for `doctor`).
    pub async fn ping(&self) -> Result<(), ModelError> {
        let resp = self
            .client
            .get(self.tags_url())
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .map_err(|e| ModelError::Transport(e.to_string()))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(ModelError::Http {
                provider: self.id.clone(),
                status: resp.status().as_u16(),
                body: String::new(),
            })
        }
    }

    fn http_error(&self, _op: &str, status: u16, body: &str) -> ModelError {
        if status == 404 {
            ModelError::ModelNotFound(body.to_string())
        } else {
            ModelError::Http {
                provider: self.id.clone(),
                status,
                body: body.to_string(),
            }
        }
    }

    /// Fetch (and cache) the `/api/show` `capabilities` array for a tag.
    /// Shared by the tool and vision probes so one tag costs one request.
    async fn show_capabilities(&self, model: &str) -> Result<Vec<String>, ModelError> {
        if let Some(caps) = self
            .show_caps
            .lock()
            .expect("show_caps poisoned")
            .get(model)
        {
            return Ok(caps.clone());
        }
        let url = self
            .base_url
            .join("api/show")
            .map_err(|e| ModelError::InvalidResponse(format!("invalid base_url: {e}")))?;
        let resp = self
            .client
            .post(url)
            .json(&serde_json::json!({ "model": model }))
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| ModelError::Transport(e.to_string()))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(self.http_error("show", status.as_u16(), &body));
        }
        let v: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| ModelError::InvalidResponse(e.to_string()))?;
        let caps: Vec<String> = v
            .get("capabilities")
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| c.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        self.show_caps
            .lock()
            .expect("show_caps poisoned")
            .insert(model.to_string(), caps.clone());
        Ok(caps)
    }
}

#[async_trait::async_trait]
impl ModelProvider for OllamaProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn info(&self) -> ModelInfo {
        ModelInfo {
            id: self.id.clone(),
            kind: ProviderKind::Ollama,
            display_name: "Ollama".to_string(),
            default_model: self.default_model.clone(),
        }
    }

    async fn generate(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        let model = req.model_id.clone().or_else(|| self.default_model.clone());
        let model = model.ok_or_else(|| ModelError::NotConfigured(self.id.clone()))?;

        let payload = build_chat_payload(
            &model,
            &req.messages,
            &req.images,
            &req.parameters,
            &req.tools,
        );
        let resp = self
            .client
            .post(self.chat_url())
            .json(&payload)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| ModelError::Transport(e.to_string()))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(self.http_error("chat", status.as_u16(), &body));
        }
        parse_chat_response(&body).map(|mut r| {
            r.model = model.clone();
            // PART 21: keep the raw body for `/debug model-response`
            // (capped so a pathological response can't grow memory).
            const CAP: usize = 64 * 1024;
            let captured = if body.len() > CAP {
                format!("{}\n…[truncated at {CAP} bytes]", &body[..CAP])
            } else {
                body.clone()
            };
            *self.last_response.lock().expect("last_response poisoned") = Some(captured);
            r
        })
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.capabilities
    }

    /// Ollama accepts a per-request `options.num_ctx` override and does NOT
    /// certify a hard maximum: `default_context` is 8192 (what runs with no
    /// override) but that is a default, not a ceiling. An operator's larger
    /// request (e.g. 65536) must reach the backend unchanged — Ollama itself
    /// owns KV allocation and will error if its real limit is exceeded. This
    /// is the exact behavior the rigged request-builder tests assert: the
    /// provider never silently downgrades 65536 to 8192.
    fn context_capability(&self) -> ContextCapability {
        ContextCapability {
            default_context: self.capabilities.context_window,
            maximum_context: None,
            maximum_known: false,
            configurable: true,
        }
    }

    /// SSE streaming over the same `/api/chat` contract (Part B).
    ///
    /// `stream: true` payloads come back as `data:` lines. Only the backend's
    /// `done: true` frame ends the stream — a gap with no deltas is idle, so
    /// we keep waiting; a stream that ends without `done: true` is a
    /// structured provider error, never a silent truncated completion.
    async fn generate_stream(
        &self,
        req: ModelRequest,
        sink: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> Result<ModelResponse, ModelError> {
        let model = req.model_id.clone().or_else(|| self.default_model.clone());
        let model = model.ok_or_else(|| ModelError::NotConfigured(self.id.clone()))?;

        let mut payload = build_chat_payload(
            &model,
            &req.messages,
            &req.images,
            &req.parameters,
            &req.tools,
        );
        payload["stream"] = serde_json::json!(true);
        let mut resp = self
            .client
            .post(self.chat_url())
            .json(&payload)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| ModelError::Transport(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(self.http_error("chat", status.as_u16(), &body));
        }

        let mut text = String::new();
        let mut tool_calls: Vec<ProviderToolCall> = Vec::new();
        let mut usage = ModelUsage::default();
        let mut finish = FinishReason::Stop;
        let mut seen_done = false;
        let mut sse = SseParser::default();
        const CAP: usize = 64 * 1024;
        let mut captured = String::new();
        loop {
            match resp.chunk().await {
                Ok(Some(bytes)) => {
                    for line in sse.feed(&bytes) {
                        let Some(line) = line else { continue };
                        if captured.len() < CAP {
                            captured.push_str(&line);
                        }
                        let Ok(frame) = serde_json::from_str::<serde_json::Value>(&line) else {
                            continue;
                        };
                        let done = frame.get("done").and_then(|d| d.as_bool()).unwrap_or(false);
                        if let Some(m) = frame.get("message") {
                            if let Some(content) = m.get("content").and_then(|c| c.as_str()) {
                                if !content.is_empty() {
                                    text.push_str(content);
                                    if sink
                                        .send(StreamEvent::Token(content.to_string()))
                                        .await
                                        .is_err()
                                    {
                                        // Receiver dropped; keep parsing so the
                                        // final ModelResponse stays accurate.
                                    }
                                }
                            }
                            tool_calls.extend(parse_tool_calls(m.get("tool_calls"))?);
                        }
                        if let Some(r) = frame.get("done_reason").and_then(|r| r.as_str()) {
                            finish = match r {
                                "length" => FinishReason::Length,
                                "tool_calls" => FinishReason::ToolCalls,
                                _ => FinishReason::Stop,
                            };
                        }
                        if done {
                            usage = ModelUsage {
                                input_tokens: frame
                                    .get("prompt_eval_count")
                                    .and_then(|x| x.as_u64()),
                                output_tokens: frame.get("eval_count").and_then(|x| x.as_u64()),
                            };
                            seen_done = true;
                            break;
                        }
                    }
                    if seen_done {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => return Err(ModelError::Transport(e.to_string())),
            }
        }
        if !seen_done {
            return Err(ModelError::InvalidResponse(
                "stream ended without a `done` frame (idle was not completion)".into(),
            ));
        }
        if finish != FinishReason::ToolCalls && !tool_calls.is_empty() {
            finish = FinishReason::ToolCalls;
        }
        let captured = if captured.len() > CAP {
            format!("{}\n…[truncated at {CAP} bytes]", &captured[..CAP])
        } else {
            captured
        };
        *self.last_response.lock().expect("last_response poisoned") = Some(captured);
        let _ = sink.send(StreamEvent::Done).await;
        Ok(ModelResponse {
            model,
            text,
            tool_calls,
            finish_reason: finish,
            usage,
        })
    }

    /// Honest per-model probe: Ollama's `/api/show` reports a `capabilities`
    /// array (`completion`, `tools`, `vision`, …) for the exact tag. Results
    /// are cached per model tag; transport failures surface as errors (the
    /// caller decides how to display "unknown").
    async fn probe_tool_support(&self, model: &str) -> Result<Option<bool>, ModelError> {
        if let Some(v) = self
            .tool_support
            .lock()
            .expect("tool_support cache poisoned")
            .get(model)
        {
            return Ok(Some(*v));
        }
        let caps = self.show_capabilities(model).await?;
        let supported = caps.iter().any(|c| c == "tools");
        self.tool_support
            .lock()
            .expect("tool_support cache poisoned")
            .insert(model.to_string(), supported);
        Ok(Some(supported))
    }

    /// Honest per-model vision probe from the same `/api/show` capabilities
    /// (`vision` present = the tag accepts image inputs).
    async fn probe_vision(&self, model: &str) -> Result<Option<bool>, ModelError> {
        let caps = self.show_capabilities(model).await?;
        Ok(Some(caps.iter().any(|c| c == "vision")))
    }

    fn base_url(&self) -> Option<String> {
        Some(self.base_url.as_str().to_string())
    }

    fn last_raw_response(&self) -> Option<String> {
        self.last_response
            .lock()
            .expect("last_response poisoned")
            .clone()
    }
}

fn build_chat_payload(
    model: &str,
    messages: &[Message],
    images: &[String],
    params: &crate::types::ModelParameters,
    tools: &[serde_json::Value],
) -> serde_json::Value {
    // PART 7: resolve the canonical call id → tool name so tool-result
    // messages can carry Ollama's `tool_name` + `tool_call_id` correlation
    // fields (templates that key on tool name keep working; ids round-trip).
    let mut id_to_name: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for m in messages {
        if m.role == openrt_core::MessageRole::Assistant {
            for tc in &m.tool_calls {
                id_to_name.insert(tc.id.clone(), tc.tool_id.clone());
            }
        }
    }
    let mut wire: Vec<_> = messages
        .iter()
        .map(|m| {
            let mut w = core_message_to_wire(m);
            if m.role == openrt_core::MessageRole::Tool {
                if let Some(call_id) = &m.tool_call_id {
                    if let Some(name) = id_to_name.get(call_id) {
                        w["tool_name"] = serde_json::json!(name);
                    }
                }
            }
            w
        })
        .collect();
    // Images transport (M9.3): Ollama accepts base64 images on a message.
    // Attach them to the last user message (the one describing the task).
    if !images.is_empty() {
        let target = wire
            .iter()
            .rposition(|m| m.get("role") == Some(&serde_json::json!("user")))
            .or_else(|| wire.len().checked_sub(1));
        if let Some(i) = target {
            let arr: Vec<serde_json::Value> = images
                .iter()
                .map(|b64| serde_json::Value::String(b64.clone()))
                .collect();
            wire[i]["images"] = serde_json::Value::Array(arr);
        }
    }
    let mut payload = serde_json::json!({
        "model": model,
        "messages": wire,
        "stream": false,
    });
    // Generation steering (resource bounds): only emit options the caller
    // actually set — Ollama applies its own defaults otherwise.
    let mut opts = serde_json::Map::new();
    if let Some(t) = params.temperature {
        opts.insert("temperature".into(), serde_json::json!(t));
    }
    if let Some(t) = params.top_p {
        opts.insert("top_p".into(), serde_json::json!(t));
    }
    if let Some(n) = params.max_tokens {
        opts.insert("num_predict".into(), serde_json::json!(n));
    } else if params.unbounded_output {
        // Part B: explicit no-cap generation — Ollama's `num_predict = -1`.
        // Only when the caller asked for it; an unset cap keeps Ollama's own
        // default instead of quietly changing generation behavior.
        opts.insert("num_predict".into(), serde_json::json!(-1));
    }
    if let Some(ctx) = params.context_window {
        // Part B: per-request context window (num_ctx). A larger window is
        // more memory; the caller owns this bound, never the transport.
        opts.insert("num_ctx".into(), serde_json::json!(ctx));
    }
    if !params.stop.is_empty() {
        opts.insert("stop".into(), serde_json::json!(params.stop.clone()));
    }
    if let Some(seed) = params.seed {
        opts.insert("seed".into(), serde_json::json!(seed));
    }
    if !opts.is_empty() {
        payload["options"] = serde_json::Value::Object(opts);
    }
    // Tool definitions: translate the provider-neutral
    // {"name","description","parameters"} shape into Ollama's OpenAI-style
    // function envelope at the provider boundary. Entries already in wire
    // format pass through untouched.
    if !tools.is_empty() {
        let wire: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                if t.get("type").is_some() || t.get("function").is_some() {
                    t.clone()
                } else {
                    serde_json::json!({ "type": "function", "function": t })
                }
            })
            .collect();
        payload["tools"] = serde_json::Value::Array(wire);
    }
    payload
}

fn core_message_to_wire(m: &Message) -> serde_json::Value {
    let role = match m.role {
        openrt_core::MessageRole::System => "system",
        openrt_core::MessageRole::User => "user",
        openrt_core::MessageRole::Assistant => "assistant",
        openrt_core::MessageRole::Tool => "tool",
    };
    let mut obj = serde_json::json!({
        "role": role,
        "content": m.content,
    });
    if !m.tool_calls.is_empty() {
        let calls: Vec<_> = m
            .tool_calls
            .iter()
            .map(|tc| serde_json::json!({ "function": { "name": tc.tool_id, "arguments": tc.input } }))
            .collect();
        obj["tool_calls"] = serde_json::Value::Array(calls);
    }
    if m.role == openrt_core::MessageRole::Tool {
        // Ollama 0.33 `api.Message` carries `tool_name` + `tool_call_id`;
        // there is no `name` field (setting it was silently ignored and
        // dropped the correlation info). PART 7: the canonical call id
        // round-trips verbatim; the tool name is resolved by the caller
        // (build_chat_payload) from the assistant tool_calls history.
        if let Some(call_id) = &m.tool_call_id {
            obj["tool_call_id"] = serde_json::json!(call_id);
        }
    }
    obj
}

/// Parse a non-streaming `/api/chat` response body. Exposed (not pub-gated) so
/// unit tests can exercise parsing without a live server.
pub fn parse_chat_response(body: &str) -> Result<ModelResponse, ModelError> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| ModelError::InvalidResponse(e.to_string()))?;
    let message = v
        .get("message")
        .ok_or_else(|| ModelError::InvalidResponse("missing `message`".into()))?;
    let text = message
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or_default()
        .to_string();

    let tool_calls = parse_tool_calls(message.get("tool_calls"))?;

    let finish = if !tool_calls.is_empty() {
        FinishReason::ToolCalls
    } else {
        FinishReason::Stop
    };

    let usage = ModelUsage {
        input_tokens: v.get("prompt_eval_count").and_then(|x| x.as_u64()),
        output_tokens: v.get("eval_count").and_then(|x| x.as_u64()),
    };

    Ok(ModelResponse {
        model: v
            .get("model")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
        text,
        tool_calls,
        finish_reason: finish,
        usage,
    })
}

/// Map a `message.tool_calls` field to provider tool calls, sharing the
/// canonical-id and argument-normalization rules between the non-streaming
/// and SSE paths. `None` / `null` → no tool calls; a malformed value is a
/// structured provider error (never a panic, never a silent empty completion).
fn parse_tool_calls(
    calls_field: Option<&serde_json::Value>,
) -> Result<Vec<ProviderToolCall>, ModelError> {
    let Some(calls_field) = calls_field else {
        return Ok(Vec::new());
    };
    if calls_field.is_null() {
        return Ok(Vec::new());
    }
    let calls = calls_field.as_array().ok_or_else(|| {
        ModelError::InvalidResponse("`message.tool_calls` is not an array".into())
    })?;
    let mut out = Vec::new();
    for (i, call) in calls.iter().enumerate() {
        if !call.is_object() {
            return Err(ModelError::InvalidResponse(format!(
                "`message.tool_calls[{i}]` is not an object"
            )));
        }
        let name = call
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or_default()
            .to_string();
        // PART 4: structured object / array args pass through;
        // stringified JSON is parsed; prose stays raw for the agent
        // loop to reject as a structured failure.
        let arguments = normalize_arguments(call.get("function").and_then(|f| f.get("arguments")));
        // PART 2: string / numeric provider ids are preserved
        // canonically; missing ones get an OpenRT-generated id —
        // never `ollama_call_{i}`, never a rejected response.
        let id = canonical_tool_call_id(call.get("id"));
        out.push(ProviderToolCall {
            id,
            name,
            arguments,
        });
    }
    Ok(out)
}

/// Incremental Server-Sent-Events line parser. Ollama `/api/chat?stream=true`
/// frames are `data: {json}` lines separated by blank lines; a chunk may
/// split a line, so the leftover bytes stay buffered until the next `\n`.
#[derive(Default)]
struct SseParser {
    pending: Vec<u8>,
}

impl SseParser {
    /// Feed bytes; returns the payload of each complete `data:` line, `None`
    /// for any other line (`event:`, `event_msg`, comments, blanks).
    fn feed(&mut self, bytes: &[u8]) -> Vec<Option<String>> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(pos) = self.pending.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.pending.drain(..=pos).collect();
            line.pop(); // '\n'
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line).trim_end().to_string();
            out.push(line.strip_prefix("data:").map(|p| p.trim().to_string()));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_chat_response() {
        let body = r#"
        {
          "model": "qwen3:4b",
          "message": { "role": "assistant", "content": "hello from ollama" },
          "done": true,
          "prompt_eval_count": 10,
          "eval_count": 5
        }
        "#;
        let r = parse_chat_response(body).unwrap();
        assert_eq!(r.text, "hello from ollama");
        assert!(r.tool_calls.is_empty());
        assert_eq!(r.usage.input_tokens, Some(10));
        assert_eq!(r.usage.output_tokens, Some(5));
    }

    #[test]
    fn parse_tool_call_response() {
        let body = r#"
        {
          "model": "qwen3:4b",
          "message": {
            "role": "assistant",
            "content": "",
            "tool_calls": [
              { "function": { "name": "echo", "arguments": { "text": "hello" } } }
            ]
          },
          "done": true,
          "prompt_eval_count": 20
        }
        "#;
        let r = parse_chat_response(body).unwrap();
        assert_eq!(r.finish_reason, FinishReason::ToolCalls);
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].name, "echo");
        assert_eq!(r.tool_calls[0].arguments["text"], "hello");
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(parse_chat_response("not json").is_err());
    }

    #[test]
    fn wire_payload_maps_messages() {
        let msg = Message::user("hi");
        let v = core_message_to_wire(&msg);
        assert_eq!(v["role"], "user");
        assert_eq!(v["content"], "hi");
    }

    #[test]
    fn images_attach_to_last_user_message() {
        let msgs = vec![
            Message::system("sys"),
            Message::user("look at this"),
            Message::assistant("ok"),
        ];
        let v = build_chat_payload(
            "vision",
            &msgs,
            &["aW1n".to_string()],
            &crate::types::ModelParameters::default(),
            &[],
        );
        let wire = v["messages"].as_array().unwrap();
        assert!(wire[0].get("images").is_none());
        assert_eq!(wire[1]["images"][0], "aW1n");
        assert!(wire[2].get("images").is_none());
    }

    #[test]
    fn seed_is_carried_as_option() {
        let mut params = crate::types::ModelParameters::default();
        params.seed = Some(42);
        let v = build_chat_payload("m", &[Message::user("hi")], &[], &params, &[]);
        assert_eq!(v["options"]["seed"], 42);
    }

    #[test]
    fn max_tokens_becomes_num_predict_option() {
        let mut params = crate::types::ModelParameters::default();
        params.max_tokens = Some(256);
        params.temperature = Some(0.0);
        let v = build_chat_payload("m", &[Message::user("hi")], &[], &params, &[]);
        assert_eq!(v["options"]["num_predict"], 256);
        assert_eq!(v["options"]["temperature"], 0.0);
        let v = build_chat_payload(
            "m",
            &[Message::user("hi")],
            &[],
            &crate::types::ModelParameters::default(),
            &[],
        );
        assert!(v.get("options").is_none(), "unset params emit no options");
    }

    /// Part B: an explicit unbounded request maps to Ollama's `num_predict
    /// = -1`; `max_tokens` still wins when both are present; an unset cap
    /// never changes Ollama's default.
    #[test]
    fn unbounded_output_maps_to_num_predict_minus_one() {
        let mut params = crate::types::ModelParameters::default();
        params.unbounded_output = true;
        let v = build_chat_payload("m", &[Message::user("hi")], &[], &params, &[]);
        assert_eq!(v["options"]["num_predict"], -1);

        let mut both = crate::types::ModelParameters::default();
        both.unbounded_output = true;
        both.max_tokens = Some(64);
        let v = build_chat_payload("m", &[Message::user("hi")], &[], &both, &[]);
        assert_eq!(v["options"]["num_predict"], 64, "explicit cap wins");

        let v = build_chat_payload(
            "m",
            &[Message::user("hi")],
            &[],
            &crate::types::ModelParameters::default(),
            &[],
        );
        assert!(v.get("options").is_none(), "unset cap stays Ollama default");
    }

    /// Part B: a per-request `context_window` becomes Ollama's `num_ctx`; an
    /// unset window stays absent on the wire (Ollama default).
    #[test]
    fn context_window_maps_to_num_ctx() {
        let mut params = crate::types::ModelParameters::default();
        params.context_window = Some(65536);
        let v = build_chat_payload("m", &[Message::user("hi")], &[], &params, &[]);
        assert_eq!(v["options"]["num_ctx"], 65536);

        let v = build_chat_payload(
            "m",
            &[Message::user("hi")],
            &[],
            &crate::types::ModelParameters::default(),
            &[],
        );
        assert!(v.get("options").is_none(), "no window => no num_ctx");
    }

    #[test]
    fn no_images_means_no_images_key() {
        let v = build_chat_payload(
            "m",
            &[Message::user("hi")],
            &[],
            &crate::types::ModelParameters::default(),
            &[],
        );
        assert!(v["messages"][0].get("images").is_none());
    }

    #[test]
    fn tools_reach_the_wire_in_function_envelope() {
        let tools = vec![serde_json::json!({
            "name": "terminal.run",
            "description": "run a command",
            "parameters": {"type": "object", "properties": {"command": {"type": "string"}}}
        })];
        let v = build_chat_payload(
            "llama3.1:8b",
            &[Message::user("hi")],
            &[],
            &crate::types::ModelParameters::default(),
            &tools,
        );
        let wire = v["tools"].as_array().expect("tools attached");
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0]["type"], "function");
        assert_eq!(wire[0]["function"]["name"], "terminal.run");
        assert!(wire[0]["function"]["parameters"]["properties"]["command"].is_object());
        // No tools => key absent (backward-compatible payloads).
        let v2 = build_chat_payload(
            "m",
            &[Message::user("hi")],
            &[],
            &crate::types::ModelParameters::default(),
            &[],
        );
        assert!(v2.get("tools").is_none());
    }

    #[test]
    fn tools_already_enveloped_pass_through_untouched() {
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {"name": "echo", "description": "d", "parameters": {}}
        })];
        let v = build_chat_payload(
            "m",
            &[Message::user("hi")],
            &[],
            &crate::types::ModelParameters::default(),
            &tools,
        );
        assert_eq!(v["tools"][0], tools[0]);
    }

    /// Regression (PART 21): the exact selected model tag reaches the wire —
    /// no normalization, no hyphenization.
    #[test]
    fn exact_model_id_reaches_payload() {
        let v = build_chat_payload(
            "llama3.1:8b",
            &[Message::user("hi")],
            &[],
            &crate::types::ModelParameters::default(),
            &[],
        );
        assert_eq!(v["model"], "llama3.1:8b");
    }

    // ---- PART 14/22: malformed-response regression battery -------------

    /// PART 3: multiple tool calls in a single response are all mapped, with
    /// distinct ids and intact arguments.
    #[test]
    fn multiple_tool_calls_all_mapped_with_distinct_ids() {
        let body = r#"{
          "model": "llama3.1:8b",
          "message": {
            "role": "assistant",
            "content": "",
            "tool_calls": [
              { "function": { "name": "filesystem.read", "arguments": { "path": "a.txt" } } },
              { "function": { "name": "terminal.run", "arguments": { "command": "ls" } } }
            ]
          },
          "done": true
        }"#;
        let r = parse_chat_response(body).unwrap();
        assert_eq!(r.tool_calls.len(), 2);
        assert_eq!(r.tool_calls[0].name, "filesystem.read");
        assert_eq!(r.tool_calls[1].name, "terminal.run");
        assert_ne!(r.tool_calls[0].id, r.tool_calls[1].id);
        assert!(r.tool_calls[0].id.starts_with("openrt-call-"));
        assert_eq!(r.finish_reason, FinishReason::ToolCalls);
    }

    /// PART 3: text and tool_calls in the same response — both survive.
    #[test]
    fn text_and_tool_calls_coexist() {
        let body = r#"{
          "model": "llama3.1:8b",
          "message": {
            "role": "assistant",
            "content": "I will read the file first.",
            "tool_calls": [
              { "function": { "name": "filesystem.read", "arguments": { "path": "a.txt" } } }
            ]
          },
          "done": true
        }"#;
        let r = parse_chat_response(body).unwrap();
        assert_eq!(r.text, "I will read the file first.");
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.finish_reason, FinishReason::ToolCalls);
    }

    /// PART 3: stringified JSON arguments are parsed into structured values.
    #[test]
    fn stringified_json_arguments_are_parsed() {
        let body = r#"{
          "model": "m",
          "message": {
            "role": "assistant",
            "content": "",
            "tool_calls": [
              { "function": { "name": "echo", "arguments": "{\"text\":\"hello\"}" } }
            ]
          },
          "done": true
        }"#;
        let r = parse_chat_response(body).unwrap();
        assert_eq!(
            r.tool_calls[0].arguments,
            serde_json::json!({"text": "hello"})
        );
    }

    /// PART 4: prose arguments stay raw (a string) so the agent loop can
    /// reject them — they are never silently coerced or executed.
    #[test]
    fn prose_arguments_stay_raw_for_agent_rejection() {
        let body = r#"{
          "model": "m",
          "message": {
            "role": "assistant",
            "content": "",
            "tool_calls": [
              { "function": { "name": "echo", "arguments": "please echo this prose" } }
            ]
          },
          "done": true
        }"#;
        let r = parse_chat_response(body).unwrap();
        assert_eq!(
            r.tool_calls[0].arguments,
            serde_json::json!("please echo this prose")
        );
    }

    /// PART 4: missing arguments become `{}`.
    #[test]
    fn missing_arguments_become_empty_object() {
        let body = r#"{
          "model": "m",
          "message": {
            "role": "assistant",
            "content": "",
            "tool_calls": [ { "function": { "name": "system.info" } } ]
          },
          "done": true
        }"#;
        let r = parse_chat_response(body).unwrap();
        assert_eq!(r.tool_calls[0].arguments, serde_json::json!({}));
    }

    /// PART 2: numeric / missing ids are tolerated and canonicalized.
    #[test]
    fn numeric_and_missing_ids_are_canonicalized() {
        let body = r#"{
          "model": "m",
          "message": {
            "role": "assistant",
            "content": "",
            "tool_calls": [
              { "id": 42, "function": { "name": "echo", "arguments": {"text":"a"} } },
              { "function": { "name": "echo", "arguments": {"text":"b"} } }
            ]
          },
          "done": true
        }"#;
        let r = parse_chat_response(body).unwrap();
        assert_eq!(r.tool_calls[0].id, "42");
        assert!(r.tool_calls[1].id.starts_with("openrt-call-"));
        assert_ne!(r.tool_calls[0].id, r.tool_calls[1].id);
    }

    /// PART 14: `tool_calls` of the wrong type is a structured provider
    /// error (ModelError), not a panic and not a silent empty completion.
    #[test]
    fn invalid_tool_calls_type_is_structured_provider_error() {
        let body = r#"{
          "model": "m",
          "message": { "role": "assistant", "content": "", "tool_calls": "nope" },
          "done": true
        }"#;
        let err = parse_chat_response(body).unwrap_err();
        assert!(
            matches!(err, crate::ModelError::InvalidResponse(_)),
            "{err:?}"
        );
    }

    /// PART 14: a non-object entry inside `tool_calls` is a structured
    /// provider error.
    #[test]
    fn non_object_tool_call_entry_is_structured_provider_error() {
        let body = r#"{
          "model": "m",
          "message": { "role": "assistant", "content": "", "tool_calls": ["echo"] },
          "done": true
        }"#;
        let err = parse_chat_response(body).unwrap_err();
        assert!(
            matches!(err, crate::ModelError::InvalidResponse(_)),
            "{err:?}"
        );
    }

    /// PART 14: missing function name → empty name (unknown-tool structured
    /// failure at the agent loop), the rest of the call is still mapped.
    #[test]
    fn missing_function_name_maps_to_unknown_tool() {
        let body = r#"{
          "model": "m",
          "message": {
            "role": "assistant",
            "content": "",
            "tool_calls": [ { "function": { "arguments": {"x":1} } } ]
          },
          "done": true
        }"#;
        let r = parse_chat_response(body).unwrap();
        assert_eq!(r.tool_calls[0].name, "");
        assert_eq!(r.tool_calls[0].arguments, serde_json::json!({"x": 1}));
    }

    /// PART 6/7: tool round-trip on the wire — assistant tool_calls and the
    /// tool result's correlation fields are present, ids stable.
    #[test]
    fn tool_round_trip_reaches_the_wire() {
        let mut assistant = Message::assistant("reading now");
        assistant.tool_calls = vec![openrt_core::PendingToolCall {
            id: "openrt-call-1-7".into(),
            tool_id: "filesystem.read".into(),
            input: serde_json::json!({"path": "a.txt"}),
        }];
        let msgs = vec![
            Message::user("read a.txt"),
            assistant,
            Message::tool("openrt-call-1-7", "{\"ok\":true}"),
        ];
        let v = build_chat_payload(
            "llama3.1:8b",
            &msgs,
            &[],
            &crate::types::ModelParameters::default(),
            &[],
        );
        let wire = v["messages"].as_array().unwrap();
        // Assistant round-trips its tool_calls (id at the OpenRT layer, the
        // Ollama wire carries name+arguments per api.ToolCall).
        assert_eq!(
            wire[1]["tool_calls"][0]["function"]["name"],
            "filesystem.read"
        );
        assert_eq!(
            wire[1]["tool_calls"][0]["function"]["arguments"],
            serde_json::json!({"path": "a.txt"})
        );
        // Tool result correlates via tool_call_id + resolved tool_name
        // (Ollama 0.33 api.Message fields — NOT the ignored `name`).
        assert_eq!(wire[2]["tool_call_id"], "openrt-call-1-7");
        assert_eq!(wire[2]["tool_name"], "filesystem.read");
        assert!(wire[2].get("name").is_none(), "legacy `name` is not sent");
    }

    /// PART 7: provider-supplied string ids round-trip unchanged.
    #[test]
    fn provider_string_id_is_stable_across_parse_and_wire() {
        let body = r#"{
          "model": "m",
          "message": {
            "role": "assistant",
            "content": "",
            "tool_calls": [
              { "id": "call_abc", "function": { "name": "echo", "arguments": {"text":"x"} } }
            ]
          },
          "done": true
        }"#;
        let r = parse_chat_response(body).unwrap();
        assert_eq!(r.tool_calls[0].id, "call_abc");
        // The parsed id becomes the PendingToolCall id, which round-trips in
        // the tool message's tool_call_id on the next request.
        let mut assistant = Message::assistant("");
        assistant.tool_calls = vec![openrt_core::PendingToolCall {
            id: r.tool_calls[0].id.clone(),
            tool_id: r.tool_calls[0].name.clone(),
            input: r.tool_calls[0].arguments.clone(),
        }];
        let msgs = vec![assistant, Message::tool(&r.tool_calls[0].id, "ok")];
        let v = build_chat_payload(
            "m",
            &msgs,
            &[],
            &crate::types::ModelParameters::default(),
            &[],
        );
        let wire = v["messages"].as_array().unwrap();
        assert_eq!(wire[1]["tool_call_id"], "call_abc");
    }

    /// PART 21: the raw response body is captured for `/debug model-response`
    /// (capped, redaction happens at the display boundary).
    #[tokio::test]
    async fn last_raw_response_is_captured_after_generate() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body =
            r#"{"model":"llama3.1:8b","message":{"role":"assistant","content":"hi"},"done":true}"#;
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes()).await;
        });
        let p = OllamaProvider::new(
            "ollama",
            format!("http://{addr}"),
            Some("llama3.1:8b".into()),
        )
        .unwrap();
        assert!(p.last_raw_response().is_none(), "nothing captured yet");
        let out = p
            .generate(ModelRequest::new(vec![Message::user("hi")]))
            .await
            .unwrap();
        assert_eq!(out.text, "hi");
        let raw = p.last_raw_response().expect("body captured");
        assert!(raw.contains("\"done\":true"), "{raw}");
    }

    /// PART 19: the vision probe reads the SAME `/api/show` capabilities
    /// (cached per tag) — `vision` present = yes, absent = no, never a
    /// name guess.
    #[tokio::test]
    async fn vision_probe_reads_show_capabilities() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut hits = 0usize;
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                hits += 1;
                // First request: tools+vision; this proves both probes share
                // one fetch (the tools probe warms the cache).
                let body = if hits == 1 {
                    r#"{"capabilities":["completion","tools","vision"]}"#
                } else {
                    r#"{"capabilities":["completion"]}"#
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            }
        });
        let p = OllamaProvider::new("ollama", format!("http://{addr}"), None).unwrap();
        // Tools probe first (warms the capability cache)…
        assert_eq!(p.probe_tool_support("m").await.unwrap(), Some(true));
        // …then the vision probe answers from the SAME cached fetch.
        assert_eq!(p.probe_vision("m").await.unwrap(), Some(true));
    }

    // ---- Part B: SSE streaming -----------------------------------------

    /// The SSE line parser survives events split across arbitrary chunks and
    /// only yields `data:` payloads (non-data lines report `None`).
    #[test]
    fn sse_parser_handles_split_frames() {
        use crate::ollama::SseParser;
        let mut sse = SseParser::default();
        // First chunk carries a full event plus the start of the next line.
        let out = sse.feed(b"data: {\"done\":false}\n\ndata: {\"d");
        assert_eq!(out, vec![Some("{\"done\":false}".into()), None]);
        // The remainder of the split line arrives later.
        let out = sse.feed(b"one\":true}\n\n");
        assert_eq!(out, vec![Some("{\"done\":true}".into()), None]);
    }

    fn sse_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
        )
    }

    async fn serve_sse(body: String) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).await.unwrap();
            let raw = String::from_utf8_lossy(&buf[..n]).into_owned();
            stream
                .write_all(sse_response(&body).as_bytes())
                .await
                .unwrap();
            stream.shutdown().await.ok();
            assert!(raw.contains("\"stream\":true"), "{raw}");
        });
        format!("http://{addr}")
    }

    /// Idle ≠ completion: only the backend's `done:true` frame ends the
    /// stream; an empty (idle) frame contributes no token and no completion.
    #[tokio::test]
    async fn stream_accumulates_deltas_and_waits_for_done() {
        use crate::types::StreamEvent;
        let body = concat!(
            "data: {\"model\":\"qwen3:4b\",\"message\":{\"role\":\"assistant\",\"content\":\"Hel\"},\"done\":false}\n\n",
            "data: {\"model\":\"qwen3:4b\",\"message\":{\"role\":\"assistant\",\"content\":\"lo\"},\"done\":false}\n\n",
            "data: {\"model\":\"qwen3:4b\",\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":false}\n\n",
            "data: {\"model\":\"qwen3:4b\",\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"done_reason\":\"stop\",\"prompt_eval_count\":3,\"eval_count\":2}\n\n",
        );
        let url = serve_sse(body.to_string()).await;
        let p = OllamaProvider::new("ollama", url, None).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let out = p
            .generate_stream(
                ModelRequest::new(vec![Message::user("hi")]).with_model("qwen3:4b"),
                tx,
            )
            .await
            .unwrap();
        assert_eq!(out.text, "Hello");
        assert_eq!(out.finish_reason, FinishReason::Stop);
        assert_eq!(out.usage.input_tokens, Some(3));
        assert_eq!(out.usage.output_tokens, Some(2));
        // Events: two real deltas, then exactly one Done — the idle frame
        // produced nothing.
        assert_eq!(rx.recv().await, Some(StreamEvent::Token("Hel".into())));
        assert_eq!(rx.recv().await, Some(StreamEvent::Token("lo".into())));
        assert_eq!(rx.recv().await, Some(StreamEvent::Done));
        assert!(rx.recv().await.is_none(), "no events after Done");
        let raw = p.last_raw_response().unwrap();
        assert!(raw.contains("\"done\":true"), "{raw}");
    }

    /// A stream that ends without a `done` frame is a structured error —
    /// an idle tail is never silently promoted to a completion.
    #[tokio::test]
    async fn stream_without_done_is_a_structured_error() {
        let body = concat!(
            "data: {\"model\":\"qwen3:4b\",\"message\":{\"role\":\"assistant\",\"content\":\"partial\"},\"done\":false}\n\n",
        );
        let url = serve_sse(body.to_string()).await;
        let p = OllamaProvider::new("ollama", url, None).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let err = p
            .generate_stream(
                ModelRequest::new(vec![Message::user("hi")]).with_model("qwen3:4b"),
                tx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ModelError::InvalidResponse(_)), "{err:?}");
    }

    /// A streaming tool-call turn maps calls and the finish reason from the
    /// `done` frame's `done_reason`, sharing the non-streaming rules.
    #[tokio::test]
    async fn stream_maps_tool_calls_from_the_done_frame() {
        let body = concat!(
            "data: {\"model\":\"m\",\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":false}\n\n",
            "data: {\"model\":\"m\",\"message\":{\"role\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"function\":{\"name\":\"echo\",\"arguments\":{\"text\":\"hi\"}}}]},\"done\":true,\"done_reason\":\"tool_calls\",\"prompt_eval_count\":1,\"eval_count\":4}\n\n",
        );
        let url = serve_sse(body.to_string()).await;
        let p = OllamaProvider::new("ollama", url, None).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let out = p
            .generate_stream(
                ModelRequest::new(vec![Message::user("hi")]).with_model("m"),
                tx,
            )
            .await
            .unwrap();
        assert_eq!(out.finish_reason, FinishReason::ToolCalls);
        assert_eq!(out.tool_calls.len(), 1);
        assert_eq!(out.tool_calls[0].name, "echo");
        assert_eq!(out.tool_calls[0].arguments["text"], "hi");
        assert!(out.tool_calls[0].id.starts_with("openrt-call-"));
        assert_eq!(rx.recv().await, Some(StreamEvent::Done));
    }
}
