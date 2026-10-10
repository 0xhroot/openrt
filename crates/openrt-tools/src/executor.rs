//! The gate-side executor contract.
//!
//! Agents receive one of these and must never touch a [`Tool`] directly, so
//! permission and resource checks cannot be bypassed by calling internals.

use serde::{Deserialize, Serialize};

use crate::{ToolError, ToolOutput};

/// Context a gate needs about the caller.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolExecutionContext {
    pub session_id: Option<String>,
    pub task_id: Option<String>,
    pub channel: Option<String>,
}

/// One tool invocation crossing the gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolExecution {
    pub tool: String,
    pub arguments: serde_json::Value,
    pub context: ToolExecutionContext,
}

#[async_trait::async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Resolve, authorize, resource-check, and execute one tool call.
    /// Implementations must guarantee that a denied call never runs the tool.
    async fn execute(
        &self,
        tool: &str,
        args: serde_json::Value,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolOutput, ToolError>;
}

/// Convenience impl for tests and demos: an executor that bypasses gates.
///
/// # Warning
///
/// **This executes tools with no permission or resource enforcement.** The
/// runtime's own gate executor is the one agents actually see; this type only
/// exists for deterministic unit tests and offline demos. It is compiled only
/// when the `demo` feature (or `cfg(test)`) is enabled, so a default release
/// build cannot even link it (SEC-04).
#[doc(hidden)]
#[cfg(any(test, feature = "demo"))]
pub struct DirectExecutor {
    registry: crate::ToolRegistry,
}

#[doc(hidden)]
#[cfg(any(test, feature = "demo"))]
impl DirectExecutor {
    pub fn new(registry: crate::ToolRegistry) -> Self {
        Self { registry }
    }
}

#[async_trait::async_trait]
#[cfg(any(test, feature = "demo"))]
impl ToolExecutor for DirectExecutor {
    async fn execute(
        &self,
        tool: &str,
        args: serde_json::Value,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolOutput, ToolError> {
        let t = self
            .registry
            .get(tool)
            .ok_or_else(|| ToolError::NotFound(tool.to_string()))?;
        t.run(args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn direct_executor_runs_builtins() {
        let exec = DirectExecutor::new(crate::ToolRegistry::builtins());
        let ctx = ToolExecutionContext {
            session_id: Some("s1".into()),
            task_id: Some("t1".into()),
            channel: None,
        };
        let out = exec
            .execute("echo", serde_json::json!({ "text": "hi" }), &ctx)
            .await
            .unwrap();
        assert_eq!(out.render(), "hi");
        let missing = exec
            .execute("nope", serde_json::json!({}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(missing, ToolError::NotFound(_)));
    }
}
