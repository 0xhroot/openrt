//! Config intelligence: read-only scanners for WM/compositor config as
//! *environment knowledge* (launchers, workspaces, keybindings as fallback
//! hints). Never writes to `~/.config`; never treats keybindings as core
//! primitives.
//!
//! Parsers accept both classic config text and fixture strings so tests never
//! touch the user's real files.

use serde::{Deserialize, Serialize};

/// A launcher / application entry discovered from config or desktop files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppEntry {
    pub id: String,
    pub name: String,
    pub command: String,
    pub source: String,
}

/// A keybinding recorded as environment knowledge (fallback only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeybindingHint {
    pub keys: String,
    pub action: String,
    pub source: String,
}

/// Workspace names/ids known from config (or empty when unknown).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceKnowledge {
    pub workspaces: Vec<String>,
    pub source: String,
}

/// Result of scanning one config document.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigKnowledge {
    pub apps: Vec<AppEntry>,
    pub keybindings: Vec<KeybindingHint>,
    #[serde(default)]
    pub workspaces: WorkspaceKnowledge,
    pub notes: Vec<String>,
}

impl ConfigKnowledge {
    pub fn merge(&mut self, other: ConfigKnowledge) {
        for a in other.apps {
            if !self.apps.contains(&a) {
                self.apps.push(a);
            }
        }
        for k in other.keybindings {
            if !self.keybindings.contains(&k) {
                self.keybindings.push(k);
            }
        }
        if self.workspaces.workspaces.is_empty() && !other.workspaces.workspaces.is_empty() {
            self.workspaces = other.workspaces;
        }
        self.notes.extend(other.notes);
    }
}

/// Parse classic Hyprland `hyprland.conf` (bind/exec/bindm lines).
/// Lua/Omarchy configs yield empty knowledge + a note (hyprctl is the source
/// of truth at runtime).
pub fn parse_hyprland_conf(text: &str) -> ConfigKnowledge {
    let mut out = ConfigKnowledge {
        workspaces: WorkspaceKnowledge {
            workspaces: Vec::new(),
            source: "hyprland.conf".into(),
        },
        notes: source_notes(),
        ..Default::default()
    };
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with("bind") {
            // bind, binde, bindm, bindl — extract key + dispatcher after '='
            if let Some(eq) = line.find('=') {
                let rhs = line[eq + 1..].trim();
                // Form: MODS, key, dispatcher, args
                let parts: Vec<&str> = rhs.splitn(4, ',').map(|s| s.trim()).collect();
                if parts.len() >= 3 {
                    let keys = format!("{} {}", parts[0], parts[1]);
                    let action = if parts.len() >= 4 && !parts[3].is_empty() {
                        format!("{} {}", parts[2], parts[3])
                    } else {
                        parts[2].to_string()
                    };
                    out.keybindings.push(KeybindingHint {
                        keys,
                        action,
                        source: "hyprland.conf".into(),
                    });
                }
            }
        } else if let Some(eq) = line.find('=') {
            let key = line[..eq].trim();
            let val = line[eq + 1..].trim();
            if key == "exec-once" || key == "exec" {
                out.apps.push(AppEntry {
                    id: slug(val),
                    name: val.to_string(),
                    command: val.to_string(),
                    source: format!("hyprland.conf:{key}"),
                });
            }
        }
    }
    out
}

