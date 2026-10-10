//! Deterministic, zero-network provider for tests and `openrt --mock` demos.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use openrt_core::MessageRole;
use tokio::time::sleep;

use crate::error::ModelError;
use crate::provider::{ModelCapabilities, ModelProvider, ProviderLoadTelemetry};
use crate::types::{
    FinishReason, ModelInfo, ModelRequest, ModelResponse, ModelUsage, ProviderKind,
    ProviderToolCall, StreamEvent,
};

/// Behaviour of [`MockProvider`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockMode {
    /// Echo the last user turn back as text. No tools, immediate.
    Echo,
    /// Scripted tool-calling: returns an `echo` tool call for the first
    /// request, then a final answer once the tool result is fed back.
    /// Demonstrates the full agent→tool loop without a model.
    ToolSequence,
    /// Always fail after `latency`, returning `MockMode`-agnostic error.
    Fail,
    /// Sleep for `latency` then echo. Lets tests exercise timeouts/cancellation.
    Slow,
    /// Like `Slow` but never completes: only usable with an explicit cancel.
    Hang,
    /// Return an empty assistant response with no tool calls (exercise
    /// empty-response parsing/retry path).
    Empty,
}

/// A provider that never touches the network.
pub struct MockProvider {
    id: String,
    default_model: String,
    mode: MockMode,
    latency: Duration,
    calls: Arc<AtomicU64>,
    /// Scripted tool calls for `MockMode::ToolSequence`: one entry per
    /// model turn (in order), then a final stop. Empty → the default single
    /// `echo` call.
    script: Vec<(String, serde_json::Value)>,
    // Part C: declared capabilities plus an optional post-first-token silence
    // that exercises agent idle watchdogs without a real model.
    supports_streaming: bool,
    context_window: Option<usize>,
    stream_gap: Option<Duration>,
    // M11: tier hit/miss instrumentation. The mock genuinely observes its own
    // lifecycle: a generation arriving while no session is active found an
    // idle backend — a cold load and a tier miss (residency had to be
    // re-established); one arriving while a session is active was served from
    // the resident state — a warm load and a tier hit.
    active_sessions: Arc<AtomicU64>,
    cold_loads: Arc<AtomicU64>,
    warm_loads: Arc<AtomicU64>,
    tier_hits: Arc<AtomicU64>,
    tier_misses: Arc<AtomicU64>,
}

impl std::fmt::Debug for MockProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockProvider")
            .field("id", &self.id)
            .field("mode", &self.mode)
            .field("calls", &self.calls.load(Ordering::Relaxed))
            .field("tier_hits", &self.tier_hits.load(Ordering::Relaxed))
            .field("tier_misses", &self.tier_misses.load(Ordering::Relaxed))
            .finish()
    }
}

/// Releases a mock session on drop so a cancelled/failed generation still
/// returns the backend to idle (or, for `Hang`, truthfully stays resident).
struct ActiveSession(Arc<AtomicU64>);

impl Drop for ActiveSession {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl MockProvider {
    pub fn new(id: impl Into<String>, mode: MockMode) -> Self {
        Self {
            id: id.into(),
            default_model: "mock://default".to_string(),
            mode,
            latency: Duration::ZERO,
            calls: Arc::new(AtomicU64::new(0)),
            script: Vec::new(),
            supports_streaming: false,
            context_window: Some(4096),
            stream_gap: None,
            active_sessions: Arc::new(AtomicU64::new(0)),
            cold_loads: Arc::new(AtomicU64::new(0)),
            warm_loads: Arc::new(AtomicU64::new(0)),
            tier_hits: Arc::new(AtomicU64::new(0)),
            tier_misses: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Echo-only provider (default for `--mock` when no tool use needed).
    pub fn echo(id: impl Into<String>) -> Self {
        Self::new(id, MockMode::Echo)
    }

    /// Scripted tool-call provider, ideal for integration tests / demos.
    pub fn tool_sequence(id: impl Into<String>) -> Self {
        Self::new(id, MockMode::ToolSequence)
    }

    pub fn with_latency(mut self, latency: Duration) -> Self {
        self.latency = latency;
        self
    }

    /// Advertise streaming support so agent idle-watchdog paths can be
    /// exercised without a network. The mock streams the response text in
    /// small deltas and finishes with a `Done` carrying the full response.
    pub fn with_streaming(mut self) -> Self {
        self.supports_streaming = true;
        self
    }

    /// Override the declared context window (`None` disables the guard's
    /// window downstream, exercising the no-window path).
    pub fn with_context_window(mut self, window: Option<usize>) -> Self {
        self.context_window = window;
        self
    }

    /// Hold the stream silent for `gap` after the first token before the
    /// rest — an idle gap that agent idle timeouts must treat as a *transient*
    /// error, never a silent completion.
    pub fn with_stream_gap(mut self, gap: Duration) -> Self {
        self.stream_gap = Some(gap);
        self
    }

    /// Override the reported default model (empty string = a provider with
    /// **no** default model — the first-run/setup code path).
    pub fn with_default_model(mut self, id: impl Into<String>) -> Self {
        self.default_model = id.into();
        self
    }

    /// Script the `ToolSequence` turns: each model turn that has not yet
    /// seen a tool result returns the next `(name, arguments)` entry; when
    /// the script is exhausted the provider finishes with a stop response.
    pub fn with_tool_script(mut self, script: Vec<(impl Into<String>, serde_json::Value)>) -> Self {
        self.script = script.into_iter().map(|(n, a)| (n.into(), a)).collect();
        self
    }

    pub fn call_count(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    /// Zero every load counter (for deterministic test fixtures).
    pub fn reset_load_telemetry(&self) {
        self.cold_loads.store(0, Ordering::Relaxed);
        self.warm_loads.store(0, Ordering::Relaxed);
        self.tier_hits.store(0, Ordering::Relaxed);
        self.tier_misses.store(0, Ordering::Relaxed);
    }
}

impl MockProvider {
    fn last_user_text<'a>(&self, req: &'a ModelRequest) -> Option<&'a str> {
        req.messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::User && m.tool_calls.is_empty())
            .map(|m| m.content.as_str())
    }

