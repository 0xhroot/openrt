//! KDE Plasma backend surface. Uses `kdotool` when present; otherwise
//! reports honest UNAVAILABLE/UNSUPPORTED rows (no fake GREEN).

use async_trait::async_trait;

use super::run_cmd;
use crate::backend::{
    BackendKind, ComputerBackend, ControlError, ControlOp, MonitorInfo, Observation,
    OperationResult, WindowRef,
};
use crate::capability::{CapabilityReport, CapabilityStatus, ComputerCapability};
use crate::profile::EnvironmentProfile;

pub struct KdeBackend {
    profile: EnvironmentProfile,
}

impl KdeBackend {
    pub fn new(profile: EnvironmentProfile) -> Self {
        Self { profile }
    }
}

#[async_trait]
impl ComputerBackend for KdeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Kde
    }

    fn capabilities(&self) -> Vec<CapabilityReport> {
        let b = BackendKind::Kde.as_str();
        let has_kdotool = self.profile.has_tool("kdotool");
        let mut rows = vec![
            CapabilityReport::new(
                ComputerCapability::EnvDiscover,
                CapabilityStatus::Supported,
                "",
                b,
            ),
            if has_kdotool {
                CapabilityReport::supported(ComputerCapability::WindowList, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::WindowList,
                    "kdotool not found on PATH",
                    b,
                )
            },
            if has_kdotool {
                CapabilityReport::supported(ComputerCapability::WindowFocus, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::WindowFocus,
                    "kdotool not found on PATH",
                    b,
                )
            },
            if has_kdotool {
                CapabilityReport::supported(ComputerCapability::WindowClose, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::WindowClose,
                    "kdotool not found on PATH",
                    b,
                )
            },
            CapabilityReport::new(
                ComputerCapability::WorkspaceSwitch,
                if has_kdotool {
                    CapabilityStatus::Supported
                } else {
                    CapabilityStatus::Unavailable
                },
                if has_kdotool {
                    ""
                } else {
                    "kdotool not found on PATH"
                },
                b,
            ),
            if self.profile.has_tool("gdbus") {
                CapabilityReport::supported(ComputerCapability::MonitorList, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::MonitorList,
                    "gdbus not found on PATH",
                    b,
                )
            },
            if self.profile.has_tool("grim") {
                CapabilityReport::supported(ComputerCapability::ScreenCapture, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::ScreenCapture,
                    "no screenshot tool for this KDE session (Wayland portal not wired)",
                    b,
                )
            },
            CapabilityReport::unavailable(
                ComputerCapability::PointerInject,
                "no pointer inject tool registered for KDE",
                b,
            ),
            if has_kdotool {
                CapabilityReport::supported(ComputerCapability::KeyboardInject, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::KeyboardInject,
                    "kdotool not found on PATH",
                    b,
                )
            },
            if self.profile.has_tool("wl-paste") {
                CapabilityReport::supported(ComputerCapability::ClipboardGet, b)
            } else if self.profile.has_tool("xclip") || self.profile.has_tool("xsel") {
                CapabilityReport::supported(ComputerCapability::ClipboardGet, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::ClipboardGet,
                    "no clipboard tool on PATH",
                    b,
                )
            },
            if self.profile.has_tool("wl-copy") {
                CapabilityReport::supported(ComputerCapability::ClipboardSet, b)
            } else if self.profile.has_tool("xclip") || self.profile.has_tool("xsel") {
                CapabilityReport::supported(ComputerCapability::ClipboardSet, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::ClipboardSet,
                    "no clipboard tool on PATH",
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
                "KDE a11y tree not wired in this pass",
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
                if !self.profile.has_tool("kdotool") {
                    return Ok(OperationResult::unavailable(
                        "list_windows",
                        "kdotool not found on PATH",
                    ));
                }
                let (ok, out, err, status) = run_cmd("kdotool", &["search", "--name", ""], 5_000)
                    .await
                    .map_err(ControlError::Command)?;
                if !ok {
                    return Err(ControlError::Command(format!(
                        "kdotool search failed ({status}): {err}"
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
                if !self.profile.has_tool("kdotool") {
                    return Ok(OperationResult::unavailable(
                        "focus_window",
                        "kdotool not found on PATH",
                    ));
                }
                let (ok, _, err, status) = run_cmd("kdotool", &["windowactivate", &id], 5_000)
                    .await
                    .map_err(ControlError::Command)?;
                if ok {
                    Ok(OperationResult::ok(
                        "focus_window",
                        serde_json::json!({ "focused": id }),
                    ))
                } else {
                    Err(ControlError::Command(format!(
                        "kdotool windowactivate failed ({status}): {err}"
                    )))
                }
            }
            ControlOp::CloseWindow(id) => {
                if !self.profile.has_tool("kdotool") {
                    return Ok(OperationResult::unavailable(
                        "close_window",
                        "kdotool not found on PATH",
                    ));
                }
                let (ok, _, err, status) = run_cmd("kdotool", &["windowclose", &id], 5_000)
                    .await
                    .map_err(ControlError::Command)?;
                if ok {
                    Ok(OperationResult::ok(
                        "close_window",
                        serde_json::json!({ "closed": id }),
                    ))
                } else {
                    Err(ControlError::Command(format!(
                        "kdotool windowclose failed ({status}): {err}"
                    )))
                }
            }
            ControlOp::ListMonitors => {
                if self.profile.has_tool("gdbus") {
                    let (ok, out, err, status) = run_cmd(
                        "gdbus",
                        &[
                            "call",
                            "--session",
                            "--dest",
                            "org.kde.KWin",
                            "--object-path",
                            "/KWin",
                            "--method",
                            "org.freedesktop.DBus.Properties.Get",
                            "org.kde.KWin",
                            "workspace",
                        ],
                        5_000,
                    )
                    .await
                    .map_err(ControlError::Command)?;
                    if ok {
                        return Ok(OperationResult::partial(
                            "list_monitors",
                            "KWin properties returned; geometry parse not implemented",
                            serde_json::json!({ "raw": out }),
                        ));
                    }
                    return Ok(OperationResult::unavailable(
                        "list_monitors",
                        format!("gdbus KWin query failed ({status}): {err}"),
                    ));
                }
                Ok(OperationResult::unavailable(
                    "list_monitors",
                    "gdbus not found on PATH",
                ))
            }
            ControlOp::CaptureScreen => {
                if self.profile.has_tool("grim") {
                    let path = std::env::temp_dir()
                        .join(format!("openrt-p7-kde-{}.png", std::process::id()));
                    let path_s = path.to_string_lossy().to_string();
                    let (ok, _, err, status) = run_cmd("grim", &[&path_s], 15_000)
                        .await
                        .map_err(ControlError::Command)?;
                    if ok && path.exists() {
                        return Ok(OperationResult::ok(
                            "capture_screen",
                            serde_json::json!({ "path": path_s, "backend": "kde" }),
                        ));
                    }
                    return Ok(OperationResult::unavailable(
                        "capture_screen",
                        format!("grim failed ({status}): {err}"),
                    ));
                }
                Ok(OperationResult::unavailable(
                    "capture_screen",
                    "no screenshot tool; KDE portal screenshot not wired in this pass",
                ))
            }
            ControlOp::MovePointer(_, _) | ControlOp::ClickPointer => Ok(
                OperationResult::unavailable("pointer", "no pointer inject tool for KDE"),
            ),
            ControlOp::TypeText(text) => {
                if !self.profile.has_tool("kdotool") {
                    return Ok(OperationResult::unavailable(
                        "type_text",
                        "kdotool not found on PATH",
                    ));
                }
                let (ok, _, err, status) = run_cmd("kdotool", &["type", &text], 10_000)
                    .await
                    .map_err(ControlError::Command)?;
                if ok {
                    Ok(OperationResult::ok(
                        "type_text",
                        serde_json::json!({ "length": text.len() }),
                    ))
                } else {
                    Err(ControlError::Command(format!(
                        "kdotool type failed ({status}): {err}"
                    )))
                }
            }
            ControlOp::SendKey(key) => {
                if !self.profile.has_tool("kdotool") {
                    return Ok(OperationResult::unavailable(
                        "send_key",
                        "kdotool not found on PATH",
                    ));
                }
                let (ok, _, err, status) = run_cmd("kdotool", &["key", &key], 5_000)
                    .await
                    .map_err(ControlError::Command)?;
                if ok {
                    Ok(OperationResult::ok(
                        "send_key",
                        serde_json::json!({ "key": key }),
                    ))
                } else {
                    Err(ControlError::Command(format!(
                        "kdotool key failed ({status}): {err}"
                    )))
                }
            }
            ControlOp::ClipboardGet | ControlOp::ClipboardSet(_) => {
                if self.profile.session_type == crate::profile::SessionType::Wayland {
                    crate::backends::wayland::wayland_clipboard(&self.profile, op).await
                } else {
                    crate::backends::x11::x11_clipboard(&self.profile, op).await
                }
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
                if !self.profile.has_tool("kdotool") {
                    return Ok(OperationResult::unavailable(
                        "switch_workspace",
                        "kdotool not found on PATH",
                    ));
                }
                let (ok, _, err, status) = run_cmd("kdotool", &["workspace", &ws], 5_000)
                    .await
                    .map_err(ControlError::Command)?;
                if ok {
                    Ok(OperationResult::ok(
                        "switch_workspace",
                        serde_json::json!({ "workspace": ws }),
                    ))
                } else {
                    Err(ControlError::Command(format!(
                        "kdotool workspace failed ({status}): {err}"
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
            Ok(r) if r.ok || r.status == CapabilityStatus::Partial => {
                serde_json::from_value(r.data).unwrap_or_default()
            }
            _ => Vec::<MonitorInfo>::new(),
        };
        Ok(Observation {
            backend: "kde".into(),
            active_window: None,
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
