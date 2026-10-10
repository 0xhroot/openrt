//! `ComputerBackend` contract and the environment-priority resolver.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::capability::{CapabilityReport, CapabilityStatus};
use crate::profile::{DesktopKind, EnvironmentProfile, SessionType};

/// Identifier of a concrete backend implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    Hyprland,
    Sway,
    I3,
    Bspwm,
    Kde,
    Gnome,
    Wayland,
    X11,
    Windows,
    Macos,
    Sim,
}

impl BackendKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Hyprland => "hyprland",
            Self::Sway => "sway",
            Self::I3 => "i3",
            Self::Bspwm => "bspwm",
            Self::Kde => "kde",
            Self::Gnome => "gnome",
            Self::Wayland => "wayland",
            Self::X11 => "x11",
            Self::Windows => "windows",
            Self::Macos => "macos",
            Self::Sim => "sim",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "hyprland" => Some(Self::Hyprland),
            "sway" => Some(Self::Sway),
            "i3" => Some(Self::I3),
            "bspwm" => Some(Self::Bspwm),
            "kde" => Some(Self::Kde),
            "gnome" => Some(Self::Gnome),
            "wayland" => Some(Self::Wayland),
            "x11" => Some(Self::X11),
            "windows" => Some(Self::Windows),
            "macos" => Some(Self::Macos),
            "sim" => Some(Self::Sim),
            _ => None,
        }
    }
}

/// Stable window identity across backends. Titles are metadata only —
/// never the primary key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowIdentity {
    /// Backend-scoped primary key (Hyprland address, X11 window id, HWND…).
    pub id: String,
    /// Application id / WM_CLASS / process image name when known.
    pub app_id: Option<String>,
    pub pid: Option<i32>,
    /// Human title (metadata).
    pub title: Option<String>,
    pub workspace: Option<String>,
    pub focused: bool,
}

/// Convenience alias used in tool results.
pub type WindowRef = WindowIdentity;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub x: i32,
    pub y: i32,
    pub focused: bool,
}

/// Post-action observation used for observe→verify loops.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub backend: String,
    pub active_window: Option<WindowRef>,
    pub window_count: usize,
    pub monitors: Vec<MonitorInfo>,
    pub cursor: Option<(i32, i32)>,
    pub captured_at_ms: u128,
}

/// Structured result of one control operation (never a bare bool).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationResult {
    pub op: String,
    pub ok: bool,
    pub status: CapabilityStatus,
    pub message: String,
    pub data: serde_json::Value,
}

impl OperationResult {
    pub fn ok(op: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            op: op.into(),
            ok: true,
            status: CapabilityStatus::Supported,
            message: String::new(),
            data,
        }
    }

    pub fn unsupported(op: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            op: op.into(),
            ok: false,
            status: CapabilityStatus::Unsupported,
            message: reason.into(),
            data: serde_json::Value::Null,
        }
    }

    pub fn unavailable(op: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            op: op.into(),
            ok: false,
            status: CapabilityStatus::Unavailable,
            message: reason.into(),
            data: serde_json::Value::Null,
        }
    }

    pub fn permission_required(op: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            op: op.into(),
            ok: false,
            status: CapabilityStatus::PermissionRequired,
            message: reason.into(),
            data: serde_json::Value::Null,
        }
    }

    pub fn partial(
        op: impl Into<String>,
        reason: impl Into<String>,
        data: serde_json::Value,
    ) -> Self {
        Self {
            op: op.into(),
            ok: false,
            status: CapabilityStatus::Partial,
            message: reason.into(),
            data,
        }
    }
}

/// Errors from backend construction / hard failures (not capability rows).
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("backend unavailable: {0}")]
    Unavailable(String),
    #[error("operation unsupported: {0}")]
    Unsupported(String),
    #[error("permission required: {0}")]
    PermissionRequired(String),
    #[error("command failed: {0}")]
    Command(String),
    #[error("invalid arguments: {0}")]
    Invalid(String),
    #[error("verify failed: {0}")]
    Verify(String),
}

/// High-level control verbs the Tool Fabric exposes. Backends implement what
/// they can; the rest return structured `UNSUPPORTED`/`UNAVAILABLE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlOp {
    ListWindows,
    FocusWindow(String),
    CloseWindow(String),
    ListMonitors,
    CaptureScreen,
    MovePointer(i32, i32),
    ClickPointer,
    TypeText(String),
    SendKey(String),
    ClipboardGet,
    ClipboardSet(String),
    LaunchApp(String),
    SwitchWorkspace(String),
    Observe,
}

