//! Failure classification — a *policy view* of existing error types used by the
//! recovery planner. This is **not** a competing taxonomy; it extends the typed
//! errors in `openrt-agents`, `openrt-tools`, and `openrt-persistence` with a
//! category that recovery policy can reason about.

use std::fmt;

/// Categorization of a failure for retry/rollback decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    /// Transient infra/provider/network noise (retryable).
    Transient,
    /// Policy denied the action (not retryable).
    PermissionDenied,
    /// Hard budget or host limit hit (not retryable by default).
    ResourceExceeded,
    /// Internal tool error (the current `ToolError::Internal` — retryable).
    ToolFailure,
    /// Provider returned an error (`AgentError::Model`).
    ModelFailure,
    /// Structurally invalid input/config (not retryable).
    ValidationFailure,
    /// Explicit cancellation via token / Ctrl-C (not retryable).
    Cancellation,
    /// ADR-008 safety halt blocked the action (not retryable).
    SafetyHalt,
    /// Storage layer IO / persistence failure (not retryable by default).
    PersistenceFailure,
    /// Stored record is corrupt / checksum mismatch (not retryable).
    CheckpointCorrupt,
    /// Outcome could not be determined (running at snapshot — not retryable;
    /// resume decision is policy-driven).
    UnknownOutcome,
    /// Structural / permanent failure (not retryable).
    Fatal,
}

impl FailureCategory {
    /// Default retry eligibility — policy may override this.
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::Transient | Self::ToolFailure | Self::ModelFailure
        )
    }
}

impl fmt::Display for FailureCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Transient => "transient",
            Self::PermissionDenied => "permission_denied",
            Self::ResourceExceeded => "resource_exceeded",
            Self::ToolFailure => "tool_failure",
            Self::ModelFailure => "model_failure",
            Self::ValidationFailure => "validation_failure",
            Self::Cancellation => "cancellation",
            Self::SafetyHalt => "safety_halt",
            Self::PersistenceFailure => "persistence_failure",
            Self::CheckpointCorrupt => "checkpoint_corrupt",
            Self::UnknownOutcome => "unknown_outcome",
            Self::Fatal => "fatal",
        };
        f.write_str(s)
    }
}

impl std::str::FromStr for FailureCategory {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "transient" => Ok(Self::Transient),
            "permission_denied" => Ok(Self::PermissionDenied),
            "resource_exceeded" => Ok(Self::ResourceExceeded),
            "tool_failure" => Ok(Self::ToolFailure),
            "model_failure" => Ok(Self::ModelFailure),
            "validation_failure" => Ok(Self::ValidationFailure),
            "cancellation" => Ok(Self::Cancellation),
            "safety_halt" => Ok(Self::SafetyHalt),
            "persistence_failure" => Ok(Self::PersistenceFailure),
            "checkpoint_corrupt" => Ok(Self::CheckpointCorrupt),
            "unknown_outcome" => Ok(Self::UnknownOutcome),
            "fatal" => Ok(Self::Fatal),
            _ => Err(format!("unknown failure category `{s}`")),
        }
    }
}

/// Classify an [`openrt_agents::AgentError`].
pub fn classify_agent_error(e: &openrt_agents::AgentError) -> FailureCategory {
    match e {
        openrt_agents::AgentError::Model(_) => FailureCategory::ModelFailure,
        openrt_agents::AgentError::Tool(_) => FailureCategory::ToolFailure,
        openrt_agents::AgentError::Cancelled => FailureCategory::Cancellation,
        openrt_agents::AgentError::Internal(_) => FailureCategory::Transient,
    }
}

/// Classify a [`openrt_tools::ToolError`].
pub fn classify_tool_error(e: &openrt_tools::ToolError) -> FailureCategory {
    match e {
        openrt_tools::ToolError::NotFound(_) => FailureCategory::Fatal,
        openrt_tools::ToolError::InvalidArguments(_, _) => FailureCategory::ValidationFailure,
        openrt_tools::ToolError::PermissionDenied { .. } => FailureCategory::PermissionDenied,
        openrt_tools::ToolError::ResourceDenied { .. } => FailureCategory::ResourceExceeded,
        openrt_tools::ToolError::ScopeViolation { .. } => FailureCategory::ValidationFailure,
        openrt_tools::ToolError::Cancelled(_) => FailureCategory::Cancellation,
        openrt_tools::ToolError::KillSwitchEngaged { .. } => FailureCategory::SafetyHalt,
        openrt_tools::ToolError::Internal(_, _) => FailureCategory::ToolFailure,
    }
}

/// Classify a [`openrt_persistence::PersistenceError`].
pub fn classify_persistence_error(e: &openrt_persistence::PersistenceError) -> FailureCategory {
    match e {
        openrt_persistence::PersistenceError::Io { .. } => FailureCategory::PersistenceFailure,
        openrt_persistence::PersistenceError::Corrupt(..) => FailureCategory::CheckpointCorrupt,
        openrt_persistence::PersistenceError::Serialization(_) => {
            FailureCategory::ValidationFailure
        }
        _ => FailureCategory::Fatal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_error_classifications() {
        assert_eq!(
            classify_agent_error(&openrt_agents::AgentError::Model("x".into())),
            FailureCategory::ModelFailure
        );
        assert_eq!(
            classify_agent_error(&openrt_agents::AgentError::Cancelled),
            FailureCategory::Cancellation
        );
    }

    #[test]
    fn tool_error_classifications() {
        assert_eq!(
            classify_tool_error(&openrt_tools::ToolError::PermissionDenied {
                tool: "x".into(),
                reason: "y".into(),
            }),
            FailureCategory::PermissionDenied
        );
        assert_eq!(
            classify_tool_error(&openrt_tools::ToolError::KillSwitchEngaged {
                tool: "x".into(),
                reason: "y".into(),
            }),
            FailureCategory::SafetyHalt
        );
        assert_eq!(
            classify_tool_error(&openrt_tools::ToolError::Internal("a".into(), "b".into())),
            FailureCategory::ToolFailure
        );
    }

    #[test]
    fn retryable_categories() {
        assert!(FailureCategory::Transient.is_retryable());
        assert!(FailureCategory::ModelFailure.is_retryable());
        assert!(FailureCategory::ToolFailure.is_retryable());
        assert!(!FailureCategory::PermissionDenied.is_retryable());
        assert!(!FailureCategory::Cancellation.is_retryable());
        assert!(!FailureCategory::SafetyHalt.is_retryable());
        assert!(!FailureCategory::CheckpointCorrupt.is_retryable());
        assert!(!FailureCategory::PersistenceFailure.is_retryable());
        assert!(!FailureCategory::UnknownOutcome.is_retryable());
    }

    #[test]
    fn category_round_trip() {
        for cat in [
            FailureCategory::Transient,
            FailureCategory::PermissionDenied,
            FailureCategory::ResourceExceeded,
            FailureCategory::ToolFailure,
            FailureCategory::ModelFailure,
            FailureCategory::ValidationFailure,
            FailureCategory::Cancellation,
            FailureCategory::SafetyHalt,
            FailureCategory::PersistenceFailure,
            FailureCategory::CheckpointCorrupt,
            FailureCategory::UnknownOutcome,
            FailureCategory::Fatal,
        ] {
            let s = cat.to_string();
            let back: FailureCategory = s.parse().unwrap();
            assert_eq!(cat, back);
        }
    }
}
