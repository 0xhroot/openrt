//! i3 backend via `i3-msg` (JSON IPC).

use async_trait::async_trait;

use super::run_cmd;
use crate::backend::{
    BackendKind, ComputerBackend, ControlError, ControlOp, MonitorInfo, Observation,
    OperationResult, WindowRef,
};
use crate::capability::{CapabilityReport, CapabilityStatus, ComputerCapability};
use crate::profile::EnvironmentProfile;

pub struct I3Backend {
    profile: EnvironmentProfile,
}

impl I3Backend {
    pub fn new(profile: EnvironmentProfile) -> Self {
        Self { profile }
    }

    async fn msg(&self, args: &[&str]) -> Result<String, ControlError> {
        let (ok, out, err, status) = run_cmd("i3-msg", args, 5_000)
            .await
            .map_err(ControlError::Command)?;
        if !ok {
            return Err(ControlError::Command(format!(
                "i3-msg {:?} ({status}): {err}",
                args
            )));
        }
        Ok(out)
    }
}

fn collect_windows(nodes: &[serde_json::Value], out: &mut Vec<WindowRef>) {
    for n in nodes {
        let window = n["window"].as_i64();
        let name = n["name"].as_str().map(|s| s.to_string());
        let focused = n["focused"].as_bool().unwrap_or(false);
        if window.is_some() {
            out.push(WindowRef {
                id: window.map(|w| w.to_string()).unwrap_or_default(),
                app_id: n["window_properties"]["class"]
                    .as_str()
                    .map(|s| s.to_string()),
                pid: n["pid"].as_i64().map(|p| p as i32),
                title: name,
                workspace: None,
                focused,
            });
        }
        if let Some(children) = n["nodes"].as_array() {
            collect_windows(children, out);
        }
        if let Some(floating) = n["floating_nodes"].as_array() {
            collect_windows(floating, out);
        }
    }
}

fn parse_outputs(json: &str) -> Result<Vec<MonitorInfo>, ControlError> {
    let arr: Vec<serde_json::Value> = serde_json::from_str(json)
        .map_err(|e| ControlError::Command(format!("outputs JSON: {e}")))?;
    Ok(arr
        .into_iter()
        .map(|o| MonitorInfo {
            name: o["name"].as_str().unwrap_or("").to_string(),
            width: o["rect"]["width"].as_i64().unwrap_or(0) as u32,
            height: o["rect"]["height"].as_i64().unwrap_or(0) as u32,
            scale: o["scale"].as_f64().unwrap_or(1.0),
            x: o["rect"]["x"].as_i64().unwrap_or(0) as i32,
            y: o["rect"]["y"].as_i64().unwrap_or(0) as i32,
            focused: false,
        })
        .collect())
}

#[async_trait]
impl ComputerBackend for I3Backend {
    fn kind(&self) -> BackendKind {
        BackendKind::I3
    }

    fn capabilities(&self) -> Vec<CapabilityReport> {
        let b = BackendKind::I3.as_str();
        let mut rows = vec![
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
                "import/scrot not probed; configure screen.capture tool for X11 captures",
                b,
            ),
            CapabilityReport::unavailable(
                ComputerCapability::PointerInject,
                "xdotool optional; not assumed",
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
            CapabilityReport::unavailable(
                ComputerCapability::AccessibilityTree,
                "AT-SPI not selected as control backend in this pass",
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
                let out = self.msg(&["-t", "get_tree"]).await?;
                let tree: serde_json::Value =
                    serde_json::from_str(&out).map_err(|e| ControlError::Command(e.to_string()))?;
                let mut wins = Vec::new();
                let nodes = tree["nodes"].as_array().cloned().unwrap_or_default();
                collect_windows(&nodes, &mut wins);
                Ok(OperationResult::ok(
                    "list_windows",
                    serde_json::to_value(&wins).unwrap_or_default(),
                ))
            }
            ControlOp::FocusWindow(id) => {
                self.msg(&[&format!("[con_id={id}] focus")]).await?;
                Ok(OperationResult::ok(
                    "focus_window",
                    serde_json::json!({ "focused": id }),
                ))
            }
            ControlOp::CloseWindow(id) => {
                self.msg(&[&format!("[con_id={id}] kill")]).await?;
                Ok(OperationResult::ok(
                    "close_window",
                    serde_json::json!({ "closed": id }),
                ))
            }
            ControlOp::ListMonitors => {
                let out = self.msg(&["-t", "get_outputs"]).await?;
                let mons = parse_outputs(&out)?;
                Ok(OperationResult::ok(
                    "list_monitors",
                    serde_json::to_value(&mons).unwrap_or_default(),
                ))
            }
            ControlOp::CaptureScreen => Ok(OperationResult::unavailable(
                "capture_screen",
                "no capture tool registered for i3 backend (use screen.capture tool)",
            )),
            ControlOp::MovePointer(_, _) | ControlOp::ClickPointer => {
                Ok(OperationResult::unavailable(
                    "pointer",
                    "xdotool not assumed; pointer inject unavailable",
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
                self.msg(&[&format!("exec {app}")]).await?;
                Ok(OperationResult::ok(
                    "launch_app",
                    serde_json::json!({ "app": app }),
                ))
            }
            ControlOp::SwitchWorkspace(ws) => {
                self.msg(&[&format!("workspace number {ws}")]).await?;
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
        let out = self.msg(&["-t", "get_tree"]).await?;
        let tree: serde_json::Value =
            serde_json::from_str(&out).map_err(|e| ControlError::Command(e.to_string()))?;
        let mut windows = Vec::new();
        let nodes = tree["nodes"].as_array().cloned().unwrap_or_default();
        collect_windows(&nodes, &mut windows);
        let outs = self.msg(&["-t", "get_outputs"]).await?;
        let monitors = parse_outputs(&outs)?;
        let active = windows.iter().find(|w| w.focused).cloned();
        Ok(Observation {
            backend: "i3".into(),
            active_window: active,
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