/// Contract every platform backend must implement. Methods return
/// [`OperationResult`] so partial/unsupported paths stay structured instead
/// of fake successes.
#[async_trait]
pub trait ComputerBackend: Send + Sync {
    fn kind(&self) -> BackendKind;

    /// Idempotent capability probe (cheap; no side effects).
    fn capabilities(&self) -> Vec<CapabilityReport>;

    async fn execute(&self, op: ControlOp) -> Result<OperationResult, ControlError>;

    /// Snapshot for observe→verify loops.
    async fn observe(&self) -> Result<Observation, ControlError> {
        let windows = match self.execute(ControlOp::ListWindows).await {
            Ok(r) if r.ok => serde_json::from_value(r.data).unwrap_or_default(),
            _ => Vec::<WindowRef>::new(),
        };
        let monitors = match self.execute(ControlOp::ListMonitors).await {
            Ok(r) if r.ok => serde_json::from_value(r.data).unwrap_or_default(),
            _ => Vec::<MonitorInfo>::new(),
        };
        let active = windows.iter().find(|w| w.focused).cloned();
        Ok(Observation {
            backend: self.kind().as_str().to_string(),
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

/// Priority layers from the Tool Fabric mission (documented in docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PriorityLayer {
    /// Layer 1: native compositor / platform IPC (hyprctl, swaymsg, …).
    NativeCompositor = 1,
    /// Layer 2: accessibility API (AT-SPI, UIA) — presence-checked only here.
    Accessibility = 2,
    /// Layer 3: portal / desktop protocol (xdg-desktop-portal, D-Bus).
    Portal = 3,
    /// Layer 4: display-server backend (X11 XTest / Wayland protocols).
    DisplayServer = 4,
    /// Layer 5: generic input injection tools (wtype, ydotool, xdotool).
    InputInjection = 5,
    /// Layer 6: configured keyboard-shortcut fallback (knowledge only).
    ConfiguredShortcut = 6,
}

/// Why a candidate was chosen or skipped (goes into the capability matrix).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolverDecision {
    pub backend: String,
    pub layer: u8,
    pub selected: bool,
    pub reason: String,
}

/// Selects a backend from the environment using the six-layer priority.
/// Returns the selected kind plus the full decision log (audit-friendly).
pub struct BackendResolver;

impl BackendResolver {
    pub fn resolve(profile: &EnvironmentProfile) -> (BackendKind, Vec<ResolverDecision>) {
        Self::resolve_with(profile, None)
    }

    /// `force` pins a backend (config override / tests). Still records why
    /// the natural priority would have chosen differently.
    pub fn resolve_with(
        profile: &EnvironmentProfile,
        force: Option<BackendKind>,
    ) -> (BackendKind, Vec<ResolverDecision>) {
        let mut decisions = Vec::new();
        let mut natural: Option<BackendKind> = None;

        // Helper: record a candidate; first usable candidate wins natural selection.
        fn consider(
            decisions: &mut Vec<ResolverDecision>,
            natural: &mut Option<BackendKind>,
            kind: BackendKind,
            layer: PriorityLayer,
            usable: bool,
            reason: &str,
        ) {
            decisions.push(ResolverDecision {
                backend: kind.as_str().to_string(),
                layer: layer as u8,
                selected: false,
                reason: reason.to_string(),
            });
            if usable && natural.is_none() {
                *natural = Some(kind);
            }
        }

        // Layer 1 — native compositor IPC (tool must exist).
        match profile.desktop {
            DesktopKind::Hyprland => {
                consider(
                    &mut decisions,
                    &mut natural,
                    BackendKind::Hyprland,
                    PriorityLayer::NativeCompositor,
                    profile.has_tool("hyprctl"),
                    if profile.has_tool("hyprctl") {
                        "hyprland + hyprctl present"
                    } else {
                        "hyprland detected but hyprctl missing"
                    },
                );
            }
            DesktopKind::Sway => consider(
                &mut decisions,
                &mut natural,
                BackendKind::Sway,
                PriorityLayer::NativeCompositor,
                profile.has_tool("swaymsg"),
                if profile.has_tool("swaymsg") {
                    "sway + swaymsg present"
                } else {
                    "sway detected but swaymsg missing"
                },
            ),
            DesktopKind::I3 => consider(
                &mut decisions,
                &mut natural,
                BackendKind::I3,
                PriorityLayer::NativeCompositor,
                profile.has_tool("i3-msg"),
                if profile.has_tool("i3-msg") {
                    "i3 + i3-msg present"
                } else {
                    "i3 detected but i3-msg missing"
                },
            ),
            DesktopKind::Bspwm => consider(
                &mut decisions,
                &mut natural,
                BackendKind::Bspwm,
                PriorityLayer::NativeCompositor,
                profile.has_tool("bspc"),
                if profile.has_tool("bspc") {
                    "bspwm + bspc present"
                } else {
                    "bspwm detected but bspc missing"
                },
            ),
            DesktopKind::Kde => consider(
                &mut decisions,
                &mut natural,
                BackendKind::Kde,
                PriorityLayer::NativeCompositor,
                profile.has_tool("kdotool") || profile.has_tool("gdbus"),
                "kde: kdotool/gdbus presence determines depth",
            ),
            DesktopKind::Gnome => consider(
                &mut decisions,
                &mut natural,
                BackendKind::Gnome,
                PriorityLayer::NativeCompositor,
                profile.has_tool("gdbus") || profile.has_tool("gsettings"),
                "gnome: gdbus/gsettings presence determines depth",
            ),
            DesktopKind::Windows => consider(
                &mut decisions,
                &mut natural,
                BackendKind::Windows,
                PriorityLayer::NativeCompositor,
                cfg!(windows),
                if cfg!(windows) {
                    "windows host"
                } else {
                    "windows backend not usable on this OS"
                },
            ),
            DesktopKind::Macos => consider(
                &mut decisions,
                &mut natural,
                BackendKind::Macos,
                PriorityLayer::NativeCompositor,
                cfg!(target_os = "macos"),
                "macos backend gated on target_os",
            ),
            DesktopKind::Xfce | DesktopKind::Unknown => {
                decisions.push(ResolverDecision {
                    backend: "none".into(),
                    layer: PriorityLayer::NativeCompositor as u8,
                    selected: false,
                    reason: "no native compositor adapter for this desktop".into(),
                });
            }
        }

        // Layer 2 — accessibility (presence only; tree API not auto-selected
        // as a control backend without a concrete a11y controller).
        decisions.push(ResolverDecision {
            backend: "a11y".into(),
            layer: PriorityLayer::Accessibility as u8,
            selected: false,
            reason: if profile.a11y_bus {
                "a11y bus present; no a11y control backend registered (tree API later)"
            } else {
                "a11y bus not present"
            }
            .into(),
        });

        // Layer 3 — portal (presence only for now).
        decisions.push(ResolverDecision {
            backend: "portal".into(),
            layer: PriorityLayer::Portal as u8,
            selected: false,
            reason: "portal backend not registered in this pass".into(),
        });

        // Layer 4 — display server fallback.
        if natural.is_none() {
            match profile.session_type {
                SessionType::Wayland => consider(
                    &mut decisions,
                    &mut natural,
                    BackendKind::Wayland,
                    PriorityLayer::DisplayServer,
                    profile.has_tool("grim") || profile.has_tool("wtype"),
                    "generic wayland tools (grim/wtype) as display-server fallback",
                ),
                SessionType::X11 => consider(
                    &mut decisions,
                    &mut natural,
                    BackendKind::X11,
                    PriorityLayer::DisplayServer,
                    profile.has_tool("xdotool") || profile.has_tool("import"),
                    if profile.has_tool("xdotool") {
                        "x11 + xdotool present"
                    } else {
                        "x11 session but xdotool missing"
                    },
                ),
                _ => decisions.push(ResolverDecision {
                    backend: "display".into(),
                    layer: PriorityLayer::DisplayServer as u8,
                    selected: false,
                    reason: "no display-server fallback for this session".into(),
                }),
            }
        }

        // Layers 5–6 — never auto-selected as *the* backend (input injection
        // augments a selected backend; shortcuts are knowledge only).
        decisions.push(ResolverDecision {
            backend: "input_injection".into(),
            layer: PriorityLayer::InputInjection as u8,
            selected: false,
            reason: "input injection is an augment, not a primary backend".into(),
        });
        decisions.push(ResolverDecision {
            backend: "configured_shortcut".into(),
            layer: PriorityLayer::ConfiguredShortcut as u8,
            selected: false,
            reason: "keybindings are environment knowledge / last-resort fallback only".into(),
        });

        // Sim never auto-selects on a real host — only via force.
        if force == Some(BackendKind::Sim) {
            decisions.push(ResolverDecision {
                backend: "sim".into(),
                layer: 0,
                selected: true,
                reason: "forced sim backend".into(),
            });
            return (BackendKind::Sim, decisions);
        }

        let mut selected = force.or(natural).unwrap_or(BackendKind::Sim);
        if force.is_some() {
            selected = force.unwrap();
        }
        for d in decisions.iter_mut() {
            if d.backend == selected.as_str() && !d.selected {
                d.selected = true;
                d.reason = format!("{} (selected)", d.reason);
            }
        }
        if force.is_none() && natural.is_none() {
            // Honest sim fallback only when nothing real is available.
            decisions.push(ResolverDecision {
                backend: "sim".into(),
                layer: 0,
                selected: true,
                reason: "no real backend available; sim harness".into(),
            });
        }
        (selected, decisions)
    }

    /// Construct the concrete backend for a resolved kind.
    pub async fn build(
        kind: BackendKind,
        profile: &EnvironmentProfile,
    ) -> Arc<dyn ComputerBackend> {
        match kind {
            BackendKind::Hyprland => Arc::new(crate::backends::hyprland::HyprlandBackend::new(
                profile.clone(),
            )),
            BackendKind::Sway => Arc::new(crate::backends::sway::SwayBackend::new(profile.clone())),
            BackendKind::I3 => Arc::new(crate::backends::i3::I3Backend::new(profile.clone())),
            BackendKind::Bspwm => {
                Arc::new(crate::backends::bspwm::BspwmBackend::new(profile.clone()))
            }
            BackendKind::Kde => Arc::new(crate::backends::kde::KdeBackend::new(profile.clone())),
            BackendKind::Gnome => {
                Arc::new(crate::backends::gnome::GnomeBackend::new(profile.clone()))
            }
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::sim::SimBackend;
    use crate::profile::{DiscoveryInputs, SessionType};

    fn profile(tools: &[&str], desktop: DesktopKind) -> EnvironmentProfile {
        EnvironmentProfile::discover(&DiscoveryInputs {
            session_type_override: Some(SessionType::Wayland),
            desktop_override: Some(desktop),
            tools_probe: tools.iter().map(|s| s.to_string()).collect(),
            path_dirs: vec![std::path::PathBuf::from("/usr/bin")],
            config_home: Some(std::path::PathBuf::from("/nonexistent-openrt")),
            a11y_bus_present: Some(false),
            ..Default::default()
        })
    }

    #[test]
    fn hyprland_hyprctl_wins_layer1() {
        let p = profile(&["hyprctl", "grim", "wl-copy"], DesktopKind::Hyprland);
        let (kind, log) = BackendResolver::resolve(&p);
        assert_eq!(kind, BackendKind::Hyprland);
        assert!(log.iter().any(|d| d.selected && d.backend == "hyprland"));
    }

    #[test]
    fn hyprland_without_hyprctl_falls_through_to_wayland() {
        let p = profile(&["grim", "wtype"], DesktopKind::Hyprland);
        let (kind, _) = BackendResolver::resolve(&p);
        assert_eq!(kind, BackendKind::Wayland);
    }

    #[test]
    fn no_real_backend_yields_sim() {
        let p = profile(&[], DesktopKind::Unknown);
        let (kind, log) = BackendResolver::resolve(&p);
        assert_eq!(kind, BackendKind::Sim);
        assert!(log.iter().any(|d| d.backend == "sim" && d.selected));
    }

    #[test]
    fn force_sim_short_circuits() {
        let p = profile(&["hyprctl"], DesktopKind::Hyprland);
        let (kind, _) = BackendResolver::resolve_with(&p, Some(BackendKind::Sim));
        assert_eq!(kind, BackendKind::Sim);
    }

    #[tokio::test]
    async fn sim_backend_contract() {
        let b = SimBackend::new();
        let r = b.execute(ControlOp::ListWindows).await.unwrap();
        assert!(r.ok);
        let obs = b.observe().await.unwrap();
        assert_eq!(obs.backend, "sim");
        let ptr = b.execute(ControlOp::MovePointer(1, 2)).await.unwrap();
        assert!(ptr.ok, "{ptr:?}");
    }
}
