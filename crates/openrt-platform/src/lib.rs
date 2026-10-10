//! Phase 7 Tool Fabric: environment discovery, capability negotiation, and
//! the `ComputerBackend` abstraction for cross-platform computer control.
//!
//! This crate owns the platform layer under the existing Phase 2 tools: it
//! discovers *where* we are (session type, compositor, desktop, available
//! tools, config knowledge), reports an honest capability matrix, and exposes
//! a high-level [`ComputerControl`] API that resolves a real backend through
//! [`BackendResolver`] priority (native compositor → accessibility → portal
//! → display server → input injection → configured-shortcut fallback).
//!
//! Keybindings and WM layouts are *environment knowledge* (scanners under
//! [`config_scan`]), never core primitives: prefer `open_application("terminal")`
//! over `press_key("SUPER+Q")`. Wayland is first-class; X11 is never a silent
//! Wayland fallback. No backend is fabricated — unsupported operations return
//! structured `UNSUPPORTED`/`UNAVAILABLE`/`PERMISSION_REQUIRED` diagnostics.

pub mod apps;
pub mod backend;
pub mod backends;
pub mod capability;
pub mod config_scan;
pub mod control;
pub mod profile;
pub mod tools;

pub use backend::{
    BackendKind, BackendResolver, ComputerBackend, ControlError, ControlOp, MonitorInfo,
    Observation, OperationResult, WindowIdentity, WindowRef,
};
pub use capability::{CapabilityReport, CapabilityStatus, ComputerCapability};
pub use control::ComputerControl;
pub use profile::{DesktopKind, DiscoveryInputs, EnvironmentProfile, SessionType};
pub use tools::computer_tools;
