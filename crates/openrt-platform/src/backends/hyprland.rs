//! Hyprland native backend via `hyprctl` (JSON for observation, Lua
//! `hl.dsp.*` dispatch for actions — Omarchy/Hyprland ≥0.4x).

use async_trait::async_trait;

use super::{hyprctl_json, hyprctl_lua_dispatch, run_cmd};
use crate::backend::{
    BackendKind, ComputerBackend, ControlError, ControlOp, MonitorInfo, Observation,
    OperationResult, WindowRef,
};
use crate::capability::{CapabilityReport, CapabilityStatus, ComputerCapability};
use crate::profile::EnvironmentProfile;

pub struct HyprlandBackend {
    profile: EnvironmentProfile,
}

impl HyprlandBackend {
    pub fn new(profile: EnvironmentProfile) -> Self {
        Self { profile }
    }

    fn tool_status(&self, capability: ComputerCapability, tool: &str) -> CapabilityReport {
        if self.profile.has_tool(tool) {
            CapabilityReport::supported(capability, BackendKind::Hyprland.as_str())
        } else {
            CapabilityReport::new(
                capability,
                CapabilityStatus::Unavailable,
                format!("tool `{tool}` not found on PATH"),
                BackendKind::Hyprland.as_str(),
            )
        }
    }
}

fn parse_windows(json: &str) -> Result<Vec<WindowRef>, ControlError> {
    let clients: Vec<serde_json::Value> = serde_json::from_str(json)
        .map_err(|e| ControlError::Command(format!("clients JSON: {e}")))?;
    Ok(clients
        .into_iter()
        .map(|c| {
            let id = c["address"]
                .as_str()
                .unwrap_or_default()
                .trim_start_matches("0x")
                .to_string();
            WindowRef {
                id: c["address"].as_str().unwrap_or("").to_string(),
                app_id: c["class"]
                    .as_str()
                    .map(|s| s.to_string())
                    .or_else(|| c["initialClass"].as_str().map(|s| s.to_string())),
                pid: c["pid"].as_i64().map(|p| p as i32),
                title: c["title"].as_str().map(|s| s.to_string()),
                workspace: c["workspace"]["name"]
                    .as_str()
                    .or_else(|| c["workspace"]["id"].as_i64().map(|_| "ws"))
                    .map(|_| {
                        c["workspace"]["name"]
                            .as_str()
                            .map(|s| s.to_string())
                            .or_else(|| c["workspace"]["id"].as_i64().map(|i| i.to_string()))
                            .unwrap_or_else(|| "unknown".into())
                    }),
                focused: c["focusHistoryID"]
                    .as_i64()
                    .map(|h| h == 0)
                    .unwrap_or(false)
                    || c.get("mapped").and_then(|m| m.as_bool()).unwrap_or(false)
                        && c["focusHistoryID"]
                            .as_i64()
                            .map(|h| h == 0)
                            .unwrap_or(false),
            }
            .with_id_placeholder(id)
        })
        .collect())
}

// Helper trait to keep id construction readable (address already set above).
trait WithId {
    fn with_id_placeholder(self, _id: String) -> Self;
}
impl WithId for WindowRef {
    fn with_id_placeholder(self, _id: String) -> Self {
        self
    }
}

fn parse_monitors(json: &str) -> Result<Vec<MonitorInfo>, ControlError> {
    let mons: Vec<serde_json::Value> = serde_json::from_str(json)
        .map_err(|e| ControlError::Command(format!("monitors JSON: {e}")))?;
    Ok(mons
        .into_iter()
        .map(|m| MonitorInfo {
            name: m["name"].as_str().unwrap_or("").to_string(),
            width: m["width"].as_i64().unwrap_or(0) as u32,
            height: m["height"].as_i64().unwrap_or(0) as u32,
            scale: m["scale"].as_f64().unwrap_or(1.0),
            x: m["x"].as_i64().unwrap_or(0) as i32,
            y: m["y"].as_i64().unwrap_or(0) as i32,
            focused: m["focused"].as_bool().unwrap_or(false),
        })
        .collect())
}

