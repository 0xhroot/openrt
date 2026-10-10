//! Phase 1 agent: single-turn from a prompt to completion via provider +
//! gated tool executor. No complex planning in this phase.

use async_trait::async_trait;
use openrt_core::message::{Message, MessageRole, PendingToolCall};
use openrt_core::RedactionPolicy;
use std::sync::Arc;
use std::time::Duration;

use openrt_models::types::StreamEvent;
use openrt_models::{ModelError, ModelParameters, ModelProvider};
use openrt_tools::{ToolError, ToolExecutionContext, ToolExecutor};

use crate::{Agent, AgentContext, AgentEndReason, AgentError, AgentResult, AgentTurnLimits};

/// A minimal, deterministic agent: send history+input to the model, execute
/// any requested tool calls through the gate, continue until a text answer or
/// a limit.
pub struct MinimalAgent {
    id: String,
    provider: Arc<dyn ModelProvider>,
    executor: Arc<dyn ToolExecutor>,
    limits: AgentTurnLimits,
    /// Provider-facing tool definitions (function-calling schema).
    tools: Vec<serde_json::Value>,
    /// Tool results fed back to the model pass this redaction boundary
    /// (SEC-05): secret-shaped output never reaches the provider or the
    /// persisted transcript.
    redaction: RedactionPolicy,
    /// Environment/context message prepended to every provider call.
    /// Injected at request time only — never persisted into the transcript
    /// or the session history, so follow-up tasks don't accumulate copies.
    system: Option<String>,
    /// Generation steering applied to every provider request (post guard).
    /// Defaults to `ModelParameters::default()` = the provider's own defaults,
    /// so an unset config changes nothing.
    parameters: ModelParameters,
}

impl std::fmt::Debug for MinimalAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MinimalAgent")
            .field("id", &self.id)
            .field("limits", &self.limits)
            .field("tools", &self.tools.len())
            .finish_non_exhaustive()
    }
}

/// Part C: how a backoff sleep ended (keeps the pause/cancel distinction).
enum SleepEnd {
    Done,
    Cancelled,
    Paused,
}

impl MinimalAgent {
    pub fn new(
        id: impl Into<String>,
        provider: Arc<dyn ModelProvider>,
        executor: Arc<dyn ToolExecutor>,
        limits: AgentTurnLimits,
        tools: Vec<serde_json::Value>,
    ) -> Self {
        let id: String = id.into();
        Self {
            id,
            provider,
            executor,
            limits,
            tools,
            redaction: RedactionPolicy::default(),
            system: None,
            parameters: ModelParameters::default(),
        }
    }

    /// Environment context for the model (workspace root, tool-result
    /// conventions). Sent with every provider request, never persisted.
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system = Some(prompt.into());
        self
    }

    /// Explicit generation steering for every provider request. When the
    /// configured `context_window` exceeds the provider's *declared* window,
    /// the request is bounded by the declared window at assembly time — an
    /// unsafe `num_ctx` is never forced into the backend.
    pub fn with_parameters(mut self, parameters: ModelParameters) -> Self {
        self.parameters = parameters;
        self
    }

    pub fn with_default_limits(
        id: impl Into<String>,
        provider: Arc<dyn ModelProvider>,
        executor: Arc<dyn ToolExecutor>,
        tools: Vec<serde_json::Value>,
    ) -> Self {
        Self::new(id, provider, executor, AgentTurnLimits::default(), tools)
    }

    fn tool_context(&self, ctx: &AgentContext) -> ToolExecutionContext {
        ToolExecutionContext {
            session_id: Some(ctx.session_id.as_uuid().to_string()),
            task_id: Some(ctx.task_id.as_uuid().to_string()),
            channel: None,
        }
    }

    /// Request-time message assembly: the environment system prompt (if set)
    /// is prepended for the provider call only — never persisted. Part C
    /// stages the assembly: (A) environment framing, (B) the context guard
    /// sizes the request against the provider's *effective* context window
    /// — which, for a configurable backend (Ollama `num_ctx`), is the
    /// operator's requested window passed through unclamped, not the
    /// provider's smaller *default* — and (C) compaction trims the oldest
    /// turns with an explicit marker when the budget is exceeded — never
    /// silent truncation, never a fabricated window. A requested context that
    /// exceeds a *certified* hard maximum is refused explicitly, never
    /// silently rewritten. The current input and the environment framing are
    /// never dropped; an input that alone exceeds the budget is an honest
    /// refusal.
    fn staged_request(
        &self,
        messages: &[Message],
    ) -> Result<openrt_models::ModelRequest, AgentError> {
        let mut framed: Vec<Message> = match &self.system {
            Some(sys) => std::iter::once(Message::system(sys.clone()))
                .chain(messages.iter().cloned())
                .collect(),
            None => messages.to_vec(),
        };

        // (B) context guard. `effective_window` = a passable per-request
        // override when the backend is configurable (Ollama forwards num_ctx
        // unchanged), otherwise the provider default. Refusing a certified
        // hard-max breach here is the ONLY value adjustment the agent makes;
        // everything else passes through verbatim.
        let cap = self.provider.context_capability();
        if let Some(requested) = self.parameters.context_window {
            if cap.breaches_certified_maximum(requested as usize) {
                return Err(AgentError::Internal(format!(
                    "context guard: requested context window {requested} exceeds the provider's \
                     certified hard maximum {}; refusing rather than silently downgrading",
                    cap.maximum_context.unwrap_or(0)
                )));
            }
        }
        if let Some(window) = cap.effective_window(self.parameters.context_window) {
            let budget = ((window as f64) * self.limits.context_guard_ratio as f64) as usize;
            let total_before: usize = framed.iter().map(|m| estimate_tokens(&m.content)).sum();
            let (_, dropped) = compact_transcript(&framed, budget, "");
            if dropped == 0 && total_before > budget {
                // Even the protected head + the bare input exceed the budget:
                // honest refusal, never a truncated instruction.
                return Err(AgentError::Internal(format!(
                    "context guard: the request itself is ~{total_before} tokens against a \
                     context budget of {budget} (window {window}, guard {:.0}%); \
                     raise the model's context window or shorten the input",
                    self.limits.context_guard_ratio * 100.0
                )));
            }
            if dropped > 0 {
                // Re-compact with the honest drop count in the marker so the
                // model sees exactly what was omitted.
                let marker = format!(
                    "[(context compaction) {dropped} earlier turns were omitted to stay within \
                     the model's context window — their tool results remain in the persisted \
                     session]"
                );
                framed = compact_transcript(&framed, budget, &marker).0;
            }
        }

        let parameters = self.parameters.clone();
        // The requested context window passes through VERBATIM (the certified
        // hard-max refusal above is the only context adjustment the agent is
        // allowed to make). Transport layers send it when they support it
        // (Ollama `num_ctx`); a backend that does not ignore it, exactly as if
        // the value were unset — never silently rewritten.
        Ok(openrt_models::ModelRequest::new(framed)
            .with_parameters(parameters)
            .with_tools(self.tools.clone()))
    }

    /// Part C: only a streaming provider with an *explicitly configured* idle
    /// budget uses the stream path. Everything else keeps the plain
    /// non-streaming call — a long agent is never bounded by an artificial
    /// deadline.
    fn streaming_enabled(&self) -> bool {
        self.limits.idle_timeout_ms.is_some() && self.provider.capabilities().supports_streaming
    }

    /// Part C: a future that completes when the run's cooperative pause token
    /// becomes engaged, or never when no pause was configured (select simply
    /// ignores it). Mirrors `CancellationFut` semantics for the select arms.
    fn pause_wait(ctx: &AgentContext) -> impl std::future::Future<Output = ()> + '_ {
        async move {
            match &ctx.pause {
                Some(p) => {
                    let _ = p.wait_paused().await;
                }
                None => std::future::pending().await,
            }
        }
    }

    /// Part C: delay before the next retry, interrupting on cancel/pause so a
    /// long backoff never swallows an operator action.
    async fn backoff_sleep(&self, ctx: &AgentContext, attempt: u32) -> SleepEnd {
        let delay = backoff_delay(
            self.limits.retry_backoff_base_ms,
            self.limits.retry_backoff_max_ms,
            attempt,
        );
        tokio::select! {
            _ = tokio::time::sleep(delay) => SleepEnd::Done,
            _ = ctx.cancel.future() => SleepEnd::Cancelled,
            _ = Self::pause_wait(ctx) => SleepEnd::Paused,
        }
    }

    /// One provider access. The streaming path drains the delta sink with a
    /// per-token idle watchdog: a gap with no token for the configured budget
    /// is a *transient* error (backoff-retried), never a silent completion.
    /// A provider that advertises streaming but answers `StreamingUnsupported`
    /// is corrected once with the deterministic `generate` fallback — never a
    /// hidden retry loop against a lying capability.
    async fn call_provider(
        &self,
        ctx: &AgentContext,
        req: openrt_models::ModelRequest,
    ) -> Result<openrt_models::ModelResponse, ProviderCallError> {
        if !self.streaming_enabled() {
            return self
                .provider
                .generate(req)
                .await
                .map_err(ProviderCallError::Model);
        }
        let idle = Duration::from_millis(self.limits.idle_timeout_ms.unwrap_or(0));
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let provider = self.provider.clone();
        let stream_req = req.clone();
        let task = tokio::spawn(async move { provider.generate_stream(stream_req, tx).await });
        loop {
            tokio::select! {
                biased;
                _ = ctx.cancel.future() => {
                    task.abort();
                    return Err(ProviderCallError::Cancelled);
                }
                _ = Self::pause_wait(ctx) => {
                    task.abort();
                    return Err(ProviderCallError::Paused);
                }
                ev = tokio::time::timeout(idle, rx.recv()) => match ev {
                    Err(_elapsed) => {
                        task.abort();
                        return Err(ProviderCallError::Model(ModelError::Transport(format!(
                            "stream idle: no token delta for {} ms (agent_idle_timeout_ms)",
                            idle.as_millis()
                        ))));
                    }
                    Ok(Some(StreamEvent::Token(_))) => continue,
                    // Done received or the channel closed: the provider's own
                    // result (or error) is authoritative.
                    Ok(Some(StreamEvent::Done)) => break,
                    Ok(None) => break,
                },
            }
        }
        let joined = task
            .await
            .map_err(|e| ProviderCallError::Model(ModelError::Unexpected(e.to_string())));
        match joined {
            Ok(res) => match res {
                Ok(r) => Ok(r),
                Err(ModelError::StreamingUnsupported(_)) => self
                    .provider
                    .generate(req)
                    .await
                    .map_err(ProviderCallError::Model),
                Err(e) => Err(ProviderCallError::Model(e)),
            },
            Err(e) => Err(e),
        }
    }

    /// Structured tool failure fed back to the model instead of killing the
    /// task: the model gets a machine-readable error and may adapt (different
    /// tool, different arguments) or explain the obstacle to the user.
    fn tool_failure(&self, call: &PendingToolCall, err: &ToolError) -> String {
        serde_json::json!({
            "ok": false,
            "tool": call.tool_id,
            "error": err.to_string(),
        })
        .to_string()
    }
}

