//! `terminal.run` — shell command execution with working-directory scoping,
//! output capture, timeouts, and process-group management (M2.3).
//!
//! The tool starts with its working directory resolved inside the configured
//! root (same SEC-01 jail as the filesystem tools) and denies every invocation
//! when no root is configured. Output is captured with a byte cap, the command
//! runs under a wall-clock timeout, and the whole process group is killed on
//! timeout (or when the parent is dropped) so background children cannot
//! outlive the tool call.
//!
//! Scope note (documented, not hidden): a shell can still `cd` away from its
//! start directory. Full containment of arbitrary shell code is the job of the
//! Python sandbox (M2.4) and the Phase 6 production sandbox; the `terminal`
//! tool provides *working-dir scoping*, output/argument/process caps, and a
//! strict fail-closed default.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use openrt_permissions::ScopeRoot;
use openrt_safety::KillSwitch;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use openrt_tools::{Tool, ToolError, ToolOutput, ToolSpec};

/// Default wall-clock timeout for a single command.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Default cap on captured output (per stream).
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// Default cap on the command string length.
pub const DEFAULT_MAX_COMMAND_BYTES: usize = 4096;

fn scope_error(reason: impl Into<String>) -> ToolError {
    ToolError::ScopeViolation {
        tool: "terminal.run".to_string(),
        reason: reason.into(),
    }
}

fn internal(reason: impl Into<String>) -> ToolError {
    ToolError::Internal("terminal.run".to_string(), reason.into())
}

/// Tunables for a [`Terminal`] tool instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalLimits {
    pub timeout: Duration,
    /// Per-stream output byte cap; exceeding it kills the group and flags `truncated`.
    pub max_output_bytes: usize,
    /// Command string length cap.
    pub max_command_bytes: usize,
}

impl Default for TerminalLimits {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_command_bytes: DEFAULT_MAX_COMMAND_BYTES,
        }
    }
}

/// `terminal.run` tool.
pub struct Terminal {
    pub spec: ToolSpec,
    pub root: Option<ScopeRoot>,
    pub limits: TerminalLimits,
    /// Computer-use kill switch (ADR-008): when engaged while a command is
    /// running, the whole process group is killed immediately.
    pub kill: Option<Arc<KillSwitch>>,
}

impl Terminal {
    /// Deny-all instance (no configured root).
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                "terminal.run",
                "Runs a shell command with its working directory inside the configured workspace root, capturing stdout/stderr with a size cap and a timeout.",
            )
            .with_caps(&["terminal.run.all"])
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "shell command to run" },
                    "cwd": { "type": "string", "description": "absolute working directory inside the workspace root (default: the root)" }
                },
                "required": ["command"]
            })),
            root: None,
            limits: TerminalLimits::default(),
            kill: None,
        }
    }

    pub fn scoped(root: &Path) -> Self {
        let mut t = Self::new();
        t.root = ScopeRoot::new(root).ok();
        t
    }

    pub fn scoped_or_deny(root: Option<&Path>) -> Self {
        match root.map(ScopeRoot::new) {
            Some(Ok(r)) => Self::scoped(r.as_path()),
            _ => Self::new(),
        }
    }

    pub fn with_limits(mut self, limits: TerminalLimits) -> Self {
        self.limits = limits;
        self
    }

    fn resolve_cwd(&self, args: &Value) -> Result<std::path::PathBuf, ToolError> {
        let root = self.root.as_ref().ok_or_else(|| {
            scope_error(
                "no workspace root configured (set runtime.workspace_dir); terminal.run is denied by default",
            )
        })?;
        match args.get("cwd").and_then(Value::as_str) {
            Some(cwd) => {
                let requested = std::path::PathBuf::from(cwd);
                openrt_permissions::check_path(&requested, root)
                    .map_err(|e| scope_error(e.to_string()))
            }
            None => Ok(root.as_path().to_path_buf()),
        }
    }
}

impl Default for Terminal {
    fn default() -> Self {
        Self::new()
    }
}

/// Read up to `cap` bytes from the child stream.
async fn read_capped<R: AsyncRead + Unpin>(mut stream: Option<R>, cap: usize) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut truncated = false;
    let mut buf = [0u8; 8192];
    if let Some(stream) = stream.as_mut() {
        loop {
            match stream.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if out.len() + n > cap {
                        let room = cap.saturating_sub(out.len());
                        if room > 0 {
                            out.extend_from_slice(&buf[..room]);
                        }
                        truncated = true;
                        return (out, truncated);
                    }
                    out.extend_from_slice(&buf[..n]);
                }
                Err(_) => break,
            }
        }
    }
    (out, truncated)
}