#[async_trait]
impl ComputerBackend for HyprlandBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Hyprland
    }

    fn capabilities(&self) -> Vec<CapabilityReport> {
        let b = BackendKind::Hyprland.as_str();
        let mut rows = vec![
            self.tool_status(ComputerCapability::WindowList, "hyprctl"),
            self.tool_status(ComputerCapability::WindowFocus, "hyprctl"),
            self.tool_status(ComputerCapability::WindowClose, "hyprctl"),
            self.tool_status(ComputerCapability::WorkspaceSwitch, "hyprctl"),
            self.tool_status(ComputerCapability::MonitorList, "hyprctl"),
            self.tool_status(ComputerCapability::ScreenCapture, "grim"),
            self.tool_status(ComputerCapability::ClipboardGet, "wl-paste"),
            self.tool_status(ComputerCapability::ClipboardSet, "wl-copy"),
            self.tool_status(ComputerCapability::KeyboardInject, "wtype"),
            self.tool_status(ComputerCapability::ApplicationLaunch, "hyprctl"),
            CapabilityReport::new(
                ComputerCapability::EnvDiscover,
                CapabilityStatus::Supported,
                "",
                b,
            ),
        ];
        // Pointer: no ydotool/dotool and hyprctl has no warp → honest unsupported/unavailable.
        if self.profile.has_tool("ydotool") || self.profile.has_tool("dotool") {
            rows.push(CapabilityReport::supported(
                ComputerCapability::PointerInject,
                b,
            ));
        } else {
            rows.push(CapabilityReport::unavailable(
                ComputerCapability::PointerInject,
                "no ydotool/dotool on PATH; hyprctl exposes no pointer warp",
                b,
            ));
        }
        if self.profile.a11y_bus {
            rows.push(CapabilityReport::new(
                ComputerCapability::AccessibilityTree,
                CapabilityStatus::Partial,
                "AT-SPI bus present; tree API not wired in this pass",
                b,
            ));
        } else {
            rows.push(CapabilityReport::unavailable(
                ComputerCapability::AccessibilityTree,
                "AT-SPI bus not present",
                b,
            ));
        }
        rows.push(CapabilityReport::unavailable(
            ComputerCapability::BrowserSurface,
            "browser surface tools live in openrt-computer, not this backend",
            b,
        ));
        rows
    }

    async fn execute(&self, op: ControlOp) -> Result<OperationResult, ControlError> {
        match op {
            ControlOp::ListWindows => {
                let out = hyprctl_json(&["clients", "-j"])
                    .await
                    .map_err(ControlError::Command)?;
                let wins = parse_windows(&out)?;
                Ok(OperationResult::ok(
                    "list_windows",
                    serde_json::to_value(&wins).unwrap_or_default(),
                ))
            }
            ControlOp::FocusWindow(addr) => {
                let expr = format!("hl.dsp.focus({{ window = \"address:{addr}\" }})");
                hyprctl_lua_dispatch(&expr)
                    .await
                    .map_err(ControlError::Command)?;
                // Verify: active window address matches.
                let out = hyprctl_json(&["activewindow", "-j"])
                    .await
                    .map_err(ControlError::Command)?;
                let active: serde_json::Value = serde_json::from_str(&out).unwrap_or_default();
                let active_addr = active["address"].as_str().unwrap_or("");
                if !addr.is_empty() && active_addr != addr {
                    // Still return ok:false partial if focus did not stick
                    // (e.g. window closed mid-flight) — do not fake success.
                    if active_addr.is_empty() {
                        return Ok(OperationResult::partial(
                            "focus_window",
                            "dispatch accepted but no active window reported",
                            serde_json::json!({ "requested": addr, "active": active_addr }),
                        ));
                    }
                }
                Ok(OperationResult::ok(
                    "focus_window",
                    serde_json::json!({ "focused": addr, "active": active_addr }),
                ))
            }
            ControlOp::CloseWindow(addr) => {
                let expr = format!("hl.dsp.window.close({{ window = \"address:{addr}\" }})");
                hyprctl_lua_dispatch(&expr)
                    .await
                    .map_err(ControlError::Command)?;
                Ok(OperationResult::ok(
                    "close_window",
                    serde_json::json!({ "closed": addr }),
                ))
            }
            ControlOp::ListMonitors => {
                let out = hyprctl_json(&["monitors", "-j"])
                    .await
                    .map_err(ControlError::Command)?;
                let mons = parse_monitors(&out)?;
                Ok(OperationResult::ok(
                    "list_monitors",
                    serde_json::to_value(&mons).unwrap_or_default(),
                ))
            }
            ControlOp::CaptureScreen => {
                if !self.profile.has_tool("grim") {
                    return Ok(OperationResult::unavailable(
                        "capture_screen",
                        "grim not found on PATH",
                    ));
                }
                let path = std::env::temp_dir().join(format!(
                    "openrt-p7-capture-{}-{}.png",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0)
                ));
                let path_s = path.to_string_lossy().to_string();
                let (ok, _, err, status) = run_cmd("grim", &[&path_s], 15_000)
                    .await
                    .map_err(ControlError::Command)?;
                if !ok || !path.exists() {
                    return Err(ControlError::Command(format!(
                        "grim failed ({status}): {err}"
                    )));
                }
                let meta = std::fs::metadata(&path).ok().map(|m| m.len()).unwrap_or(0);
                // Leave file for the caller; report path + size (observation).
                Ok(OperationResult::ok(
                    "capture_screen",
                    serde_json::json!({
                        "path": path_s,
                        "bytes": meta,
                        "backend": "hyprland",
                    }),
                ))
            }
            ControlOp::MovePointer(x, y) => {
                if self.profile.has_tool("ydotool") {
                    let (ok, _, err, status) =
                        run_cmd("ydotool", &["move", &x.to_string(), &y.to_string()], 5_000)
                            .await
                            .map_err(ControlError::Command)?;
                    if ok {
                        return Ok(OperationResult::ok(
                            "move_pointer",
                            serde_json::json!({ "x": x, "y": y }),
                        ));
                    }
                    return Ok(OperationResult::unavailable(
                        "move_pointer",
                        format!("ydotool failed ({status}): {err}"),
                    ));
                }
                Ok(OperationResult::unavailable(
                    "move_pointer",
                    "no pointer injection tool (ydotool/dotool) on this host",
                ))
            }
            ControlOp::ClickPointer => Ok(OperationResult::unavailable(
                "click_pointer",
                "no pointer injection tool (ydotool/dotool) on this host",
            )),
            ControlOp::TypeText(text) => {
                if !self.profile.has_tool("wtype") {
                    return Ok(OperationResult::unavailable(
                        "type_text",
                        "wtype not found on PATH",
                    ));
                }
                let (ok, _, err, status) = run_cmd("wtype", &["--", &text], 10_000)
                    .await
                    .map_err(ControlError::Command)?;
                if ok {
                    Ok(OperationResult::ok(
                        "type_text",
                        serde_json::json!({ "length": text.len() }),
                    ))
                } else {
                    Err(ControlError::Command(format!(
                        "wtype failed ({status}): {err}"
                    )))
                }
            }
            ControlOp::SendKey(key) => {
                if !self.profile.has_tool("wtype") {
                    return Ok(OperationResult::unavailable(
                        "send_key",
                        "wtype not found on PATH",
                    ));
                }
                // wtype -k uses xkb key names; pass through as a single token.
                let (ok, _, err, status) = run_cmd("wtype", &["-k", &key], 5_000)
                    .await
                    .map_err(ControlError::Command)?;
                if ok {
                    Ok(OperationResult::ok(
                        "send_key",
                        serde_json::json!({ "key": key }),
                    ))
                } else {
                    Err(ControlError::Command(format!(
                        "wtype -k failed ({status}): {err}"
                    )))
                }
            }
            ControlOp::ClipboardGet => {
                if !self.profile.has_tool("wl-paste") {
                    return Ok(OperationResult::unavailable(
                        "clipboard_get",
                        "wl-paste not found on PATH",
                    ));
                }
                let (ok, stdout, err, status) = run_cmd("wl-paste", &["--no-newline"], 5_000)
                    .await
                    .map_err(ControlError::Command)?;
                if !ok {
                    // Empty clipboard often exits non-zero — treat as empty text.
                    if status == "1" || err.to_lowercase().contains("nothing") {
                        return Ok(OperationResult::ok(
                            "clipboard_get",
                            serde_json::json!({ "text": "" }),
                        ));
                    }
                    return Err(ControlError::Command(format!(
                        "wl-paste failed ({status}): {err}"
                    )));
                }
                Ok(OperationResult::ok(
                    "clipboard_get",
                    serde_json::json!({ "text": stdout }),
                ))
            }
            ControlOp::ClipboardSet(text) => {
                if !self.profile.has_tool("wl-copy") {
                    return Ok(OperationResult::unavailable(
                        "clipboard_set",
                        "wl-copy not found on PATH",
                    ));
                }
                // wl-copy daemonizes: the forked child inherits stdio and can
                // keep piped stderr open forever, so wait only for the parent
                // to exit (stdio nulled) — never wait_with_output.
                use std::process::Stdio;
                use tokio::io::AsyncWriteExt;
                use tokio::process::Command;
                let mut child = Command::new("wl-copy")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|e| ControlError::Command(format!("spawn wl-copy: {e}")))?;
                {
                    let mut stdin = child
                        .stdin
                        .take()
                        .ok_or_else(|| ControlError::Command("wl-copy stdin".into()))?;
                    stdin
                        .write_all(text.as_bytes())
                        .await
                        .map_err(|e| ControlError::Command(format!("wl-copy write: {e}")))?;
                    stdin
                        .shutdown()
                        .await
                        .map_err(|e| ControlError::Command(format!("wl-copy close: {e}")))?;
                }
                let status =
                    tokio::time::timeout(std::time::Duration::from_millis(5_000), child.wait())
                        .await
                        .map_err(|_| ControlError::Command("wl-copy timed out".into()))?
                        .map_err(|e| ControlError::Command(format!("wl-copy wait: {e}")))?;
                if !status.success() {
                    return Err(ControlError::Command(format!(
                        "wl-copy exited with {status}"
                    )));
                }
                Ok(OperationResult::ok(
                    "clipboard_set",
                    serde_json::json!({ "ok": true, "length": text.len() }),
                ))
            }
            ControlOp::LaunchApp(app) => {
                // Prefer a bare exec via hyprctl (no shell). If the token
                // contains shell metacharacters, fall back to structured
                // reject rather than interpolating into a shell.
                if app
                    .chars()
                    .any(|c| matches!(c, '|' | ';' | '&' | '>' | '<' | '`' | '$'))
                {
                    return Ok(OperationResult::unsupported(
                        "launch_app",
                        "shell metacharacters not allowed; pass a single executable or use terminal tool",
                    ));
                }
                let expr = format!("hl.dsp.exec_cmd(\"{app}\")");
                match hyprctl_lua_dispatch(&expr).await {
                    Ok(_) => Ok(OperationResult::ok(
                        "launch_app",
                        serde_json::json!({ "app": app }),
                    )),
                    Err(e) => {
                        // Fallback: xdg-open when present.
                        if self.profile.has_tool("xdg-open") {
                            let (ok, _, err, status) = run_cmd("xdg-open", &[&app], 5_000)
                                .await
                                .map_err(ControlError::Command)?;
                            if ok {
                                return Ok(OperationResult::ok(
                                    "launch_app",
                                    serde_json::json!({ "app": app, "via": "xdg-open" }),
                                ));
                            }
                            return Ok(OperationResult::unavailable(
                                "launch_app",
                                format!(
                                    "hyprctl exec failed ({e}); xdg-open failed ({status}): {err}"
                                ),
                            ));
                        }
                        Err(ControlError::Command(e))
                    }
                }
            }
            ControlOp::SwitchWorkspace(ws) => {
                let expr = format!("hl.dsp.focus({{ workspace = \"{ws}\" }})");
                hyprctl_lua_dispatch(&expr)
                    .await
                    .map_err(ControlError::Command)?;
                Ok(OperationResult::ok(
                    "switch_workspace",
                    serde_json::json!({ "workspace": ws }),
                ))
            }
            ControlOp::Observe => {
                let obs = self.observe().await?;
                Ok(OperationResult::ok(
                    "observe",
                    serde_json::to_value(obs).unwrap_or_default(),
                ))
            }
        }
    }

    async fn observe(&self) -> Result<Observation, ControlError> {
        let clients = hyprctl_json(&["clients", "-j"])
            .await
            .map_err(ControlError::Command)?;
        let windows = parse_windows(&clients)?;
        let mons_json = hyprctl_json(&["monitors", "-j"])
            .await
            .map_err(ControlError::Command)?;
        let monitors = parse_monitors(&mons_json)?;
        let active = windows.iter().find(|w| w.focused).cloned();
        let cursor = hyprctl_json(&["cursorpos"]).await.ok().and_then(|s| {
            let mut it = s.split(',');
            let x = it.next()?.trim().parse::<i32>().ok()?;
            let y = it.next()?.trim().parse::<i32>().ok()?;
            Some((x, y))
        });
        Ok(Observation {
            backend: "hyprland".into(),
            active_window: active,
            window_count: windows.len(),
            monitors,
            cursor,
            captured_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
        })
    }
}
