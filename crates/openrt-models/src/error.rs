//! Model provider errors.

/// Errors surfaced by model providers.
///
/// Transport-agnostic: providers translate their own client errors into these
/// variants so consumers (runtime, CLI) never depend on the HTTP stack.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ModelError {
    #[error("provider `{0}` is not configured with a base url")]
    NotConfigured(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("HTTP error {status} from {provider}: {body}")]
    Http {
        provider: String,
        status: u16,
        body: String,
    },
    #[error("model `{0}` not found on provider")]
    ModelNotFound(String),
    #[error("request timed out after {0}s")]
    Timeout(u64),
    #[error("provider returned an unparsable response: {0}")]
    InvalidResponse(String),
    #[error("model invocation was cancelled")]
    Cancelled,
    /// Phase 13 (Part E): the runtime egress guard refused this provider's
    /// network transport before any client call (Sovereign / egress policy).
    #[error("model egress refused: {0}")]
    EgressRefused(String),
    /// Part B: a provider that advertises `supports_streaming` must implement
    /// the streaming path; anything else states it plainly instead of lying.
    #[error("streaming not supported by this provider: {0}")]
    StreamingUnsupported(String),
    #[error("unexpected: {0}")]
    Unexpected(String),
}

impl ModelError {
    pub fn is_permanent(&self) -> bool {
        matches!(
            self,
            Self::ModelNotFound(_) | Self::NotConfigured(_) | Self::InvalidResponse(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permanent_classification() {
        assert!(ModelError::ModelNotFound("x".into()).is_permanent());
        assert!(!ModelError::Timeout(5).is_permanent());
    }
}
