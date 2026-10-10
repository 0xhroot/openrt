//! Minimal OpenAI-compatible HTTP client shared by the llama.cpp and vLLM
//! provider ports (and usable by any `openrt-models` consumer in front of an
//! OpenAI-compatible `/v1/chat/completions` endpoint).
//!
//! Feature-gated (`openai-http`): like `ollama`, the default build never
//! hard-depends on the HTTP/TLS stack.

use std::time::Duration;

use openrt_core::Message;
use serde::{Deserialize, Serialize};

use crate::error::ModelError;
use crate::toolcall::{canonical_tool_call_id, normalize_arguments};
use crate::types::{FinishReason, ModelRequest, ModelResponse, ModelUsage, ProviderToolCall};

/// Connection details for one OpenAI-compatible endpoint.
#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    /// Base URL of the server, e.g. `http://localhost:8080`. A trailing `/`
    /// is tolerated (stripped before joining API paths).
    pub base_url: String,
    /// Default model, used when the request carries no `model_id`.
    pub model: Option<String>,
    /// Per-request timeout, in seconds.
    pub timeout_secs: u64,
    /// Reused HTTP client; callers customise proxies and limits here.
    pub client: reqwest::Client,
}

impl OpenAiConfig {
    /// Convenience constructor with a fresh `reqwest` client (correct for
    /// single-host setups; use the struct directly to customise).
    pub fn new(base_url: impl Into<String>, timeout_secs: u64) -> Self {
        Self {
            base_url: base_url.into(),
            model: None,
            timeout_secs,
            client: reqwest::Client::new(),
        }
    }
}

/// POST `{base}/v1/chat/completions` and map the non-streaming response.
pub async fn chat_completions(
    cfg: &OpenAiConfig,
    req: &ModelRequest,
) -> Result<ModelResponse, ModelError> {
    let model = req.model_id.clone().or_else(|| cfg.model.clone());
    let model = model.ok_or_else(|| ModelError::NotConfigured(cfg.base_url.clone()))?;

    // Honesty (M9.3): this OpenAI-compatible text transport carries no image
    // parts — a vision request must fail closed, never run without its image.
    if !req.images.is_empty() {
        return Err(ModelError::NotConfigured(
            "provider does not support image inputs (no images transport)".into(),
        ));
    }

    let payload = ChatRequest {
        model: model.clone(),
        messages: req.messages.iter().map(request_message_from_core).collect(),
        temperature: req.parameters.temperature,
        top_p: req.parameters.top_p,
        max_tokens: req.parameters.max_tokens,
        stop: req.parameters.stop.clone(),
        tools: req
            .tools
            .iter()
            .map(|t| {
                if t.get("type").is_some() || t.get("function").is_some() {
                    t.clone()
                } else {
                    serde_json::json!({ "type": "function", "function": t })
                }
            })
            .collect(),
    };

    let url = format!("{}/v1/chat/completions", cfg.base_url.trim_end_matches('/'));
    let resp = cfg
        .client
        .post(url)
        .json(&payload)
        .timeout(Duration::from_secs(cfg.timeout_secs))
        .send()
        .await
        .map_err(|e| ModelError::Transport(e.to_string()))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(ModelError::Http {
            provider: cfg.base_url.clone(),
            status: status.as_u16(),
            body,
        });
    }
    parse_chat_response(&body)
}

