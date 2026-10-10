//! Dynamic environment discovery for the Tool Fabric.
//!
//! Discovery is *observed*, never hard-coded to one user's setup: session
//! type, desktop/compositor, config-dir probes, and tool presence on `PATH`.
//! Config files are read for knowledge only — never modified.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Display-server / session type (Wayland is first-class).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionType {
    Wayland,
    X11,
    Tty,
    Unknown,
}

impl SessionType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Wayland => "wayland",
            Self::X11 => "x11",
            Self::Tty => "tty",
            Self::Unknown => "unknown",
        }
    }
}

/// Known desktop environments / compositors we can target with adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopKind {
    Hyprland,
    Sway,
    I3,
    Bspwm,
    Kde,
    Gnome,
    Xfce,
    Windows,
    Macos,
    Unknown,
}

impl DesktopKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Hyprland => "hyprland",
            Self::Sway => "sway",
            Self::I3 => "i3",
            Self::Bspwm => "bspwm",
            Self::Kde => "kde",
            Self::Gnome => "gnome",
            Self::Xfce => "xfce",
            Self::Windows => "windows",
            Self::Macos => "macos",
            Self::Unknown => "unknown",
        }
    }

    /// Map XDG_CURRENT_DESKTOP / DESKTOP_SESSION / HYPRLAND_INSTANCE_SIGNATURE
    /// style signals to a desktop kind.
    pub fn from_signals(
        xdg_desktop: Option<&str>,
        desktop_session: Option<&str>,
        hyprland_sig: Option<&str>,
    ) -> Self {
        let blob = [
            xdg_desktop.unwrap_or(""),
            desktop_session.unwrap_or(""),
            // Omarchy uses DESKTOP_SESSION=omarchy but is Hyprland-based.
            if hyprland_sig.is_some() {
                "hyprland"
            } else {
                ""
            },
        ]
        .join(":")
        .to_ascii_lowercase();

        if blob.contains("hyprland") || blob.contains("omarchy") {
            Self::Hyprland
        } else if blob.contains("sway") {
            Self::Sway
        } else if blob.contains("i3") {
            Self::I3
        } else if blob.contains("bspwm") {
            Self::Bspwm
        } else if blob.contains("kde") || blob.contains("plasma") {
            Self::Kde
        } else if blob.contains("gnome") {
            Self::Gnome
        } else if blob.contains("xfce") {
            Self::Xfce
        } else {
            Self::Unknown
        }
    }
}

/// Snapshot of the host environment relevant to computer control.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentProfile {
    pub os_family: String,
    pub session_type: SessionType,
    pub desktop: DesktopKind,
    pub display: Option<String>,
    pub wayland_display: Option<String>,
    pub xdg_runtime_dir: Option<String>,
    /// Config directories that exist under `~/.config` (read-only probes).
    pub config_dirs: Vec<String>,
    /// External tools discovered on PATH (name → absolute path).
    pub tools: Vec<(String, String)>,
    /// Whether a11y bus is reachable (presence only; tree API is separate).
    pub a11y_bus: bool,
    pub profile_id: String,
}

