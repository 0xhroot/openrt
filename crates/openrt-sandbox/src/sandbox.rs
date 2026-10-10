//! Core sandbox runner and `python.run` tool.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use openrt_tools::{Tool, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use super::limits::SandboxLimits;

static INSTANCE_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Backend chosen for a [`PythonSandbox`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxBackend {
    /// Full user/PID/network/IPC isolation via `bwrap` (kernel namespaces).
    Bubblewrap,
    /// Interpreted fallback: clean env + timeout + output cap, no OS
    /// confinement.
    Subprocess,
}

impl SandboxBackend {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Bubblewrap => "bubblewrap",
            Self::Subprocess => "subprocess",
        }
    }
}

/// What went wrong when the sandbox itself fails (as opposed to the script
/// returning non-zero).
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("python interpreter not found: {0}")]
    PythonNotFound(String),
    #[error("sandbox backend init failed: {0}")]
    Backend(String),
    #[error("sandbox process spawn failed: {0}")]
    Spawn(String),
    #[error("wall-clock timeout ({0:?}) exceeded")]
    Timeout(Duration),
    #[error("killed by computer-use kill switch: {0}")]
    Halted(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Successful outcome of a sandboxed script run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SandboxOutcome {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub backend: SandboxBackend,
}

/// The sandbox itself, ready to run scripts.
pub struct PythonSandbox {
    pub backend: SandboxBackend,
    python: PathBuf,
    /// Computer-use kill switch (ADR-008): when engaged, an in-flight script
    /// is killed immediately.
    pub kill: Option<std::sync::Arc<openrt_safety::KillSwitch>>,
}

impl PythonSandbox {
    /// Detect the best available backend. `python` is the interpreter binary
    /// to invoke (should be an absolute path).
    pub fn detect(python: PathBuf) -> Self {
        Self {
            backend: Self::detect_backend(),
            python,
            kill: None,
        }
    }

    /// Build with an explicit backend (`bubblewrap` or `subprocess`) — used by
    /// tests and by operators who need to preview the degraded path.
    pub fn with_backend(python: PathBuf, backend: SandboxBackend) -> Self {
        Self {
            backend,
            python,
            kill: None,
        }
    }

    /// Absolute interpreter path this sandbox runs (used to build unified
    /// `SandboxRequest`s when the manager path is active).
    pub fn python_path(&self) -> &Path {
        &self.python
    }

    /// Detect bubblewrap availability without constructing a sandbox (used by
    /// `openrt doctor`).
    pub fn is_bwrap_available() -> bool {
        matches!(
            std::process::Command::new("bwrap").arg("--version").output(),
            Ok(o) if o.status.success()
        )
    }

    fn detect_backend() -> SandboxBackend {
        if Self::is_bwrap_available() {
            SandboxBackend::Bubblewrap
        } else {
            SandboxBackend::Subprocess
        }
    }

    fn python_abs(&self) -> Result<&str, SandboxError> {
        let p = self
            .python
            .to_str()
            .ok_or_else(|| SandboxError::PythonNotFound("non-UTF-8 python path".into()))?;
        if Path::new(p).is_absolute() {
            Ok(p)
        } else {
            Err(SandboxError::PythonNotFound(format!(
                "python path `{p}` is not absolute"
            )))
        }
    }

    /// Write the user script into a fresh scratch directory and run it with
    /// the enforced limits. The scratch directory is removed afterwards.
    pub async fn run(
        &self,
        script: &str,
        limits: &SandboxLimits,
    ) -> Result<SandboxOutcome, SandboxError> {
        let workdir = Self::create_workdir()?;
        std::fs::write(workdir.join("script.py"), script)?;
        let outcome = match self.backend {
            SandboxBackend::Bubblewrap => self.run_bwrap(&workdir, limits).await,
            SandboxBackend::Subprocess => self.run_subprocess(&workdir, limits).await,
        };
        let _ = std::fs::remove_dir_all(&workdir);
        outcome
    }

    fn create_workdir() -> Result<PathBuf, SandboxError> {
        let n = INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("openrt-sandbox-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    async fn run_bwrap(
        &self,
        workdir: &Path,
        limits: &SandboxLimits,
    ) -> Result<SandboxOutcome, SandboxError> {
        let python = self.python_abs()?;
        // This bwrap build has no --rlimit-* flags, so CPU/memory/process/file
        // limits are applied by a tiny Python prelude that sets rlimits and
        // then execv's the real script (rlimits survive exec).
        const BOOTSTRAP: &str = r#"import json, os, resource, sys
def settable(name):
    try:
        return getattr(resource, name)
    except AttributeError:
        return None

def setr(name, lim):
    r = settable(name)
    if r is not None and isinstance(lim, int) and lim > 0:
        try:
            resource.setrlimit(r, (lim, lim))
        except (OSError, ValueError):
            pass

try:
    cfg = json.loads(sys.argv[1])
except Exception:
    cfg = {}
setr('RLIMIT_AS', cfg.get('as'))
setr('RLIMIT_CPU', cfg.get('cpu'))
setr('RLIMIT_FSIZE', cfg.get('fsize'))
setr('RLIMIT_NPROC', cfg.get('nproc'))
setr('RLIMIT_NOFILE', cfg.get('nofile'))
os.execv(sys.executable, [sys.executable, '-I'] + sys.argv[2:])
"#;
        std::fs::write(workdir.join("bootstrap.py"), BOOTSTRAP)?;

        let cfg = json!({
            "as": limits.memory_bytes,
            "cpu": limits.cpu_seconds,
            "fsize": limits.output_bytes,
            "nproc": limits.max_procs,
            "nofile": 128,
        });

        let args: Vec<String> = vec![
            "--unshare-net".into(),
            "--unshare-pid".into(),
            "--unshare-ipc".into(),
            "--new-session".into(),
            "--die-with-parent".into(),
            "--ro-bind".into(),
            "/usr".into(),
            "/usr".into(),
            "--ro-bind".into(),
            "/usr/local".into(),
            "/usr/local".into(),
            "--ro-bind".into(),
            "/lib".into(),
            "/lib".into(),
            "--ro-bind".into(),
            "/lib64".into(),
            "/lib64".into(),
            "--ro-bind".into(),
            "/bin".into(),
            "/bin".into(),
            "--ro-bind".into(),
            "/etc".into(),
            "/etc".into(),
            "--proc".into(),
            "/proc".into(),
            "--dev".into(),
            "/dev".into(),
            "--dir".into(),
            "/sbx".into(),
            "--bind".into(),
            workdir.display().to_string(),
            "/sbx".into(),
            "--chdir".into(),
            "/sbx".into(),
            "--clearenv".into(),
            "--setenv".into(),
            "PATH".into(),
            "/usr/local/bin:/usr/bin:/bin".into(),
            python.into(),
            "-I".into(),
            "/sbx/bootstrap.py".into(),
            cfg.to_string(),
            "/sbx/script.py".into(),
        ];
        let outcome = collect_output(
            "bwrap",
            &args,
            workdir,
            limits,
            SandboxBackend::Bubblewrap,
            self.kill.clone(),
        )
        .await?;

        // Distinguish "bwrap could not set up the sandbox" from "the script
        // ran and exited non-zero". bwrap init errors land on stderr prefixed
        // with "bwrap:".
        if outcome.exit_code != Some(0) && outcome.stderr.trim_start().starts_with("bwrap:") {
            return Err(SandboxError::Backend(outcome.stderr.trim().to_string()));
        }
        Ok(outcome)
    }

    async fn run_subprocess(
        &self,
        workdir: &Path,
        limits: &SandboxLimits,
    ) -> Result<SandboxOutcome, SandboxError> {
        let python = self.python_abs()?;
        let script = workdir.join("script.py");
        // The interpreter binary is the spawn target; `-I` only adds the flags.
        let args = ["-I".to_string(), script.display().to_string()];
        collect_output(
            python,
            &args,
            workdir,
            limits,
            SandboxBackend::Subprocess,
            self.kill.clone(),
        )
        .await
    }
}

/// Read up to `cap` bytes from a child stream.
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

/// Spawn `bin args...` with a clean environment and the scratch dir as the
/// working directory, capture output capped, and enforce the wall timeout and
/// the computer-use kill switch (ADR-008).
async fn collect_output(
    bin: &str,
    args: &[String],
    workdir: &Path,
    limits: &SandboxLimits,
    backend: SandboxBackend,
    kill: Option<std::sync::Arc<openrt_safety::KillSwitch>>,
) -> Result<SandboxOutcome, SandboxError> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .current_dir(workdir)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            SandboxError::PythonNotFound(bin.to_string())
        } else {
            SandboxError::Spawn(e.to_string())
        }
    })?;

    let out_cap = limits.output_bytes;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_task = tokio::task::spawn(async move { read_capped(stdout, out_cap).await });
    let stderr_task = tokio::task::spawn(async move { read_capped(stderr, out_cap).await });

    let status = tokio::select! {
        res = tokio::time::timeout(limits.wall_timeout, child.wait()) => {
            match res {
                Ok(Ok(status)) => status,
                Ok(Err(e)) => return Err(SandboxError::Spawn(format!("wait: {e}"))),
                Err(_) => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return Err(SandboxError::Timeout(limits.wall_timeout));
                }
            }
        }
        reason = openrt_safety::halt_reason(kill) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(SandboxError::Halted(reason));
        }
    };

    let (out_b, out_trunc) = stdout_task
        .await
        .map_err(|_| SandboxError::Timeout(limits.wall_timeout))?;
    let (err_b, err_trunc) = stderr_task
        .await
        .map_err(|_| SandboxError::Timeout(limits.wall_timeout))?;

    Ok(SandboxOutcome {
        exit_code: status.code(),
        stdout: String::from_utf8_lossy(&out_b).into_owned(),
        stderr: String::from_utf8_lossy(&err_b).into_owned(),
        truncated: out_trunc || err_trunc,
        backend,
    })
}