    fn last_tool_result<'a>(&self, req: &'a ModelRequest) -> Option<&'a str> {
        req.messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::Tool)
            .map(|m| m.content.as_str())
    }

    /// M11: mark this generation as arriving, for the load-lifecycle
    /// counters; the returned guard releases the slot when dropped (or
    /// cancelled), returning the backend to idle.
    fn enter_session(&self) -> ActiveSession {
        let was_idle = self.active_sessions.fetch_add(1, Ordering::AcqRel) == 0;
        if was_idle {
            self.cold_loads.fetch_add(1, Ordering::Relaxed);
            self.tier_misses.fetch_add(1, Ordering::Relaxed);
        } else {
            self.warm_loads.fetch_add(1, Ordering::Relaxed);
            self.tier_hits.fetch_add(1, Ordering::Relaxed);
        }
        ActiveSession(self.active_sessions.clone())
    }

    /// Scripted, latency/hang-aware response — shared by `generate` and
    /// `generate_stream` so streaming never drifts from the non-streaming
    /// behaviour the tests assert against.
    async fn produce(&self, req: &ModelRequest) -> Result<ModelResponse, ModelError> {
        if self.latency > Duration::ZERO {
            sleep(self.latency).await;
        }

        match self.mode {
            MockMode::Fail => Err(ModelError::Unexpected("mock failure".into())),
            MockMode::Hang => {
                // never returns; caller must cancel or time out
                use std::future::pending;
                pending::<()>().await;
                unreachable!()
            }
            MockMode::Echo => {
                let text = self
                    .last_user_text(req)
                    .map(|t| format!("mock echo: {t}"))
                    .unwrap_or_else(|| "mock echo: <no user text>".to_string());
                Ok(ModelResponse {
                    model: self.default_model.clone(),
                    text,
                    tool_calls: vec![],
                    finish_reason: FinishReason::Stop,
                    usage: ModelUsage {
                        input_tokens: Some(3),
                        output_tokens: Some(4),
                    },
                })
            }
            MockMode::Empty => Ok(ModelResponse {
                model: self.default_model.clone(),
                text: String::new(),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage: ModelUsage::default(),
            }),
            MockMode::ToolSequence => {
                let turn = req
                    .messages
                    .iter()
                    .filter(|m| m.role == MessageRole::Tool)
                    .count();
                if self.script.is_empty() {
                    if let Some(result) = self.last_tool_result(req) {
                        return Ok(ModelResponse {
                            model: self.default_model.clone(),
                            text: format!("mock final: {result}"),
                            tool_calls: vec![],
                            finish_reason: FinishReason::Stop,
                            usage: ModelUsage {
                                input_tokens: Some(8),
                                output_tokens: Some(6),
                            },
                        });
                    }
                    let text = self.last_user_text(req).unwrap_or("hello").to_string();
                    return Ok(ModelResponse {
                        model: self.default_model.clone(),
                        text: String::new(),
                        tool_calls: vec![ProviderToolCall {
                            id: "mock_call_1".to_string(),
                            name: "echo".to_string(),
                            arguments: serde_json::json!({ "text": text }),
                        }],
                        finish_reason: FinishReason::ToolCalls,
                        usage: ModelUsage {
                            input_tokens: Some(5),
                            output_tokens: Some(2),
                        },
                    });
                }
                match self.script.get(turn) {
                    Some((name, args)) => Ok(ModelResponse {
                        model: self.default_model.clone(),
                        text: String::new(),
                        tool_calls: vec![ProviderToolCall {
                            id: format!("mock_call_{turn}"),
                            name: name.clone(),
                            arguments: args.clone(),
                        }],
                        finish_reason: FinishReason::ToolCalls,
                        usage: ModelUsage {
                            input_tokens: Some(5),
                            output_tokens: Some(2),
                        },
                    }),
                    None => {
                        let text = self
                            .last_tool_result(req)
                            .map(|r| format!("mock final: {r}"))
                            .unwrap_or_else(|| "mock final".to_string());
                        Ok(ModelResponse {
                            model: self.default_model.clone(),
                            text,
                            tool_calls: vec![],
                            finish_reason: FinishReason::Stop,
                            usage: ModelUsage {
                                input_tokens: Some(8),
                                output_tokens: Some(6),
                            },
                        })
                    }
                }
            }
            MockMode::Slow => {
                let text = self
                    .last_user_text(req)
                    .map(|t| format!("mock echo (slow): {t}"))
                    .unwrap_or_default();
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok(ModelResponse {
                    model: self.default_model.clone(),
                    text,
                    tool_calls: vec![],
                    finish_reason: FinishReason::Stop,
                    usage: ModelUsage::default(),
                })
            }
        }
    }
}