/// Parse Sway/i3 config (`bindsym`, `exec`, `workspace`).
pub fn parse_sway_or_i3_conf(text: &str, source: &str) -> ConfigKnowledge {
    let mut out = ConfigKnowledge {
        workspaces: WorkspaceKnowledge {
            workspaces: Vec::new(),
            source: source.to_string(),
        },
        notes: vec![format!("scanned {source}")],
        ..Default::default()
    };
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("bindsym ") {
            if let Some(eq) = rest.find(|c: char| c.is_whitespace()) {
                let keys = rest[..eq].trim().to_string();
                let action = rest[eq..].trim().to_string();
                out.keybindings.push(KeybindingHint {
                    keys,
                    action,
                    source: source.to_string(),
                });
            } else {
                out.keybindings.push(KeybindingHint {
                    keys: rest.trim().to_string(),
                    action: String::new(),
                    source: source.to_string(),
                });
            }
        } else if let Some(rest) = line.strip_prefix("exec ") {
            out.apps.push(AppEntry {
                id: slug(rest.trim()),
                name: rest.trim().to_string(),
                command: rest.trim().to_string(),
                source: source.to_string(),
            });
        } else if let Some(rest) = line.strip_prefix("exec_always ") {
            out.apps.push(AppEntry {
                id: slug(rest.trim()),
                name: rest.trim().to_string(),
                command: rest.trim().to_string(),
                source: source.to_string(),
            });
        } else if let Some(rest) = line.strip_prefix("workspace ") {
            // Forms: `workspace 1`, `workspace 1 output HDMI-A-1`,
            // `workspace "$ws" output ...` — take the first token only.
            let first = rest.split_whitespace().next().unwrap_or("");
            let ws = first.trim_matches('"').to_string();
            if !ws.is_empty() && !out.workspaces.workspaces.contains(&ws) {
                out.workspaces.workspaces.push(ws);
            }
        }
    }
    out
}

/// Parse bspwmrc shell-style config (bspc config lines only; ignore exports).
pub fn parse_bspwmrc(text: &str) -> ConfigKnowledge {
    let mut out = ConfigKnowledge {
        workspaces: WorkspaceKnowledge {
            workspaces: Vec::new(),
            source: "bspwmrc".into(),
        },
        notes: vec!["scanned bspwmrc".into()],
        ..Default::default()
    };
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with("bspc monitor") && line.contains("-d") {
            if let Some(idx) = line.find("-d") {
                let rest = &line[idx + 2..];
                for name in rest.split_whitespace() {
                    let n = name.trim_matches('"');
                    if !n.is_empty() && !out.workspaces.workspaces.contains(&n.to_string()) {
                        out.workspaces.workspaces.push(n.to_string());
                    }
                }
            }
        } else if line.starts_with("bspc") && (line.contains("node") || line.contains("config")) {
            out.notes.push(format!("bspc: {line}"));
        }
    }
    out
}

/// Parse sxhkdrc (`key: action` blocks).
pub fn parse_sxhkdrc(text: &str) -> ConfigKnowledge {
    let mut out = ConfigKnowledge {
        workspaces: WorkspaceKnowledge {
            workspaces: Vec::new(),
            source: "sxhkdrc".into(),
        },
        notes: vec!["scanned sxhkdrc".into()],
        ..Default::default()
    };
    // sxhkd format: a non-indented key line (optionally ending with ':'),
    // followed by indented command lines. Blank lines separate bindings.
    let mut pending_keys: Option<String> = None;
    let mut pending_action: Vec<String> = Vec::new();
    let flush = |keys: &mut Option<String>, action: &mut Vec<String>, out: &mut ConfigKnowledge| {
        if let Some(k) = keys.take() {
            let joined = action.join("\n");
            action.clear();
            if !joined.trim().is_empty() {
                out.keybindings.push(KeybindingHint {
                    keys: k,
                    action: joined.trim().to_string(),
                    source: "sxhkdrc".into(),
                });
                out.apps.push(AppEntry {
                    id: slug(joined.trim()),
                    name: joined.trim().to_string(),
                    command: joined.trim().to_string(),
                    source: "sxhkdrc".into(),
                });
            }
        }
    };
    for raw in text.lines() {
        if raw.starts_with('#') {
            continue;
        }
        if raw.trim().is_empty() {
            flush(&mut pending_keys, &mut pending_action, &mut out);
            continue;
        }
        if !raw.starts_with(char::is_whitespace) {
            // New key line.
            flush(&mut pending_keys, &mut pending_action, &mut out);
            let keys = raw.trim().trim_end_matches(':').to_string();
            if !keys.is_empty() {
                pending_keys = Some(keys);
            }
        } else if pending_keys.is_some() {
            pending_action.push(raw.trim().to_string());
        }
    }
    flush(&mut pending_keys, &mut pending_action, &mut out);
    out
}