/// PART 5 gate: only structured input may reach the executor.
///
/// Objects and arrays are passed through (the tool schema validates shape);
/// `null` becomes `{}`. Raw strings, numbers and booleans — including JSON
/// prose the model emitted as *text* — are rejected with a structured
/// failure so no assistant-authored blob ever executes as a tool call.
fn validate_tool_input(input: &serde_json::Value) -> Result<serde_json::Value, ToolError> {
    match input {
        v @ serde_json::Value::Object(_) => Ok(v.clone()),
        v @ serde_json::Value::Array(_) => Ok(v.clone()),
        serde_json::Value::Null => Ok(serde_json::Value::Object(Default::default())),
        serde_json::Value::String(s) => Err(ToolError::InvalidArguments(
            "<tool>".into(),
            format!(
                "malformed tool arguments: raw text instead of a JSON object ({} chars)",
                s.len()
            ),
        )),
        serde_json::Value::Number(_) | serde_json::Value::Bool(_) => {
            Err(ToolError::InvalidArguments(
                "<tool>".into(),
                "malformed tool arguments: expected a JSON object".into(),
            ))
        }
    }
}

/// Part C: how one provider access ended, so the loop can distinguish a
/// transient/permanent model error from cancellation and cooperative pause.
enum ProviderCallError {
    Model(ModelError),
    Cancelled,
    Paused,
}

/// Part C guard heuristic: ~4 chars per token. Used ONLY to size the context
/// guard's compaction pass — never reported as a tokenizer measurement.
pub(crate) fn estimate_tokens(s: &str) -> usize {
    (s.chars().count() + 3) / 4
}

/// Shared, honest compaction used BOTH inside one segment (request assembly,
/// [`MinimalAgent::staged_request`]) AND by the runtime supervisor between
/// automatically-chained segments (bis-segment transcript trim, keeping the
/// multi-segment task from growing without bound).
///
/// Contract:
/// - a leading `System` framing message is never dropped;
/// - the FINAL message (the current input during assembly, the session tail at
///   a segment boundary) is never dropped;
/// - everything between is dropped oldest-first (never arbitrary) until the
///   estimated total fits `budget`;
/// - the drop is always explicit: a `System` marker turn is seated right after
///   the protected head so the model — and the durable session — show what was
///   compacted. Never silent truncation, never a fabricated window.
///
/// Returns `(compacted, dropped)`. `dropped == 0` with a response different
/// from the input means the input itself exceeds the budget and the CALLER must
/// refuse rather than loop helplessly.
pub fn compact_transcript(input: &[Message], budget: usize, marker: &str) -> (Vec<Message>, usize) {
    let mut total: usize = input.iter().map(|m| estimate_tokens(&m.content)).sum();
    if total <= budget {
        return (input.to_vec(), 0);
    }
    let head_end = usize::from(
        input
            .first()
            .map(|m| m.role == MessageRole::System)
            .unwrap_or(false),
    );
    let mut idx = head_end;
    let mut dropped = 0usize;
    while total > budget && idx + 1 < input.len() {
        total -= estimate_tokens(&input[idx].content);
        idx += 1;
        dropped += 1;
    }
    if dropped == 0 {
        return (input.to_vec(), 0);
    }
    let mut out: Vec<Message> = input[..head_end].to_vec();
    out.push(Message::system(marker.to_string()));
    out.extend_from_slice(&input[idx..]);
    (out, dropped)
}

/// Part C: bounded exponential backoff `base * 2^(attempt-1)`, capped at
/// `max`, so a degraded provider retries with delay but never spins hot.
/// Deterministic (no jitter) so tests and acceptance are reproducible.
fn backoff_delay(base_ms: u64, max_ms: u64, attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(16);
    Duration::from_millis(base_ms.saturating_mul(1u64 << shift).min(max_ms))
}

