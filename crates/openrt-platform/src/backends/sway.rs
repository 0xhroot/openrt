//! Sway backend via `swaymsg` (JSON IPC).

use async_trait::async_trait;

use super::run_cmd;
use crate::backend::{
    BackendKind, ComputerBackend, ControlError, ControlOp, MonitorInfo, Observation,
    OperationResult, WindowRef,
};
use crate::capability::{CapabilityReport, CapabilityStatus, ComputerCapability};
use crate::profile::EnvironmentProfile;

pub struct SwayBackend {
    profile: EnvironmentProfile,
}

impl SwayBackend {
    pub fn new(profile: EnvironmentProfile) -> Self {
        Self { profile }
    }

    async fn msg(&self, args: &[&str]) -> Result<String, ControlError> {
        let (ok, out, err, status) = run_cmd("swaymsg", args, 5_000)
            .await
            .map_err(ControlError::Command)?;
        if !ok {
            return Err(ControlError::Command(format!(
                "swaymsg {:?} ({status}): {err}",
                args
            )));
        }
        Ok(out)
    }
}

fn parse_tree_windows(tree: &serde_json::Value, out: &mut Vec<WindowRef>) {
    let app_id = tree["app_id"].as_str().map(|s| s.to_string());
    let window_props = &tree["window_properties"];
    let class = window_props["class"].as_str().map(|s| s.to_string());
    let name = tree["name"].as_str().map(|s| s.to_string());
    let id = tree["id"].as_i64();
    if let (Some(_), true) = (
        id,
        tree["type"].as_str() == Some("con") || tree["type"].as_str() == Some("floating_con"),
    ) {
        let has_content = app_id.is_some() || class.is_some() || tree.get("pid").is_some();
        if has_content
            && tree["nodes"]
                .as_array()
                .map(|n| n.is_empty())
                .unwrap_or(true)
            || has_content && tree.get("shell").is_some()
        {
            out.push(WindowRef {
                id: id.map(|i| i.to_string()).unwrap_or_default(),
                app_id: app_id.clone().or_else(|| class.clone()),
                pid: tree["pid"].as_i64().map(|p| p as i32),
                title: name.clone(),
                workspace: None,
                focused: tree["focused"].as_bool().unwrap_or(false),
            });
        }
    }
    if let Some(nodes) = tree["nodes"].as_array() {
        for n in nodes {
            parse_tree_windows(n, out);
        }
    }
    if let Some(fnodes) = tree["floating_nodes"].as_array() {
        for n in fnodes {
            parse_tree_windows(n, out);
        }
    }
}

fn parse_monitors(json: &str) -> Result<Vec<MonitorInfo>, ControlError> {
    let arr: Vec<serde_json::Value> = serde_json::from_str(json)
        .map_err(|e| ControlError::Command(format!("outputs JSON: {e}")))?;
    Ok(arr
        .into_iter()
        .map(|m| MonitorInfo {
            name: m["name"].as_str().unwrap_or("").to_string(),
            width: m["rect"]["width"].as_i64().unwrap_or(0) as u32,
            height: m["rect"]["height"].as_i64().unwrap_or(0) as u32,
            scale: m["scale"].as_f64().unwrap_or(1.0),
            x: m["rect"]["x"].as_i64().unwrap_or(0) as i32,
            y: m["rect"]["y"].as_i64().unwrap_or(0) as i32,
            focused: m["focused"].as_bool().unwrap_or(false),
        })
        .collect())
}

#[async_trait]
impl ComputerBackend for SwayBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Sway
    }

    fn capabilities(&self) -> Vec<CapabilityReport> {
        let b = BackendKind::Sway.as_str();
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
        ];
        if self.profile.has_tool("grim") {
            rows.push(CapabilityReport::supported(
                ComputerCapability::ScreenCapture,
                b,
            ));
        } else {
            rows.push(CapabilityReport::unavailable(
                ComputerCapability::ScreenCapture,
                "grim not found on PATH",
                b,
            ));
        }
        if self.profile.has_tool("wl-copy") {
            rows.push(CapabilityReport::supported(
                ComputerCapability::ClipboardSet,
                b,
            ));
            rows.push(CapabilityReport::supported(
                ComputerCapability::ClipboardGet,
                b,
            ));
        } else {
            rows.push(CapabilityReport::unavailable(
                ComputerCapability::ClipboardSet,
                "wl-copy not found on PATH",
                b,
            ));
            rows.push(CapabilityReport::unavailable(
                ComputerCapability::ClipboardGet,
                "wl-paste not found on PATH",
                b,
            ));
        }
        if self.profile.has_tool("wtype") {
            rows.push(CapabilityReport::supported(
                ComputerCapability::KeyboardInject,
                b,
            ));
        } else {
            rows.push(CapabilityReport::unavailable(
                ComputerCapability::KeyboardInject,
                "wtype not found on PATH",
                b,
            ));
        }
        if self.profile.has_tool("ydotool") || self.profile.has_tool("dotool") {
            rows.push(CapabilityReport::supported(
                ComputerCapability::PointerInject,
                b,
            ));
        } else {
            rows.push(CapabilityReport::unavailable(
                ComputerCapability::PointerInject,
                "no pointer injection tool on this host",
                b,
            ));
        }
        rows.push(CapabilityReport::new(
            ComputerCapability::AccessibilityTree,
            if self.profile.a11y_bus {
                CapabilityStatus::Partial
            } else {
                CapabilityStatus::Unavailable
            },
            if self.profile.a11y_bus {
                "AT-SPI bus present; tree API not wired"
            } else {
                "AT-SPI bus not present"
            },
            b,
        ));
        rows.push(CapabilityReport::unavailable(
            ComputerCapability::BrowserSurface,
            "owned by openrt-computer browser tools",
            b,
        ));
        rows
    }

    async fn execute(&self, op: ControlOp) -> Result<OperationResult, ControlError> {
        match op {
            ControlOp::ListWindows => {
                let out = self.msg(&["-t", "get_tree", "-r"]).await?;
                let tree: serde_json::Value =
                    serde_json::from_str(&out).map_err(|e| ControlError::Command(e.to_string()))?;
                let mut wins = Vec::new();
                parse_tree_windows(&tree, &mut wins);
                Ok(OperationResult::ok(
                    "list_windows",
                    serde_json::to_value(&wins).unwrap_or_default(),
                ))
            }
            ControlOp::FocusWindow(id) => {
                // swaymsg accepts a quoted criteria + command as one arg.
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
                let path =
                    std::env::temp_dir().join(format!("openrt-p7-sway-{}.png", std::process::id()));
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
                    serde_json::json!({ "path": path_s, "backend": "sway" }),
                ))
            }
            ControlOp::MovePointer(_, _) | ControlOp::ClickPointer => {
                Ok(OperationResult::unavailable(
                    "pointer",
                    "no pointer injection tool registered for sway backend",
                ))
            }
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
                // Share with generic wayland clipboard helper.
                super::wayland::wayland_clipboard(&self.profile, op).await
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
        let tree_out = self.msg(&["-t", "get_tree", "-r"]).await?;
        let tree: serde_json::Value =
            serde_json::from_str(&tree_out).map_err(|e| ControlError::Command(e.to_string()))?;
        let mut windows = Vec::new();
        parse_tree_windows(&tree, &mut windows);
        let outs = self.msg(&["-t", "get_outputs"]).await?;
        let monitors = parse_monitors(&outs)?;
        let active = windows.iter().find(|w| w.focused).cloned();
        Ok(Observation {
            backend: "sway".into(),
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
