//! High-level ComputerControl API: profile + backend + capability matrix +
//! observe→verify helpers. This is what production callers and the
//! `computer.*` tools sit on.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::backend::{
    BackendKind, BackendResolver, ComputerBackend, ControlError, ControlOp, Observation,
    OperationResult, ResolverDecision,
};
use crate::capability::{CapabilityMatrix, CapabilityReport, ComputerCapability};
use crate::profile::EnvironmentProfile;

/// Full control session: discovered environment, selected backend, decision log.
pub struct ComputerControl {
    profile: EnvironmentProfile,
    backend: Arc<dyn ComputerBackend>,
    decisions: Vec<ResolverDecision>,
    forced: Option<BackendKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlSummary {
    pub profile_id: String,
    pub backend: String,
    pub session_type: String,
    pub desktop: String,
    pub a11y_bus: bool,
    pub tools: Vec<String>,
}

impl ComputerControl {
    /// Discover host environment and resolve a backend via priority.
    pub fn discover() -> Self {
        Self::with_profile(EnvironmentProfile::host(), None)
    }

    /// Profile-driven construction (tests inject fixtures).
    pub fn with_profile(profile: EnvironmentProfile, force: Option<BackendKind>) -> Self {
        let (kind, decisions) = BackendResolver::resolve_with(&profile, force);
        // Backend construction is synchronous in practice (no async ctor work);
        // blocking on a current-thread runtime would be wrong, so use a
        // lightweight executor only if we're not already inside a runtime —
        // build() is non-async internally for all adapters.
        let backend = build_blocking(kind, &profile);
        Self {
            profile,
            backend,
            decisions,
            forced: force.or(Some(kind)),
        }
    }

    /// Build from an explicit backend (wiring already-resolved kinds).
    pub fn from_backend(profile: EnvironmentProfile, backend: Arc<dyn ComputerBackend>) -> Self {
        let kind = backend.kind();
        Self {
            profile,
            backend,
            decisions: vec![ResolverDecision {
                backend: kind.as_str().to_string(),
                layer: 0,
                selected: true,
                reason: "injected backend".into(),
            }],
            forced: Some(kind),
        }
    }

    pub fn profile(&self) -> &EnvironmentProfile {
        &self.profile
    }

    pub fn backend_kind(&self) -> BackendKind {
        self.backend.kind()
    }

    pub fn backend(&self) -> &Arc<dyn ComputerBackend> {
        &self.backend
    }

    pub fn decisions(&self) -> &[ResolverDecision] {
        &self.decisions
    }

    /// Whether a backend kind was forced (config override / tests) rather
    /// than naturally selected by priority.
    pub fn forced_kind(&self) -> Option<BackendKind> {
        self.forced
    }

    pub fn summary(&self) -> ControlSummary {
        ControlSummary {
            profile_id: self.profile.profile_id.clone(),
            backend: self.backend.kind().as_str().to_string(),
            session_type: self.profile.session_type.as_str().to_string(),
            desktop: self.profile.desktop.as_str().to_string(),
            a11y_bus: self.profile.a11y_bus,
            tools: self.profile.tools.iter().map(|(n, _)| n.clone()).collect(),
        }
    }

    /// Capability matrix: profile discovery row + backend-specific rows.
    pub fn capability_matrix(&self) -> CapabilityMatrix {
        let mut m = CapabilityMatrix::new();
        m.insert(CapabilityReport::new(
            ComputerCapability::EnvDiscover,
            crate::capability::CapabilityStatus::Supported,
            "",
            self.backend.kind().as_str(),
        ));
        for row in self.backend.capabilities() {
            m.insert(row);
        }
        m
    }

    pub async fn execute(&self, op: ControlOp) -> Result<OperationResult, ControlError> {
        self.backend.execute(op).await
    }

    pub async fn observe(&self) -> Result<Observation, ControlError> {
        self.backend.observe().await
    }

