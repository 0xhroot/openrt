//! bspwm backend via `bspc`.

use async_trait::async_trait;

use super::run_cmd;
use crate::backend::{
    BackendKind, ComputerBackend, ControlError, ControlOp, MonitorInfo, Observation,
    OperationResult, WindowRef,
};
use crate::capability::{CapabilityReport, CapabilityStatus, ComputerCapability};
use crate::profile::EnvironmentProfile;

pub struct BspwmBackend {
    profile: EnvironmentProfile,
}

impl BspwmBackend {
    pub fn new(profile: EnvironmentProfile) -> Self {
        Self { profile }
    }

    async fn bspc(&self, args: &[&str]) -> Result<String, ControlError> {
        let (ok, out, err, status) = run_cmd("bspc", args, 5_000)
            .await
            .map_err(ControlError::Command)?;
        if !ok {
            return Err(ControlError::Command(format!(
                "bspc {:?} ({status}): {err}",
                args
            )));
        }
        Ok(out)
    }
}

#[async_trait]
impl ComputerBackend for BspwmBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Bspwm
    }

    fn capabilities(&self) -> Vec<CapabilityReport> {
        let b = BackendKind::Bspwm.as_str();
        vec![
            CapabilityReport::supported(ComputerCapability::WindowList, b),
            CapabilityReport::supported(ComputerCapability::WindowFocus, b),
            CapabilityReport::supported(ComputerCapability::WindowClose, b),
            CapabilityReport::supported(ComputerCapability::WorkspaceSwitch, b),
            CapabilityReport::supported(ComputerCapability::MonitorList, b),
            CapabilityReport::supported(ComputerCapability::ApplicationLaunch, b),
            CapabilityReport::new(
                ComputerCapability::EnvDiscover,
                CapabilityStatus::Supported,
                "",
                b,
            ),
            CapabilityReport::unavailable(
                ComputerCapability::ScreenCapture,
                "grim/import not wired for bspwm backend in this pass",
                b,
            ),
            CapabilityReport::unavailable(
                ComputerCapability::PointerInject,
                "xdotool not assumed",
                b,
            ),
            if self.profile.has_tool("xdotool") {
                CapabilityReport::supported(ComputerCapability::KeyboardInject, b)
            } else {
                CapabilityReport::unavailable(
                    ComputerCapability::KeyboardInject,
                    "xdotool not found on PATH",
                    b,
                )
            },
            CapabilityReport::unavailable(
                ComputerCapability::ClipboardGet,
                "xclip/xsel not probed as default",
                b,
            ),
            CapabilityReport::unavailable(
                ComputerCapability::ClipboardSet,
                "xclip/xsel not probed as default",
                b,
            ),
            CapabilityReport::unavailable(
                ComputerCapability::AccessibilityTree,
                "a11y not selected for bspwm control",
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
            ControlOp::ListWindows => {
                let out = self.bspc(&["query", "-N", "-n", ".window"]).await?;
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
                self.bspc(&["node", &id, "-f"]).await?;
                Ok(OperationResult::ok(
                    "focus_window",
                    serde_json::json!({ "focused": id }),
                ))
            }
            ControlOp::CloseWindow(id) => {
                self.bspc(&["node", &id, "-c"]).await?;
                Ok(OperationResult::ok(
                    "close_window",
                    serde_json::json!({ "closed": id }),
                ))
            }
            ControlOp::ListMonitors => {
                let out = self.bspc(&["query", "-M"]).await?;
                let mons: Vec<MonitorInfo> = out
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|id| MonitorInfo {
                        name: id.trim().to_string(),
                        width: 0,
                        height: 0,
                        scale: 1.0,
                        x: 0,
                        y: 0,
                        focused: false,
                    })
                    .collect();
                Ok(OperationResult::ok(
                    "list_monitors",
                    serde_json::to_value(&mons).unwrap_or_default(),
                ))
            }
            ControlOp::CaptureScreen => Ok(OperationResult::unavailable(
                "capture_screen",
                "no capture tool wired for bspwm backend",
            )),
            ControlOp::MovePointer(_, _) | ControlOp::ClickPointer => {
                Ok(OperationResult::unavailable(
                    "pointer",
                    "pointer inject not available without xdotool",
                ))
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
                        "xdotool failed ({status}): {err}"
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
                crate::backends::x11::x11_clipboard(&self.profile, op).await
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
                // bspc rule --launch expects an application; use xdg-open/exec
                // via bspc node -i is not for apps — use xdg-open if present.
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
                    "no launch helper (xdg-open) on PATH",
                ))
            }
            ControlOp::SwitchWorkspace(ws) => {
                self.bspc(&["desktop", "-f", &format!("^{ws}")]).await?;
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
        let out = self.bspc(&["query", "-N", "-n", ".window"]).await?;
        let windows: Vec<WindowRef> = out
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
        let mons_out = self.bspc(&["query", "-M"]).await.unwrap_or_default();
        let monitors: Vec<MonitorInfo> = mons_out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|id| MonitorInfo {
                name: id.trim().to_string(),
                width: 0,
                height: 0,
                scale: 1.0,
                x: 0,
                y: 0,
                focused: false,
            })
            .collect();
        Ok(Observation {
            backend: "bspwm".into(),
            active_window: windows.first().cloned(),
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
