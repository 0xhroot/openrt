//! Capability negotiation for computer-control operations.
//!
//! Every environment reports an honest matrix: never claim "works everywhere".
//! Statuses map to the structured diagnostics the Tool Fabric contract
//! requires (`SUPPORTED` / `UNSUPPORTED` / `PERMISSION_REQUIRED` / `UNAVAILABLE`).

use serde::{Deserialize, Serialize};

/// Structured support status for one computer-control capability in the
/// current environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CapabilityStatus {
    /// The capability is implemented and usable right now.
    Supported,
    /// Partially usable (e.g. list works, focus is best-effort).
    Partial,
    /// Mechanism exists but needs an OS/user permission we do not hold.
    PermissionRequired,
    /// Mechanism is not available on this host/session.
    Unavailable,
    /// Backend never implements this operation (honest refusal, not a crash).
    Unsupported,
}

impl CapabilityStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Supported => "SUPPORTED",
            Self::Partial => "PARTIAL",
            Self::PermissionRequired => "PERMISSION_REQUIRED",
            Self::Unavailable => "UNAVAILABLE",
            Self::Unsupported => "UNSUPPORTED",
        }
    }
}

/// Coarse capability domains the Tool Fabric exposes. These are *platform*
/// capabilities (what the OS/compositor can do), distinct from gate
/// capabilities (`computer.window.all`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerCapability {
    EnvDiscover,
    WindowList,
    WindowFocus,
    WindowClose,
    WorkspaceSwitch,
    MonitorList,
    ScreenCapture,
    PointerInject,
    KeyboardInject,
    ClipboardGet,
    ClipboardSet,
    ApplicationLaunch,
    AccessibilityTree,
    BrowserSurface,
}

impl ComputerCapability {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::EnvDiscover => "env_discover",
            Self::WindowList => "window_list",
            Self::WindowFocus => "window_focus",
            Self::WindowClose => "window_close",
            Self::WorkspaceSwitch => "workspace_switch",
            Self::MonitorList => "monitor_list",
            Self::ScreenCapture => "screen_capture",
            Self::PointerInject => "pointer_inject",
            Self::KeyboardInject => "keyboard_inject",
            Self::ClipboardGet => "clipboard_get",
            Self::ClipboardSet => "clipboard_set",
            Self::ApplicationLaunch => "application_launch",
            Self::AccessibilityTree => "accessibility_tree",
            Self::BrowserSurface => "browser_surface",
        }
    }
}

/// One row of the capability matrix: capability + status + machine-readable
/// reason (always populated for non-Supported statuses).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityReport {
    pub capability: String,
    pub status: CapabilityStatus,
    /// Short reason: tool missing, permission denied, display absent, etc.
    pub reason: String,
    /// Backend id that would serve this capability (empty when none).
    pub backend: String,
}

impl CapabilityReport {
    pub fn new(
        capability: ComputerCapability,
        status: CapabilityStatus,
        reason: impl Into<String>,
        backend: impl Into<String>,
    ) -> Self {
        Self {
            capability: capability.as_str().to_string(),
            status,
            reason: reason.into(),
            backend: backend.into(),
        }
    }

    pub fn supported(capability: ComputerCapability, backend: impl Into<String>) -> Self {
        Self::new(capability, CapabilityStatus::Supported, "", backend)
    }

    pub fn unsupported(
        capability: ComputerCapability,
        reason: impl Into<String>,
        backend: impl Into<String>,
    ) -> Self {
        Self::new(capability, CapabilityStatus::Unsupported, reason, backend)
    }

    pub fn unavailable(
        capability: ComputerCapability,
        reason: impl Into<String>,
        backend: impl Into<String>,
    ) -> Self {
        Self::new(capability, CapabilityStatus::Unavailable, reason, backend)
    }

    pub fn permission_required(
        capability: ComputerCapability,
        reason: impl Into<String>,
        backend: impl Into<String>,
    ) -> Self {
        Self::new(
            capability,
            CapabilityStatus::PermissionRequired,
            reason,
            backend,
        )
    }

    pub fn is_usable(&self) -> bool {
        matches!(
            self.status,
            CapabilityStatus::Supported | CapabilityStatus::Partial
        )
    }
}

/// Builder for a full matrix from a set of reports (dedupes by capability,
/// last write wins — callers compose backend-specific rows first, then
/// environment overlays).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CapabilityMatrix {
    rows: Vec<CapabilityReport>,
}

impl CapabilityMatrix {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, report: CapabilityReport) {
        if let Some(existing) = self
            .rows
            .iter_mut()
            .find(|r| r.capability == report.capability)
        {
            *existing = report;
        } else {
            self.rows.push(report);
        }
    }

    pub fn get(&self, capability: ComputerCapability) -> Option<&CapabilityReport> {
        self.rows
            .iter()
            .find(|r| r.capability == capability.as_str())
    }

    pub fn rows(&self) -> &[CapabilityReport] {
        &self.rows
    }

    pub fn into_value(self) -> serde_json::Value {
        serde_json::json!({ "capabilities": self.rows })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_strings_match_contract() {
        assert_eq!(CapabilityStatus::Supported.as_str(), "SUPPORTED");
        assert_eq!(CapabilityStatus::Unsupported.as_str(), "UNSUPPORTED");
        assert_eq!(
            CapabilityStatus::PermissionRequired.as_str(),
            "PERMISSION_REQUIRED"
        );
        assert_eq!(CapabilityStatus::Unavailable.as_str(), "UNAVAILABLE");
    }

    #[test]
    fn matrix_dedupes_and_reports_reasons() {
        let mut m = CapabilityMatrix::new();
        m.insert(CapabilityReport::supported(
            ComputerCapability::WindowList,
            "hyprland",
        ));
        m.insert(CapabilityReport::unavailable(
            ComputerCapability::PointerInject,
            "no ydotool/dotool on PATH",
            "hyprland",
        ));
        m.insert(CapabilityReport::unsupported(
            ComputerCapability::PointerInject,
            "compositor exposes no pointer warp",
            "hyprland",
        ));
        assert_eq!(m.rows().len(), 2);
        let row = m.get(ComputerCapability::PointerInject).unwrap();
        assert_eq!(row.status, CapabilityStatus::Unsupported);
        assert!(!row.reason.is_empty());
    }
}