/// Parse an OpenAI-compatible chat-completion body into the core
/// [`ModelResponse`]. Public so `openrt doctor` can self-test the REAL
/// response-parsing path with a canned body (no network, no name guessing).
pub fn parse_chat_response(body: &str) -> Result<ModelResponse, ModelError> {
    let parsed: ChatEndpointResponse =
        serde_json::from_str(body).map_err(|e| ModelError::InvalidResponse(e.to_string()))?;
    let choice = parsed
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| ModelError::InvalidResponse("empty `choices` array".into()))?;

    let tool_calls: Vec<ProviderToolCall> = choice
        .message
        .tool_calls
        .into_iter()
        .map(|tc| ProviderToolCall {
            // PART 2: never reject an otherwise valid call for a missing /
            // null / non-string id — canonicalize at the boundary.
            id: canonical_tool_call_id(tc.id.as_ref()),
            name: tc.function.name.unwrap_or_default(),
            arguments: normalize_arguments(tc.function.arguments.as_ref()),
        })
        .collect();

    // PART 14: `finish_reason: "tool_calls"` with no calls is a malformed
    // response — surface a structured provider error, never a silent
    // empty completion or fabricated execution.
    let finish_reason = map_finish_reason(choice.finish_reason.as_deref());
    if finish_reason == FinishReason::ToolCalls && tool_calls.is_empty() {
        return Err(ModelError::InvalidResponse(
            "finish_reason `tool_calls` but response contains no tool calls".into(),
        ));
    }

    let usage = parsed
        .usage
        .map(|u| ModelUsage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
        })
        .unwrap_or_default();

    Ok(ModelResponse {
        model: parsed.model,
        text: choice.message.content.unwrap_or_default(),
        tool_calls,
        finish_reason,
        usage,
    })
}

/// GET `{base}/v1/models` and return the advertised model ids.
pub async fn list_models(cfg: &OpenAiConfig) -> Result<Vec<String>, ModelError> {
    let url = format!("{}/v1/models", cfg.base_url.trim_end_matches('/'));
    let resp = cfg
        .client
        .get(url)
        .timeout(Duration::from_secs(cfg.timeout_secs))
        .send()
        .await
        .map_err(|e| ModelError::Transport(e.to_string()))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(ModelError::Http {
            provider: cfg.base_url.clone(),
            status: status.as_u16(),
            body,
        });
    }
    let parsed: ModelsResponse =
        serde_json::from_str(&body).map_err(|e| ModelError::InvalidResponse(e.to_string()))?;
    Ok(parsed.data.into_iter().map(|entry| entry.id).collect())
}

/// GET `{base}/health`. `200` is ready; any other status is not ready.
pub async fn is_ready(cfg: &OpenAiConfig) -> Result<bool, ModelError> {
    let url = format!("{}/health", cfg.base_url.trim_end_matches('/'));
    let resp = cfg
        .client
        .get(url)
        .timeout(Duration::from_secs(cfg.timeout_secs))
        .send()
        .await
        .map_err(|e| ModelError::Transport(e.to_string()))?;
    Ok(resp.status() == reqwest::StatusCode::OK)
}

fn map_finish_reason(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("stop") => FinishReason::Stop,
        Some("length") => FinishReason::Length,
        Some("tool_calls") => FinishReason::ToolCalls,
        _ => FinishReason::Interrupted,
    }
}

fn request_message_from_core(m: &Message) -> RequestMessage {
    let role = match m.role {
        openrt_core::MessageRole::System => "system",
        openrt_core::MessageRole::User => "user",
        openrt_core::MessageRole::Assistant => "assistant",
        openrt_core::MessageRole::Tool => "tool",
    };
    // PART 6/7: keep the tool round-trip on the wire. Assistant turns carry
    // their `tool_calls` (arguments re-serialized as the JSON string the
    // OpenAI protocol requires); tool turns carry `tool_call_id` so the
    // server can correlate the result with the call. The canonical id is
    // never re-invented here — it is echoed verbatim.
    let tool_calls = if role == "assistant" && !m.tool_calls.is_empty() {
        Some(
            m.tool_calls
                .iter()
                .map(|tc| {
                    serde_json::json!({
                        "id": tc.id,
                        "type": "function",
                        "function": {
                            "name": tc.tool_id,
                            "arguments": serde_json::to_string(&tc.input)
                                .unwrap_or_else(|_| "{}".to_string()),
                        }
                    })
                })
                .collect::<Vec<_>>(),
        )
    } else {
        None
    };
    RequestMessage {
        role,
        content: m.content.clone(),
        tool_calls,
        tool_call_id: m.tool_call_id.clone(),
    }
}

