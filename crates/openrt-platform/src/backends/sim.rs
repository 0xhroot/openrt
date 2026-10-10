//! Deterministic in-memory backend for tests and headless hosts.
//! Never pretends to be a real compositor: capability rows say `sim`.

use async_trait::async_trait;
use std::sync::Mutex;

use crate::backend::{
    BackendKind, ComputerBackend, ControlError, ControlOp, MonitorInfo, Observation,
    OperationResult, WindowRef,
};
use crate::capability::{CapabilityReport, CapabilityStatus, ComputerCapability};

#[derive(Debug)]
struct SimState {
    windows: Vec<WindowRef>,
    monitors: Vec<MonitorInfo>,
    clipboard: String,
    focused: Option<String>,
    cursor: (i32, i32),
    typed: Vec<String>,
    launched: Vec<String>,
}

pub struct SimBackend {
    kind: BackendKind,
    state: Mutex<SimState>,
}

impl Default for SimBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SimBackend {
    pub fn new() -> Self {
        Self::new_with_kind(BackendKind::Sim)
    }

    pub fn new_with_kind(kind: BackendKind) -> Self {
        let win = WindowRef {
            id: "sim-1".into(),
            app_id: Some("sim.app".into()),
            pid: Some(1),
            title: Some("Sim Window".into()),
            workspace: Some("1".into()),
            focused: true,
        };
        Self {
            kind,
            state: Mutex::new(SimState {
                windows: vec![win],
                monitors: vec![MonitorInfo {
                    name: "SIM-1".into(),
                    width: 1280,
                    height: 720,
                    scale: 1.0,
                    x: 0,
                    y: 0,
                    focused: true,
                }],
                clipboard: String::new(),
                focused: Some("sim-1".into()),
                cursor: (0, 0),
                typed: Vec::new(),
                launched: Vec::new(),
            }),
        }
    }

    /// Test hook: typed text log.
    pub fn typed(&self) -> Vec<String> {
        self.state.lock().unwrap().typed.clone()
    }

    pub fn launched(&self) -> Vec<String> {
        self.state.lock().unwrap().launched.clone()
    }
}

#[async_trait]
impl ComputerBackend for SimBackend {
    fn kind(&self) -> BackendKind {
        self.kind
    }

    fn capabilities(&self) -> Vec<CapabilityReport> {
        let b = self.kind.as_str();
        let all = [
            ComputerCapability::EnvDiscover,
            ComputerCapability::WindowList,
            ComputerCapability::WindowFocus,
            ComputerCapability::WindowClose,
            ComputerCapability::WorkspaceSwitch,
            ComputerCapability::MonitorList,
            ComputerCapability::ScreenCapture,
            ComputerCapability::PointerInject,
            ComputerCapability::KeyboardInject,
            ComputerCapability::ClipboardGet,
            ComputerCapability::ClipboardSet,
            ComputerCapability::ApplicationLaunch,
        ];
        let mut rows: Vec<CapabilityReport> = all
            .into_iter()
            .map(|c| {
                CapabilityReport::new(
                    c,
                    CapabilityStatus::Supported,
                    "sim harness (not a real compositor)",
                    b,
                )
            })
            .collect();
        rows.push(CapabilityReport::new(
            ComputerCapability::AccessibilityTree,
            CapabilityStatus::Unsupported,
            "sim has no accessibility tree",
            b,
        ));
        rows.push(CapabilityReport::new(
            ComputerCapability::BrowserSurface,
            CapabilityStatus::Unsupported,
            "sim has no browser surface",
            b,
        ));
        rows
    }

    async fn execute(&self, op: ControlOp) -> Result<OperationResult, ControlError> {
        let mut st = self.state.lock().unwrap();
        match op {
            ControlOp::ListWindows => Ok(OperationResult::ok(
                "list_windows",
                serde_json::to_value(&st.windows).unwrap_or_default(),
            )),
            ControlOp::FocusWindow(id) => {
                let exists = st.windows.iter().any(|w| w.id == id);
                if exists {
                    for w in st.windows.iter_mut() {
                        w.focused = w.id == id;
                    }
                    st.focused = Some(id.clone());
                    Ok(OperationResult::ok(
                        "focus_window",
                        serde_json::json!({ "focused": st.focused }),
                    ))
                } else {
                    Err(ControlError::Invalid(format!("unknown window {id}")))
                }
            }
            ControlOp::CloseWindow(id) => {
                let before = st.windows.len();
                st.windows.retain(|w| w.id != id);
                if st.windows.len() == before {
                    Err(ControlError::Invalid(format!("unknown window {id}")))
                } else {
                    Ok(OperationResult::ok(
                        "close_window",
                        serde_json::json!({ "closed": id, "remaining": st.windows.len() }),
                    ))
                }
            }
            ControlOp::ListMonitors => Ok(OperationResult::ok(
                "list_monitors",
                serde_json::to_value(&st.monitors).unwrap_or_default(),
            )),
            ControlOp::CaptureScreen => Ok(OperationResult::ok(
                "capture_screen",
                serde_json::json!({
                    "width": st.monitors.first().map(|m| m.width).unwrap_or(0),
                    "height": st.monitors.first().map(|m| m.height).unwrap_or(0),
                    "backend": self.kind.as_str(),
                    "simulated": true,
                }),
            )),
            ControlOp::MovePointer(x, y) => {
                st.cursor = (x, y);
                Ok(OperationResult::ok(
                    "move_pointer",
                    serde_json::json!({ "x": x, "y": y }),
                ))
            }
            ControlOp::ClickPointer => Ok(OperationResult::ok(
                "click_pointer",
                serde_json::json!({ "x": st.cursor.0, "y": st.cursor.1 }),
            )),
            ControlOp::TypeText(t) => {
                st.typed.push(t.clone());
                Ok(OperationResult::ok(
                    "type_text",
                    serde_json::json!({ "length": t.len() }),
                ))
            }
            ControlOp::SendKey(k) => Ok(OperationResult::ok(
                "send_key",
                serde_json::json!({ "key": k }),
            )),
            ControlOp::ClipboardGet => Ok(OperationResult::ok(
                "clipboard_get",
                serde_json::json!({ "text": st.clipboard }),
            )),
            ControlOp::ClipboardSet(t) => {
                st.clipboard = t;
                Ok(OperationResult::ok(
                    "clipboard_set",
                    serde_json::json!({ "ok": true }),
                ))
            }
            ControlOp::LaunchApp(app) => {
                st.launched.push(app.clone());
                Ok(OperationResult::ok(
                    "launch_app",
                    serde_json::json!({ "app": app }),
                ))
            }
            ControlOp::SwitchWorkspace(ws) => Ok(OperationResult::ok(
                "switch_workspace",
                serde_json::json!({ "workspace": ws }),
            )),
            ControlOp::Observe => Ok(OperationResult::ok(
                "observe",
                serde_json::json!({
                    "backend": self.kind.as_str(),
                    "window_count": st.windows.len(),
                }),
            )),
        }
    }

    async fn observe(&self) -> Result<Observation, ControlError> {
        let st = self.state.lock().unwrap();
        Ok(Observation {
            backend: self.kind.as_str().to_string(),
            active_window: st.windows.iter().find(|w| w.focused).cloned(),
            window_count: st.windows.len(),
            monitors: st.monitors.clone(),
            cursor: Some(st.cursor),
            captured_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
        })
    }
}
