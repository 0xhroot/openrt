//! Phase 11 M11.2: skill-backed extension tool bindings (one registry
//! authority).
//!
//! An extension ships no executable code — a declarative [`ExtensionTool`]
//! entry binds a tool name to an existing *Active* skill. The binding is
//! registered into the ONE [`openrt_tools::ToolRegistry`] during
//! `RuntimeBuilder::build` (Active at build ⇒ bound at build — before the
//! `tools.list` snapshot and the gate's registry clone, so discovery, the
//! provider tool list and execution all see the SAME entry).
//!
//! Invocation goes through the ONE skill execution path
//! (`skill_execute_input` → skill load → workflow/task → gate → policy →
//! sandbox → shield) with a **fresh child session**: extension execution
//! never takes the invoking session's SEC-08 lock (a nested same-session run
//! would deadlock on the non-reentrant session mutex and clobber the parent
//! transcript — ADR-006). The handle is a `Weak<Runtime>` wired after
//! construction — a binding never keeps the runtime alive, and an unwired or
//! torn-down binding fails closed.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{OnceLock, Weak};

use openrt_tools::{Tool, ToolError, ToolOutput, ToolSpec};
use openrt_workspace_assets::extension::ExtensionTool;

use crate::Runtime;

/// Maximum concurrently-nested invocations of the SAME extension tool (an
/// agent re-entering its own binding). Deeper nesting fails closed instead of
/// risking an unbounded recursion of child sessions.
const MAX_INFLIGHT_PER_TOOL: usize = 2;

/// One skill-backed tool binding of an Active extension.
pub(crate) struct ExtensionToolBinding {
    spec: ToolSpec,
    /// Referenced skill id (the ONE skill authority executes it).
    skill: String,
    /// Minimum acceptable skill version (`None` = whatever resolves, never
    /// substituted down).
    min_version: Option<u32>,
    /// Wired post-construction by `RuntimeBuilder::build`.
    runtime: OnceLock<Weak<Runtime>>,
    /// Re-entrancy guard (see [`MAX_INFLIGHT_PER_TOOL`]).
    inflight: AtomicUsize,
}

impl ExtensionToolBinding {
    /// Build the binding's spec from the declarative manifest entry. The
    /// tool NAME is validated by the registry at registration (a conflict or
    /// invalid name fails the build — never a silent skip).
    pub(crate) fn new(ext_tool: &ExtensionTool) -> Self {
        let mut spec = ToolSpec::new(&ext_tool.name, &ext_tool.description);
        let caps: Vec<&str> = ext_tool.required_caps.iter().map(String::as_str).collect();
        spec = spec.with_caps(&caps);
        if !ext_tool.parameters_schema.is_null() {
            spec = spec.with_schema(ext_tool.parameters_schema.clone());
        }
        Self {
            spec,
            skill: ext_tool.skill.0.clone(),
            min_version: (ext_tool.min_version.0 != 0).then_some(ext_tool.min_version.0),
            runtime: OnceLock::new(),
            inflight: AtomicUsize::new(0),
        }
    }

    /// Wire the built runtime (idempotent — first wire wins; later calls are
    /// ignored so double-wiring can never repoint a live binding).
    pub(crate) fn bind(&self, runtime: Weak<Runtime>) {
        let _ = self.runtime.set(runtime);
    }
}

/// Decrements the re-entrancy counter on every exit path (including error
/// returns), so a failed invocation never wedges the binding.
struct InflightGuard<'a>(&'a AtomicUsize);

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl Tool for ExtensionToolBinding {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value) -> Result<ToolOutput, ToolError> {
        let name = self.spec.name.clone();
        self.inflight
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < MAX_INFLIGHT_PER_TOOL).then_some(n + 1)
            })
            .map_err(|_| {
                ToolError::Internal(
                    name.clone(),
                    format!(
                        "nested execution exceeded {MAX_INFLIGHT_PER_TOOL} levels for this binding"
                    ),
                )
            })?;
        let _guard = InflightGuard(&self.inflight);

        // Fail closed: an unwired binding (build-time registration without
        // the post-construction wire) or a runtime that already dropped can
        // never execute.
        let runtime = self
            .runtime
            .get()
            .and_then(|w| w.upgrade())
            .ok_or_else(|| {
                ToolError::Internal(name.clone(), "binding has no live runtime".to_string())
            })?;

        // Model/invoker-provided arguments become the skill's *input*; an
        // empty object or null means "no input".
        let empty = args.is_null() || args.as_object().is_some_and(|o| o.is_empty());
        let input = if empty {
            None
        } else {
            Some(serde_json::to_string_pretty(&args).unwrap_or_else(|_| args.to_string()))
        };

        // Minimum-version semantics: resolve the HIGHEST stored version
        // (never substituted down) and refuse when it fell below the
        // binding's floor after the extension was activated.
        if let Some(min) = self.min_version {
            let probe = runtime
                .skill_inspect(&self.skill, None)
                .await
                .map_err(|e| {
                    ToolError::Internal(
                        name.clone(),
                        format!("skill {} unavailable: {e}", self.skill),
                    )
                })?;
            if probe.version < min {
                return Err(ToolError::Internal(
                    name.clone(),
                    format!(
                        "skill {}@{} is below the binding's minimum version {min}",
                        self.skill, probe.version
                    ),
                ));
            }
        }

        // Fresh child session per invocation (see module docs): durable
        // create → run → archive, so `run_task`'s `load_session` finds a
        // real session while the caller's session lock/transcript stay
        // untouched.
        let session = runtime
            .create_session(&format!("extension tool {name}"))
            .await
            .map_err(|e| {
                ToolError::Internal(name.clone(), format!("child session create failed: {e}"))
            })?;
        let exec = runtime
            .skill_execute_input(session.id, &self.skill, None, input.as_deref())
            .await;
        // Always archive the child session, even when execution failed.
        let ended = runtime.session_end(session.id).await;
        let exec = exec.map_err(|e| ToolError::Internal(name.clone(), e.to_string()))?;
        ended.map_err(|e| {
            ToolError::Internal(name.clone(), format!("child session end failed: {e}"))
        })?;

        let mut out = serde_json::json!({
            "skill": exec.id,
            "version": exec.version,
            "mode": exec.mode,
            "effective_permissions": exec.effective_permissions,
        });
        if let Some(task) = &exec.task {
            out["task"] = serde_json::to_value(task).unwrap_or(serde_json::Value::Null);
        }
        if let Some(report) = &exec.workflow_report {
            out["workflow"] = serde_json::to_value(report).unwrap_or(serde_json::Value::Null);
        }
        Ok(ToolOutput::Json(out))
    }
}