#[derive(Serialize)]
struct RequestMessage {
    role: &'static str,
    content: String,
    /// Assistant-only: the tool calls being round-tripped.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<serde_json::Value>>,
    /// Tool-only: the id of the call this result answers.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<RequestMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop: Vec<String>,
    /// Native tool definitions (OpenAI function shape). OpenAI-compatible
    /// servers receive the provider-neutral entries wrapped at the boundary.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
}

#[derive(Deserialize, Default)]
struct ToolCallFunction {
    /// PART 14: a missing function name must not reject the whole response;
    /// it becomes an unknown-tool structured failure at the agent loop.
    #[serde(default)]
    name: Option<String>,
    /// PART 4: missing arguments normalize to `{}`; raw strings are parsed
    /// or kept for structured rejection.
    #[serde(default)]
    arguments: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct ToolCall {
    /// PART 2: string / number / null / missing are all tolerated; the id is
    /// canonicalized after parsing instead of failing the response.
    #[serde(default)]
    id: Option<serde_json::Value>,
    #[serde(default)]
    function: ToolCallFunction,
}

#[derive(Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct ChatEndpointResponse {
    model: String,
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<ModelEntry>,
}

#[cfg(all(test, feature = "openai-http"))]
mod tests {
    use super::*;
    use openrt_core::Message;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    fn test_config(base_url: String) -> OpenAiConfig {
        OpenAiConfig {
            base_url,
            model: Some("qwen3:4b".into()),
            timeout_secs: 5,
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
        }
    }

    fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    async fn read_request(stream: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let n = stream.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if find_subsequence(&buf, b"\r\n\r\n").is_some() {
                break;
            }
        }
        let Some(end) = find_subsequence(&buf, b"\r\n\r\n") else {
            return String::from_utf8_lossy(&buf).into_owned();
        };
        let headers = String::from_utf8_lossy(&buf[..end]).into_owned();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        let mut body = buf[end + 4..].to_vec();
        while body.len() < content_length {
            let n = stream.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        let body_str =
            String::from_utf8_lossy(&body[..content_length.min(body.len())]).into_owned();
        format!("{headers}\r\n\r\n{body_str}")
    }

    async fn serve_once(
        status_line: String,
        body: String,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            let response = format!(
                "{status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.shutdown().await.ok();
            request
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn chat_completion_round_trip() {
        let body = r#"{
            "id": "chatcmpl-1",
            "model": "qwen3:4b",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "hello from openai-compatible" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 12, "completion_tokens": 7 }
        }"#;
        let (url, handle) = serve_once("HTTP/1.1 200 OK".to_string(), body.to_string()).await;
        let cfg = test_config(url);
        let req = ModelRequest::new(vec![Message::user("hi")]).with_model("qwen3:4b");
        let out = chat_completions(&cfg, &req).await.unwrap();
        assert_eq!(out.text, "hello from openai-compatible");
        assert_eq!(out.model, "qwen3:4b");
        assert_eq!(out.finish_reason, FinishReason::Stop);
        assert_eq!(out.usage.input_tokens, Some(12));
        assert_eq!(out.usage.output_tokens, Some(7));
        assert!(out.tool_calls.is_empty());
        let request = handle.await.unwrap();
        assert!(request.contains("\"model\":\"qwen3:4b\""));
        assert!(request.contains("\"role\":\"user\""));
    }