/// `python.run` — run untrusted Python inside the sandbox.
pub struct PythonRun {
    pub spec: ToolSpec,
    pub sandbox: PythonSandbox,
    pub limits: SandboxLimits,
    /// Whether the unconfined `subprocess` fallback may run when bubblewrap is
    /// not installed. **Defaults to `false` (fail closed)**: without `bwrap`,
    /// `python.run` reports a clear error instead of silently running without
    /// isolation. Setting this true is an explicit operator decision.
    pub allow_unconfined_fallback: bool,
    /// Phase 8 unified sandbox authority. When attached (runtime wiring), runs
    /// go through admission → execution → verified cleanup and the output
    /// carries the sandbox identity; when absent (unit tests / embedded use)
    /// the Phase 2.4 legacy path runs unchanged.
    pub manager: Option<std::sync::Arc<crate::manager::SandboxManager>>,
    /// Sandbox profile used for manager-path admission.
    pub profile: crate::policy::SandboxProfile,
}

impl PythonRun {
    pub fn new(python: PathBuf) -> Self {
        Self {
            spec: ToolSpec::new(
                "python.run",
                "Runs Python code in an isolated sandbox. With bubblewrap installed the script has no host filesystem or network access; otherwise the interpreter-runtime tools (timeout, output cap, -I isolation) still apply but NOT OS-level confinement — such hosts must opt in explicitly.",
            )
            .with_caps(&["python.run.all"])
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "script": { "type": "string", "description": "Python source code to execute" }
                },
                "required": ["script"]
            })),
            sandbox: PythonSandbox::detect(python),
            limits: SandboxLimits::default(),
            allow_unconfined_fallback: false,
            manager: None,
            profile: crate::policy::SandboxProfile::Strict,
        }
    }

    /// Attach the unified sandbox authority (Phase 8 runtime wiring).
    pub fn with_manager(
        mut self,
        manager: std::sync::Arc<crate::manager::SandboxManager>,
        profile: crate::policy::SandboxProfile,
    ) -> Self {
        self.manager = Some(manager);
        self.profile = profile;
        self
    }

    pub fn with_limits(mut self, limits: SandboxLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Explicitly opt in to the degraded, unconfined `subprocess` fallback when
    /// bubblewrap is not installed (fail-safe default is to refuse).
    pub fn with_unconfined_fallback(mut self, allow: bool) -> Self {
        self.allow_unconfined_fallback = allow;
        self
    }
}

