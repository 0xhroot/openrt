//! Execution engine for admitted sandbox requests (M8.2 resource limits,
//! M8.3/M8.4 mount + network enforcement, M8.6 PID-identity-safe kills).
//!
//! The executor dispatches to a resolved [`BackendKind`], applies rlimits
//! through an in-sandbox bootstrap, enforces the wall timeout / output cap /
//! kill switch, and verifies that no process from the run survives.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::backend::BackendKind;
use crate::model::{
    NetworkMode, SandboxErrorCode, SandboxFailure, SandboxId, SandboxRequest, SandboxState,
};
use crate::policy::SandboxPolicy;

/// Bootstrap run in-sandbox: applies rlimits, brings loopback up for
/// `local_only`, installs allowlist audit hooks for python payloads, then
/// execs (or runs) the target.
const BOOTSTRAP_PY: &str = r#"import json, os, sys

def setr(name, lim):
    try:
        res = __import__('resource')
        r = getattr(res, name)
    except (AttributeError, ImportError):
        return
    if isinstance(lim, int) and lim > 0:
        try:
            res.setrlimit(r, (lim, lim))
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
setr('RLIMIT_NOFILE', 128)

if cfg.get('local_only'):
    for ipbin in ('/usr/bin/ip', '/bin/ip', '/sbin/ip'):
        if os.path.exists(ipbin):
            try:
                import subprocess
                subprocess.run([ipbin, 'link', 'set', 'lo', 'up'],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            except Exception:
                pass
            break

if cfg.get('allowlist'):
    import fnmatch, runpy
    allowed = list(cfg.get('allowlist') or [])
    def _match(host):
        host = '' if host is None else str(host)
        if not host:
            return True
        return any(fnmatch.fnmatch(host, p) for p in allowed)
    def _hook(event, args):
        if event == 'socket.getaddrinfo':
            if not _match(args[0] if args else None):
                raise PermissionError(
                    'NETWORK_DENIED: getaddrinfo %r not in allowlist' % (args[0],))
        elif event == 'socket.connect':
            addr = args[1] if len(args) > 1 else None
            host = None
            if isinstance(addr, (tuple, list)) and addr:
                host = addr[0]
            elif isinstance(addr, str):
                host = addr
            if host is not None and not _match(host):
                raise PermissionError(
                    'NETWORK_DENIED: connect %r not in allowlist' % (host,))
    sys.addaudithook(_hook)
    script = cfg.get('script', 'script.py')
    sys.argv = [script]
    runpy.run_path(script, run_name='__main__')
else:
    target = cfg.get('target') or []
    if not target:
        raise SystemExit('bootstrap: missing target')
    os.execv(target[0], target)
"#;

pub(crate) struct ExecutionMeta {
    pub workspace: PathBuf,
    pub pid: u32,
    pub starttime: String,
    pub state: SandboxState,
    pub kill_reason: Option<String>,
    pub output_limited: bool,
}

pub(crate) struct ExecutionOutcome {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub output_limited: bool,
}

fn create_workspace(id: &SandboxId) -> std::io::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "openrt-sandbox-{}-{}",
        std::process::id(),
        &id.0[..8.min(id.0.len())]
    ));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Resolve a program name against PATH (for `execv`, which needs an
/// absolute path).
fn resolve_program(program: &str) -> Option<PathBuf> {
    let p = Path::new(program);
    if p.is_absolute() {
        return p.exists().then(|| p.to_path_buf());
    }
    if p.components().count() > 1 {
        return std::fs::canonicalize(p).ok();
    }
    let path = std::env::var("PATH").ok()?;
    path.split(':').find_map(|dir| {
        let cand = Path::new(dir).join(program);
        cand.is_file().then_some(cand)
    })
}

/// Find a python interpreter for the rlimit/allowlist bootstrap when the
/// target itself is not python.
fn bootstrap_interpreter(target: &str) -> Option<PathBuf> {
    if target.contains("python") {
        return resolve_program(target);
    }
    for cand in ["/usr/bin/python3", "/bin/python3", "/usr/local/bin/python3"] {
        if Path::new(cand).is_file() {
            return Some(PathBuf::from(cand));
        }
    }
    resolve_program("python3")
}

/// Read `/proc/<pid>/stat` starttime (field 22) — the PID-reuse guard.
fn proc_starttime(pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = text.rsplit(')').next()?;
    let fields: Vec<&str> = after.split_whitespace().collect();
    // After ") ": [0]=state … starttime is overall field 22 → index 19.
    fields.get(19).map(|s| s.to_string())
}

fn proc_exists(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Any live process still in `pgid`?
fn pgid_alive(pgid: u32) -> bool {
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some(after) = text.rsplit(')').next() else {
            continue;
        };
        let fields: Vec<&str> = after.split_whitespace().collect();
        // After ") ": [0]=state, [1]=ppid, [2]=pgrp.
        if fields.get(2).and_then(|f| f.parse::<u32>().ok()) == Some(pgid) {
            return true;
        }
    }
    false
}

/// SIGKILL the run's process group + leader **after** verifying the PID still
/// belongs to our process (starttime identity), then verify nothing from the
/// run survives. Returns true only when cleanup is verified.
///
/// The payload stays in the spawned process group (this executor never passes
/// `--new-session`), so `kill -KILL -<pgid>` hits every member; on the
/// bubblewrap backend the PID-namespace init is a group member, so its death
/// makes the kernel reap the whole namespace. The unconfined `subprocess`
/// fallback cannot contain a payload that calls `setsid()` — that limitation
/// is part of its documented degraded status.
pub(crate) async fn kill_and_verify(pid: u32, starttime: &str) -> bool {
    match proc_starttime(pid) {
        None => {
            let mut wait = 0;
            while pgid_alive(pid) && wait < 40 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                wait += 1;
            }
            return !proc_exists(pid) && !pgid_alive(pid);
        }
        Some(st) if st != starttime => {
            // PID reused by an unrelated process — refuse to kill it.
            return false;
        }
        Some(_) => {}
    }
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(format!("-{pid}"))
        .stdin(Stdio::null())
        .status()
        .await;
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .status()
        .await;
    let mut wait = 0;
    while (proc_exists(pid) || pgid_alive(pid)) && wait < 60 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        wait += 1;
    }
    !proc_exists(pid) && !pgid_alive(pid)
}

