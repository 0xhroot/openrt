//! `computer.*` tool wrappers — registered through the existing ToolSpec +
//! GateExecutor (no second registry/gate). Caps are `computer.<res>.all`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use openrt_tools::{Tool, ToolError, ToolOutput, ToolSpec};

use crate::backend::ControlOp;
use crate::control::ComputerControl;

fn invalid(tool: &str, msg: impl Into<String>) -> ToolError {
    ToolError::InvalidArguments(tool.into(), msg.into())
}

fn internal(tool: &str, msg: impl Into<String>) -> ToolError {
    ToolError::Internal(tool.into(), msg.into())
}

/// `computer.env` — environment profile + capability matrix + resolver log.
pub struct ComputerEnvTool {
    pub spec: ToolSpec,
    pub control: Arc<ComputerControl>,
}

#[async_trait]
impl Tool for ComputerEnvTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, _args: Value) -> Result<ToolOutput, ToolError> {
        let m = self.control.capability_matrix();
        Ok(ToolOutput::Json(json!({
            "summary": self.control.summary(),
            "profile": self.control.profile().to_value(),
            "resolver": self.control.decisions(),
            "capabilities": m.rows(),
        })))
    }
}

/// `computer.window` — list / focus (with verify) / close.
pub struct ComputerWindowTool {
    pub spec: ToolSpec,
    pub control: Arc<ComputerControl>,
}

#[async_trait]
impl Tool for ComputerWindowTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
        match action {
            "list" => {
                let r = self
                    .control
                    .execute(ControlOp::ListWindows)
                    .await
                    .map_err(|e| internal("computer.window", e.to_string()))?;
                Ok(ToolOutput::Json(json!({
                    "action": "list",
                    "ok": r.ok,
                    "status": r.status,
                    "message": r.message,
                    "windows": r.data,
                })))
            }
            "focus" => {
                let id = args
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("computer.window", "focus requires `id` string"))?;
                let r = self
                    .control
                    .focus_and_verify(id)
                    .await
                    .map_err(|e| internal("computer.window", e.to_string()))?;
                Ok(ToolOutput::Json(json!({
                    "action": "focus",
                    "ok": r.ok,
                    "status": r.status,
                    "id": id,
                    "message": r.message,
                    "data": r.data,
                })))
            }
            "close" => {
                let id = args
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("computer.window", "close requires `id` string"))?;
                let r = self
                    .control
                    .execute(ControlOp::CloseWindow(id.to_string()))
                    .await
                    .map_err(|e| internal("computer.window", e.to_string()))?;
                Ok(ToolOutput::Json(json!({
                    "action": "close",
                    "ok": r.ok,
                    "status": r.status,
                    "id": id,
                    "message": r.message,
                    "data": r.data,
                })))
            }
            other => Err(invalid(
                "computer.window",
                format!("unknown action `{other}` (list|focus|close)"),
            )),
        }
    }
}

/// `computer.screen` — capture + monitor geometry.
pub struct ComputerScreenTool {
    pub spec: ToolSpec,
    pub control: Arc<ComputerControl>,
}

#[async_trait]
impl Tool for ComputerScreenTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("capture");
        match action {
            "capture" => {
                let r = self
                    .control
                    .execute(ControlOp::CaptureScreen)
                    .await
                    .map_err(|e| internal("computer.screen", e.to_string()))?;
                Ok(ToolOutput::Json(json!({
                    "action": "capture",
                    "ok": r.ok,
                    "status": r.status,
                    "message": r.message,
                    "data": r.data,
                })))
            }
            "monitors" => {
                let r = self
                    .control
                    .execute(ControlOp::ListMonitors)
                    .await
                    .map_err(|e| internal("computer.screen", e.to_string()))?;
                Ok(ToolOutput::Json(json!({
                    "action": "monitors",
                    "ok": r.ok,
                    "status": r.status,
                    "message": r.message,
                    "monitors": r.data,
                })))
            }
            "observe" => {
                let obs = self
                    .control
                    .observe()
                    .await
                    .map_err(|e| internal("computer.screen", e.to_string()))?;
                Ok(ToolOutput::Json(
                    json!({ "action": "observe", "observation": obs }),
                ))
            }
            other => Err(invalid(
                "computer.screen",
                format!("unknown action `{other}` (capture|monitors|observe)"),
            )),
        }
    }
}

/// `computer.input` — keyboard/pointer through the platform backend.
pub struct ComputerInputTool {
    pub spec: ToolSpec,
    pub control: Arc<ComputerControl>,
}