#[async_trait::async_trait]
impl Tool for PythonRun {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let script = args.get("script").and_then(Value::as_str).ok_or_else(|| {
            ToolError::InvalidArguments("python.run".into(), "missing `script` string".into())
        })?;

        // Phase 8: when the unified authority is attached, run through the
        // full admission → execution → verified-cleanup flow.
        if let Some(manager) = &self.manager {
            return self.run_managed(manager, script).await;
        }

        // Fail-safe: bubblewrap is the only confined backend. Without it, the
        // tool refuses unless the operator explicitly opted into the degraded
        // unconfined fallback (missing isolation helpers must fail safely —
        // no unrestricted fallback occurs).
        if self.sandbox.backend == SandboxBackend::Subprocess && !self.allow_unconfined_fallback {
            return Err(ToolError::Internal(
                "python.run".into(),
                "bubblewrap is not installed: refusing to run python without OS-level \
                 filesystem/network confinement (fail-safe). Set \
                 `python_sandbox.allow_unconfined_fallback` to true only if you accept \
                 the degraded unconfined mode"
                    .into(),
            ));
        }

        match self.sandbox.run(script, &self.limits).await {
            Ok(o) => Ok(ToolOutput::json(json!({
                "exit_code": o.exit_code,
                "stdout": o.stdout,
                "stderr": o.stderr,
                "truncated": o.truncated,
                "backend": o.backend.as_str(),
            }))),
            Err(e) => match e {
                SandboxError::Halted(reason) => Err(ToolError::KillSwitchEngaged {
                    tool: "python.run".into(),
                    reason,
                }),
                other => Err(ToolError::Internal(
                    "python.run".into(),
                    format!("sandbox error: {other}"),
                )),
            },
        }
    }
}
impl PythonRun {
    /// Manager-path execution (Phase 8 production wiring).
    async fn run_managed(
        &self,
        manager: &crate::manager::SandboxManager,
        script: &str,
    ) -> Result<ToolOutput, ToolError> {
        use crate::model::{SandboxErrorCode, SandboxRequest, SandboxState};
        let req =
            SandboxRequest::python_script(self.sandbox.python_path().display().to_string(), script)
                .with_limits(self.limits.clone());
        match manager.run(&req, self.profile, "python.run").await {
            Ok(res) => {
                if res.state == SandboxState::Timeout {
                    return Err(ToolError::Internal(
                        "python.run".into(),
                        format!(
                            "sandbox error: {}",
                            res.kill_reason.unwrap_or_else(|| "timeout".into())
                        ),
                    ));
                }
                if res.state == SandboxState::Killed
                    && res.kill_reason.as_deref() != Some("output_limit")
                {
                    // ADR-008 halt (or operator kill) mid-run.
                    return Err(ToolError::KillSwitchEngaged {
                        tool: "python.run".into(),
                        reason: res.kill_reason.unwrap_or_else(|| "halted".into()),
                    });
                }
                Ok(ToolOutput::json(json!({
                    "exit_code": res.exit_code,
                    "stdout": res.stdout,
                    "stderr": res.stderr,
                    "truncated": res.output_limited,
                    "backend": res.backend.as_str(),
                    "sandbox_id": res.id.to_string(),
                    "state": res.state.as_str(),
                    "profile": res.profile,
                    "isolation": res.isolation_level.as_str(),
                    "network": res.network.as_str(),
                    "policy_digest": res.policy_digest,
                    "cleanup_verified": res.cleanup.verified,
                })))
            }
            Err(f) => {
                let denial = matches!(
                    f.code,
                    SandboxErrorCode::AdmissionDenied
                        | SandboxErrorCode::PolicyDenied
                        | SandboxErrorCode::FilesystemDenied
                        | SandboxErrorCode::NetworkDenied
                        | SandboxErrorCode::ResourceDenied
                        | SandboxErrorCode::SyscallDenied
                        | SandboxErrorCode::ContainerUnavailable
                );
                if denial {
                    Err(ToolError::PermissionDenied {
                        tool: "python.run".into(),
                        reason: f.render(),
                    })
                } else {
                    Err(ToolError::Internal("python.run".into(), f.render()))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox() -> PythonSandbox {
        PythonSandbox::detect(PathBuf::from("/usr/bin/python3"))
    }

    #[tokio::test]
    async fn runs_script_and_prints() {
        let sb = sandbox();
        if sb.backend != SandboxBackend::Bubblewrap {
            eprintln!("skipping: bwrap not available");
            return;
        }
        let out = sb
            .run("print('hello sandbox')", &SandboxLimits::default())
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert!(out.stdout.contains("hello sandbox"), "{}", out.stdout);
    }

    #[tokio::test]
    async fn filesystem_escape_denied() {
        let sb = sandbox();
        if sb.backend != SandboxBackend::Bubblewrap {
            eprintln!("skipping: bwrap not available");
            return;
        }
        let script = "\
import os
try:
    open('/etc/openrt-pwned', 'w').write('x')
    print('WROTE_ETC')
except Exception as e:
    print('etc denied:', type(e).__name__)
try:
    open('/home/openrt-readme', 'r')
    print('READ_HOME')
except Exception as e:
    print('home denied:', type(e).__name__)
open('inside.txt', 'w').write('ok')
print('cwd write ok')
";
        let out = sb.run(script, &SandboxLimits::default()).await.unwrap();
        assert!(!out.stdout.contains("WROTE_ETC"), "wrote to /etc! {out:?}");
        assert!(!out.stdout.contains("READ_HOME"), "read /home! {out:?}");
        assert!(out.stdout.contains("cwd write ok"), "{out:?}");
        assert!(!Path::new("/etc/openrt-pwned").exists());
    }

    #[tokio::test]
    async fn network_denied() {
        let sb = sandbox();
        if sb.backend != SandboxBackend::Bubblewrap {
            eprintln!("skipping: bwrap not available");
            return;
        }
        let script = "\
import socket
try:
    s = socket.socket()
    s.settimeout(3)
    s.connect(('1.1.1.1', 80))
    print('connected')
except Exception as e:
    print('net denied:', type(e).__name__)
";
        let out = sb.run(script, &SandboxLimits::default()).await.unwrap();
        assert!(
            !out.stdout.contains("connected"),
            "network reachable! {out:?}"
        );
    }

    #[tokio::test]
    async fn memory_limit_enforced() {
        let sb = sandbox();
        if sb.backend != SandboxBackend::Bubblewrap {
            eprintln!("skipping: bwrap not available");
            return;
        }
        let script = "\
import time
buf = bytearray(512 * 1024 * 1024)
buf[0] = 1
print('allocated', len(buf))
time.sleep(0.1)
";
        let limits = SandboxLimits {
            memory_bytes: 128 * 1024 * 1024,
            cpu_seconds: 10,
            ..SandboxLimits::default()
        };
        let out = sb.run(script, &limits).await.unwrap();
        assert_ne!(out.exit_code, Some(0), "allocation succeeded: {out:?}");
        assert!(!out.stdout.contains("allocated"), "{out:?}");
    }

    #[tokio::test]
    async fn wall_timeout_killed() {
        let sb = sandbox();
        if sb.backend != SandboxBackend::Bubblewrap {
            eprintln!("skipping: bwrap not available");
            return;
        }
        let limits = SandboxLimits {
            wall_timeout: Duration::from_millis(500),
            ..SandboxLimits::default()
        };
        let err = sb
            .run("while True:\n    pass\n", &limits)
            .await
            .unwrap_err();
        assert!(matches!(err, SandboxError::Timeout(_)), "{err:?}");
    }

    #[tokio::test]
    async fn cpu_budget_enforced() {
        let sb = sandbox();
        if sb.backend != SandboxBackend::Bubblewrap {
            eprintln!("skipping: bwrap not available");
            return;
        }
        let limits = SandboxLimits {
            cpu_seconds: 1,
            wall_timeout: Duration::from_secs(20),
            ..SandboxLimits::default()
        };
        let ran = sb.run("while True:\n    pass\n", &limits).await;
        match ran {
            Ok(out) => assert_ne!(out.exit_code, Some(0), "{out:?}"),
            Err(SandboxError::Timeout(_)) => {}
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[tokio::test]
    async fn output_truncated() {
        let sb = sandbox();
        if sb.backend != SandboxBackend::Bubblewrap {
            eprintln!("skipping: bwrap not available");
            return;
        }
        let limits = SandboxLimits {
            output_bytes: 1024,
            ..SandboxLimits::default()
        };
        let out = sb.run("print('x' * 20000)\n", &limits).await.unwrap();
        assert!(out.truncated, "{out:?}");
        assert!(out.stdout.len() <= 1024);
    }

    #[tokio::test]
    async fn fork_bomb_contained() {
        let sb = sandbox();
        if sb.backend != SandboxBackend::Bubblewrap {
            eprintln!("skipping: bwrap not available");
            return;
        }
        let limits = SandboxLimits {
            max_procs: 16,
            wall_timeout: Duration::from_secs(10),
            ..SandboxLimits::default()
        };
        let script = "\
import os, sys
while True:
    try:
        os.fork()
    except OSError:
        print('fork denied — contained')
        sys.exit(7)
";
        let ran = sb.run(script, &limits).await;
        let out = match ran {
            Ok(out) => out,
            Err(SandboxError::Timeout(_)) => {
                eprintln!("fork bomb timed out (contained)");
                return;
            }
            Err(e) => panic!("unexpected error: {e}"),
        };
        // The bomb must never run to completion successfully: RLIMIT_NPROC /
        // the PID namespace stops forking and the script exits non-zero.
        assert_ne!(out.exit_code, Some(0), "{out:?}");
        assert!(out.stdout.contains("contained"), "{out:?}");
    }

    #[tokio::test]
    async fn missing_bwrap_refuses_by_default() {
        // The fail-safe: without bubblewrap the tool must NOT silently run
        // unconfined. This is backend-explicit so it never depends on the
        // host having bwrap.
        let mut tool = PythonRun::new(PathBuf::from("/usr/bin/python3"));
        tool.sandbox = PythonSandbox::with_backend(
            PathBuf::from("/usr/bin/python3"),
            SandboxBackend::Subprocess,
        );
        let err = tool
            .run(json!({ "script": "print('should not run')" }))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::Internal(tool_id, _) if tool_id == "python.run"),
            "{err:?}"
        );
        assert!(
            err.to_string().contains("bubblewrap is not installed"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn explicit_unconfined_fallback_runs() {
        // The subprocess backend CAN run only when the operator explicitly opts
        // in (`with_unconfined_fallback(true)`) — proving the fail-safe gate
        // and the opt-in path end to end. Interpreter isolation still applies
        // (python -I, timeout, output caps).
        let mut tool = PythonRun::new(PathBuf::from("/usr/bin/python3"))
            .with_limits(SandboxLimits::default())
            .with_unconfined_fallback(true);
        tool.sandbox = PythonSandbox::with_backend(
            PathBuf::from("/usr/bin/python3"),
            SandboxBackend::Subprocess,
        );
        let out = tool
            .run(json!({ "script": "print('fallback-ok')" }))
            .await
            .unwrap();
        assert!(out.render().contains("fallback-ok"), "{out:?}");
    }
}