#[async_trait]
impl Agent for MinimalAgent {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &AgentContext, input: &str) -> Result<AgentResult, AgentError> {
        if ctx.cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }

        let mut messages = ctx.history.clone();
        messages.push(Message::user(input));
        // Everything the agent appends after the incoming history: the input
        // turn, every assistant tool-call turn, tool results, structured
        // failures. The pipeline folds this into the session so tool rounds
        // persist across follow-up tasks.
        let transcript_base = ctx.history.len();

        let mut turns = 0u32;
        let mut tool_calls_used = 0u32;
        // Honest, observable provider-retry counter surfaced on `AgentResult`
        // (never fabricated; incremented exactly where a bounded retry fires).
        let mut retries_used = 0u32;
        let mut answer = String::new();
        let mut ended = AgentEndReason::Completed;

        loop {
            if ctx.cancel.is_cancelled() {
                return Err(AgentError::Cancelled);
            }
            // Part C: cooperative pause at a turn boundary.
            if ctx.pause.as_ref().map(|p| p.is_paused()).unwrap_or(false) {
                ended = AgentEndReason::Paused;
                break;
            }
            if turns >= self.limits.max_turns || tool_calls_used >= self.limits.max_tool_calls {
                ended = AgentEndReason::LimitReached;
                break;
            }

            // Environment context rides every provider call, staged with the
            // Part C context guard (budget + honest compaction marker).
            // PART 16 + Part C: bounded retry WITH backoff — transient
            // provider failures (transport, timeout, HTTP, stream idle) get up
            // to `provider_retries` retries, each delayed by the bounded
            // exponential backoff (never a hot spin); permanent errors (parse
            // / configuration / egress) fail fast with a single attempt so a
            // malformed request is never resent in a loop.
            let mut retry_attempt = 1u32;
            let mut provider_attempts_left = self.limits.provider_retries;
            let mut empty_attempts_left = self.limits.provider_empty_response_retries;
            let responses: Option<openrt_models::ModelResponse> = 'call: loop {
                let req = self.staged_request(&messages)?;
                match self.call_provider(ctx, req).await {
                    Ok(r) => {
                        // BUG 1/3/4: empty assistant response with no tool calls is not a
                        // successful final answer — retry generation (bounded) reusing
                        // the same conversation state.
                        if r.tool_calls.is_empty() && r.text.trim().is_empty() {
                            if empty_attempts_left > 0 {
                                empty_attempts_left -= 1;
                                retries_used += 1;
                                match self.backoff_sleep(ctx, retry_attempt).await {
                                    SleepEnd::Cancelled => return Err(AgentError::Cancelled),
                                    SleepEnd::Paused => {
                                        ended = AgentEndReason::Paused;
                                        break None;
                                    }
                                    SleepEnd::Done => {}
                                }
                                retry_attempt += 1;
                                continue 'call;
                            }
                            // Retries exhausted: return structured MODEL_TOOL_CALL_PARSE_ERROR
                            return Err(AgentError::Model(
                                "MODEL_TOOL_CALL_PARSE_ERROR: provider returned an empty assistant response without a tool call (retries exhausted)".into(),
                            ));
                        }
                        break Some(r);
                    }
                    Err(ProviderCallError::Paused) => {
                        ended = AgentEndReason::Paused;
                        break None;
                    }
                    Err(ProviderCallError::Cancelled) => return Err(AgentError::Cancelled),
                    Err(ProviderCallError::Model(e))
                        if !e.is_permanent() && provider_attempts_left > 0 =>
                    {
                        provider_attempts_left -= 1;
                        retries_used += 1;
                        match self.backoff_sleep(ctx, retry_attempt).await {
                            SleepEnd::Cancelled => return Err(AgentError::Cancelled),
                            SleepEnd::Paused => {
                                ended = AgentEndReason::Paused;
                                break None;
                            }
                            SleepEnd::Done => {}
                        }
                        retry_attempt += 1;
                        continue 'call;
                    }
                    Err(ProviderCallError::Model(e)) => {
                        return Err(AgentError::Model(e.to_string()))
                    }
                }
            };
            let Some(responses) = responses else {
                debug_assert_eq!(ended, AgentEndReason::Paused);
                break;
            };

            // PART 5/6: execute whenever the model produced *any* tool calls —
            // the finish_reason gate below would silently drop structured calls
            // from providers that report `stop`. Text-only responses never
            // reach the executor.
            if !responses.tool_calls.is_empty() {
                let calls: Vec<PendingToolCall> = responses
                    .tool_calls
                    .iter()
                    .map(|c| PendingToolCall {
                        id: c.id.clone(),
                        tool_id: c.name.clone(),
                        input: c.arguments.clone(),
                    })
                    .collect();

                // Assistant message carrying the tool calls back to the model.
                // PART 6: any co-emitted text is preserved (redacted at the
                // same boundary) so the next round-trip keeps context.
                messages.push(Message {
                    role: MessageRole::Assistant,
                    content: self.redaction.redact_text(&responses.text),
                    tool_calls: calls.clone(),
                    tool_call_id: None,
                    at: None,
                });

                let mut hit_tool_limit = false;
                for call in &calls {
                    if ctx.cancel.is_cancelled() {
                        return Err(AgentError::Cancelled);
                    }
                    if tool_calls_used >= self.limits.max_tool_calls {
                        hit_tool_limit = true;
                        messages.push(Message::tool(
                            &call.id,
                            serde_json::json!({
                                "ok": false,
                                "tool": call.tool_id,
                                "error": "tool call limit reached",
                            })
                            .to_string(),
                        ));
                        continue;
                    }
                    // PART 5: only structured input executes. Raw prose /
                    // scalars from an assistant turn are a structured failure
                    // fed back to the model — never run through the executor.
                    tool_calls_used += 1;
                    if let Err(e) = validate_tool_input(&call.input) {
                        messages.push(Message::tool(&call.id, self.tool_failure(call, &e)));
                        continue;
                    }
                    match self
                        .executor
                        .execute(&call.tool_id, call.input.clone(), &self.tool_context(ctx))
                        .await
                    {
                        Ok(output) => {
                            // Redaction boundary before the model (and the
                            // persisted transcript) sees the payload.
                            let payload = self.redaction.redact_text(&output.render());
                            messages.push(Message::tool(&call.id, payload));
                        }
                        Err(ToolError::Cancelled(_)) => return Err(AgentError::Cancelled),
                        Err(e) => {
                            // Denied/failed calls are structured feedback,
                            // not fatal: the loop continues with the model
                            // free to adapt or report the obstacle.
                            let payload = self.tool_failure(call, &e);
                            messages.push(Message::tool(&call.id, payload));
                        }
                    }
                }
                turns += 1;
                if hit_tool_limit {
                    ended = AgentEndReason::LimitReached;
                    break;
                }
                continue;
            }

            // Plain text: terminal answer. Passes the same redaction
            // boundary — the durable transcript/task result must not carry
            // secret-shaped content either.
            answer = self.redaction.redact_text(&responses.text);
            turns += 1;
            break;
        }

        Ok(AgentResult {
            final_text: answer,
            ended,
            turns,
            tool_calls: tool_calls_used,
            retries: retries_used,
            messages: messages.split_off(transcript_base),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_core::Id;
    use openrt_models::mock::MockProvider;
    use openrt_models::ModelError;
    use openrt_tools::{DirectExecutor, ToolRegistry};

    fn setup() -> (
        Arc<dyn ModelProvider>,
        Arc<dyn ToolExecutor>,
        Vec<serde_json::Value>,
    ) {
        let provider: Arc<dyn ModelProvider> = Arc::new(MockProvider::tool_sequence("mock"));
        let registry = ToolRegistry::builtins();
        let tools = registry.provider_tools();
        let executor: Arc<dyn ToolExecutor> = Arc::new(DirectExecutor::new(registry));
        (provider, executor, tools)
    }

    fn ctx() -> AgentContext {
        AgentContext::new(Id::new(), Id::new())
    }

    #[tokio::test]
    async fn completes_scripted_tool_run() {
        let (p, e, tools) = setup();
        let agent = MinimalAgent::with_default_limits("minimal", p, e, tools);
        let result = agent.run(&ctx(), "use echo to say hello").await.unwrap();
        assert_eq!(result.ended, AgentEndReason::Completed);
        assert!(result.final_text.contains("hello"));
        assert!(result.turns >= 2);
    }

    #[tokio::test]
    async fn respects_max_turns() {
        let (p, e, tools) = setup();
        let limits = AgentTurnLimits {
            max_turns: 1,
            ..AgentTurnLimits::default()
        };
        let agent = MinimalAgent::new("minimal", p, e, limits, tools);
        let result = agent.run(&ctx(), "loop forever").await.unwrap();
        assert_eq!(result.ended, AgentEndReason::LimitReached);
        assert!(result.final_text.is_empty());
    }

    #[tokio::test]
    async fn cancellation_stops_loop() {
        let (p, e, tools) = setup();
        let agent = MinimalAgent::with_default_limits("minimal", p, e, tools);
        let c = ctx();
        c.cancel.cancel();
        let err = agent.run(&c, "halt").await.unwrap_err();
        assert_eq!(err, AgentError::Cancelled);
    }

    #[tokio::test]
    async fn transcript_carries_tool_rounds() {
        let (p, e, tools) = setup();
        let agent = MinimalAgent::with_default_limits("minimal", p, e, tools);
        let c = ctx().with_history(vec![Message::assistant("earlier answer")]);
        let result = agent.run(&c, "use echo to say hello").await.unwrap();
        // History stays out; input + tool rounds are in.
        assert!(!result
            .messages
            .iter()
            .any(|m| m.content == "earlier answer"));
        assert_eq!(result.messages.first().unwrap().role, MessageRole::User);
        assert!(result
            .messages
            .iter()
            .any(|m| m.role == MessageRole::Assistant && !m.tool_calls.is_empty()));
        assert!(result.messages.iter().any(|m| m.role == MessageRole::Tool));
        // The final answer is reported separately, not in the transcript.
        assert!(!result
            .messages
            .iter()
            .any(|m| m.role == MessageRole::Assistant
                && m.tool_calls.is_empty()
                && m.content == result.final_text));
    }

    #[tokio::test]
    async fn denied_tool_call_is_structured_feedback_not_fatal() {
        let provider = MockProvider::tool_sequence("mock")
            .with_tool_script(vec![("no_such_tool", serde_json::json!({}))]);
        let registry = ToolRegistry::builtins();
        let tools = registry.provider_tools();
        let executor: Arc<dyn ToolExecutor> = Arc::new(DirectExecutor::new(registry));
        let agent =
            MinimalAgent::with_default_limits("minimal", Arc::new(provider), executor, tools);
        let result = agent.run(&ctx(), "try it").await.unwrap();
        assert_eq!(result.ended, AgentEndReason::Completed);
        // The failure came back to the model as a tool message, and the
        // model produced a final answer afterwards — the task did not die.
        assert!(result.final_text.contains("mock final"));
        assert!(result.final_text.contains("unknown tool"));
        assert!(result.final_text.contains("\"ok\":false"));
        let tool_msgs: Vec<_> = result
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .collect();
        assert_eq!(tool_msgs.len(), 1);
        assert!(tool_msgs[0].content.contains("\"ok\":false"));
    }

    #[tokio::test]
    async fn max_tool_calls_is_enforced() {
        let script = vec![
            ("echo", serde_json::json!({ "text": "one" })),
            ("echo", serde_json::json!({ "text": "two" })),
            ("echo", serde_json::json!({ "text": "three" })),
        ];
        let provider = Arc::new(MockProvider::tool_sequence("mock").with_tool_script(script));
        let registry = ToolRegistry::builtins();
        let tools = registry.provider_tools();
        let executor: Arc<dyn ToolExecutor> = Arc::new(DirectExecutor::new(registry));
        let limits = AgentTurnLimits {
            max_tool_calls: 1,
            max_turns: 6,
            ..AgentTurnLimits::default()
        };
        let agent = MinimalAgent::new("minimal", provider.clone(), executor, limits, tools);
        let result = agent.run(&ctx(), "keep going").await.unwrap();
        assert_eq!(result.ended, AgentEndReason::LimitReached);
        assert_eq!(provider.call_count(), 1, "no generation after the limit");
        assert_eq!(
            result
                .messages
                .iter()
                .filter(|m| m.role == MessageRole::Tool)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn tool_definitions_reach_the_model_request() {
        struct Recorder {
            seen: std::sync::Mutex<Vec<serde_json::Value>>,
        }
        #[async_trait::async_trait]
        impl ModelProvider for Recorder {
            fn id(&self) -> &str {
                "recorder"
            }
            fn info(&self) -> openrt_models::ModelInfo {
                openrt_models::ModelInfo {
                    id: "recorder".into(),
                    kind: openrt_models::ProviderKind::Mock,
                    display_name: "Recorder".into(),
                    default_model: Some("mock://default".into()),
                }
            }
            async fn generate(
                &self,
                req: openrt_models::ModelRequest,
            ) -> Result<openrt_models::ModelResponse, openrt_models::ModelError> {
                self.seen
                    .lock()
                    .unwrap()
                    .push(serde_json::json!({ "tools": req.tools }));
                Ok(openrt_models::ModelResponse {
                    model: "mock://default".into(),
                    text: "done".into(),
                    tool_calls: vec![],
                    finish_reason: openrt_models::FinishReason::Stop,
                    usage: openrt_models::ModelUsage::default(),
                })
            }
            fn capabilities(&self) -> openrt_models::ModelCapabilities {
                openrt_models::ModelCapabilities {
                    supports_tools: true,
                    supports_streaming: false,
                    context_window: Some(4096),
                }
            }
        }
        let provider = Arc::new(Recorder {
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let registry = ToolRegistry::builtins();
        let tools = registry.provider_tools();
        let expected_tool_count = tools.len();
        let executor: Arc<dyn ToolExecutor> = Arc::new(DirectExecutor::new(registry));
        let agent = MinimalAgent::with_default_limits("minimal", provider.clone(), executor, tools);
        agent.run(&ctx(), "go").await.unwrap();
        let seen = provider.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let tools_in_request = &seen[0]["tools"];
        assert_eq!(
            tools_in_request.as_array().unwrap().len(),
            expected_tool_count
        );
        assert!(tools_in_request
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "echo"));
    }

    #[tokio::test]
    async fn system_prompt_reaches_provider_but_not_transcript() {
        struct RoleRecorder {
            first: std::sync::Mutex<Vec<(String, String)>>,
        }
        #[async_trait::async_trait]
        impl ModelProvider for RoleRecorder {
            fn id(&self) -> &str {
                "roles"
            }
            fn info(&self) -> openrt_models::ModelInfo {
                openrt_models::ModelInfo {
                    id: "roles".into(),
                    kind: openrt_models::ProviderKind::Mock,
                    display_name: "RoleRecorder".into(),
                    default_model: Some("mock://default".into()),
                }
            }
            async fn generate(
                &self,
                req: openrt_models::ModelRequest,
            ) -> Result<openrt_models::ModelResponse, openrt_models::ModelError> {
                let first = req
                    .messages
                    .first()
                    .map(|m| (format!("{:?}", m.role), m.content.clone()))
                    .unwrap_or_default();
                self.first.lock().unwrap().push(first);
                Ok(openrt_models::ModelResponse {
                    model: "mock://default".into(),
                    text: "done".into(),
                    tool_calls: vec![],
                    finish_reason: openrt_models::FinishReason::Stop,
                    usage: openrt_models::ModelUsage::default(),
                })
            }
            fn capabilities(&self) -> openrt_models::ModelCapabilities {
                openrt_models::ModelCapabilities {
                    supports_tools: true,
                    supports_streaming: false,
                    context_window: Some(4096),
                }
            }
        }
        let provider = Arc::new(RoleRecorder {
            first: std::sync::Mutex::new(Vec::new()),
        });
        let registry = ToolRegistry::builtins();
        let tools = registry.provider_tools();
        let executor: Arc<dyn ToolExecutor> = Arc::new(DirectExecutor::new(registry));
        let agent = MinimalAgent::with_default_limits("minimal", provider.clone(), executor, tools)
            .with_system_prompt("workspace root is `/tmp/ws`");
        let result = agent.run(&ctx(), "go").await.unwrap();

        // Request-time injection: the provider sees the system message first.
        let seen = provider.first.lock().unwrap();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].0, "System", "{seen:?}");
        assert!(seen[0].1.contains("/tmp/ws"), "{seen:?}");

        // Transcript injection: the persisted delta never carries it.
        assert!(
            result
                .messages
                .iter()
                .all(|m| m.role != MessageRole::System),
            "{:?}",
            result.messages
        );
        assert_eq!(result.messages.len(), 1, "{:?}", result.messages);
    }

    #[tokio::test]
    async fn secret_in_final_answer_is_redacted_before_persistence() {
        struct SecretText;
        #[async_trait::async_trait]
        impl ModelProvider for SecretText {
            fn id(&self) -> &str {
                "secrettext"
            }
            fn info(&self) -> openrt_models::ModelInfo {
                openrt_models::ModelInfo {
                    id: "secrettext".into(),
                    kind: openrt_models::ProviderKind::Mock,
                    display_name: "SecretText".into(),
                    default_model: Some("mock://default".into()),
                }
            }
            async fn generate(
                &self,
                _req: openrt_models::ModelRequest,
            ) -> Result<openrt_models::ModelResponse, openrt_models::ModelError> {
                Ok(openrt_models::ModelResponse {
                    model: "mock://default".into(),
                    text: "Done. password: supersecret9999 is now set.".into(),
                    tool_calls: vec![],
                    finish_reason: openrt_models::FinishReason::Stop,
                    usage: openrt_models::ModelUsage::default(),
                })
            }
            fn capabilities(&self) -> openrt_models::ModelCapabilities {
                openrt_models::ModelCapabilities {
                    supports_tools: false,
                    supports_streaming: false,
                    context_window: Some(4096),
                }
            }
        }
        let provider = Arc::new(SecretText);
        let registry = ToolRegistry::builtins();
        let tools = registry.provider_tools();
        let executor: Arc<dyn ToolExecutor> = Arc::new(DirectExecutor::new(registry));
        let agent = MinimalAgent::with_default_limits("minimal", provider, executor, tools);
        let result = agent.run(&ctx(), "set it").await.unwrap();
        // The durable transcript/task result must not carry the secret.
        assert!(
            !result.final_text.contains("supersecret9999"),
            "{}",
            result.final_text
        );
        assert!(
            result.final_text.contains("[REDACTED]"),
            "{}",
            result.final_text
        );
        assert!(
            result
                .messages
                .iter()
                .all(|m| !m.content.contains("supersecret9999")),
            "{:?}",
            result.messages
        );
    }

    #[tokio::test]
    async fn secret_shaped_tool_output_is_redacted_before_the_model() {
        let (p, executor, tools) = setup();
        let agent = MinimalAgent::with_default_limits("minimal", p, executor, tools);
        let result = agent
            .run(&ctx(), "echo Bearer supersecretvalue123")
            .await
            .unwrap();
        assert_eq!(result.ended, AgentEndReason::Completed);
        let tool_payloads: String = result
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!tool_payloads.contains("supersecretvalue123"));
        assert!(tool_payloads.contains(openrt_core::REDACTED));
        assert!(!result.final_text.contains("supersecretvalue123"));
    }

    // ---- PART 5/13/14/16: tool-loop regression battery ------------------

    /// Test double: scripted responses; records every request.
    struct ScriptedProvider {
        script: std::sync::Mutex<Vec<openrt_models::ModelResponse>>,
        requests: std::sync::Mutex<Vec<openrt_models::ModelRequest>>,
        fail_first_n: std::sync::atomic::AtomicU32,
        fail_kind: ModelError,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<openrt_models::ModelResponse>) -> Self {
            Self {
                script: std::sync::Mutex::new(responses),
                requests: std::sync::Mutex::new(Vec::new()),
                fail_first_n: std::sync::atomic::AtomicU32::new(0),
                fail_kind: ModelError::Transport("flaky".into()),
            }
        }

        fn failing_first(mut self, n: u32, kind: ModelError) -> Self {
            self.fail_first_n
                .store(n, std::sync::atomic::Ordering::SeqCst);
            self.fail_kind = kind;
            self
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for ScriptedProvider {
        fn id(&self) -> &str {
            "scripted"
        }
        fn info(&self) -> openrt_models::ModelInfo {
            openrt_models::ModelInfo {
                id: "scripted".into(),
                kind: openrt_models::ProviderKind::Mock,
                display_name: "Scripted".into(),
                default_model: Some("scripted".into()),
            }
        }
        async fn generate(
            &self,
            req: openrt_models::ModelRequest,
        ) -> Result<openrt_models::ModelResponse, openrt_models::ModelError> {
            let left = self
                .fail_first_n
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |v| v.checked_sub(1),
                )
                .is_ok();
            if left {
                return Err(self.fail_kind.clone());
            }
            self.requests.lock().unwrap().push(req);
            let mut script = self.script.lock().unwrap();
            if script.is_empty() {
                return Err(ModelError::Unexpected("script exhausted".into()));
            }
            Ok(script.remove(0))
        }
        fn capabilities(&self) -> openrt_models::ModelCapabilities {
            openrt_models::ModelCapabilities {
                supports_tools: true,
                supports_streaming: false,
                context_window: Some(4096),
            }
        }
    }

    fn call_response(
        id: Option<serde_json::Value>,
        args: serde_json::Value,
        name: &str,
    ) -> openrt_models::ModelResponse {
        openrt_models::ModelResponse {
            model: "scripted".into(),
            text: String::new(),
            tool_calls: vec![openrt_models::ProviderToolCall {
                id: id
                    .map(|v| v.to_string().trim_matches('"').to_string())
                    .unwrap_or_else(|| "openrt-call-gen".into()),
                name: name.into(),
                arguments: args,
            }],
            finish_reason: openrt_models::FinishReason::ToolCalls,
            usage: openrt_models::ModelUsage::default(),
        }
    }

    fn text_response(text: &str) -> openrt_models::ModelResponse {
        openrt_models::ModelResponse {
            model: "scripted".into(),
            text: text.into(),
            tool_calls: vec![],
            finish_reason: openrt_models::FinishReason::Stop,
            usage: openrt_models::ModelUsage::default(),
        }
    }

    fn direct_agent(provider: Arc<dyn ModelProvider>, limits: AgentTurnLimits) -> MinimalAgent {
        let registry = ToolRegistry::builtins();
        let tools = registry.provider_tools();
        let executor: Arc<dyn ToolExecutor> = Arc::new(DirectExecutor::new(registry));
        MinimalAgent::new("minimal", provider, executor, limits, tools)
    }

    /// PART 5: prose arguments never execute — structured failure comes
    /// back to the model, the executor is not consulted.
    #[tokio::test]
    async fn raw_text_arguments_never_execute() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            call_response(None, serde_json::json!("run the thing now"), "echo"),
            text_response("cannot do it"),
        ]));
        let agent = direct_agent(provider, AgentTurnLimits::default());
        let result = agent.run(&ctx(), "go").await.unwrap();
        assert_eq!(result.ended, AgentEndReason::Completed);
        let tool_msgs: Vec<_> = result
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .collect();
        assert_eq!(tool_msgs.len(), 1, "one structured failure fed back");
        let payload = &tool_msgs[0].content;
        assert!(payload.contains("\"ok\":false"), "{payload}");
        assert!(payload.contains("malformed tool arguments"), "{payload}");
        // The echo tool's real output ("...") never appeared.
        assert!(!payload.contains("run the thing now"), "{payload}");
    }

    /// PART 5: numeric/bool arguments are rejected the same way.
    #[tokio::test]
    async fn scalar_arguments_never_execute() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            call_response(None, serde_json::json!(42), "echo"),
            text_response("done"),
        ]));
        let agent = direct_agent(provider, AgentTurnLimits::default());
        let result = agent.run(&ctx(), "go").await.unwrap();
        let payload = result
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Tool)
            .map(|m| m.content.clone())
            .unwrap();
        assert!(payload.contains("\"ok\":false"), "{payload}");
        assert!(payload.contains("malformed tool arguments"), "{payload}");
    }

    /// PART 4/5: a valid structured call still executes after normalization.
    #[tokio::test]
    async fn structured_object_arguments_execute() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            call_response(None, serde_json::json!({"text": "hello"}), "echo"),
            text_response("done"),
        ]));
        let agent = direct_agent(provider, AgentTurnLimits::default());
        let result = agent.run(&ctx(), "go").await.unwrap();
        let payload = result
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Tool)
            .map(|m| m.content.clone())
            .unwrap();
        assert!(payload.contains("hello"), "{payload}");
    }

    /// BUG 1/3/4: empty assistant response with no tool calls is not
    /// a successful completion — after bounded retry exhausted, the agent
    /// returns a structured MODEL_TOOL_CALL_PARSE_ERROR.
    #[tokio::test]
    async fn empty_response_without_tool_calls_is_structured_failure() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            text_response(""),
            text_response(""),
            text_response("should not reach"),
        ]));
        let limits = AgentTurnLimits {
            provider_empty_response_retries: 1,
            ..AgentTurnLimits::default()
        };
        let agent = direct_agent(provider.clone(), limits);
        let err = agent.run(&ctx(), "go").await.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("MODEL_TOOL_CALL_PARSE_ERROR"), "{msg}");
        assert!(msg.contains("empty assistant response"), "{msg}");
    }

    /// BUG 1/3/4: empty response once, then a valid answer → retry consumed,
    /// task completes.
    #[tokio::test]
    async fn empty_response_retries_once_then_recovers() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            text_response(""),
            text_response("recovered"),
        ]));
        let limits = AgentTurnLimits {
            provider_empty_response_retries: 2,
            ..AgentTurnLimits::default()
        };
        let agent = direct_agent(provider.clone(), limits);
        let result = agent.run(&ctx(), "go").await.unwrap();
        assert_eq!(result.final_text, "recovered");
        assert_eq!(result.ended, AgentEndReason::Completed);
    }

    /// PART 5: text-only responses never reach the executor, even when the
    /// text itself looks like JSON tool calls.
    #[tokio::test]
    async fn assistant_text_json_is_never_executed() {
        let provider = Arc::new(ScriptedProvider::new(vec![text_response(
            "{\"tool_calls\":[{\"function\":{\"name\":\"echo\",\"arguments\":{\"text\":\"pwn\"}}}]}",
        )]));
        let agent = direct_agent(provider, AgentTurnLimits::default());
        let result = agent.run(&ctx(), "go").await.unwrap();
        assert_eq!(result.ended, AgentEndReason::Completed);
        assert!(
            result.messages.iter().all(|m| m.role != MessageRole::Tool),
            "no tool messages for text-only output"
        );
        assert!(result.final_text.contains("tool_calls"));
    }

    /// PART 6: tool_calls execute even when finish_reason says `stop`
    /// (providers that mislabel); text-only with `stop` answers normally.
    #[tokio::test]
    async fn tool_calls_execute_regardless_of_finish_reason() {
        let mut resp = call_response(None, serde_json::json!({"text": "hi"}), "echo");
        resp.finish_reason = openrt_models::FinishReason::Stop;
        let provider = Arc::new(ScriptedProvider::new(vec![resp, text_response("done")]));
        let agent = direct_agent(provider, AgentTurnLimits::default());
        let result = agent.run(&ctx(), "go").await.unwrap();
        assert!(
            result.messages.iter().any(|m| m.role == MessageRole::Tool),
            "tool result present"
        );
        assert_eq!(result.final_text, "done");
    }

    /// PART 16: a transient provider error is retried once and the task
    /// completes; the request count shows exactly 2 generate calls.
    #[tokio::test]
    async fn transient_provider_error_retries_once() {
        let provider = Arc::new(
            ScriptedProvider::new(vec![text_response("recovered")])
                .failing_first(1, ModelError::Transport("blip".into())),
        );
        let agent = direct_agent(provider.clone(), AgentTurnLimits::default());
        let result = agent.run(&ctx(), "go").await.unwrap();
        assert_eq!(result.final_text, "recovered");
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            1,
            "one successful request after one retry"
        );
    }

    /// PART 16: permanent errors (parse) fail fast — exactly one attempt.
    #[tokio::test]
    async fn permanent_provider_error_fails_fast() {
        let provider = Arc::new(
            ScriptedProvider::new(vec![text_response("never")]).failing_first(
                99,
                ModelError::InvalidResponse("Expected 'id' to be a string.".into()),
            ),
        );
        let agent = direct_agent(provider.clone(), AgentTurnLimits::default());
        let err = agent.run(&ctx(), "go").await.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("Expected 'id' to be a string."), "{msg}");
        assert!(
            provider.requests.lock().unwrap().is_empty(),
            "no request ever succeeded; permanent errors are not retried"
        );
    }

    /// PART 16: `provider_retries: 0` fails on the first transient error
    /// (workflow engine owns retry accounting).
    #[tokio::test]
    async fn zero_retry_budget_fails_immediately() {
        let provider = Arc::new(
            ScriptedProvider::new(vec![text_response("never")])
                .failing_first(99, ModelError::Transport("down".into())),
        );
        let limits = AgentTurnLimits {
            provider_retries: 0,
            ..AgentTurnLimits::default()
        };
        let agent = direct_agent(provider, limits);
        let err = agent.run(&ctx(), "go").await.unwrap_err();
        assert!(matches!(err, AgentError::Model(_)));
    }

    /// PART 14: an invalid-tool-call provider error surfaces as a structured
    /// task failure (AgentError::Model) — the session stays usable (PART 26:
    /// the error propagates, nothing panics).
    #[tokio::test]
    async fn provider_parse_error_becomes_structured_agent_error() {
        let provider = Arc::new(
            ScriptedProvider::new(vec![text_response("never")]).failing_first(
                99,
                ModelError::InvalidResponse(
                    "finish_reason `tool_calls` but response contains no tool calls".into(),
                ),
            ),
        );
        let agent = direct_agent(provider, AgentTurnLimits::default());
        let err = agent.run(&ctx(), "go").await.unwrap_err();
        assert!(matches!(err, AgentError::Model(_)));
    }

    /// PART 6/7: ids are stable through the transcript — the tool message
    /// references the assistant's call id, and follow-up history keeps it.
    #[tokio::test]
    async fn tool_call_ids_stay_stable_through_transcript() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            call_response(
                Some(serde_json::json!("call_7")),
                serde_json::json!({"text": "x"}),
                "echo",
            ),
            text_response("finished"),
        ]));
        let agent = direct_agent(provider, AgentTurnLimits::default());
        let result = agent.run(&ctx(), "go").await.unwrap();
        let assistant = result
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Assistant && !m.tool_calls.is_empty())
            .unwrap();
        let tool_msg = result
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Tool)
            .unwrap();
        assert_eq!(assistant.tool_calls[0].id, "call_7");
        assert_eq!(tool_msg.tool_call_id.as_deref(), Some("call_7"));
    }

    /// PART 6: co-emitted assistant text is preserved in the transcript.
    #[tokio::test]
    async fn co_emitted_text_is_kept_in_assistant_turn() {
        let mut resp = call_response(None, serde_json::json!({"text": "x"}), "echo");
        resp.text = "I will read the file first.".into();
        let provider = Arc::new(ScriptedProvider::new(vec![resp, text_response("ok")]));
        let agent = direct_agent(provider, AgentTurnLimits::default());
        let result = agent.run(&ctx(), "go").await.unwrap();
        let assistant = result
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Assistant && !m.tool_calls.is_empty())
            .unwrap();
        assert_eq!(assistant.content, "I will read the file first.");
    }

    // ---- PART C: backoff -------------------------------------------------

    /// PART C: the retry backoff is deterministic exponential, capped at the
    /// configured ceiling, and never below the base.
    #[test]
    fn backoff_delay_is_bounded_exponential() {
        let max_ms = 1000u64;
        assert_eq!(
            backoff_delay(100, max_ms, 1),
            Duration::from_millis(100),
            "first retry uses the base"
        );
        assert_eq!(
            backoff_delay(100, max_ms, 2),
            Duration::from_millis(200),
            "second retry doubles"
        );
        assert_eq!(
            backoff_delay(100, max_ms, 10),
            Duration::from_millis(1000),
            "growth is capped at the ceiling"
        );
        assert_eq!(
            backoff_delay(3000, max_ms, 1),
            Duration::from_millis(1000),
            "a base above the ceiling is clamped"
        );
    }

    // ---- PART C: context guard / compaction ------------------------------

    /// Test double: records every assembled request; exposes a configurable
    /// declared context window so the guard has something to measure.
    struct WindowProvider {
        window: Option<usize>,
        requests: std::sync::Arc<std::sync::Mutex<Vec<openrt_models::ModelRequest>>>,
    }

    #[async_trait::async_trait]
    impl ModelProvider for WindowProvider {
        fn id(&self) -> &str {
            "window"
        }
        fn info(&self) -> openrt_models::ModelInfo {
            openrt_models::ModelInfo {
                id: "window".into(),
                kind: openrt_models::ProviderKind::Mock,
                display_name: "Window".into(),
                default_model: Some("window".into()),
            }
        }
        async fn generate(
            &self,
            req: openrt_models::ModelRequest,
        ) -> Result<openrt_models::ModelResponse, ModelError> {
            self.requests.lock().unwrap().push(req);
            Ok(text_response("ok"))
        }
        fn capabilities(&self) -> openrt_models::ModelCapabilities {
            openrt_models::ModelCapabilities {
                supports_tools: false,
                supports_streaming: false,
                context_window: self.window,
            }
        }
    }

    /// PART C (B → C): when the assembled request breaches the guard budget,
    /// the oldest turns are compacted with an explicit marker — the input and
    /// the persisted system framing survive, and the composer never silently
    /// truncates.
    #[tokio::test]
    async fn context_guard_compacts_oldest_turns_and_keeps_input_and_system() {
        const WINDOW: usize = 200; // guard ratio 0.7 → budget 140 tokens
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = {
            let registry = ToolRegistry::builtins();
            Arc::new(DirectExecutor::new(registry)) as Arc<dyn ToolExecutor>
        };
        let provider: Arc<dyn ModelProvider> = Arc::new(WindowProvider {
            window: Some(WINDOW),
            requests: requests.clone(),
        });
        let agent = MinimalAgent::new(
            "minimal",
            provider,
            executor,
            AgentTurnLimits::default(),
            Vec::new(),
        );
        // A long history: six turns, each ~40 tokens, pushes the total well
        // past the budget on its own.
        let long_history: Vec<Message> = (0..6u32)
            .map(|i| Message::user(format!("turn {i} {}", "x".repeat(120))))
            .collect();
        // The persisted system framing rides at index 0 of the history.
        let mut framed = Vec::new();
        framed.push(Message::system(
            "you are the environment framing, long-lived and never compacted",
        ));
        framed.extend(long_history.clone());
        let mut c = ctx();
        c.history = framed;
        let result = agent.run(&c, "final input, must survive").await.unwrap();
        assert_eq!(result.final_text, "ok");
        let guard: Vec<_> = requests.lock().unwrap().clone();
        assert_eq!(guard.len(), 1);
        let sent = &guard[0].messages;
        let text = sent.iter().map(|m| m.content.as_str()).collect::<Vec<_>>();
        assert!(text[0].contains("environment framing"), "system survives");
        assert!(
            text.iter().any(|t| t.contains("(context compaction)")),
            "marker inserted: composer must never silently truncate"
        );
        assert!(
            text.iter()
                .any(|t| t == &("turn 5 ".to_string() + &"x".repeat(120))),
            "newest history is kept verbatim"
        );
        assert!(
            !text.iter().any(|t| t.starts_with("turn 0 ")),
            "oldest turns were compacted"
        );
    }

    /// PART C (B → C): an input that alone exceeds the guard budget is an
    /// honest refusal — never a truncated instruction.
    #[tokio::test]
    async fn context_guard_refuses_when_input_alone_exceeds_budget() {
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = {
            let registry = ToolRegistry::builtins();
            Arc::new(DirectExecutor::new(registry)) as Arc<dyn ToolExecutor>
        };
        let agent = MinimalAgent::new(
            "minimal",
            Arc::new(WindowProvider {
                window: Some(40), // budget 28 tokens
                requests: requests.clone(),
            }),
            executor,
            AgentTurnLimits::default(),
            Vec::new(),
        );
        let err = agent.run(&ctx(), &"y".repeat(400)).await.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("context guard"), "{msg}");
        assert!(msg.contains("raise the model's context window"), "{msg}");
        assert!(
            requests.lock().unwrap().is_empty(),
            "an out-of-budget request is refused before any provider call"
        );
    }

    /// PART C: a streaming gap longer than the configured idle budget is a
    /// transient error (backoff-retried), never a silent completion.
    #[tokio::test]
    async fn idle_watchdog_flags_stream_gap_as_transient() {
        let provider: Arc<dyn ModelProvider> = Arc::new(
            MockProvider::echo("mock")
                .with_streaming()
                .with_stream_gap(Duration::from_millis(80)),
        );
        let agent = MinimalAgent::new(
            "minimal",
            provider,
            {
                let registry = ToolRegistry::builtins();
                Arc::new(DirectExecutor::new(registry)) as Arc<dyn ToolExecutor>
            },
            AgentTurnLimits {
                idle_timeout_ms: Some(10),
                provider_retries: 0, // no masking of the idle root cause
                ..AgentTurnLimits::default()
            },
            Vec::new(),
        );
        let err = agent.run(&ctx(), "tick").await.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("stream idle"), "{msg}");
        assert!(matches!(err, AgentError::Model(_)), "{msg}");
    }

    /// PART C: a pre-engaged pause ends the run immediately at the first
    /// boundary — zero model turns, transcript preserved for resume.
    #[tokio::test]
    async fn pre_paused_run_ends_paused_immediately() {
        let pause = Arc::new(openrt_core::PauseToken::new());
        pause.pause();
        let provider: Arc<dyn ModelProvider> = Arc::new(MockProvider::echo("mock"));
        let agent = MinimalAgent::new(
            "minimal",
            provider,
            {
                let registry = ToolRegistry::builtins();
                Arc::new(DirectExecutor::new(registry)) as Arc<dyn ToolExecutor>
            },
            AgentTurnLimits::default(),
            Vec::new(),
        );
        let c = ctx().with_pause(pause);
        let result = agent.run(&c, "hello").await.unwrap();
        assert_eq!(result.ended, AgentEndReason::Paused);
        assert_eq!(result.turns, 0);
        assert_eq!(result.final_text, "");
        assert!(
            result.messages.iter().any(|m| m.role == MessageRole::User),
            "the input is preserved for a later resume"
        );
    }

    /// PART C: pausing while the agent sleeps in a backoff interrupts the
    /// sleep and stops the run at the next boundary (an operator pause is
    /// never swallowed by a long retry delay).
    #[tokio::test]
    async fn pause_during_backoff_sleep_is_honoured() {
        let pause = Arc::new(openrt_core::PauseToken::new());
        let provider: Arc<dyn ModelProvider> = Arc::new(
            ScriptedProvider::new(vec![text_response("never")])
                .failing_first(99, ModelError::Transport("down".into())),
        );
        let agent = MinimalAgent::new(
            "minimal",
            provider,
            {
                let registry = ToolRegistry::builtins();
                Arc::new(DirectExecutor::new(registry)) as Arc<dyn ToolExecutor>
            },
            AgentTurnLimits {
                provider_retries: 4,
                retry_backoff_base_ms: 60_000, // long enough to pause inside
                retry_backoff_max_ms: 60_000,
                ..AgentTurnLimits::default()
            },
            Vec::new(),
        );
        let c = ctx().with_pause(pause.clone());
        let run = tokio::spawn(async move { agent.run(&c, "go").await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        pause.pause();
        let result = run.await.unwrap().unwrap();
        assert_eq!(result.ended, AgentEndReason::Paused);
    }

    /// Integration path: generation steering configured on the agent reaches
    /// the assembled provider request verbatim (within the declared window),
    /// so the Ollama serializer can honor temperature / num_predict / num_ctx.
    #[tokio::test]
    async fn configured_generation_parameters_reach_the_provider_request() {
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let agent = MinimalAgent::new(
            "minimal",
            Arc::new(WindowProvider {
                window: Some(8192),
                requests: requests.clone(),
            }),
            {
                let registry = ToolRegistry::builtins();
                Arc::new(DirectExecutor::new(registry)) as Arc<dyn ToolExecutor>
            },
            AgentTurnLimits::default(),
            Vec::new(),
        )
        .with_parameters(openrt_models::ModelParameters {
            temperature: Some(0.7),
            top_p: None,
            max_tokens: Some(256),
            stop: vec!["</s>".into()],
            seed: Some(7),
            unbounded_output: false,
            context_window: Some(4096),
        });
        agent.run(&ctx(), "tune").await.unwrap();
        let sent: Vec<_> = requests.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].parameters.temperature,
            Some(0.7),
            "temperature must reach the provider request"
        );
        assert_eq!(sent[0].parameters.max_tokens, Some(256));
        assert_eq!(sent[0].parameters.stop, vec!["</s>".to_string()]);
        assert_eq!(sent[0].parameters.seed, Some(7));
        assert_eq!(
            sent[0].parameters.context_window,
            Some(4096),
            "a within-window request passes untouched"
        );
    }

    /// Safety rule (hardening pass, Part B): an explicitly requested context
    /// window passes through to the provider request VERBATIM. A provider's
    /// *declared* `context_window` (Ollama default 8192) is a default, NOT a
    /// certified hard maximum — clamping 65536 down to it would silently
    /// rewrite the operator's working configuration. Only a CERTIFIED hard
    /// maximum may refuse a request (see
    /// `requested_context_breaching_a_certified_maximum_is_refused`).
    #[tokio::test]
    async fn requested_context_window_passes_through_verbatim() {
        async fn sent_window(declared: Option<usize>, requested: Option<u32>) -> Option<u32> {
            let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let agent = MinimalAgent::new(
                "minimal",
                Arc::new(WindowProvider {
                    window: declared,
                    requests: requests.clone(),
                }),
                {
                    let registry = ToolRegistry::builtins();
                    Arc::new(DirectExecutor::new(registry)) as Arc<dyn ToolExecutor>
                },
                AgentTurnLimits::default(),
                Vec::new(),
            )
            .with_parameters(openrt_models::ModelParameters {
                context_window: requested,
                ..openrt_models::ModelParameters::default()
            });
            agent.run(&ctx(), "bound").await.unwrap();
            let sent: Vec<_> = requests.lock().unwrap().clone();
            sent[0].parameters.context_window
        }

        assert_eq!(
            sent_window(Some(8192), Some(65_536)).await,
            Some(65_536),
            "the declared default 8192 is NOT a hard maximum: 65536 passes through"
        );
        assert_eq!(
            sent_window(Some(8192), Some(4096)).await,
            Some(4096),
            "a within-window request is not touched"
        );
        assert_eq!(
            sent_window(None, Some(65_536)).await,
            Some(65_536),
            "an unknown declared window leaves the explicit request in place"
        );
        assert_eq!(
            sent_window(Some(65_536), Some(65_536)).await,
            Some(65_536),
            "request equal to the declared window is not clamped"
        );
    }

    /// The ONE case where a context request is refusable: the provider has
    /// certified a hard maximum and the request exceeds it. The refusal is
    /// explicit — never a silent rewrite to the maximum.
    #[tokio::test]
    async fn requested_context_breaching_a_certified_maximum_is_refused() {
        /// Test double that certifies `maximum` as a hard maximum. Declares a
        /// `window` default separate from the certified cap.
        struct CertifiedProvider {
            window: Option<usize>,
            maximum: Option<usize>,
        }
        #[async_trait::async_trait]
        impl ModelProvider for CertifiedProvider {
            fn id(&self) -> &str {
                "certified"
            }
            fn info(&self) -> openrt_models::ModelInfo {
                openrt_models::ModelInfo {
                    id: "certified".into(),
                    kind: openrt_models::ProviderKind::Mock,
                    display_name: "Certified".into(),
                    default_model: Some("certified".into()),
                }
            }
            async fn generate(
                &self,
                _req: openrt_models::ModelRequest,
            ) -> Result<openrt_models::ModelResponse, ModelError> {
                Ok(text_response("ok"))
            }
            fn capabilities(&self) -> openrt_models::ModelCapabilities {
                openrt_models::ModelCapabilities {
                    supports_tools: false,
                    supports_streaming: false,
                    context_window: self.window,
                }
            }
            fn context_capability(&self) -> openrt_models::ContextCapability {
                openrt_models::ContextCapability {
                    default_context: self.window,
                    maximum_context: self.maximum,
                    maximum_known: self.maximum.is_some(),
                    configurable: true,
                }
            }
        }

        let provider: Arc<dyn ModelProvider> = Arc::new(CertifiedProvider {
            window: Some(8192),
            maximum: Some(32768),
        });
        let agent = MinimalAgent::new(
            "minimal",
            provider,
            {
                let registry = ToolRegistry::builtins();
                Arc::new(DirectExecutor::new(registry)) as Arc<dyn ToolExecutor>
            },
            AgentTurnLimits::default(),
            Vec::new(),
        )
        .with_parameters(openrt_models::ModelParameters {
            context_window: Some(65_536),
            ..openrt_models::ModelParameters::default()
        });

        let err = agent.run(&ctx(), "bound").await.unwrap_err().to_string();
        assert!(
            err.contains("refusing rather than silently downgrading"),
            "unexpected error: {err}"
        );
    }
}
