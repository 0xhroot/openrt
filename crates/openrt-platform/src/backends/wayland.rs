//! Generic Wayland display-server backend (no compositor IPC).
//! Clipboard + keyboard via wl-clipboard/wtype; capture via grim; window
//! control UNAVAILABLE without compositor IPC (honest).

use async_trait::async_trait;

use super::run_cmd;
use crate::backend::{
    BackendKind, ComputerBackend, ControlError, ControlOp, MonitorInfo, Observation,
    OperationResult,
};
use crate::capability::{CapabilityReport, CapabilityStatus, ComputerCapability};
use crate::profile::EnvironmentProfile;

pub struct WaylandBackend {
    profile: EnvironmentProfile,
}

impl WaylandBackend {
    pub fn new(profile: EnvironmentProfile) -> Self {
        Self { profile }
    }
}

/// Shared clipboard helper for Wayland-family backends.
pub(crate) async fn wayland_clipboard(
    profile: &EnvironmentProfile,
    op: ControlOp,
) -> Result<OperationResult, ControlError> {
    match op {
        ControlOp::ClipboardGet => {
            if !profile.has_tool("wl-paste") {
                return Ok(OperationResult::unavailable(
                    "clipboard_get",
                    "wl-paste not found on PATH",
                ));
            }
            let (ok, stdout, err, status) = run_cmd("wl-paste", &["--no-newline"], 5_000)
                .await
                .map_err(ControlError::Command)?;
            if !ok {
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
            if !profile.has_tool("wl-copy") {
                return Ok(OperationResult::unavailable(
                    "clipboard_set",
                    "wl-copy not found on PATH",
                ));
            }
            // wl-copy daemonizes; wait only for the parent (stdio nulled).
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
        _ => Ok(OperationResult::unsupported(
            "clipboard",
            "operation is not a clipboard op",
        )),
    }
}

#[async_trait]
impl ComputerBackend for WaylandBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Wayland
    }

    fn capabilities(&self) -> Vec<CapabilityReport> {
        let b = BackendKind::Wayland.as_str();
        vec![
            CapabilityReport::new(
                ComputerCapability::EnvDiscover,
                CapabilityStatus::Supported,
                "",
                b,
            ),
            CapabilityReport::unsupported(
                ComputerCapability::WindowList,
                "no compositor IPC on generic wayland backend (use hyprctl/swaymsg/i3-msg)",
                b,
            ),
            CapabilityReport::unsupported(
                ComputerCapability::WindowFocus,
                "no compositor IPC on generic wayland backend",
                b,
            ),
            CapabilityReport::unsupported(
                ComputerCapability::WindowClose,
                "no compositor IPC on generic wayland backend",
                b,
            ),
            CapabilityReport::unsupported(
                ComputerCapability::WorkspaceSwitch,
                "no compositor IPC on generic wayland backend",
                b,
            ),
            if self.profile.has_tool("grim") {
                CapabilityReport::supported(ComputerCapability::ScreenCapture, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::ScreenCapture,
                    "grim not found on PATH",
                    b,
                )
            },
            CapabilityReport::unavailable(
                ComputerCapability::MonitorList,
                "no monitor IPC without a compositor adapter",
                b,
            ),
            CapabilityReport::unavailable(
                ComputerCapability::PointerInject,
                "no pointer inject tool (ydotool/dotool) on this host",
                b,
            ),
            if self.profile.has_tool("wtype") {
                CapabilityReport::supported(ComputerCapability::KeyboardInject, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::KeyboardInject,
                    "wtype not found on PATH",
                    b,
                )
            },
            if self.profile.has_tool("wl-paste") {
                CapabilityReport::supported(ComputerCapability::ClipboardGet, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::ClipboardGet,
                    "wl-paste not found on PATH",
                    b,
                )
            },
            if self.profile.has_tool("wl-copy") {
                CapabilityReport::supported(ComputerCapability::ClipboardSet, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::ClipboardSet,
                    "wl-copy not found on PATH",
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
                "a11y not wired for generic wayland backend",
                b,
            ),
            CapabilityReport::unavailable(
                ComputerCapability::BrowserSurface,
                "owned by openrt-computer browser tools",
                b,
            ),
        ]
    }

    async fn execute(&self, op: ControlOp) -> Result<OperationResult, ControlError> {
        match op {
            ControlOp::ListWindows
            | ControlOp::FocusWindow(_)
            | ControlOp::CloseWindow(_)
            | ControlOp::SwitchWorkspace(_)
            | ControlOp::ListMonitors => Ok(OperationResult::unsupported(
                "window_control",
                "generic wayland backend has no compositor IPC; select a native compositor adapter",
            )),
            ControlOp::CaptureScreen => {
                if !self.profile.has_tool("grim") {
                    return Ok(OperationResult::unavailable(
                        "capture_screen",
                        "grim not found on PATH",
                    ));
                }
                let path =
                    std::env::temp_dir().join(format!("openrt-p7-wl-{}.png", std::process::id()));
                let path_s = path.to_string_lossy().to_string();
                let (ok, _, err, status) = run_cmd("grim", &[&path_s], 15_000)
                    .await
                    .map_err(ControlError::Command)?;
                if !ok || !path.exists() {
                    return Err(ControlError::Command(format!(
                        "grim failed ({status}): {err}"
                    )));
                }
                Ok(OperationResult::ok(
                    "capture_screen",
                    serde_json::json!({ "path": path_s, "backend": "wayland" }),
                ))
            }
            ControlOp::MovePointer(_, _) | ControlOp::ClickPointer => Ok(
                OperationResult::unavailable("pointer", "no pointer injection tool on this host"),
            ),
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
            ControlOp::ClipboardGet | ControlOp::ClipboardSet(_) => {
                wayland_clipboard(&self.profile, op).await
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
        Ok(Observation {
            backend: "wayland".into(),
            active_window: None,
            window_count: 0,
            monitors: Vec::<MonitorInfo>::new(),
            cursor: None,
            captured_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
        })
    }
}