async fn read_capped<R: AsyncRead + Unpin>(
    mut stream: Option<R>,
    cap: usize,
    overflow: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
) -> (Vec<u8>, bool) {
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
                        overflow.store(true, Ordering::SeqCst);
                        notify.notify_waiters();
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

async fn join_cap(t: tokio::task::JoinHandle<(Vec<u8>, bool)>) -> (Vec<u8>, bool) {
    match tokio::time::timeout(Duration::from_secs(5), t).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => (Vec::new(), true),
        Err(_) => (Vec::new(), true),
    }
}

/// Build the bubblewrap argv for an admitted request: namespaces per network
/// mode, policy mounts, clearenv + allowlisted env, workspace at `/sbx`, and
/// the rlimit bootstrap as the payload entrypoint.
fn build_bwrap_args(
    req: &SandboxRequest,
    policy: &SandboxPolicy,
    workspace: &Path,
    cfg: &serde_json::Value,
    interp: &Path,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--unshare-pid".into(),
        "--unshare-ipc".into(),
        "--die-with-parent".into(),
    ];
    if matches!(req.network, NetworkMode::None | NetworkMode::LocalOnly) {
        args.push("--unshare-net".into());
    }
    for p in [
        "/usr",
        "/usr/local",
        "/lib",
        "/lib64",
        "/bin",
        "/sbin",
        "/etc",
    ] {
        if Path::new(p).exists() {
            args.push("--ro-bind".into());
            args.push(p.into());
            args.push(p.into());
        }
    }
    args.push("--proc".into());
    args.push("/proc".into());
    args.push("--dev".into());
    args.push("/dev".into());
    args.push("--dir".into());
    args.push("/sbx".into());
    args.push("--bind".into());
    args.push(workspace.display().to_string());
    args.push("/sbx".into());
    for m in &req.mounts {
        args.push(if m.read_only { "--ro-bind" } else { "--bind" }.into());
        args.push(m.source.display().to_string());
        args.push(m.target.display().to_string());
    }
    let chdir = match req.cwd.as_deref() {
        None | Some(".") => "/sbx".to_string(),
        Some(c) => format!("/sbx/{}", c.trim_matches('/')),
    };
    args.push("--chdir".into());
    args.push(chdir);
    args.push("--clearenv".into());
    args.push("--setenv".into());
    args.push("PATH".into());
    args.push("/usr/local/bin:/usr/bin:/bin".into());
    // Admission validated keys against the policy allowlist; re-check here
    // so a future caller cannot bypass validation by calling the executor.
    for (k, v) in &req.env {
        if policy.env_allowlist.iter().any(|x| x == k) {
            args.push("--setenv".into());
            args.push(k.clone());
            args.push(v.clone());
        }
    }
    args.push(interp.display().to_string());
    args.push("-I".into());
    args.push("/sbx/.openrt-bootstrap.py".into());
    args.push(cfg.to_string());
    args
}