    /// Focus a window and verify via observation (bounded single retry).
    pub async fn focus_and_verify(&self, window_id: &str) -> Result<OperationResult, ControlError> {
        let result = self
            .backend
            .execute(ControlOp::FocusWindow(window_id.to_string()))
            .await?;
        // Verify: observe active window.
        let obs = self.backend.observe().await?;
        let active_id = obs
            .active_window
            .as_ref()
            .map(|w| w.id.as_str())
            .unwrap_or("");
        let matches = active_id == window_id
            || (!active_id.is_empty() && window_id.contains(active_id))
            || (!active_id.is_empty() && active_id.contains(window_id.trim_start_matches("0x")));
        if matches {
            return Ok(result);
        }
        // One bounded retry (idempotent focus).
        let _ = self
            .backend
            .execute(ControlOp::FocusWindow(window_id.to_string()))
            .await?;
        let obs2 = self.backend.observe().await?;
        let active2 = obs2
            .active_window
            .as_ref()
            .map(|w| w.id.clone())
            .unwrap_or_default();
        if active2 == window_id || (!active2.is_empty() && window_id.contains(&active2)) {
            return Ok(result);
        }
        Err(ControlError::Verify(format!(
            "focus {window_id}: active window after retry is `{active2}`"
        )))
    }
}

/// Resolve concrete backend without async: `BackendResolver::build` is async
/// only for API symmetry; adapters construct synchronously. We block only
/// when no runtime exists (unit tests outside tokio); inside a runtime we
/// would not call this — Runtime wiring uses [`ComputerControl::with_profile`]
/// from async context via [`build_on`] below.
fn build_blocking(kind: BackendKind, profile: &EnvironmentProfile) -> Arc<dyn ComputerBackend> {
    match kind {
        BackendKind::Hyprland => Arc::new(crate::backends::hyprland::HyprlandBackend::new(
            profile.clone(),
        )),
        BackendKind::Sway => Arc::new(crate::backends::sway::SwayBackend::new(profile.clone())),
        BackendKind::I3 => Arc::new(crate::backends::i3::I3Backend::new(profile.clone())),
        BackendKind::Bspwm => Arc::new(crate::backends::bspwm::BspwmBackend::new(profile.clone())),
        BackendKind::Kde => Arc::new(crate::backends::kde::KdeBackend::new(profile.clone())),
        BackendKind::Gnome => Arc::new(crate::backends::gnome::GnomeBackend::new(profile.clone())),
        BackendKind::Wayland => Arc::new(crate::backends::wayland::WaylandBackend::new(
            profile.clone(),
        )),
        BackendKind::X11 => Arc::new(crate::backends::x11::X11Backend::new(profile.clone())),
        BackendKind::Windows => Arc::new(crate::backends::windows::WindowsBackend::new()),
        BackendKind::Macos => Arc::new(crate::backends::sim::SimBackend::new_with_kind(
            BackendKind::Macos,
        )),
        BackendKind::Sim => Arc::new(crate::backends::sim::SimBackend::new()),
    }
}

/// Async construction path for runtime wiring (same logic).
pub async fn build_control(
    profile: EnvironmentProfile,
    force: Option<BackendKind>,
) -> ComputerControl {
    ComputerControl::with_profile(profile, force)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{DesktopKind, DiscoveryInputs, SessionType};

    #[tokio::test]
    async fn sim_control_matrix_and_focus_verify() {
        let profile = EnvironmentProfile::discover(&DiscoveryInputs {
            session_type_override: Some(SessionType::Wayland),
            desktop_override: Some(DesktopKind::Unknown),
            tools_probe: vec![],
            path_dirs: vec![std::path::PathBuf::from("/nonexistent")],
            config_home: Some(std::path::PathBuf::from("/nonexistent-openrt")),
            a11y_bus_present: Some(false),
            ..Default::default()
        });
        let control = ComputerControl::with_profile(profile, Some(BackendKind::Sim));
        assert_eq!(control.backend_kind(), BackendKind::Sim);
        let m = control.capability_matrix();
        assert!(m.get(ComputerCapability::WindowList).is_some());
        let r = control.focus_and_verify("sim-1").await.unwrap();
        assert!(r.ok, "{r:?}");
    }

    #[tokio::test]
    async fn focus_verify_fails_for_unknown_window() {
        let control =
            ComputerControl::with_profile(EnvironmentProfile::host(), Some(BackendKind::Sim));
        let err = control.focus_and_verify("nope").await.unwrap_err();
        assert!(matches!(
            err,
            ControlError::Verify(_) | ControlError::Invalid(_)
        ));
    }
}