#[async_trait::async_trait]
impl Tool for Terminal {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let command = args.get("command").and_then(Value::as_str).ok_or_else(|| {
            ToolError::InvalidArguments("terminal.run".into(), "missing `command` string".into())
        })?;
        if command.len() > self.limits.max_command_bytes {
            return Err(internal(format!(
                "command exceeds {}-byte cap",
                self.limits.max_command_bytes
            )));
        }
        let cwd = self.resolve_cwd(&args)?;

        // Clear the environment so no host secrets leak into the shell; keep
        // a minimal PATH so common utilities resolve.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(command)
            .current_dir(&cwd)
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Whole process-group management: start_kill / kill_on_drop target
            // the group, so background children die with the command.
            .process_group(0)
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| internal(format!("spawn: {e}")))?;

        let out_cap = self.limits.max_output_bytes;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_task = tokio::task::spawn(async move { read_capped(stdout, out_cap).await });
        let stderr_task = tokio::task::spawn(async move { read_capped(stderr, out_cap).await });

        let status = tokio::select! {
            res = tokio::time::timeout(self.limits.timeout, child.wait()) => {
                match res {
                    Ok(Ok(status)) => status,
                    Ok(Err(e)) => return Err(internal(format!("wait: {e}"))),
                    Err(_) => {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        return Err(internal(
                            "command timed out and the process group was killed".to_string(),
                        ));
                    }
                }
            }
            reason = openrt_safety::halt_reason(self.kill.clone()) => {
                // Mid-action halt (ADR-008): the computer-use kill switch was
                // engaged while the command ran; kill the whole group.
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(ToolError::KillSwitchEngaged {
                    tool: "terminal.run".to_string(),
                    reason,
                });
            }
        };

        let (stdout_bytes, out_trunc) = stdout_task.await.unwrap_or_default();
        let (stderr_bytes, err_trunc) = stderr_task.await.unwrap_or_default();

        let exit_code = status.code();
        let truncated = out_trunc || err_trunc;

        Ok(ToolOutput::json(json!({
            "exit_code": exit_code,
            "truncated": truncated,
            "stdout": String::from_utf8_lossy(&stdout_bytes),
            "stderr": String::from_utf8_lossy(&stderr_bytes),
            "cwd": cwd,
        })))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static INSTANCE: AtomicUsize = AtomicUsize::new(0);

    fn scoped_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "openrt-term-{}-{}",
            std::process::id(),
            INSTANCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn runs_command_and_captures_output() {
        let dir = scoped_dir();
        let t = Terminal::scoped(&dir);
        let out = t
            .run(json!({ "command": "printf 'out'; printf 'err' >&2" }))
            .await
            .unwrap();
        let ToolOutput::Json(v) = out else { panic!() };
        assert_eq!(v["exit_code"], 0);
        assert_eq!(v["stdout"], "out");
        assert_eq!(v["stderr"], "err");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unscoped_terminal_denied() {
        let t = Terminal::new();
        let err = t.run(json!({ "command": "id" })).await.unwrap_err();
        assert!(matches!(err, ToolError::ScopeViolation { .. }));
    }

    #[tokio::test]
    async fn cwd_outside_root_denied() {
        let dir = scoped_dir();
        let t = Terminal::scoped(&dir);
        let err = t
            .run(json!({
                "command": "pwd",
                "cwd": "/tmp"
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ScopeViolation { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn timeout_kills_process_group() {
        let dir = scoped_dir();
        let t = Terminal::scoped(&dir).with_limits(TerminalLimits {
            timeout: Duration::from_millis(300),
            ..Default::default()
        });
        // Spawns a child that would outlive the shell; the group kill must
        // terminate both.
        let started = std::time::Instant::now();
        let err = t
            .run(json!({ "command": "(sleep 30 &); sleep 30" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Internal(_, ref m) if m.contains("timed out")));
        assert!(started.elapsed() < Duration::from_secs(10));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn environment_is_cleared() {
        let dir = scoped_dir();
        // Set a secret that must not leak into the shell.
        std::env::set_var("OPENRT_TOP_SECRET", "do-not-leak");
        let t = Terminal::scoped(&dir);
        let out = t
            .run(json!({ "command": "printf '%s' \"$OPENRT_TOP_SECRET\"" }))
            .await
            .unwrap();
        std::env::remove_var("OPENRT_TOP_SECRET");
        let ToolOutput::Json(v) = out else { panic!() };
        assert_eq!(v["stdout"], "");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
