//! Concrete platform backends.

pub mod bspwm;
pub mod gnome;
pub mod hyprland;
pub mod i3;
pub mod kde;
pub mod sim;
pub mod sway;
pub mod wayland;
pub mod windows;
pub mod x11;

/// Run a command with a timeout; return (success, stdout, stderr, status).
pub(crate) async fn run_cmd(
    program: &str,
    args: &[&str],
    timeout_ms: u64,
) -> Result<(bool, String, String, String), String> {
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::process::Command;

    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawn {program}: {e}"))?;

    let wait = child.wait_with_output();
    let out = tokio::time::timeout(Duration::from_millis(timeout_ms), wait)
        .await
        .map_err(|_| format!("{program} timed out after {timeout_ms}ms"))?
        .map_err(|e| format!("wait {program}: {e}"))?;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let status = out
        .status
        .code()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".into());
    Ok((out.status.success(), stdout, stderr, status))
}

/// Hyprland Lua dispatch form used by Omarchy/Hyprland ≥0.4x (legacy CLI
/// fails with `hl.dispatch(...)` parse errors on these builds).
pub(crate) async fn hyprctl_lua_dispatch(expr: &str) -> Result<bool, String> {
    let (ok, _out, err, status) = run_cmd("hyprctl", &["dispatch", expr], 5_000).await?;
    if !ok && !err.contains("ok") {
        // Some builds print ok on stdout and still exit 0; only fail on error.
        if status == "0" {
            return Ok(true);
        }
        return Err(format!("hyprctl dispatch failed ({status}): {err}"));
    }
    Ok(true)
}

pub(crate) async fn hyprctl_json(args: &[&str]) -> Result<String, String> {
    let (ok, stdout, stderr, status) = run_cmd("hyprctl", args, 5_000).await?;
    if !ok {
        return Err(format!("hyprctl {:?} failed ({status}): {stderr}", args));
    }
    Ok(stdout)
}
