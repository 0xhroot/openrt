//! Tool errors.

/// Errors from tool resolution, permission/resource gates, and execution.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ToolError {
    #[error("unknown tool `{0}`")]
    NotFound(String),
    #[error("invalid arguments for `{0}`: {1}")]
    InvalidArguments(String, String),
    #[error("permission denied for tool `{tool}`: {reason}")]
    PermissionDenied { tool: String, reason: String },
    #[error("resource limit prevented tool `{tool}`: {reason}")]
    ResourceDenied { tool: String, reason: String },
    #[error("path-scope violation for tool `{tool}`: {reason}")]
    ScopeViolation { tool: String, reason: String },
    #[error("tool `{0}` was cancelled")]
    Cancelled(String),
    #[error("tool `{tool}` halted by kill switch: {reason}")]
    KillSwitchEngaged { tool: String, reason: String },
    #[error("tool `{0}` failed: {1}")]
    Internal(String, String),
}

impl ToolError {
    pub fn tool_name(&self) -> &str {
        match self {
            Self::NotFound(t)
            | Self::InvalidArguments(t, _)
            | Self::Cancelled(t)
            | Self::Internal(t, _) => t,
            Self::PermissionDenied { tool, .. }
            | Self::ResourceDenied { tool, .. }
            | Self::ScopeViolation { tool, .. }
            | Self::KillSwitchEngaged { tool, .. } => tool,
        }
    }

    /// Whether an agent retry makes sense (transport noise) vs. a terminal
    /// Authz/config/programming error.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Internal(_, _))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_retry() {
        let e = ToolError::PermissionDenied {
            tool: "echo".into(),
            reason: "policy".into(),
        };
        assert_eq!(e.tool_name(), "echo");
        assert!(!e.is_retryable());
    }
}