#[async_trait]
impl Tool for ComputerInputTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("computer.input", "missing `action`"))?;
        let op = match action {
            "type" => {
                let text = args
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("computer.input", "type requires `text`"))?;
                ControlOp::TypeText(text.to_string())
            }
            "key" => {
                let key = args
                    .get("key")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("computer.input", "key requires `key`"))?;
                ControlOp::SendKey(key.to_string())
            }
            "move" => {
                let x = args
                    .get("x")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| invalid("computer.input", "move requires `x`"))?
                    as i32;
                let y = args
                    .get("y")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| invalid("computer.input", "move requires `y`"))?
                    as i32;
                ControlOp::MovePointer(x, y)
            }
            "click" => ControlOp::ClickPointer,
            other => {
                return Err(invalid(
                    "computer.input",
                    format!("unknown action `{other}` (type|key|move|click)"),
                ))
            }
        };
        let r = self
            .control
            .execute(op)
            .await
            .map_err(|e| internal("computer.input", e.to_string()))?;
        Ok(ToolOutput::Json(json!({
            "action": action,
            "ok": r.ok,
            "status": r.status,
            "message": r.message,
            "data": r.data,
        })))
    }
}

/// `computer.clipboard` — get/set through the platform backend.
pub struct ComputerClipboardTool {
    pub spec: ToolSpec,
    pub control: Arc<ComputerControl>,
}

#[async_trait]
impl Tool for ComputerClipboardTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("get");
        let op = match action {
            "get" => ControlOp::ClipboardGet,
            "set" => {
                let text = args
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("computer.clipboard", "set requires `text`"))?;
                ControlOp::ClipboardSet(text.to_string())
            }
            other => {
                return Err(invalid(
                    "computer.clipboard",
                    format!("unknown action `{other}` (get|set)"),
                ))
            }
        };
        let r = self
            .control
            .execute(op)
            .await
            .map_err(|e| internal("computer.clipboard", e.to_string()))?;
        Ok(ToolOutput::Json(json!({
            "action": action,
            "ok": r.ok,
            "status": r.status,
            "message": r.message,
            "data": r.data,
        })))
    }
}

/// `computer.app` — launch by name/executable (never raw shell).
pub struct ComputerAppTool {
    pub spec: ToolSpec,
    pub control: Arc<ComputerControl>,
    pub apps: Vec<crate::config_scan::AppEntry>,
}

#[async_trait]
impl Tool for ComputerAppTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("launch");
        match action {
            "list" => Ok(ToolOutput::Json(json!({
                "action": "list",
                "apps": self.apps,
            }))),
            "launch" => {
                let name = args
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("computer.app", "launch requires `name`"))?;
                // Prefer desktop-entry resolution over raw passthrough.
                let resolved = crate::apps::find_app(&self.apps, name)
                    .map(|a| a.command.clone())
                    .unwrap_or_else(|| name.to_string());
                // Never pass shell metacharacters.
                if resolved
                    .chars()
                    .any(|c| matches!(c, '|' | ';' | '&' | '>' | '<' | '`' | '$'))
                {
                    return Err(invalid(
                        "computer.app",
                        "resolved command contains shell metacharacters; refusing",
                    ));
                }
                let r = self
                    .control
                    .execute(ControlOp::LaunchApp(resolved.clone()))
                    .await
                    .map_err(|e| internal("computer.app", e.to_string()))?;
                Ok(ToolOutput::Json(json!({
                    "action": "launch",
                    "ok": r.ok,
                    "status": r.status,
                    "query": name,
                    "resolved": resolved,
                    "message": r.message,
                    "data": r.data,
                })))
            }
            other => Err(invalid(
                "computer.app",
                format!("unknown action `{other}` (list|launch)"),
            )),
        }
    }
}

