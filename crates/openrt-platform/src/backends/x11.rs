//! X11 display-server backend (xdotool / xclip / import). Never selected
//! silently on Wayland — resolver only picks this for X11 sessions.

use async_trait::async_trait;

use super::run_cmd;
use crate::backend::{
    BackendKind, ComputerBackend, ControlError, ControlOp, MonitorInfo, Observation,
    OperationResult, WindowRef,
};
use crate::capability::{CapabilityReport, CapabilityStatus, ComputerCapability};
use crate::profile::EnvironmentProfile;

pub struct X11Backend {
    profile: EnvironmentProfile,
}

impl X11Backend {
    pub fn new(profile: EnvironmentProfile) -> Self {
        Self { profile }
    }
}

/// Shared X11 clipboard helper (xclip preferred, xsel fallback).
pub(crate) async fn x11_clipboard(
    profile: &EnvironmentProfile,
    op: ControlOp,
) -> Result<OperationResult, ControlError> {
    match op {
        ControlOp::ClipboardGet => {
            if profile.has_tool("xclip") {
                let (ok, out, err, status) =
                    run_cmd("xclip", &["-selection", "clipboard", "-o"], 5_000)
                        .await
                        .map_err(ControlError::Command)?;
                if ok {
                    return Ok(OperationResult::ok(
                        "clipboard_get",
                        serde_json::json!({ "text": out }),
                    ));
                }
                // Empty clipboard often exits non-zero.
                if status == "1" {
                    return Ok(OperationResult::ok(
                        "clipboard_get",
                        serde_json::json!({ "text": "" }),
                    ));
                }
                return Err(ControlError::Command(format!(
                    "xclip -o failed ({status}): {err}"
                )));
            }
            if profile.has_tool("xsel") {
                let (ok, out, err, status) = run_cmd("xsel", &["--clipboard", "--output"], 5_000)
                    .await
                    .map_err(ControlError::Command)?;
                if ok {
                    return Ok(OperationResult::ok(
                        "clipboard_get",
                        serde_json::json!({ "text": out }),
                    ));
                }
                if status == "1" {
                    return Ok(OperationResult::ok(
                        "clipboard_get",
                        serde_json::json!({ "text": "" }),
                    ));
                }
                return Err(ControlError::Command(format!(
                    "xsel failed ({status}): {err}"
                )));
            }
            Ok(OperationResult::unavailable(
                "clipboard_get",
                "xclip/xsel not found on PATH",
            ))
        }
        ControlOp::ClipboardSet(text) => {
            if profile.has_tool("xclip") {
                use std::process::Stdio;
                use tokio::io::AsyncWriteExt;
                use tokio::process::Command;
                let mut child = Command::new("xclip")
                    .args(["-selection", "clipboard"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|e| ControlError::Command(format!("spawn xclip: {e}")))?;
                {
                    let mut stdin = child
                        .stdin
                        .take()
                        .ok_or_else(|| ControlError::Command("xclip stdin".into()))?;
                    stdin
                        .write_all(text.as_bytes())
                        .await
                        .map_err(|e| ControlError::Command(format!("xclip write: {e}")))?;
                    stdin
                        .shutdown()
                        .await
                        .map_err(|e| ControlError::Command(format!("xclip close: {e}")))?;
                }
                let out = tokio::time::timeout(
                    std::time::Duration::from_millis(5_000),
                    child.wait_with_output(),
                )
                .await
                .map_err(|_| ControlError::Command("xclip timed out".into()))?
                .map_err(|e| ControlError::Command(format!("xclip wait: {e}")))?;
                if !out.status.success() {
                    return Err(ControlError::Command(format!(
                        "xclip failed: {}",
                        String::from_utf8_lossy(&out.stderr)
                    )));
                }
                return Ok(OperationResult::ok(
                    "clipboard_set",
                    serde_json::json!({ "ok": true, "length": text.len() }),
                ));
            }
            if profile.has_tool("xsel") {
                let (ok, _, err, status) = run_cmd("xsel", &["--clipboard", "--input"], 5_000)
                    .await
                    .map_err(ControlError::Command)?;
                // xsel --input needs stdin; without it this may no-op — mark unavailable.
                if ok {
                    return Ok(OperationResult::unavailable(
                        "clipboard_set",
                        "xsel present but stdin wiring incomplete; install xclip",
                    ));
                }
                return Ok(OperationResult::unavailable(
                    "clipboard_set",
                    format!("xsel failed ({status}): {err}"),
                ));
            }
            Ok(OperationResult::unavailable(
                "clipboard_set",
                "xclip/xsel not found on PATH",
            ))
        }
        _ => Ok(OperationResult::unsupported(
            "clipboard",
            "operation is not a clipboard op",
        )),
    }
}

#[async_trait]
impl ComputerBackend for X11Backend {
    fn kind(&self) -> BackendKind {
        BackendKind::X11
    }

    fn capabilities(&self) -> Vec<CapabilityReport> {
        let b = BackendKind::X11.as_str();
        let has_xdo = self.profile.has_tool("xdotool");
        let mut rows = vec![
            CapabilityReport::new(
                ComputerCapability::EnvDiscover,
                CapabilityStatus::Supported,
                "",
                b,
            ),
            if has_xdo {
                CapabilityReport::supported(ComputerCapability::WindowList, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::WindowList,
                    "xdotool not found on PATH",
                    b,
                )
            },
            if has_xdo {
                CapabilityReport::supported(ComputerCapability::WindowFocus, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::WindowFocus,
                    "xdotool not found on PATH",
                    b,
                )
            },
            if has_xdo {
                CapabilityReport::supported(ComputerCapability::WindowClose, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::WindowClose,
                    "xdotool not found on PATH",
                    b,
                )
            },
            if has_xdo {
                CapabilityReport::supported(ComputerCapability::WorkspaceSwitch, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::WorkspaceSwitch,
                    "xdotool not found on PATH",
                    b,
                )
            },
            if has_xdo {
                CapabilityReport::supported(ComputerCapability::MonitorList, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::MonitorList,
                    "xdotool/xrandr not available for geometry probe",
                    b,
                )
            },
            if self.profile.has_tool("import") || self.profile.has_tool("grim") {
                CapabilityReport::supported(ComputerCapability::ScreenCapture, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::ScreenCapture,
                    "import/grim not found on PATH",
                    b,
                )
            },
            if has_xdo {
                CapabilityReport::supported(ComputerCapability::PointerInject, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::PointerInject,
                    "xdotool not found on PATH",
                    b,
                )
            },
            if has_xdo {
                CapabilityReport::supported(ComputerCapability::KeyboardInject, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::KeyboardInject,
                    "xdotool not found on PATH",
                    b,
                )
            },
            if self.profile.has_tool("xclip") || self.profile.has_tool("xsel") {
                CapabilityReport::supported(ComputerCapability::ClipboardGet, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::ClipboardGet,
                    "xclip/xsel not found on PATH",
                    b,
                )
            },
            if self.profile.has_tool("xclip") || self.profile.has_tool("xsel") {
                CapabilityReport::supported(ComputerCapability::ClipboardSet, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::ClipboardSet,
                    "xclip/xsel not found on PATH",
                    b,
                )
            },
            if self.profile.has_tool("xdg-open") {
                CapabilityReport::supported(ComputerCapability::ApplicationLaunch, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::ApplicationLaunch,
                    "xdg-open not found on PATH",
                    b,
                )
            },
            CapabilityReport::unavailable(
                ComputerCapability::AccessibilityTree,
                "a11y not wired for x11 backend in this pass",
                b,
            ),
            CapabilityReport::unavailable(
                ComputerCapability::BrowserSurface,
                "owned by openrt-computer browser tools",
                b,
            ),
        ];
        rows.dedup_by(|a, c| a.capability == c.capability);
        rows
    }

    async fn execute(&self, op: ControlOp) -> Result<OperationResult, ControlError> {
        match op {
            ControlOp::ListWindows => {
                if !self.profile.has_tool("xdotool") {
                    return Ok(OperationResult::unavailable(
                        "list_windows",
                        "xdotool not found on PATH",
                    ));
                }
                let (ok, out, err, status) = run_cmd("xdotool", &["search", "--name", ".*"], 5_000)
                    .await
                    .map_err(ControlError::Command)?;
                if !ok && status == "1" {
                    // No matches is an empty list, not a failure.
                    return Ok(OperationResult::ok("list_windows", serde_json::json!([])));
                }
                if !ok {
                    return Err(ControlError::Command(format!(
                        "xdotool search failed ({status}): {err}"
                    )));
                }
                let wins: Vec<WindowRef> = out
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|id| WindowRef {
                        id: id.trim().to_string(),
                        app_id: None,
                        pid: None,
                        title: None,
                        workspace: None,
                        focused: false,
                    })
                    .collect();
                Ok(OperationResult::ok(
                    "list_windows",
                    serde_json::to_value(&wins).unwrap_or_default(),
                ))
            }
            ControlOp::FocusWindow(id) => {
                if !self.profile.has_tool("xdotool") {
                    return Ok(OperationResult::unavailable(
                        "focus_window",
                        "xdotool not found on PATH",
                    ));
                }
                let (ok, _, err, status) =
                    run_cmd("xdotool", &["windowactivate", "--sync", &id], 5_000)
                        .await
                        .map_err(ControlError::Command)?;
                if ok {
                    Ok(OperationResult::ok(
                        "focus_window",
                        serde_json::json!({ "focused": id }),
                    ))
                } else {
                    Err(ControlError::Command(format!(
                        "xdotool windowactivate failed ({status}): {err}"
                    )))
                }
            }
            ControlOp::CloseWindow(id) => {
                if !self.profile.has_tool("xdotool") {
                    return Ok(OperationResult::unavailable(
                        "close_window",
                        "xdotool not found on PATH",
                    ));
                }
                let (ok, _, err, status) = run_cmd("xdotool", &["windowclose", &id], 5_000)
                    .await
                    .map_err(ControlError::Command)?;
                if ok {
                    Ok(OperationResult::ok(
                        "close_window",
                        serde_json::json!({ "closed": id }),
                    ))
                } else {
                    Err(ControlError::Command(format!(
                        "xdotool windowclose failed ({status}): {err}"
                    )))
                }
            }
            ControlOp::ListMonitors => {
                if self.profile.has_tool("xrandr") {
                    let (ok, out, err, status) = run_cmd("xrandr", &[], 5_000)
                        .await
                        .map_err(ControlError::Command)?;
                    if ok {
                        // Minimal parse: lines with " connected "
                        let mut mons = Vec::new();
                        for line in out.lines() {
                            if line.contains(" connected") {
                                let name = line.split_whitespace().next().unwrap_or("").to_string();
                                mons.push(MonitorInfo {
                                    name,
                                    width: 0,
                                    height: 0,
                                    scale: 1.0,
                                    x: 0,
                                    y: 0,
                                    focused: false,
                                });
                            }
                        }
                        return Ok(OperationResult::ok(
                            "list_monitors",
                            serde_json::to_value(&mons).unwrap_or_default(),
                        ));
                    }
                    return Ok(OperationResult::unavailable(
                        "list_monitors",
                        format!("xrandr failed ({status}): {err}"),
                    ));
                }
                Ok(OperationResult::unavailable(
                    "list_monitors",
                    "xrandr not found on PATH",
                ))
            }
            ControlOp::CaptureScreen => {
                if self.profile.has_tool("import") {
                    let path = std::env::temp_dir()
                        .join(format!("openrt-p7-x11-{}.png", std::process::id()));
                    let path_s = path.to_string_lossy().to_string();
                    let (ok, _, err, status) =
                        run_cmd("import", &["-window", "root", &path_s], 15_000)
                            .await
                            .map_err(ControlError::Command)?;
                    if ok && path.exists() {
                        return Ok(OperationResult::ok(
                            "capture_screen",
                            serde_json::json!({ "path": path_s, "backend": "x11" }),
                        ));
                    }
                    return Ok(OperationResult::unavailable(
                        "capture_screen",
                        format!("import failed ({status}): {err}"),
                    ));
                }
                Ok(OperationResult::unavailable(
                    "capture_screen",
                    "ImageMagick import not found on PATH",
                ))
            }
            ControlOp::MovePointer(x, y) => {
                if !self.profile.has_tool("xdotool") {
                    return Ok(OperationResult::unavailable(
                        "move_pointer",
                        "xdotool not found on PATH",
                    ));
                }
                let (ok, _, err, status) = run_cmd(
                    "xdotool",
                    &["mousemove", &x.to_string(), &y.to_string()],
                    5_000,
                )
                .await
                .map_err(ControlError::Command)?;
                if ok {
                    Ok(OperationResult::ok(
                        "move_pointer",
                        serde_json::json!({ "x": x, "y": y }),
                    ))
                } else {
                    Err(ControlError::Command(format!(
                        "xdotool mousemove failed ({status}): {err}"
                    )))
                }
            }
            ControlOp::ClickPointer => {
                if !self.profile.has_tool("xdotool") {
                    return Ok(OperationResult::unavailable(
                        "click_pointer",
                        "xdotool not found on PATH",
                    ));
                }
                let (ok, _, err, status) = run_cmd("xdotool", &["click", "1"], 5_000)
                    .await
                    .map_err(ControlError::Command)?;
                if ok {
                    Ok(OperationResult::ok(
                        "click_pointer",
                        serde_json::json!({ "button": 1 }),
                    ))
                } else {
                    Err(ControlError::Command(format!(
                        "xdotool click failed ({status}): {err}"
                    )))
                }
            }
            ControlOp::TypeText(text) => {
                if !self.profile.has_tool("xdotool") {
                    return Ok(OperationResult::unavailable(
                        "type_text",
                        "xdotool not found on PATH",
                    ));
                }
                let (ok, _, err, status) =
                    run_cmd("xdotool", &["type", "--clearmodifiers", "--", &text], 5_000)
                        .await
                        .map_err(ControlError::Command)?;
                if ok {
                    Ok(OperationResult::ok(
                        "type_text",
                        serde_json::json!({ "length": text.len() }),
                    ))
                } else {
                    Err(ControlError::Command(format!(
                        "xdotool type failed ({status}): {err}"
                    )))
                }
            }
            ControlOp::SendKey(key) => {
                if !self.profile.has_tool("xdotool") {
                    return Ok(OperationResult::unavailable(
                        "send_key",
                        "xdotool not found on PATH",
                    ));
                }
                let (ok, _, err, status) =
                    run_cmd("xdotool", &["key", "--clearmodifiers", &key], 5_000)
                        .await
                        .map_err(ControlError::Command)?;
                if ok {
                    Ok(OperationResult::ok(
                        "send_key",
                        serde_json::json!({ "key": key }),
                    ))
                } else {
                    Err(ControlError::Command(format!(
                        "xdotool key failed ({status}): {err}"
                    )))
                }
            }
            ControlOp::ClipboardGet | ControlOp::ClipboardSet(_) => {
                x11_clipboard(&self.profile, op).await
            }
            ControlOp::LaunchApp(app) => {
                if app
                    .chars()
                    .any(|c| matches!(c, '|' | ';' | '&' | '>' | '<' | '`' | '$'))
                {
                    return Ok(OperationResult::unsupported(
                        "launch_app",
                        "shell metacharacters not allowed",
                    ));
                }
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
                        format!("xdg-open failed ({status}): {err}"),
                    ));
                }
                Ok(OperationResult::unavailable(
                    "launch_app",
                    "xdg-open not found on PATH",
                ))
            }
            ControlOp::SwitchWorkspace(ws) => {
                if !self.profile.has_tool("xdotool") {
                    return Ok(OperationResult::unavailable(
                        "switch_workspace",
                        "xdotool not found on PATH",
                    ));
                }
                let (ok, _, err, status) = run_cmd("xdotool", &["set_desktop", &ws], 5_000)
                    .await
                    .map_err(ControlError::Command)?;
                if ok {
                    Ok(OperationResult::ok(
                        "switch_workspace",
                        serde_json::json!({ "workspace": ws }),
                    ))
                } else {
                    Err(ControlError::Command(format!(
                        "xdotool set_desktop failed ({status}): {err}"
                    )))
                }
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
        let windows = match self.execute(ControlOp::ListWindows).await {
            Ok(r) if r.ok => serde_json::from_value(r.data).unwrap_or_default(),
            _ => Vec::<WindowRef>::new(),
        };
        let monitors = match self.execute(ControlOp::ListMonitors).await {
            Ok(r) if r.ok => serde_json::from_value(r.data).unwrap_or_default(),
            _ => Vec::<MonitorInfo>::new(),
        };
        Ok(Observation {
            backend: "x11".into(),
            active_window: windows.iter().find(|w| w.focused).cloned(),
            window_count: windows.len(),
            monitors,
            cursor: None,
            captured_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
        })
    }
}
