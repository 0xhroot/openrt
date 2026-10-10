//! The computer-use loop: perceive → decide → act, with a safety check on
//! every step (M2.5).
//!
//! The loop is backend-agnostic and executor-agnostic: it observes the screen
//! through a [`ScreenBackend`], asks a `decider` what to do next, and performs
//! each action through a gated [`ToolExecutor`] (so every action crosses the
//! same permission + resource + kill-switch gates an agent would). Before each
//! perceive and before each act the loop re-checks the computer-use kill
//! switch, so a HALT aborts exactly where the operator pulled it — between
//! steps, and (for long-running tools) inside an in-flight action.

use std::sync::Arc;

use openrt_safety::KillSwitch;
use openrt_tools::{ToolError, ToolExecutionContext, ToolExecutor, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::screen::ScreenBackend;
use crate::screen::ScreenCapture;

/// One action the loop performed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub tool: String,
    pub args: Value,
}

impl ToolCall {
    pub fn new(tool: impl Into<String>, args: Value) -> Self {
        Self {
            tool: tool.into(),
            args,
        }
    }
}

/// Result of a [`ComputerUseLoop`] run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComputerLoopReport {
    /// Steps of perceive→decide→act actually executed.
    pub steps: usize,
    pub max_steps: usize,
    /// True when the decider said "done" before the budget ran out.
    pub completed: bool,
    pub actions: Vec<ToolCall>,
    /// The terminal snapshot after the last step.
    pub capture: ScreenCapture,
}

/// `perceive → decide → act` engine. `D` yields the next [`ToolCall`] from a
/// snapshot (`None` = done).
pub struct ComputerUseLoop<D> {
    pub backend: Arc<dyn ScreenBackend>,
    pub executor: Arc<dyn ToolExecutor>,
    pub kill: Option<Arc<KillSwitch>>,
    pub max_steps: usize,
    decider: D,
}

impl<D> ComputerUseLoop<D>
where
    D: FnMut(&ScreenCapture) -> Option<ToolCall>,
{
    pub fn new(
        backend: Arc<dyn ScreenBackend>,
        executor: Arc<dyn ToolExecutor>,
        kill: Option<Arc<KillSwitch>>,
        max_steps: usize,
        decider: D,
    ) -> Self {
        Self {
            backend,
            executor,
            kill,
            max_steps,
            decider,
        }
    }

    fn engaged(&self) -> Option<String> {
        self.kill.as_ref().and_then(|k| k.engaged())
    }

    pub async fn run(&mut self) -> Result<ComputerLoopReport, ToolError> {
        let ctx = ToolExecutionContext::default();
        let mut actions = Vec::new();
        let mut capture = self.backend.capture().await?;

        for step in 0..self.max_steps {
            // Safety first, before perceiving anything new.
            if let Some(reason) = self.engaged() {
                return Err(ToolError::KillSwitchEngaged {
                    tool: "computer-use-loop".into(),
                    reason,
                });
            }
            capture = self.backend.capture().await?;

            let Some(call) = (self.decider)(&capture) else {
                return Ok(ComputerLoopReport {
                    steps: step,
                    max_steps: self.max_steps,
                    completed: true,
                    actions,
                    capture,
                });
            };

            let _: ToolOutput = self
                .executor
                .execute(&call.tool, call.args.clone(), &ctx)
                .await?;
            actions.push(call);

            // And safety again, so a halt taken *during* an action stops the
            // next iteration mid-loop.
            if let Some(reason) = self.engaged() {
                return Err(ToolError::KillSwitchEngaged {
                    tool: "computer-use-loop".into(),
                    reason,
                });
            }
        }

        Ok(ComputerLoopReport {
            steps: self.max_steps,
            max_steps: self.max_steps,
            completed: false,
            actions,
            capture,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::{ScreenElement, SimScreen};
    use openrt_tools::ToolRegistry;

    /// A gate-free executor for unit tests: resolves through the registry and
    /// runs the tool directly (mirrors `DirectExecutor`, which is not built
    /// for downstream test crates).
    struct TestExecutor {
        registry: ToolRegistry,
    }

    #[async_trait::async_trait]
    impl ToolExecutor for TestExecutor {
        async fn execute(
            &self,
            tool: &str,
            args: Value,
            _ctx: &ToolExecutionContext,
        ) -> Result<openrt_tools::ToolOutput, ToolError> {
            let t = self
                .registry
                .get(tool)
                .ok_or_else(|| ToolError::NotFound(tool.to_string()))?;
            t.run(args).await
        }
    }

    fn backend() -> Arc<dyn ScreenBackend> {
        Arc::new(SimScreen::new(
            800,
            600,
            vec![
                ScreenElement::new("submit", "button", 40, 40, 120, 32),
                ScreenElement::new("log", "window", 40, 200, 400, 100),
            ],
        ))
    }

    fn exec() -> Arc<dyn ToolExecutor> {
        Arc::new(TestExecutor {
            registry: ToolRegistry::builtins(),
        })
    }

    #[tokio::test]
    async fn loop_perceives_decides_acts_until_done() {
        let screen = backend();
        let mut looped = ComputerUseLoop::new(screen.clone(), exec(), None, 10, |_cap| None);
        let report = looped.run().await.unwrap();
        assert!(report.completed);
        assert_eq!(report.steps, 0);
        assert!(report.actions.is_empty());
    }

    #[tokio::test]
    async fn loop_budget_exhausts_when_never_done() {
        let screen = backend();
        let mut looped = ComputerUseLoop::new(screen, exec(), None, 3, |_cap| {
            Some(ToolCall::new("echo", serde_json::json!({ "text": "step" })))
        });
        let report = looped.run().await.unwrap();
        assert!(!report.completed);
        assert_eq!(report.steps, 3);
        assert_eq!(report.actions.len(), 3);
    }

    #[tokio::test]
    async fn engaged_kill_switch_aborts_the_loop() {
        let screen = backend();
        let kill = Arc::new(KillSwitch::new());
        let mut looped = ComputerUseLoop::new(screen, exec(), Some(kill.clone()), 10, |_cap| {
            Some(ToolCall::new("echo", serde_json::json!({ "text": "x" })))
        });
        kill.trip("operator");
        let err = looped.run().await.unwrap_err();
        assert!(matches!(err, ToolError::KillSwitchEngaged { .. }));
    }
}