    /// Part B translation guard: an OpenAI-compatible endpoint must NEVER
    /// receive Ollama-only fields. `unbounded_output` / `context_window`
    /// surface only as Ollama's `options.num_predict=-1` / `options.num_ctx`;
    /// on the OpenAI wire they are dropped (the backend owns its own
    /// defaults) — no `num_predict`, `num_ctx`, or `stream` leaks through.
    #[tokio::test]
    async fn openai_translation_never_leaks_ollama_fields() {
        let body = r#"{
            "id": "chatcmpl-clean",
            "model": "qwen3:4b",
            "choices": [{ "index": 0,
                "message": { "role": "assistant", "content": "ok" },
                "finish_reason": "stop" }],
            "usage": {}
        }"#;
        let (url, handle) = serve_once("HTTP/1.1 200 OK".to_string(), body.to_string()).await;
        let cfg = test_config(url);
        let mut req = ModelRequest::new(vec![Message::user("hi")]).with_model("qwen3:4b");
        req.parameters.unbounded_output = true;
        req.parameters.context_window = Some(131072);
        let out = chat_completions(&cfg, &req).await.unwrap();
        assert_eq!(out.text, "ok");
        let request = handle.await.unwrap();
        assert!(
            !request.contains("num_predict"),
            "OpenAI wire must not carry num_predict: {request}"
        );
        assert!(
            !request.contains("num_ctx"),
            "OpenAI wire must not carry num_ctx: {request}"
        );
        assert!(
            !request.contains("unbounded_output"),
            "OpenAI wire must not carry unbounded_output: {request}"
        );
        assert!(
            !request.contains("\"stream\""),
            "OpenAI wire must not carry stream: {request}"
        );
        assert!(request.contains("\"model\":\"qwen3:4b\""));
    }

    #[tokio::test]
    async fn tool_calls_are_mapped() {
        let body = r#"{
            "id": "chatcmpl-2",
            "model": "qwen3:4b",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "echo", "arguments": "{\"text\":\"hi\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 20, "completion_tokens": 3 }
        }"#;
        let (url, handle) = serve_once("HTTP/1.1 200 OK".to_string(), body.to_string()).await;
        let cfg = test_config(url);
        let req = ModelRequest::new(vec![Message::assistant("use the echo tool")]);
        let out = chat_completions(&cfg, &req).await.unwrap();
        assert_eq!(out.finish_reason, FinishReason::ToolCalls);
        assert_eq!(out.tool_calls.len(), 1);
        assert_eq!(out.tool_calls[0].id, "call_1");
        assert_eq!(out.tool_calls[0].name, "echo");
        assert_eq!(
            out.tool_calls[0].arguments,
            serde_json::json!({ "text": "hi" })
        );
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn list_models_returns_data_ids() {
        let body = r#"{
            "object": "list",
            "data": [
                { "id": "qwen3:4b", "object": "model" },
                { "id": "qwen3:8b", "object": "model" }
            ]
        }"#;
        let (url, handle) = serve_once("HTTP/1.1 200 OK".to_string(), body.to_string()).await;
        let cfg = test_config(url);
        let ids = list_models(&cfg).await.unwrap();
        assert_eq!(ids, vec!["qwen3:4b".to_string(), "qwen3:8b".to_string()]);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn is_ready_maps_status() {
        let (url_ok, h1) = serve_once("HTTP/1.1 200 OK".to_string(), String::new()).await;
        assert!(is_ready(&test_config(url_ok)).await.unwrap());
        h1.await.unwrap();

        let (url_bad, h2) = serve_once(
            "HTTP/1.1 503 Service Unavailable".to_string(),
            String::new(),
        )
        .await;
        assert!(!is_ready(&test_config(url_bad)).await.unwrap());
        h2.await.unwrap();
    }

    #[tokio::test]
    async fn missing_model_is_not_configured() {
        let cfg = OpenAiConfig {
            base_url: "http://127.0.0.1:1".into(),
            model: None,
            timeout_secs: 5,
            client: reqwest::Client::new(),
        };
        let req = ModelRequest::new(vec![Message::user("hi")]);
        let err = chat_completions(&cfg, &req).await.unwrap_err();
        assert!(matches!(err, ModelError::NotConfigured(_)));
    }

    // ---- PART 14/22: malformed-response regression battery -------------

    async fn serve_and_parse(body: &str) -> Result<ModelResponse, ModelError> {
        let (url, handle) = serve_once("HTTP/1.1 200 OK".to_string(), body.to_string()).await;
        let cfg = test_config(url);
        let req = ModelRequest::new(vec![Message::user("hi")]);
        let out = chat_completions(&cfg, &req).await;
        handle.await.unwrap();
        out
    }

    /// PART 2: numeric / null / missing ids never reject the response.
    #[tokio::test]
    async fn non_string_ids_are_canonicalized() {
        let body = r#"{
            "id": "c", "model": "qwen3:4b",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": null, "tool_calls": [
                    { "id": 42, "type": "function",
                      "function": { "name": "echo", "arguments": "{}" } },
                    { "id": null, "type": "function",
                      "function": { "name": "echo", "arguments": "{}" } },
                    { "type": "function",
                      "function": { "name": "echo", "arguments": "{}" } }
                ] },
                "finish_reason": "tool_calls"
            }]
        }"#;
        let out = serve_and_parse(body).await.unwrap();
        assert_eq!(out.tool_calls.len(), 3);
        assert_eq!(out.tool_calls[0].id, "42");
        assert!(out.tool_calls[1].id.starts_with("openrt-call-"));
        assert!(out.tool_calls[2].id.starts_with("openrt-call-"));
        let mut ids: Vec<_> = out.tool_calls.iter().map(|c| c.id.clone()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 3, "ids must be distinct: {ids:?}");
    }

    /// PART 4: stringified JSON arguments are parsed; prose stays raw.
    #[tokio::test]
    async fn argument_shapes_are_normalized() {
        let body = r#"{
            "id": "c", "model": "qwen3:4b",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": null, "tool_calls": [
                    { "id": "a", "function": { "name": "echo", "arguments": "{\"text\":\"x\"}" } },
                    { "id": "b", "function": { "name": "echo", "arguments": {"text":"y"} } },
                    { "id": "d", "function": { "name": "echo", "arguments": "not json at all" } },
                    { "id": "e", "function": { "name": "echo" } }
                ] },
                "finish_reason": "tool_calls"
            }]
        }"#;
        let out = serve_and_parse(body).await.unwrap();
        assert_eq!(out.tool_calls[0].arguments, serde_json::json!({"text":"x"}));
        assert_eq!(out.tool_calls[1].arguments, serde_json::json!({"text":"y"}));
        assert_eq!(
            out.tool_calls[2].arguments,
            serde_json::json!("not json at all")
        );
        assert_eq!(out.tool_calls[3].arguments, serde_json::json!({}));
    }

    /// PART 14: `finish_reason: "tool_calls"` with zero calls is a
    /// structured provider error — never a silent empty completion.
    #[tokio::test]
    async fn finish_reason_tool_calls_without_calls_is_invalid() {
        let body = r#"{
            "id": "c", "model": "qwen3:4b",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": null, "tool_calls": [] },
                "finish_reason": "tool_calls"
            }]
        }"#;
        let err = serve_and_parse(body).await.unwrap_err();
        assert!(matches!(err, ModelError::InvalidResponse(_)), "{err:?}");
    }

    /// PART 14: `tool_calls` of the wrong type is a structured provider
    /// error, not a panic.
    #[tokio::test]
    async fn malformed_tool_calls_type_is_invalid_response() {
        let body = r#"{
            "id": "c", "model": "qwen3:4b",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": null, "tool_calls": "x" },
                "finish_reason": "tool_calls"
            }]
        }"#;
        let err = serve_and_parse(body).await.unwrap_err();
        assert!(matches!(err, ModelError::InvalidResponse(_)), "{err:?}");
    }

    /// PART 14: missing function name maps to an empty name (unknown-tool
    /// structured failure downstream), not a rejected response.
    #[tokio::test]
    async fn missing_function_name_maps_to_unknown_tool() {
        let body = r#"{
            "id": "c", "model": "qwen3:4b",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": null, "tool_calls": [
                    { "id": "a", "function": { "arguments": "{}" } }
                ] },
                "finish_reason": "tool_calls"
            }]
        }"#;
        let out = serve_and_parse(body).await.unwrap();
        assert_eq!(out.tool_calls[0].name, "");
    }

    /// PART 6: the request carries the tool round-trip — assistant
    /// tool_calls (arguments re-stringified) and tool result correlation.
    #[tokio::test]
    async fn multi_turn_request_carries_tool_history() {
        let body = r#"{
            "id": "c", "model": "qwen3:4b",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "done" },
                "finish_reason": "stop"
            }]
        }"#;
        let (url, handle) = serve_once("HTTP/1.1 200 OK".to_string(), body.to_string()).await;
        let cfg = test_config(url);

        let mut assistant = Message::assistant("let me check");
        assistant.tool_calls = vec![openrt_core::PendingToolCall {
            id: "call_abc".into(),
            tool_id: "filesystem.read".into(),
            input: serde_json::json!({"path": "a.txt"}),
        }];
        let req = ModelRequest::new(vec![
            Message::user("read a.txt"),
            assistant,
            Message::tool("call_abc", "{\"ok\":true}"),
        ]);
        chat_completions(&cfg, &req).await.unwrap();
        let request = handle.await.unwrap();

        // Assistant tool_calls present with JSON-string arguments (escaped
        // once more because the payload itself is JSON).
        assert!(request.contains("\"tool_calls\""), "{request}");
        assert!(request.contains("\"call_abc\""), "{request}");
        assert!(request.contains("\"filesystem.read\""), "{request}");
        assert!(
            request.contains("{\\\"path\\\":\\\"a.txt\\\"}"),
            "arguments must be a JSON string: {request}"
        );
        // Tool result correlated by tool_call_id.
        assert!(
            request.contains("\"tool_call_id\":\"call_abc\""),
            "{request}"
        );
        // roles present
        assert!(request.contains("\"role\":\"tool\""), "{request}");
    }

    /// PART 7: ids from the provider are echoed verbatim across turns.
    #[tokio::test]
    async fn provider_id_is_preserved_verbatim_on_the_wire() {
        let body = r#"{
            "id": "c", "model": "qwen3:4b",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": null, "tool_calls": [
                    { "id": "chatcmpl-weird_0.1", "function": { "name": "echo", "arguments": "{}" } }
                ] },
                "finish_reason": "tool_calls"
            }]
        }"#;
        let (url, h1) = serve_once("HTTP/1.1 200 OK".to_string(), body.to_string()).await;
        let out = chat_completions(
            &test_config(url),
            &ModelRequest::new(vec![Message::user("x")]),
        )
        .await
        .unwrap();
        h1.await.unwrap();
        assert_eq!(out.tool_calls[0].id, "chatcmpl-weird_0.1");

        // Second hop: that id lands in the request payload unchanged.
        let body2 = r#"{
            "id": "c", "model": "qwen3:4b",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "ok" },
                "finish_reason": "stop"
            }]
        }"#;
        let (url2, h2) = serve_once("HTTP/1.1 200 OK".to_string(), body2.to_string()).await;
        let mut assistant = Message::assistant("");
        assistant.tool_calls = vec![openrt_core::PendingToolCall {
            id: out.tool_calls[0].id.clone(),
            tool_id: "echo".into(),
            input: serde_json::json!({}),
        }];
        let req = ModelRequest::new(vec![assistant, Message::tool(&out.tool_calls[0].id, "ok")]);
        chat_completions(&test_config(url2), &req).await.unwrap();
        let request = h2.await.unwrap();
        assert!(request.contains("chatcmpl-weird_0.1"), "{request}");
    }
}