/// Execute an admitted request. Errors are structured
/// ([`SandboxErrorCode`]); on success the outcome carries everything the
/// manager needs to build a `SandboxResult`.
pub(crate) async fn execute(
    id: &SandboxId,
    req: &SandboxRequest,
    policy: &SandboxPolicy,
    backend: BackendKind,
    kill: Option<Arc<openrt_safety::KillSwitch>>,
) -> Result<(ExecutionOutcome, ExecutionMeta), SandboxFailure> {
    let workspace = create_workspace(id).map_err(|e| {
        SandboxFailure::new(
            SandboxErrorCode::BackendUnavailable,
            "could not create the sandbox workspace",
            e.to_string(),
            "scratch workspace under the system temp dir",
            "check temp-dir permissions and free disk space",
        )
    })?;
    let fail_setup = |detail: String| {
        let _ = std::fs::remove_dir_all(&workspace);
        SandboxFailure::new(
            SandboxErrorCode::BackendUnavailable,
            "sandbox setup failed before execution",
            detail,
            "SandboxRequest → executor setup",
            "verify the program path, mounts, and that bubblewrap can create namespaces",
        )
    };

    // Materialize declared files (names must be single components).
    for (name, content) in &req.files {
        if name.is_empty() || name.contains('/') || name.contains('\\') || name == ".." {
            return Err(fail_setup(format!(
                "illegal workspace file name `{name}` (single path components only)"
            )));
        }
        if let Err(e) = std::fs::write(workspace.join(name), content) {
            return Err(fail_setup(format!("writing workspace file `{name}`: {e}")));
        }
    }

    let interpreter = bootstrap_interpreter(&req.program);
    let allowlist = req.network == NetworkMode::Allowlist;
    let local_only = req.network == NetworkMode::LocalOnly;
    let want_bootstrap = interpreter.is_some();
    if allowlist && !want_bootstrap {
        // Admission already rejects non-python allowlist payloads; a python
        // payload always has an interpreter for the bootstrap.
        return Err(fail_setup(
            "allowlist enforcement requires the python bootstrap interpreter".into(),
        ));
    }

    let target: Vec<String> = {
        let prog = resolve_program(&req.program)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| req.program.clone());
        let mut v = vec![prog];
        v.extend(req.args.iter().cloned());
        v
    };
    let cfg_allowlist: Vec<String> = if allowlist {
        req.network_allowlist.clone()
    } else {
        Vec::new()
    };
    let cfg = json!({
        "as": req.limits.memory_bytes,
        "cpu": req.limits.cpu_seconds,
        "fsize": req.limits.output_bytes,
        "nproc": req.limits.max_procs,
        "local_only": local_only && want_bootstrap,
        "allowlist": cfg_allowlist,
        "script": "script.py",
        "target": target,
    });

    let mut cmd = match backend {
        BackendKind::Bubblewrap => {
            let Some(interp) = &interpreter else {
                return Err(fail_setup(
                    "no python interpreter available for the sandbox bootstrap".into(),
                ));
            };
            std::fs::write(workspace.join(".openrt-bootstrap.py"), BOOTSTRAP_PY)
                .map_err(|e| fail_setup(format!("writing bootstrap: {e}")))?;
            let args = build_bwrap_args(req, policy, &workspace, &cfg, interp);
            let mut c = Command::new("bwrap");
            c.args(args);
            c
        }
        // Docker/nspawn never reach here (no container image pipeline in
        // this phase — admission fails CONTAINER_UNAVAILABLE first).
        BackendKind::Subprocess | BackendKind::Docker | BackendKind::SystemdNspawn => {
            if want_bootstrap {
                let interp = interpreter.as_ref().expect("checked above");
                std::fs::write(workspace.join(".openrt-bootstrap.py"), BOOTSTRAP_PY)
                    .map_err(|e| fail_setup(format!("writing bootstrap: {e}")))?;
                let mut c = Command::new(interp);
                c.arg("-I")
                    .arg(workspace.join(".openrt-bootstrap.py"))
                    .arg(cfg.to_string());
                c
            } else {
                let mut c = Command::new(&req.program);
                c.args(&req.args);
                c
            }
        }
    };
    cmd.current_dir(&workspace)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    if backend == BackendKind::Subprocess {
        for (k, v) in &req.env {
            cmd.env(k, v);
        }
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| fail_setup(format!("failed to spawn `{}`: {e}", req.program)))?;

    // PID identity guard: record starttime immediately (M8.6).
    let pid = child
        .id()
        .ok_or_else(|| fail_setup("sandbox child has no pid".into()))?;
    let mut starttime = None;
    for _ in 0..20 {
        if let Some(st) = proc_starttime(pid) {
            starttime = Some(st);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let starttime = starttime.ok_or_else(|| {
        let _ = child.start_kill();
        fail_setup(format!(
            "/proc/{pid}/stat unreadable — refusing to run without a PID identity kill guard"
        ))
    })?;

    let out_cap = req.limits.output_bytes;
    let overflow = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(tokio::sync::Notify::new());
    let stdout_task = tokio::task::spawn(read_capped(
        child.stdout.take(),
        out_cap,
        overflow.clone(),
        notify.clone(),
    ));
    let stderr_task = tokio::task::spawn(read_capped(
        child.stderr.take(),
        out_cap,
        overflow,
        notify.clone(),
    ));

    enum Stop {
        Exited(std::process::ExitStatus),
        TimedOut,
        Overflow,
        Halted(String),
    }
    let stop = tokio::select! {
        res = tokio::time::timeout(req.limits.wall_timeout, child.wait()) => {
            match res {
                Ok(Ok(status)) => Stop::Exited(status),
                Ok(Err(e)) => {
                    let _ = kill_and_verify(pid, &starttime).await;
                    return Err(fail_setup(format!("waiting on the sandbox process failed: {e}")));
                }
                Err(_) => Stop::TimedOut,
            }
        }
        _ = notify.notified() => Stop::Overflow,
        reason = openrt_safety::halt_reason(kill.clone()) => Stop::Halted(reason),
    };

    // Kill (identity-checked) and drain the group/namespace.
    let mut kill_reason = None;
    let mut timed_out = false;
    let mut output_limited = false;
    let mut exit_code: Option<i32> = None;
    match stop {
        Stop::Exited(status) => exit_code = status.code(),
        Stop::TimedOut => {
            timed_out = true;
            kill_reason = Some(format!("wall timeout {:?}", req.limits.wall_timeout));
            let _ = kill_and_verify(pid, &starttime).await;
        }
        Stop::Overflow => {
            output_limited = true;
            kill_reason = Some("output_limit".to_string());
            let _ = kill_and_verify(pid, &starttime).await;
        }
        Stop::Halted(reason) => {
            kill_reason = Some(reason);
            let _ = kill_and_verify(pid, &starttime).await;
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;

    let (out_b, out_trunc) = join_cap(stdout_task).await;
    let (err_b, err_trunc) = join_cap(stderr_task).await;

    let out_limited = output_limited || out_trunc || err_trunc;
    let outcome = ExecutionOutcome {
        exit_code,
        stdout: String::from_utf8_lossy(&out_b).into_owned(),
        stderr: String::from_utf8_lossy(&err_b).into_owned(),
        output_limited: out_limited,
    };

    let state = if timed_out {
        SandboxState::Timeout
    } else if outcome.output_limited && kill_reason.as_deref() == Some("output_limit") {
        SandboxState::Killed
    } else if kill_reason.is_some() {
        SandboxState::Killed
    } else if outcome.exit_code == Some(0) {
        SandboxState::Succeeded
    } else {
        SandboxState::Failed
    };

    // bwrap setup failures surface as stderr prefixed `bwrap:`.
    if backend == BackendKind::Bubblewrap
        && outcome.exit_code != Some(0)
        && outcome.stderr.trim_start().starts_with("bwrap:")
        && !timed_out
    {
        let detail = outcome.stderr.trim().to_string();
        let _ = std::fs::remove_dir_all(&workspace);
        return Err(SandboxFailure::new(
            SandboxErrorCode::BackendUnavailable,
            "bubblewrap could not set up the sandbox",
            detail,
            "bwrap invocation (mounts/namespaces)",
            "run `openrt sandbox capabilities` and check the requested mounts exist \
             and namespaces are permitted",
        ));
    }

    Ok((
        outcome,
        ExecutionMeta {
            workspace,
            pid,
            starttime,
            state,
            kill_reason,
            output_limited: out_limited,
        },
    ))
}
