//! OpenRT agent abstraction and the Phase 1 minimal agent.
//!
//! An [`Agent`] consumes the model provider + gated tool executor and turns a
//! user request into a final answer, honoring turn/tool-call limits and
//! cancellation. The crate is deliberately thin: orchestration, persistence,
//! and lifecycle live in `openrt-runtime`.

mod context;
mod minimal;

pub use context::{AgentContext, AgentEndReason, AgentResult, AgentTurnLimits};
pub use minimal::{compact_transcript, MinimalAgent};

use async_trait::async_trait;

/// Errors agents can produce (provider + gate failures, cancellation).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AgentError {
    #[error("model provider error: {0}")]
    Model(String),
    #[error("tool gate error: {0}")]
    Tool(String),
    #[error("agent was cancelled")]
    Cancelled,
    #[error("agent internal error: {0}")]
    Internal(String),
}

/// Contract implemented by every agent.
#[async_trait]
pub trait Agent: Send + Sync {
    /// Stable identifier.
    fn id(&self) -> &str;

    /// Run until an answer, hit a limit, or cancellation.
    async fn run(&self, ctx: &AgentContext, input: &str) -> Result<AgentResult, AgentError>;
}
