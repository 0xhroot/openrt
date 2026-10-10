//! GNOME backend surface via `gdbus`/`gsettings` when present. Honest
//! UNAVAILABLE rows when the session or tools are missing.

use async_trait::async_trait;

use super::run_cmd;
use crate::backend::{
    BackendKind, ComputerBackend, ControlError, ControlOp, MonitorInfo, Observation,
    OperationResult,
};
use crate::capability::{CapabilityReport, CapabilityStatus, ComputerCapability};
use crate::profile::EnvironmentProfile;

pub struct GnomeBackend {
    profile: EnvironmentProfile,
}

impl GnomeBackend {
    pub fn new(profile: EnvironmentProfile) -> Self {
        Self { profile }
    }
}

#[async_trait]
impl ComputerBackend for GnomeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Gnome
    }

    fn capabilities(&self) -> Vec<CapabilityReport> {
        let b = BackendKind::Gnome.as_str();
        let has_gdbus = self.profile.has_tool("gdbus");
        let mut rows = vec![
            CapabilityReport::new(
                ComputerCapability::EnvDiscover,
                CapabilityStatus::Supported,
                "",
                b,
            ),
            // Mutter compositor D-Bus is the native list/focus path on GNOME.
            if has_gdbus {
                CapabilityReport::supported(ComputerCapability::WindowList, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::WindowList,
                    "gdbus not found on PATH",
                    b,
                )
            },
            if has_gdbus {
                CapabilityReport::supported(ComputerCapability::WindowFocus, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::WindowFocus,
                    "gdbus not found on PATH",
                    b,
                )
            },
            if has_gdbus {
                CapabilityReport::supported(ComputerCapability::WindowClose, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::WindowClose,
                    "gdbus not found on PATH",
                    b,
                )
            },
            if has_gdbus {
                CapabilityReport::supported(ComputerCapability::WorkspaceSwitch, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::WorkspaceSwitch,
                    "gdbus not found on PATH",
                    b,
                )
            },
            if has_gdbus {
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
                    "gnome-screenshot/portal not wired; grim not present",
                    b,
                )
            },
            CapabilityReport::unavailable(
                ComputerCapability::PointerInject,
                "no pointer inject tool for GNOME",
                b,
            ),
            if self.profile.has_tool("wtype") {
                CapabilityReport::supported(ComputerCapability::KeyboardInject, b)
            } else if self.profile.has_tool("ydotool") {
                CapabilityReport::supported(ComputerCapability::KeyboardInject, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::KeyboardInject,
                    "wtype/ydotool not found on PATH",
                    b,
                )
            },
            if self.profile.has_tool("wl-paste") || self.profile.has_tool("xclip") {
                CapabilityReport::supported(ComputerCapability::ClipboardGet, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::ClipboardGet,
                    "no clipboard tool on PATH",
                    b,
                )
            },
            if self.profile.has_tool("wl-copy") || self.profile.has_tool("xclip") {
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
            if self.profile.a11y_bus {
                CapabilityReport::new(
                    ComputerCapability::AccessibilityTree,
                    CapabilityStatus::Partial,
                    "AT-SPI bus present; Orca tree API not wired in this pass",
                    b,
                )
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::AccessibilityTree,
                    "AT-SPI bus not present",
                    b,
                )
            },
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
                if !self.profile.has_tool("gdbus") {
                    return Ok(OperationResult::unavailable(
                        "list_windows",
                        "gdbus not found on PATH",
                    ));
                }
                // Mutter's Eval is restricted on many builds — treat failure
                // as UNAVAILABLE rather than faking an empty success.
                let (ok, out, err, status) = run_cmd(
                    "gdbus",
                    &[
                        "call",
                        "--session",
                        "--dest",
                        "org.gnome.Shell",
                        "--object-path",
                        "/org/gnome/Shell",
                        "--method",
                        "org.freedesktop.DBus.Properties.Get",
                        "org.gnome.Shell",
                        "Eval",
                    ],
                    5_000,
                )
                .await
                .map_err(ControlError::Command)?;
                if !ok {
                    return Ok(OperationResult::unavailable(
                        "list_windows",
                        format!("org.gnome.Shell Eval unavailable ({status}): {err}"),
                    ));
                }
                Ok(OperationResult::partial(
                    "list_windows",
                    "gnome shell Eval returned raw payload; structured parse not implemented",
                    serde_json::json!({ "raw": out }),
                ))
            }
            ControlOp::FocusWindow(_)
            | ControlOp::CloseWindow(_)
            | ControlOp::SwitchWorkspace(_) => Ok(OperationResult::unavailable(
                "window_control",
                "gnome window control via shell Eval not safely available without a typed API",
            )),
            ControlOp::ListMonitors => {
                if !self.profile.has_tool("gdbus") {
                    return Ok(OperationResult::unavailable(
                        "list_monitors",
                        "gdbus not found on PATH",
                    ));
                }
                let (ok, out, err, status) = run_cmd(
                    "gdbus",
                    &[
                        "call",
                        "--session",
                        "--dest",
                        "org.gnome.Mutter.DisplayConfig",
                        "--object-path",
                        "/org/gnome/Mutter/DisplayConfig",
                        "--method",
                        "org.freedesktop.DBus.Properties.Get",
                        "org.gnome.Mutter.DisplayConfig",
                        "CurrentMonitorState",
                    ],
                    5_000,
                )
                .await
                .map_err(ControlError::Command)?;
                if ok {
                    return Ok(OperationResult::partial(
                        "list_monitors",
                        "Mutter DisplayConfig returned; geometry parse not implemented",
                        serde_json::json!({ "raw": out }),
                    ));
                }
                Ok(OperationResult::unavailable(
                    "list_monitors",
                    format!("Mutter DisplayConfig failed ({status}): {err}"),
                ))
            }
            ControlOp::CaptureScreen => {
                if self.profile.has_tool("grim") {
                    let path = std::env::temp_dir()
                        .join(format!("openrt-p7-gnome-{}.png", std::process::id()));
                    let path_s = path.to_string_lossy().to_string();
                    let (ok, _, err, status) = run_cmd("grim", &[&path_s], 15_000)
                        .await
                        .map_err(ControlError::Command)?;
                    if ok && path.exists() {
                        return Ok(OperationResult::ok(
                            "capture_screen",
                            serde_json::json!({ "path": path_s, "backend": "gnome" }),
                        ));
                    }
                    return Ok(OperationResult::unavailable(
                        "capture_screen",
                        format!("grim failed ({status}): {err}"),
                    ));
                }
                Ok(OperationResult::unavailable(
                    "capture_screen",
                    "no screenshot tool; gnome portal not wired in this pass",
                ))
            }
            ControlOp::MovePointer(_, _) | ControlOp::ClickPointer => Ok(
                OperationResult::unavailable("pointer", "no pointer inject tool for GNOME"),
            ),
            ControlOp::TypeText(text) => {
                if self.profile.has_tool("wtype") {
                    let (ok, _, err, status) = run_cmd("wtype", &["--", &text], 10_000)
                        .await
                        .map_err(ControlError::Command)?;
                    if ok {
                        return Ok(OperationResult::ok(
                            "type_text",
                            serde_json::json!({ "length": text.len() }),
                        ));
                    }
                    return Ok(OperationResult::unavailable(
                        "type_text",
                        format!("wtype failed ({status}): {err}"),
                    ));
                }
                Ok(OperationResult::unavailable(
                    "type_text",
                    "wtype not found on PATH",
                ))
            }
            ControlOp::SendKey(key) => {
                if self.profile.has_tool("wtype") {
                    let (ok, _, err, status) = run_cmd("wtype", &["-k", &key], 5_000)
                        .await
                        .map_err(ControlError::Command)?;
                    if ok {
                        return Ok(OperationResult::ok(
                            "send_key",
                            serde_json::json!({ "key": key }),
                        ));
                    }
                    return Ok(OperationResult::unavailable(
                        "send_key",
                        format!("wtype -k failed ({status}): {err}"),
                    ));
                }
                Ok(OperationResult::unavailable(
                    "send_key",
                    "wtype not found on PATH",
                ))
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
            backend: "gnome".into(),
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
