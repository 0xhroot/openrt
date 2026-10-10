//! Application-adapter framework (M7B.5).
//!
//! An adapter declares an application's identity, platform support, the tool
//! capabilities it powers, required permissions, its backend, and how its
//! state is verified. Launch stays with the existing `computer.app` tool
//! (Phase 7A, frozen) — the framework *describes and verifies*, it never
//! becomes a second launcher.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};

use openrt_platform::config_scan::AppEntry;
use openrt_platform::ComputerControl;
use openrt_tools::{Tool, ToolError, ToolOutput, ToolSpec};

use crate::meta::SupportState;

/// Static declaration of one application adapter.
#[derive(Debug, Clone, Serialize)]
pub struct ApplicationAdapter {
    /// Stable id (`firefox`, `libreoffice`, `sqlite`, `hyprland-shell`, …).
    pub id: String,
    pub display_name: String,
    /// Environment key → support state.
    pub platforms: BTreeMap<String, SupportState>,
    /// Tool ids this adapter powers (documentation, discovery joins).
    pub powers_tools: Vec<String>,
    /// Capabilities the powered tools require.
    pub required_caps: Vec<String>,
    /// Which backend implements it (`firefox-bidi`, `rusqlite`, `soffice`,
    /// `computer-control`, …).
    pub backend: String,
    /// How state is verified after an action (window tree, digest, re-read …).
    pub verification: String,
}

impl ApplicationAdapter {
    pub fn new(
        id: impl Into<String>,
        display_name: impl Into<String>,
        backend: impl Into<String>,
        verification: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            display_name: display_name.into(),
            platforms: BTreeMap::new(),
            powers_tools: Vec::new(),
            required_caps: Vec::new(),
            backend: backend.into(),
            verification: verification.into(),
        }
    }

    pub fn with_platform(mut self, env: &str, state: SupportState) -> Self {
        self.platforms.insert(env.to_string(), state);
        self
    }

    pub fn powers(mut self, tools: &[&str], caps: &[&str]) -> Self {
        self.powers_tools = tools.iter().map(|s| s.to_string()).collect();
        self.required_caps = caps.iter().map(|s| s.to_string()).collect();
        self
    }
}

/// Registered adapters (wiring-time; static declarations + host probing done
/// by the producing subsystems).
pub struct ApplicationRegistry {
    adapters: Vec<ApplicationAdapter>,
}

impl ApplicationRegistry {
    pub fn new() -> Self {
        Self {
            adapters: Vec::new(),
        }
    }

    pub(crate) fn from_vec(adapters: Vec<ApplicationAdapter>) -> Self {
        Self { adapters }
    }

    pub fn register(&mut self, adapter: ApplicationAdapter) {
        self.adapters.push(adapter);
    }

    pub fn list(&self) -> &[ApplicationAdapter] {
        &self.adapters
    }
}

impl Default for ApplicationRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// `application.list` — adapters + the tools/permissions they power.
pub struct ApplicationListTool {
    pub spec: ToolSpec,
    pub registry: Arc<ApplicationRegistry>,
}

#[async_trait]
impl Tool for ApplicationListTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, _args: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::Json(json!({
            "ok": true,
            "adapters": self.registry.list(),
        })))
    }
}

/// `application.verify` — verify an application's state through the existing
/// ComputerControl observation path: desktop entry found? window present?
pub struct ApplicationVerifyTool {
    pub spec: ToolSpec,
    pub control: Arc<ComputerControl>,
    pub apps: Vec<AppEntry>,
}

#[async_trait]
impl Tool for ApplicationVerifyTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let name = args.get("app").and_then(Value::as_str).ok_or_else(|| {
            ToolError::InvalidArguments("application.verify".into(), "missing `app` string".into())
        })?;
        let entry = openrt_platform::apps::find_app(&self.apps, name);
        let windows = self
            .control
            .execute(openrt_platform::backend::ControlOp::ListWindows)
            .await
            .map_err(|e| {
                ToolError::Internal(
                    "application.verify".into(),
                    format!("window observation failed: {e}"),
                )
            })?;
        let all = windows.data.as_array().cloned().unwrap_or_default();
        let needle = name.to_ascii_lowercase();
        let matching: Vec<Value> = all
            .iter()
            .filter(|w| {
                let hay = format!(
                    "{} {} {}",
                    w.get("title").and_then(Value::as_str).unwrap_or_default(),
                    w.get("app_id").and_then(Value::as_str).unwrap_or_default(),
                    w.get("id").and_then(Value::as_str).unwrap_or_default(),
                )
                .to_ascii_lowercase();
                hay.contains(&needle)
            })
            .cloned()
            .collect();
        let running = !matching.is_empty();
        let entry_found = entry.is_some();
        Ok(ToolOutput::Json(json!({
            "ok": true,
            "app": name,
            "entry_found": entry_found,
            "entry": entry.as_ref().map(|e| json!({
                "id": e.id,
                "name": e.name,
                "command": e.command,
                "source": e.source,
            })),
            "running": running,
            "windows": matching,
            "verification": {
                "method": "desktop-entry + window-tree observation",
                "note": if running { "window observed" } else if entry_found { "entry present, no window observed" } else { "no entry and no window observed" },
            },
        })))
    }
}

/// Adapter + verification tools for `application.*`.
pub fn application_tools(
    registry: Arc<ApplicationRegistry>,
    control: Arc<ComputerControl>,
    apps: Vec<AppEntry>,
) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ApplicationListTool {
            spec: ToolSpec::new(
                "application.list",
                "List declared application adapters: identity, platform support, powered tools, required capabilities, backend, and verification method.",
            )
            .with_caps(&["application.list.all"])
            .with_schema(json!({ "type": "object", "properties": {} })),
            registry,
        }),
        Arc::new(ApplicationVerifyTool {
            spec: ToolSpec::new(
                "application.verify",
                "Verify an application's state: desktop entry discovery plus window-tree observation through the platform backend.",
            )
            .with_caps(&["application.verify.all"])
            .with_schema(json!({
                "type": "object",
                "required": ["app"],
                "properties": {
                    "app": { "type": "string", "description": "application name or executable" }
                }
            })),
            control,
            apps,
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapters_declare_identity_platform_caps() {
        let mut reg = ApplicationRegistry::new();
        reg.register(
            ApplicationAdapter::new(
                "firefox",
                "Mozilla Firefox",
                "firefox-bidi",
                "BiDi context state + screenshot",
            )
            .with_platform("host", SupportState::Green)
            .powers(
                &["browser.navigate", "browser.screenshot"],
                &["browser.navigate.all", "browser.screenshot.all"],
            ),
        );
        let a = &reg.list()[0];
        assert_eq!(a.id, "firefox");
        assert_eq!(a.platforms["host"], SupportState::Green);
        assert_eq!(a.powers_tools.len(), 2);
        assert_eq!(a.required_caps.len(), 2);
        assert_eq!(a.backend, "firefox-bidi");
    }
}
