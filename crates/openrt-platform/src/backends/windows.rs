//! Windows backend surface.
//!
//! Workspace-wide `unsafe_code = "forbid"` rules out raw Win32 FFI here. On
//! non-Windows hosts every operation returns structured `UNAVAILABLE`. On a
//! Windows host we only claim what we can honestly provide without unsafe
//! (process spawn of built-in tools) — **not GREEN** on this development host.

use async_trait::async_trait;

use crate::backend::{
    BackendKind, ComputerBackend, ControlError, ControlOp, MonitorInfo, Observation,
    OperationResult, WindowRef,
};
use crate::capability::{CapabilityReport, CapabilityStatus, ComputerCapability};

pub struct WindowsBackend;

impl WindowsBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Default for WindowsBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ComputerBackend for WindowsBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Windows
    }

    fn capabilities(&self) -> Vec<CapabilityReport> {
        let b = BackendKind::Windows.as_str();
        if !cfg!(windows) {
            // Honest: this host is not Windows.
            return vec![
                CapabilityReport::new(
                    ComputerCapability::EnvDiscover,
                    CapabilityStatus::Supported,
                    "",
                    b,
                ),
                CapabilityReport::unavailable(
                    ComputerCapability::WindowList,
                    "windows backend not usable on this OS",
                    b,
                ),
                CapabilityReport::unavailable(
                    ComputerCapability::WindowFocus,
                    "windows backend not usable on this OS",
                    b,
                ),
                CapabilityReport::unavailable(
                    ComputerCapability::WindowClose,
                    "windows backend not usable on this OS",
                    b,
                ),
                CapabilityReport::unavailable(
                    ComputerCapability::WorkspaceSwitch,
                    "windows backend not usable on this OS",
                    b,
                ),
                CapabilityReport::unavailable(
                    ComputerCapability::MonitorList,
                    "windows backend not usable on this OS",
                    b,
                ),
                CapabilityReport::unavailable(
                    ComputerCapability::ScreenCapture,
                    "windows backend not usable on this OS",
                    b,
                ),
                CapabilityReport::unavailable(
                    ComputerCapability::PointerInject,
                    "windows backend not usable on this OS",
                    b,
                ),
                CapabilityReport::unavailable(
                    ComputerCapability::KeyboardInject,
                    "windows backend not usable on this OS",
                    b,
                ),
                CapabilityReport::unavailable(
                    ComputerCapability::ClipboardGet,
                    "windows backend not usable on this OS",
                    b,
                ),
                CapabilityReport::unavailable(
                    ComputerCapability::ClipboardSet,
                    "windows backend not usable on this OS",
                    b,
                ),
                CapabilityReport::unavailable(
                    ComputerCapability::ApplicationLaunch,
                    "windows backend not usable on this OS",
                    b,
                ),
                CapabilityReport::unavailable(
                    ComputerCapability::AccessibilityTree,
                    "UIA requires unsafe FFI; workspace forbids unsafe_code",
                    b,
                ),
                CapabilityReport::unavailable(
                    ComputerCapability::BrowserSurface,
                    "owned by openrt-computer browser tools",
                    b,
                ),
            ];
        }
        // On Windows without unsafe FFI: only process-based launch is honest.
        vec![
            CapabilityReport::new(
                ComputerCapability::EnvDiscover,
                CapabilityStatus::Supported,
                "",
                b,
            ),
            CapabilityReport::new(
                ComputerCapability::ApplicationLaunch,
                CapabilityStatus::Partial,
                "powershell Start-Process only; no UIA tree",
                b,
            ),
            CapabilityReport::unsupported(
                ComputerCapability::WindowList,
                "Win32 EnumWindows requires unsafe FFI; workspace forbids unsafe_code",
                b,
            ),
            CapabilityReport::unsupported(
                ComputerCapability::WindowFocus,
                "Win32 SetForegroundWindow requires unsafe FFI; workspace forbids unsafe_code",
                b,
            ),
            CapabilityReport::unsupported(
                ComputerCapability::WindowClose,
                "Win32 PostMessage requires unsafe FFI; workspace forbids unsafe_code",
                b,
            ),
            CapabilityReport::unsupported(
                ComputerCapability::WorkspaceSwitch,
                "no safe shell adapter for virtual-desktop switch",
                b,
            ),
            CapabilityReport::unsupported(
                ComputerCapability::MonitorList,
                "Win32 EnumDisplayMonitors requires unsafe FFI",
                b,
            ),
            CapabilityReport::unsupported(
                ComputerCapability::ScreenCapture,
                "GDI capture requires unsafe FFI; workspace forbids unsafe_code",
                b,
            ),
            CapabilityReport::unsupported(
                ComputerCapability::PointerInject,
                "SendInput requires unsafe FFI; workspace forbids unsafe_code",
                b,
            ),
            CapabilityReport::unsupported(
                ComputerCapability::KeyboardInject,
                "SendInput requires unsafe FFI; workspace forbids unsafe_code",
                b,
            ),
            CapabilityReport::unsupported(
                ComputerCapability::ClipboardGet,
                "OpenClipboard requires unsafe FFI; workspace forbids unsafe_code",
                b,
            ),
            CapabilityReport::unsupported(
                ComputerCapability::ClipboardSet,
                "OpenClipboard requires unsafe FFI; workspace forbids unsafe_code",
                b,
            ),
            CapabilityReport::unsupported(
                ComputerCapability::AccessibilityTree,
                "UIA requires unsafe FFI; workspace forbids unsafe_code",
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
            ControlOp::LaunchApp(app) if cfg!(windows) => {
                if app
                    .chars()
                    .any(|c| matches!(c, '|' | ';' | '&' | '>' | '<' | '`' | '$'))
                {
                    return Ok(OperationResult::unsupported(
                        "launch_app",
                        "shell metacharacters not allowed",
                    ));
                }
                // Safe process spawn only (no FFI).
                let (ok, _, err, status) = super::run_cmd(
                    "powershell",
                    &[
                        "-NoProfile",
                        "-Command",
                        &format!("Start-Process -FilePath '{app}'"),
                    ],
                    10_000,
                )
                .await
                .map_err(ControlError::Command)?;
                if ok {
                    Ok(OperationResult::ok(
                        "launch_app",
                        serde_json::json!({ "app": app, "via": "powershell" }),
                    ))
                } else {
                    Err(ControlError::Command(format!(
                        "powershell Start-Process failed ({status}): {err}"
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
            other => {
                let name = format!("{other:?}");
                if !cfg!(windows) {
                    Ok(OperationResult::unavailable(
                        name,
                        "windows backend not usable on this OS",
                    ))
                } else {
                    Ok(OperationResult::unsupported(
                        name,
                        "operation requires Win32 FFI; workspace forbids unsafe_code",
                    ))
                }
            }
        }
    }

    async fn observe(&self) -> Result<Observation, ControlError> {
        Ok(Observation {
            backend: "windows".into(),
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

/// Silence unused import on non-windows when WindowRef is only in signatures.
#[allow(dead_code)]
fn _touch(_: &WindowRef) {}
