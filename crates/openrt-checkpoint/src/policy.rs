//! Recovery policy — knobs that control how the planner classifies nodes for
//! automatic retry, resume, rollback, and operator intervention.

use serde::{Deserialize, Serialize};

use crate::classify::FailureCategory;

/// Which categories of failures are eligible for automatic retry. By default
/// all three retryable categories (`Transient`, `ToolFailure`, `ModelFailure`)
/// are enabled; the policy may override any combination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryCategories {
    pub transient: bool,
    pub tool_failure: bool,
    pub model_failure: bool,
}

impl Default for RetryCategories {
    fn default() -> Self {
        Self {
            transient: true,
            tool_failure: true,
            model_failure: true,
        }
    }
}

impl RetryCategories {
    /// Whether the given category is eligible for retry under this policy.
    pub fn allows(&self, c: &FailureCategory) -> bool {
        match c {
            FailureCategory::Transient => self.transient,
            FailureCategory::ToolFailure => self.tool_failure,
            FailureCategory::ModelFailure => self.model_failure,
            _ => false,
        }
    }
}

/// How the planner should handle nodes whose outcome is uncertain (state was
/// `Running` at snapshot time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UncertainPolicy {
    /// Automatically resume when the node's effect is `ReadOnly` or
    /// `Idempotent` (safe to redo); escalate to operator intervention
    /// otherwise.
    #[default]
    AutoWhenSafe,
    /// Any uncertain node escalates to operator intervention, regardless of
    /// its effect classification.
    AlwaysIntervene,
}

/// Controls how recovery handles nodes that are not-yet-completed, failed,
/// or uncertain. The engine never auto-runs a node with `Irreversible` or
/// `Unknown` effect during resume — that is a hardcoded safety invariant that
/// no policy may override.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryPolicy {
    /// Maximum number of cumulative attempts per node across this run and all
    /// prior resumed runs. If a node reaches this limit it is marked as
    /// exhausted.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    /// Optional back-off between retries. `None` means deterministic
    /// (immediate re-enqueue) — the engine is free to insert its own delay
    /// if a non-deterministic provider is configured.
    #[serde(default)]
    pub retry_backoff_ms: Option<u64>,
    /// Which failure categories are eligible for automatic retry.
    #[serde(default)]
    pub retryable: RetryCategories,
    /// How to handle nodes with an uncertain outcome.
    #[serde(default)]
    pub uncertain_policy: UncertainPolicy,
    /// When true the planner will attempt to attach supported rollback
    /// operations to the plan description (for informational purposes; the
    /// caller decides whether to execute them).
    #[serde(default = "default_include_rollbacks")]
    pub include_rollbacks: bool,
}

fn default_max_attempts() -> u32 {
    3
}
fn default_include_rollbacks() -> bool {
    true
}

impl Default for RecoveryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            retry_backoff_ms: None,
            retryable: RetryCategories::default(),
            uncertain_policy: UncertainPolicy::AutoWhenSafe,
            include_rollbacks: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe() {
        let p = RecoveryPolicy::default();
        assert_eq!(p.max_attempts, 3);
        assert!(p.retryable.transient);
        assert!(p.retryable.tool_failure);
        assert!(p.retryable.model_failure);
        assert_eq!(p.uncertain_policy, UncertainPolicy::AutoWhenSafe);
    }

    #[test]
    fn retryable_allows_retryable() {
        let p = RecoveryPolicy::default();
        assert!(p.retryable.allows(&FailureCategory::Transient));
        assert!(p.retryable.allows(&FailureCategory::ToolFailure));
        assert!(p.retryable.allows(&FailureCategory::ModelFailure));
    }

    #[test]
    fn retryable_blocks_non_retryable() {
        let p = RecoveryPolicy::default();
        assert!(!p.retryable.allows(&FailureCategory::PermissionDenied));
        assert!(!p.retryable.allows(&FailureCategory::Cancellation));
        assert!(!p.retryable.allows(&FailureCategory::SafetyHalt));
        assert!(!p.retryable.allows(&FailureCategory::UnknownOutcome));
    }

    #[test]
    fn policy_overrides_category() {
        let p = RecoveryPolicy {
            retryable: RetryCategories {
                transient: false,
                tool_failure: true,
                model_failure: false,
            },
            ..Default::default()
        };
        assert!(!p.retryable.allows(&FailureCategory::Transient));
        assert!(p.retryable.allows(&FailureCategory::ToolFailure));
        assert!(!p.retryable.allows(&FailureCategory::ModelFailure));
    }

    #[test]
    fn round_trip() {
        let p = RecoveryPolicy {
            max_attempts: 5,
            retry_backoff_ms: Some(100),
            ..Default::default()
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: RecoveryPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }
}