#[async_trait::async_trait]
impl ModelProvider for MockProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn info(&self) -> ModelInfo {
        ModelInfo {
            id: self.id.clone(),
            kind: ProviderKind::Mock,
            display_name: "MockProvider".to_string(),
            default_model: Some(self.default_model.clone()),
        }
    }

    async fn generate(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        self.calls.fetch_add(1, Ordering::Relaxed);

        // M11: an arrival to an idle backend is a cold load + tier miss; an
        // arrival while a session is active is a warm load + tier hit. The
        // session guard is dropped when this future completes (or is dropped
        // on cancellation), returning the backend to idle.
        let _held = self.enter_session();

        self.produce(&req).await
    }

    async fn generate_stream(
        &self,
        req: ModelRequest,
        sink: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> Result<ModelResponse, ModelError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let _held = self.enter_session();
        let res = self.produce(&req).await?;

        // Stream the text in small deltas; a configured gap sits after the
        // first token so idle watchdogs observe genuine inter-token silence.
        const CHUNK: usize = 4;
        let chars: Vec<char> = res.text.chars().collect();
        let mut first = true;
        for chunk in chars.chunks(CHUNK) {
            if !first {
                if let Some(gap) = self.stream_gap {
                    sleep(gap).await;
                }
            }
            first = false;
            let piece: String = chunk.iter().collect();
            if sink.send(StreamEvent::Token(piece)).await.is_err() {
                // Caller dropped the sink (cancelled / idle timeout) — stop.
                break;
            }
        }
        sink.send(StreamEvent::Done).await.ok();
        Ok(res)
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            supports_tools: matches!(self.mode, MockMode::ToolSequence) || self.supports_streaming,
            supports_streaming: self.supports_streaming,
            context_window: self.context_window,
        }
    }

    // M11: the mock genuinely observes its own load lifecycle — a generation
    // arriving to an idle backend is a cold load + tier miss, one arriving
    // while a session is resident is a warm load + tier hit.
    fn load_telemetry(&self) -> ProviderLoadTelemetry {
        ProviderLoadTelemetry {
            cold_load_count: Some(self.cold_loads.load(Ordering::Relaxed)),
            warm_load_count: Some(self.warm_loads.load(Ordering::Relaxed)),
            tier_hit_count: Some(self.tier_hits.load(Ordering::Relaxed)),
            tier_miss_count: Some(self.tier_misses.load(Ordering::Relaxed)),
            observed_by: format!("mock:{}", self.id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_core::Message;

    fn req(text: &str) -> ModelRequest {
        ModelRequest::new(vec![Message::user(text)])
    }

    #[tokio::test]
    async fn echo_returns_text() {
        let p = MockProvider::echo("m");
        let out = p.generate(req("ping")).await.unwrap();
        assert_eq!(out.text, "mock echo: ping");
        assert!(out.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn tool_sequence_returns_tool_call_then_final() {
        let p = MockProvider::tool_sequence("m");
        let first = p.generate(req("say hello to echo")).await.unwrap();
        assert_eq!(first.finish_reason, FinishReason::ToolCalls);
        assert_eq!(first.tool_calls.len(), 1);
        assert_eq!(first.tool_calls[0].name, "echo");
        // Feed the tool result back and get the final answer.
        let mut msgs = vec![Message::user("say hello to echo")];
        msgs.push(openrt_core::Message::assistant(""));
        msgs.push(Message::tool("echo", "tool result: hello"));
        let second = p.generate(ModelRequest::new(msgs)).await.unwrap();
        assert!(second.text.contains("mock final"));
    }

    #[tokio::test]
    async fn fail_mode_errors() {
        let p = MockProvider::new("m", MockMode::Fail);
        assert!(matches!(
            p.generate(req("x")).await,
            Err(ModelError::Unexpected(_))
        ));
    }

    #[tokio::test]
    async fn call_count_tracks_invocations() {
        let p = MockProvider::echo("m");
        p.generate(req("a")).await.unwrap();
        p.generate(req("b")).await.unwrap();
        assert_eq!(p.call_count(), 2);
    }

    #[tokio::test]
    async fn slow_respects_latency() {
        let p = MockProvider::new("m", MockMode::Slow).with_latency(Duration::from_millis(50));
        let out = p.generate(req("hi")).await.unwrap();
        assert!(out.text.contains("slow"));
    }

    #[tokio::test]
    async fn m11_idle_arrival_is_a_cold_load_and_tier_miss() {
        let p = MockProvider::echo("m");
        p.generate(req("a")).await.unwrap();
        let t = p.load_telemetry();
        assert_eq!(
            t.cold_load_count,
            Some(1),
            "first arrival had no resident model"
        );
        assert_eq!(t.warm_load_count, Some(0));
        assert_eq!(t.tier_miss_count, Some(1));
        assert_eq!(t.tier_hit_count, Some(0));
        assert_eq!(t.observed_by, "mock:m");
    }

    #[tokio::test]
    async fn m11_concurrent_arrival_is_a_warm_hit_against_a_resident_model() {
        let p = Arc::new(MockProvider::new("m", MockMode::Slow));
        let p2 = p.clone();
        let first = tokio::spawn(async move { p.generate(req("long")).await });
        // Give the first generation time to grab the session...
        tokio::time::sleep(Duration::from_millis(30)).await;
        // ...then a second arrival finds it resident.
        let _second = p2.as_ref().generate(req("fast")).await.unwrap();
        first.await.unwrap().unwrap();
        let t = p2.load_telemetry();
        assert_eq!(
            t.cold_load_count,
            Some(1),
            "only the first arrival was cold"
        );
        assert_eq!(t.warm_load_count, Some(1), "the second was served warm");
        assert_eq!(t.tier_hit_count, Some(1));
        assert_eq!(t.tier_miss_count, Some(1));
        use std::sync::atomic::Ordering as O;
        // Both futures have finished, so the backend is idle again.
        // (active_sessions may race to 0; that is the counter's job.)
        assert_eq!(p2.active_sessions.load(O::Relaxed), 0);
    }

    #[tokio::test]
    async fn m11_unobserved_is_the_honest_default() {
        assert!(ProviderLoadTelemetry::unobserved().is_unobserved());
        assert!(ProviderLoadTelemetry::default().is_unobserved());
    }

    #[tokio::test]
    async fn streaming_advertises_and_reassembles_text() {
        let p = MockProvider::echo("m").with_streaming();
        assert!(p.capabilities().supports_streaming);
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let called = p
            .generate_stream(req("ping"), tx)
            .await
            .expect("stream completes");
        let mut text = String::new();
        let mut done = false;
        while let Some(ev) = rx.recv().await {
            match ev {
                StreamEvent::Token(t) => text.push_str(&t),
                StreamEvent::Done => done = true,
            }
        }
        assert_eq!(text, "mock echo: ping");
        assert_eq!(called.text, "mock echo: ping");
        assert!(done, "stream ends with a Done frame");
    }

    #[tokio::test]
    async fn streaming_tool_call_sends_only_a_done_frame() {
        let p = MockProvider::tool_sequence("m").with_streaming();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        p.generate_stream(req("use echo"), tx).await.unwrap();
        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev);
        }
        assert!(
            events.iter().all(|e| matches!(e, StreamEvent::Done)),
            "tool-call turns are carried by the Done frame"
        );
    }

    #[tokio::test]
    async fn context_window_override_is_declared() {
        let p = MockProvider::echo("m").with_streaming();
        assert_eq!(p.capabilities().context_window, Some(4096));
        let none = MockProvider::echo("m").with_context_window(None);
        assert_eq!(none.capabilities().context_window, None);
    }
}
