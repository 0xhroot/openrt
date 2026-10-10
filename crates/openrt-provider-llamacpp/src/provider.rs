//! llama.cpp `server` backend — a real model provider talking to the
//! OpenAI-compatible HTTP API exposed by the llama.cpp `server` binary.

use async_trait::async_trait;
use openrt_models::openai::OpenAiConfig;
use openrt_models::{
    BackendCapabilities, ModelCapabilities, ModelError, ModelInfo, ModelProvider, ModelRequest,
    ModelResponse, ProviderKind,
};

/// Provider backed by a llama.cpp `server` endpoint.
pub struct LlamacppProvider {
    id: String,
    cfg: OpenAiConfig,
}

impl LlamacppProvider {
    pub fn new(id: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            cfg: OpenAiConfig {
                base_url: base_url.into(),
                model: None,
                timeout_secs: 30,
                client: reqwest::Client::default(),
            },
        }
    }
}

#[async_trait]
impl ModelProvider for LlamacppProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn info(&self) -> ModelInfo {
        ModelInfo::new(&self.id, ProviderKind::Llamacpp, self.cfg.model.clone())
    }

    async fn generate(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        openrt_models::openai::chat_completions(&self.cfg, &req).await
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            supports_tools: true,
            supports_streaming: false,
            context_window: None,
        }
    }

    // M4: an HTTP/OpenAI-compatible transport. OpenRT does not see this
    // server's tensor layout, filesystem, or residency, so nothing local is
    // claimed — the conservative remote posture, by explicit declaration.
    fn backend_capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::remote_http()
    }

    fn base_url(&self) -> Option<String> {
        Some(self.cfg.base_url.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_core::Message;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    fn provider_for(url: String) -> LlamacppProvider {
        let mut p = LlamacppProvider::new("llamacpp", url);
        p.cfg.client = reqwest::Client::builder().no_proxy().build().unwrap();
        p
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
    async fn generate_returns_text() {
        let body = r#"{
            "id": "chatcmpl-llama",
            "model": "llama-3.1-8b",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "ok" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 4, "completion_tokens": 1 }
        }"#;
        let (url, handle) = serve_once("HTTP/1.1 200 OK".to_string(), body.to_string()).await;
        let p = provider_for(url);
        let req = ModelRequest::new(vec![Message::user("ping")]).with_model("llama-3.1-8b");
        let out = p.generate(req).await.unwrap();
        assert_eq!(out.text, "ok");
        assert_eq!(out.model, "llama-3.1-8b");
        assert_eq!(out.usage.input_tokens, Some(4));
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn generate_maps_http_500() {
        let (url, handle) = serve_once(
            "HTTP/1.1 500 Internal Server Error".to_string(),
            "boom".to_string(),
        )
        .await;
        let p = provider_for(url);
        let req = ModelRequest::new(vec![Message::user("hi")]).with_model("llama-3.1-8b");
        let err = p.generate(req).await.unwrap_err();
        match err {
            ModelError::Http { status, .. } => assert_eq!(status, 500),
            other => panic!("expected Http 500, got {other:?}"),
        }
        handle.await.unwrap();
    }

    #[test]
    fn remote_capabilities_are_conservative_regardless_of_configuration() {
        // M4: even a fully-configured HTTP provider claims nothing local and
        // never fabricates remote anatomy.
        let p = provider_for("http://127.0.0.1:8080".to_string());
        let caps = p.backend_capabilities();
        assert_eq!(caps, BackendCapabilities::remote_http());
        assert_eq!(caps.access, openrt_models::AnatomyAccess::RemoteProvider);
        assert!(!caps.has_local_anatomy());
        assert_eq!(
            caps.supported_count(),
            0,
            "an HTTP provider claims nothing local"
        );
    }

    #[tokio::test]
    async fn hint_submission_to_remote_provider_is_a_clean_noop() {
        let p = provider_for("http://127.0.0.1:8080".to_string());
        let receipt = p
            .submit_memory_hint(openrt_models::MemoryHint::advisory(
                "llama-3.1-8b",
                openrt_models::HintPriority::Low,
                "advisory",
            ))
            .await
            .unwrap();
        assert_eq!(receipt, openrt_models::HintReceipt::Unsupported);
        assert!(receipt.is_noop(), "an unsupported hint never errors");
    }
}