impl EnvironmentProfile {
    /// Inputs for [`EnvironmentProfile::discover`] — separated so tests can
    /// inject fixture environments without touching the real `~/.config`.
    pub fn discover(inputs: &DiscoveryInputs) -> Self {
        let os_family = std::env::consts::OS.to_string();
        let session_type = if inputs.session_type_override.is_some() {
            inputs.session_type_override.unwrap()
        } else if std::env::var("WAYLAND_DISPLAY").is_ok()
            || std::env::var("XDG_SESSION_TYPE")
                .map(|v| v == "wayland")
                .unwrap_or(false)
        {
            SessionType::Wayland
        } else if std::env::var("DISPLAY").is_ok()
            || std::env::var("XDG_SESSION_TYPE")
                .map(|v| v == "x11")
                .unwrap_or(false)
        {
            SessionType::X11
        } else if std::env::var("XDG_SESSION_TYPE")
            .map(|v| v == "tty")
            .unwrap_or(false)
        {
            SessionType::Tty
        } else {
            SessionType::Unknown
        };

        let xdg_desktop = inputs.xdg_current_desktop.clone();
        let desktop_session = inputs.desktop_session.clone();
        let hyprland_sig = inputs
            .hyprland_instance_signature
            .clone()
            .or_else(|| std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok());
        let desktop = inputs.desktop_override.unwrap_or_else(|| {
            DesktopKind::from_signals(
                xdg_desktop.as_deref(),
                desktop_session.as_deref(),
                hyprland_sig.as_deref(),
            )
        });

        let display = inputs
            .display
            .clone()
            .or_else(|| std::env::var("DISPLAY").ok().filter(|s| !s.is_empty()));
        let wayland_display = inputs.wayland_display.clone().or_else(|| {
            std::env::var("WAYLAND_DISPLAY")
                .ok()
                .filter(|s| !s.is_empty())
        });

        let config_root = inputs.config_home.clone().or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        });
        let config_dirs = match &config_root {
            Some(root) => inputs
                .config_dirs_probe
                .iter()
                .filter(|name| root.join(name).is_dir())
                .cloned()
                .collect(),
            None => Vec::new(),
        };

        let tools: Vec<(String, String)> = inputs
            .tools_probe
            .iter()
            .filter_map(|t| which_in_path(t, &inputs.path_dirs).map(|p| (t.clone(), p)))
            .collect();

        let a11y_bus = inputs
            .a11y_bus_present
            .or_else(|| probe_a11y_bus())
            .unwrap_or(false);

        let profile_id = format!(
            "{}/{}/{}/{}",
            os_family,
            session_type.as_str(),
            desktop.as_str(),
            if a11y_bus { "a11y" } else { "no-a11y" }
        );

        Self {
            os_family,
            session_type,
            desktop,
            display,
            wayland_display,
            xdg_runtime_dir: inputs
                .xdg_runtime_dir
                .clone()
                .or_else(|| std::env::var("XDG_RUNTIME_DIR").ok()),
            config_dirs,
            tools,
            a11y_bus,
            profile_id,
        }
    }

    /// Default discovery against the live host.
    pub fn host() -> Self {
        Self::discover(&DiscoveryInputs::host())
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.iter().any(|(t, _)| t == name)
    }

    pub fn tool_path(&self, name: &str) -> Option<&str> {
        self.tools
            .iter()
            .find(|(t, _)| t == name)
            .map(|(_, p)| p.as_str())
    }

    /// Machine-readable profile (for the `computer.env` tool and docs).
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}))
    }
}

/// Inputs controlling discovery. All fields optional; `None` means "read from
/// the live process environment / PATH".
#[derive(Debug, Clone, Default)]
pub struct DiscoveryInputs {
    pub session_type_override: Option<SessionType>,
    pub desktop_override: Option<DesktopKind>,
    pub xdg_current_desktop: Option<String>,
    pub desktop_session: Option<String>,
    pub hyprland_instance_signature: Option<String>,
    pub display: Option<String>,
    pub wayland_display: Option<String>,
    pub xdg_runtime_dir: Option<String>,
    pub config_home: Option<PathBuf>,
    /// Subdirectories of config home to probe for existence.
    pub config_dirs_probe: Vec<String>,
    /// Tool names to locate on PATH.
    pub tools_probe: Vec<String>,
    /// PATH directories (empty → use `PATH` env).
    pub path_dirs: Vec<PathBuf>,
    /// Force a11y presence (tests); `None` → live probe.
    pub a11y_bus_present: Option<bool>,
}

impl DiscoveryInputs {
    pub fn host() -> Self {
        Self {
            xdg_current_desktop: std::env::var("XDG_CURRENT_DESKTOP").ok(),
            desktop_session: std::env::var("DESKTOP_SESSION").ok(),
            hyprland_instance_signature: std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok(),
            display: std::env::var("DISPLAY").ok(),
            wayland_display: std::env::var("WAYLAND_DISPLAY").ok(),
            xdg_runtime_dir: std::env::var("XDG_RUNTIME_DIR").ok(),
            config_home: std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config")),
            config_dirs_probe: default_config_probes()
                .iter()
                .map(|s| s.to_string())
                .collect(),
            tools_probe: default_tool_probes()
                .iter()
                .map(|s| s.to_string())
                .collect(),
            path_dirs: Vec::new(),
            a11y_bus_present: None,
            session_type_override: None,
            desktop_override: None,
        }
    }
}

fn default_config_probes() -> &'static [&'static str] {
    &["hypr", "sway", "i3", "bspwm", "sxhkd", "kwinrc", "gnome"]
}

