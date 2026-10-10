//! Context and result types shared by agents.

use openrt_core::cancel::CancellationToken;
use openrt_core::message::Message;
use openrt_core::{PauseToken, SessionId, TaskId};
use serde::{Deserialize, Serialize};

/// Handle passed to an agent for one task. Carries ids plus the message
/// history snapshot the runtime reads from the session before the run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentContext {
    pub session_id: SessionId,
    pub task_id: TaskId,
    /// Conversation history *before* this task's input (snapshot). Agent
    /// implementations fold this into the provider request.
    #[serde(default)]
    pub history: Vec<Message>,
    /// Cancellation signal; agents check it between turns and before
    /// invoking the provider. Derived via `Token::child()`.
    #[serde(skip)]
    pub cancel: CancellationToken,
    /// Part C: resettable cooperative pause signal; agents check it at the
    /// same boundaries as cancellation and stop at the next turn with
    /// [`AgentEndReason::Paused`] (transcript preserved). `None` = pause
    /// unsupported for this run.
    #[serde(skip)]
    pub pause: Option<std::sync::Arc<PauseToken>>,
}

impl AgentContext {
    pub fn new(session_id: SessionId, task_id: TaskId) -> Self {
        Self {
            session_id,
            task_id,
            history: Vec::new(),
            cancel: CancellationToken::new(),
            pause: None,
        }
    }

    pub fn with_history(mut self, history: Vec<Message>) -> Self {
        self.history = history;
        self
    }

    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn with_pause(mut self, pause: std::sync::Arc<PauseToken>) -> Self {
        self.pause = Some(pause);
        self
    }
}

/// Per-task loop limits.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AgentTurnLimits {
    /// Maximum consecutive tool calls in one task (protects against a model
    /// spinning in a tool loop).
    pub max_tool_calls: u32,
    /// Maximum model generations (text + tool-call rounds).
    pub max_turns: u32,
    /// Bounded provider-level retries for *transient* errors (transport /
    /// timeout / HTTP / stream idle). Permanent errors (parse, configuration,
    /// egress) never retry regardless of this budget — a malformed request is
    /// never resent.
    ///
    /// `0` means fail on the first provider error: use it where an outer layer
    /// (e.g. the workflow engine's node retries / RecoveryPolicy) already owns
    /// retry accounting, so failure counts stay coherent.
    pub provider_retries: u32,
    /// Bounded retries for empty assistant responses (likely provider tool-call
    /// parse loss). Each turn gets a fresh budget; retries reuse the same
    /// conversation state.
    #[serde(default = "default_empty_response_retries")]
    pub provider_empty_response_retries: u32,
    /// Part C: initial backoff (ms) before the first retry after a transient
    /// provider error. Exponential (`base * 2^(attempt-1)`), capped at
    /// [`Self::retry_backoff_max_ms`], so a degraded provider never spins hot.
    #[serde(default = "default_retry_backoff_base_ms")]
    pub retry_backoff_base_ms: u64,
    /// Part C: ceiling (ms) for the exponential retry backoff.
    #[serde(default = "default_retry_backoff_max_ms")]
    pub retry_backoff_max_ms: u64,
    /// Part C: per-generation idle budget (ms) for *streaming* providers. A
    /// gap with no token delta for this long is a transient error subject to
    /// backoff retries — never a silent completion. `None` disables the
    /// watchdog: no artificial termination. Applies only when the provider
    /// advertises `supports_streaming`; otherwise the caller's model timeout
    /// governs.
    #[serde(default)]
    pub idle_timeout_ms: Option<u64>,
    /// Part C: fraction of the provider's *declared* context window treated as
    /// the guard budget the assembled request may not exceed. On breach the
    /// agent compacts the oldest turns with an explicit marker — never silent
    /// truncation, never a fabricated window.
    #[serde(default = "default_context_guard_ratio")]
    pub context_guard_ratio: f32,
}

fn default_empty_response_retries() -> u32 {
    1
}

fn default_retry_backoff_base_ms() -> u64 {
    500
}

fn default_retry_backoff_max_ms() -> u64 {
    30_000
}

fn default_context_guard_ratio() -> f32 {
    0.7
}

impl Default for AgentTurnLimits {
    fn default() -> Self {
        Self {
            max_tool_calls: 8,
            max_turns: 6,
            provider_retries: 1,
            provider_empty_response_retries: default_empty_response_retries(),
            retry_backoff_base_ms: default_retry_backoff_base_ms(),
            retry_backoff_max_ms: default_retry_backoff_max_ms(),
            idle_timeout_ms: None,
            context_guard_ratio: default_context_guard_ratio(),
        }
    }
}

/// Outcome of one agent run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentResult {
    pub final_text: String,
    pub ended: AgentEndReason,
    pub turns: u32,
    /// Tool-call turns the run actually executed (honest counter the
    /// supervisor sums across segments — never fabricated). `#[serde(default)]`
    /// keeps old recovered results loadable.
    #[serde(default)]
    pub tool_calls: u32,
    /// Provider-level retries actually incurred (transport/timeout/HTTP/stream
    /// idle). Honest and correlated with `turns` so a prolonged recovery is
    /// visible, not folded away.
    #[serde(default)]
    pub retries: u32,
    /// Full transcript produced by this run (this task's input plus every
    /// assistant tool-call turn, tool result, and the final answer). The
    /// pipeline folds it into the session so tool turns are persisted and
    /// visible in follow-up rounds. Empty for agents that do not expose a
    /// transcript — callers then fall back to input + `final_text`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentEndReason {
    Completed,
    LimitReached,
    Cancelled,
    /// Part C: stopped cooperatively at a turn boundary on an operator pause
    /// request. Not an error — the transcript preserves the tool rounds and
    /// the task resumes from it.
    Paused,
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_core::Id;

    #[test]
    fn context_round_trips_without_cancel() {
        let ctx = AgentContext::new(Id::new(), Id::new()).with_history(vec![Message::user("hi")]);
        let j = serde_json::to_string(&ctx).unwrap();
        assert!(!j.contains("cancel"));
        let back: AgentContext = serde_json::from_str(&j).unwrap();
        assert_eq!(back.history.len(), 1);
    }

    #[test]
    fn default_limits_sane() {
        let l = AgentTurnLimits::default();
        assert!(l.max_turns >= 1 && l.max_tool_calls >= 1);
    }
}