/// Parse a subset of a `.desktop` file (Name/Exec/StartupWMClass).
pub fn parse_desktop_entry(text: &str, source: &str) -> Option<AppEntry> {
    if !text.contains("[Desktop Entry]") {
        return None;
    }
    let mut name = None;
    let mut exec = None;
    let mut wm_class = None;
    let mut is_app = true;
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(v) = line.strip_prefix("Name=") {
            if name.is_none() {
                name = Some(v.to_string());
            }
        } else if let Some(v) = line.strip_prefix("Exec=") {
            exec = Some(v.split_whitespace().next().unwrap_or(v).to_string());
        } else if let Some(v) = line.strip_prefix("StartupWMClass=") {
            wm_class = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("Type=") {
            is_app = v == "Application";
        } else if let Some(v) = line.strip_prefix("NoDisplay=") {
            if v.eq_ignore_ascii_case("true") {
                is_app = false;
            }
        }
    }
    if !is_app {
        return None;
    }
    let command = exec?;
    let display = name.unwrap_or_else(|| command.clone());
    Some(AppEntry {
        id: wm_class.unwrap_or_else(|| slug(&display)),
        name: display,
        command,
        source: source.to_string(),
    })
}

fn slug(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .chars()
        .take(64)
        .collect()
}

fn source_notes() -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hyprland_classic_binds_and_exec() {
        let conf = r#"
# comment
bind = SUPER, Q, exec, foot
bindm = SUPER, mouse:272, movewindow
exec-once = waybar
"#;
        let k = parse_hyprland_conf(conf);
        assert!(k
            .keybindings
            .iter()
            .any(|b| b.keys.contains("Q") && b.action.contains("foot")));
        assert!(k.apps.iter().any(|a| a.command == "waybar"));
    }

    #[test]
    fn sway_bindsym_and_workspace() {
        let conf = r#"
bindsym $mod+Return exec foot
workspace 1 output HDMI-A-1
exec --no-startup-id nm-applet
"#;
        // i3 uses exec --no-startup-id; sway uses exec
        let k = parse_sway_or_i3_conf(conf, "i3.config");
        assert!(k.keybindings.iter().any(|b| b.keys.contains("Return")));
        assert!(k.workspaces.workspaces.iter().any(|w| w == "1"));
        assert!(k.apps.iter().any(|a| a.command.contains("nm-applet")));
    }

    #[test]
    fn bspwm_monitor_desktops() {
        let rc = r#"
#!/bin/sh
bspc monitor HDMI-A-1 -d 1 2 3 4
bspc config border_width 2
"#;
        let k = parse_bspwmrc(rc);
        assert_eq!(k.workspaces.workspaces, vec!["1", "2", "3", "4"]);
    }

    #[test]
    fn sxhkd_block() {
        let rc = r#"
super + Return
    foot

super + shift + q
    bspc node -c
"#;
        let k = parse_sxhkdrc(rc);
        assert!(k.keybindings.len() >= 1, "{:?}", k.keybindings);
        assert!(
            k.keybindings[0].action.contains("foot")
                || k.keybindings.iter().any(|b| b.action.contains("foot"))
        );
    }

    #[test]
    fn desktop_entry_parses_exec() {
        let d = "[Desktop Entry]\nType=Application\nName=Foot Terminal\nExec=foot\n";
        let e = parse_desktop_entry(d, "foot.desktop").unwrap();
        assert_eq!(e.command, "foot");
        assert_eq!(e.name, "Foot Terminal");
    }

    #[test]
    fn desktop_entry_skips_non_app() {
        let d = "[Desktop Entry]\nType=Link\nName=Docs\nURL=http://x\n";
        assert!(parse_desktop_entry(d, "x.desktop").is_none());
    }
}