fn default_tool_probes() -> &'static [&'static str] {
    &[
        "hyprctl",
        "swaymsg",
        "i3-msg",
        "bspc",
        "xdotool",
        "wtype",
        "ydotool",
        "grim",
        "slurp",
        "import",
        "wl-copy",
        "wl-paste",
        "xclip",
        "xsel",
        "xdg-open",
        "kdotool",
        "gdbus",
        "dbus-send",
        "wofi",
        "rofi",
        "fuzzel",
    ]
}

fn path_dirs_from_env() -> Vec<PathBuf> {
    std::env::var("PATH")
        .map(|p| {
            std::env::split_paths(&p)
                .filter(|d| !d.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn which_in_path(name: &str, override_dirs: &[PathBuf]) -> Option<String> {
    let dirs = if override_dirs.is_empty() {
        path_dirs_from_env()
    } else {
        override_dirs.to_vec()
    };
    for dir in dirs {
        let candidate = dir.join(name);
        if candidate.is_file() && is_executable(&candidate) {
            return candidate.to_str().map(|s| s.to_string());
        }
    }
    None
}

fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    {
        path.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        true
    }
}

/// Cheap a11y presence probe: AT-SPI bus socket under XDG_RUNTIME_DIR.
fn probe_a11y_bus() -> Option<bool> {
    let runtime = std::env::var("XDG_RUNTIME_DIR").ok()?;
    // Common AT-SPI bus layout: bus addresses vary; check for the a11y bus
    // socket or at-spi2 directory without opening a connection.
    let a11y_dir = std::path::Path::new(&runtime).join("at-spi");
    if a11y_dir.exists() {
        return Some(true);
    }
    // Fallback: many desktops expose the bus only via D-Bus; treat absence of
    // the socket as "unknown / not claimed" rather than inventing status.
    Some(false)
}

/// Distinct tool names from the profile (stable order).
pub fn tool_names(profile: &EnvironmentProfile) -> Vec<String> {
    let set: BTreeSet<&str> = profile.tools.iter().map(|(t, _)| t.as_str()).collect();
    set.into_iter().map(|s| s.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_kind_signals() {
        assert_eq!(
            DesktopKind::from_signals(Some("Hyprland"), None, None),
            DesktopKind::Hyprland
        );
        assert_eq!(
            DesktopKind::from_signals(Some("sway"), None, None),
            DesktopKind::Sway
        );
        assert_eq!(
            DesktopKind::from_signals(Some("GNOME"), None, None),
            DesktopKind::Gnome
        );
        assert_eq!(
            DesktopKind::from_signals(Some("KDE"), None, None),
            DesktopKind::Kde
        );
        // Omarchy: desktop session name is not hyprland but signature is set.
        assert_eq!(
            DesktopKind::from_signals(None, Some("omarchy"), Some("sig")),
            DesktopKind::Hyprland
        );
        assert_eq!(
            DesktopKind::from_signals(None, None, None),
            DesktopKind::Unknown
        );
    }

    #[test]
    fn fixture_discovery_does_not_need_live_env() {
        let tmp = std::env::temp_dir().join(format!(
            "openrt-p7-profile-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(tmp.join("hypr")).unwrap();
        let tool_path = tmp.join("definitely-not-a-real-tool-xyz");
        std::fs::write(&tool_path, b"#!/bin/sh\nexit 0\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tool_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let inputs = DiscoveryInputs {
            session_type_override: Some(SessionType::Wayland),
            desktop_override: Some(DesktopKind::Hyprland),
            xdg_current_desktop: Some("Hyprland".into()),
            display: Some(":0".into()),
            wayland_display: Some("wayland-1".into()),
            config_home: Some(tmp.clone()),
            config_dirs_probe: vec!["hypr".into(), "i3".into()],
            tools_probe: vec!["definitely-not-a-real-tool-xyz".into()],
            path_dirs: vec![tmp.clone()],
            a11y_bus_present: Some(true),
            ..Default::default()
        };
        let p = EnvironmentProfile::discover(&inputs);
        assert_eq!(p.session_type, SessionType::Wayland);
        assert_eq!(p.desktop, DesktopKind::Hyprland);
        assert!(p.config_dirs.contains(&"hypr".to_string()));
        assert!(!p.config_dirs.contains(&"i3".to_string()));
        assert!(p.a11y_bus);
        assert!(p.has_tool("definitely-not-a-real-tool-xyz"));
        std::fs::remove_dir_all(&tmp).ok();
    }
}