/// Full `computer.*` tool set over one control session.
pub fn computer_tools(
    control: Arc<ComputerControl>,
    apps: Vec<crate::config_scan::AppEntry>,
) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ComputerEnvTool {
            spec: ToolSpec::new(
                "computer.env",
                "Discover the environment profile, capability matrix, and backend resolver decisions.",
            )
            .with_caps(&["computer.env.all"])
            .with_schema(json!({ "type": "object", "properties": {} })),
            control: control.clone(),
        }),
        Arc::new(ComputerWindowTool {
            spec: ToolSpec::new(
                "computer.window",
                "Focus an already-open window, list windows, or close a window (observation \
                 verify on focus). NOT for opening applications: \"open Firefox\" is \
                 computer.app.",
            )
            .with_caps(&["computer.window.all"])
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "focus", "close"], "default": "list" },
                    "id": { "type": "string", "description": "window identity (backend id, not title)" }
                }
            })),
            control: control.clone(),
        }),
        Arc::new(ComputerScreenTool {
            spec: ToolSpec::new(
                "computer.screen",
                "Capture the screen, list monitors, or observe active window state.",
            )
            .with_caps(&["computer.screen.all"])
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["capture", "monitors", "observe"], "default": "capture" }
                }
            })),
            control: control.clone(),
        }),
        Arc::new(ComputerInputTool {
            spec: ToolSpec::new(
                "computer.input",
                "Keyboard/pointer input through the platform backend (unsupported ops report structured UNAVAILABLE).",
            )
            .with_caps(&["computer.input.all"])
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["type", "key", "move", "click"] },
                    "text": { "type": "string" },
                    "key": { "type": "string" },
                    "x": { "type": "integer" },
                    "y": { "type": "integer" }
                }
            })),
            control: control.clone(),
        }),
        Arc::new(ComputerClipboardTool {
            spec: ToolSpec::new(
                "computer.clipboard",
                "Get or set the system clipboard through the platform backend.",
            )
            .with_caps(&["computer.clipboard.all"])
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["get", "set"], "default": "get" },
                    "text": { "type": "string" }
                }
            })),
            control: control.clone(),
        }),
        Arc::new(ComputerAppTool {
            spec: ToolSpec::new(
                "computer.app",
                "Launch a desktop application by name - pass `name` (e.g. \"Firefox\", \
                 \"VS Code\", \"Terminal\"), or list installed apps with action=list. Use for \
                 every \"open/launch/start <application>\" request, even if it may already be \
                 running. Never opens URLs (use browser.navigate) or shell commands.",
            )
            .with_caps(&["computer.app.all"])
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "launch"], "default": "launch" },
                    "name": { "type": "string" }
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
    use crate::backend::BackendKind;
    use crate::profile::EnvironmentProfile;

    fn specs() -> Vec<ToolSpec> {
        let control =
            ComputerControl::with_profile(EnvironmentProfile::host(), Some(BackendKind::Sim));
        computer_tools(Arc::new(control), vec![])
            .into_iter()
            .map(|t| t.spec().clone())
            .collect()
    }

    fn desc<'a>(specs: &'a [ToolSpec], name: &str) -> &'a str {
        &specs
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("{name} missing"))
            .description
    }

    /// `computer.app` must read as the application-launch entry point: it
    /// routes URL/website intent to `browser.navigate` and shell intent away,
    /// so "open Firefox" resolves to launch instead of navigate.
    #[test]
    fn computer_app_description_routes_launch_intent() {
        let specs = specs();
        let d = desc(&specs, "computer.app");
        assert!(d.starts_with("Launch a desktop application"), "{d}");
        assert!(d.contains("open/launch/start"), "{d}");
        assert!(d.contains("browser.navigate"), "{d}");
        assert!(d.contains("Never opens URLs"), "{d}");
        assert!(d.contains("or shell commands"), "{d}");
        assert!(d.contains("list installed apps"), "{d}");
        assert!(d.contains("pass `name`"), "{d}");
        assert!(d.contains("already be running"), "{d}");
    }

    /// `computer.window` manages existing windows — it must not read as a
    /// launcher, and must point launchers at `computer.app`.
    #[test]
    fn computer_window_description_rules_out_launching() {
        let specs = specs();
        let d = desc(&specs, "computer.window");
        assert!(d.contains("already-open window"), "{d}");
        assert!(d.contains("NOT for opening applications"), "{d}");
        assert!(d.contains("\"open Firefox\" is computer.app"), "{d}");
    }

    /// Every `computer.*` spec keeps a non-empty intent-bearing description.
    #[test]
    fn all_computer_tool_descriptions_are_present() {
        for s in specs() {
            assert!(!s.description.trim().is_empty(), "{}", s.name);
        }
    }

    /// Item: real ComputerControl launching — `computer.app` executes
    /// LaunchApp through the ONE ComputerControl (Sim backend here; the
    /// real backends take the identical ControlOp path) and reports the
    /// launch honestly.
    #[tokio::test]
    async fn computer_app_launch_runs_through_computer_control() {
        let control =
            ComputerControl::with_profile(EnvironmentProfile::host(), Some(BackendKind::Sim));
        let tools = computer_tools(Arc::new(control), vec![]);
        let app_tool = tools
            .iter()
            .find(|t| t.spec().name == "computer.app")
            .expect("computer.app registered");
        let out = app_tool
            .run(json!({ "action": "launch", "name": "Firefox" }))
            .await
            .expect("Sim backend launches");
        match out {
            ToolOutput::Json(v) => {
                assert_eq!(v["action"], "launch", "{v}");
                assert_eq!(v["data"]["app"], "Firefox", "{v}");
            }
            other => panic!("expected JSON output, got {other:?}"),
        }
        // List action stays honest too (no launch happened for it).
        let out = app_tool.run(json!({ "action": "list" })).await.unwrap();
        match out {
            ToolOutput::Json(v) => assert_eq!(v["action"], "list", "{v}"),
            other => panic!("expected JSON output, got {other:?}"),
        }
    }
}
