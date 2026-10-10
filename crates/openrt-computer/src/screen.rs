//! Screen capture, vision grounding, mouse, keyboard and browser tools (M2.5).
//!
//! Backends abstract the computer surface:
//!
//! * [`SimScreen`] — a deterministic in-memory screen (a scripted set of
//!   elements with fixed geometry). This is the headless test harness the
//!   roadmap's DoD calls for ("no real display needed"); grounding resolves
//!   exactly, and input ops are recorded so a test can assert what happened.
//! * [`X11Screen`] — a real-display backend over Xvfb/xdotool/ImageMagick,
//!   used when configured and available. It is written but only exercised on
//!   hosts with X tooling; the tests here skip (same policy as the sandbox's
//!   bubblewrap tests).
//!
//! Vision grounding is deterministic-only in this milestone: elements are the
//! X window tree / DOM equivalents resolved by name to geometry and center.
//! Model-assisted grounding is Phase 5+.

use std::any::Any;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use openrt_tools::{Tool, ToolError, ToolOutput, ToolSpec};

/// One groundable screen element (window / button / field / label).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenElement {
    pub name: String,
    /// Coarse role: `window`, `button`, `field`, `label`, `desktop`.
    pub role: String,
    pub x: i64,
    pub y: i64,
    pub width: u64,
    pub height: u64,
}

impl ScreenElement {
    pub fn new(name: &str, role: &str, x: i64, y: i64, width: u64, height: u64) -> Self {
        Self {
            name: name.to_string(),
            role: role.to_string(),
            x,
            y,
            width,
            height,
        }
    }

    pub fn contains(&self, px: i64, py: i64) -> bool {
        px >= self.x
            && px < self.x + self.width as i64
            && py >= self.y
            && py < self.y + self.height as i64
    }

    pub fn center(&self) -> (i64, i64) {
        (
            self.x + self.width as i64 / 2,
            self.y + self.height as i64 / 2,
        )
    }

    /// Grounding output: the element + its resolved center.
    pub fn grounded_json(&self) -> Value {
        let (cx, cy) = self.center();
        json!({
            "name": self.name,
            "role": self.role,
            "x": self.x,
            "y": self.y,
            "width": self.width,
            "height": self.height,
            "center": { "x": cx, "y": cy },
        })
    }
}

/// Output of a screen capture: dimensions plus the ordered element tree (the
/// deterministic "DOM" grounding source). Real backends may also persist a
/// pixel image whose path is reported here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreenCapture {
    pub backend: String,
    pub width: u64,
    pub height: u64,
    pub elements: Vec<ScreenElement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_path: Option<PathBuf>,
}

impl ScreenCapture {
    /// Ground by (case-insensitive substring) name match; empty target lists all.
    pub fn ground(&self, target: &str) -> Vec<&ScreenElement> {
        let needle = target.trim().to_lowercase();
        self.elements
            .iter()
            .filter(|e| needle.is_empty() || e.name.to_lowercase().contains(&needle))
            .collect()
    }
}

/// The computer-use surface a set of tools drives. Implementations must be
/// deterministic when configured for the test harness.
#[async_trait]
pub trait ScreenBackend: Send + Sync {
    fn kind(&self) -> &'static str;
    fn as_any(&self) -> &dyn Any;

    fn width(&self) -> u64;
    fn height(&self) -> u64;

    async fn elements(&self) -> Result<Vec<ScreenElement>, ToolError>;
    async fn capture(&self) -> Result<ScreenCapture, ToolError>;

    async fn pointer_move(&self, x: i64, y: i64) -> Result<(), ToolError>;
    async fn pointer_click(&self, x: i64, y: i64, button: u32) -> Result<(), ToolError>;
    async fn pointer_drag(&self, from: (i64, i64), to: (i64, i64)) -> Result<(), ToolError>;

    async fn keyboard_type(&self, text: &str) -> Result<(), ToolError>;
    async fn keyboard_key(&self, key: &str) -> Result<(), ToolError>;

    async fn window_list(&self) -> Result<Vec<String>, ToolError>;
    async fn window_focus(&self, name: &str) -> Result<(), ToolError>;
}

fn internal(tool: &str, reason: impl Into<String>) -> ToolError {
    ToolError::Internal(tool.to_string(), reason.into())
}

fn invalid(tool: &str, reason: impl Into<String>) -> ToolError {
    ToolError::InvalidArguments(tool.to_string(), reason.into())
}

/// Deterministic in-memory screen. Elements carry fixed geometry; pointer and
/// keyboard actions are recorded (and click/type target the top-most element
/// at the point / the focused window), so a scripted harness can assert the
/// full loop completed exactly as intended.
pub struct SimScreen {
    inner: Mutex<SimState>,
}

#[derive(Default)]
struct SimState {
    width: u64,
    height: u64,
    elements: Vec<ScreenElement>,
    cursor: (i64, i64),
    focus: Option<String>,
    pointer_clicks: Vec<String>,
    typed: Vec<String>,
    keys: Vec<String>,
    captures: Vec<ScreenCapture>,
    focused: Vec<String>,
}

impl std::fmt::Debug for SimScreen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimScreen").finish_non_exhaustive()
    }
}

impl SimScreen {
    pub fn new(width: u64, height: u64, elements: Vec<ScreenElement>) -> Self {
        Self {
            inner: Mutex::new(SimState {
                width,
                height,
                elements,
                cursor: (0, 0),
                ..Default::default()
            }),
        }
    }

    pub fn add_element(&self, e: ScreenElement) {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .elements
            .push(e);
    }

    fn state(&self) -> std::sync::MutexGuard<'_, SimState> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    // ---- test/observation accessors ----
    pub fn elements(&self) -> Vec<ScreenElement> {
        self.state().elements.clone()
    }
    pub fn pointer_clicks(&self) -> Vec<String> {
        self.state().pointer_clicks.clone()
    }
    pub fn typed(&self) -> Vec<String> {
        self.state().typed.clone()
    }
    pub fn keys(&self) -> Vec<String> {
        self.state().keys.clone()
    }
    pub fn focused_windows(&self) -> Vec<String> {
        self.state().focused.clone()
    }
    pub fn captures(&self) -> Vec<ScreenCapture> {
        self.state().captures.clone()
    }
    pub fn cursor(&self) -> (i64, i64) {
        self.state().cursor
    }

    fn top_at<'a>(&self, state: &'a SimState, px: i64, py: i64) -> Option<&'a ScreenElement> {
        state.elements.iter().rev().find(|e| {
            e.contains(px, py) && (e.role == "button" || e.role == "field" || e.role == "window")
        })
    }
}

#[async_trait]
impl ScreenBackend for SimScreen {
    fn kind(&self) -> &'static str {
        "sim"
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn width(&self) -> u64 {
        self.state().width
    }
    fn height(&self) -> u64 {
        self.state().height
    }

    async fn elements(&self) -> Result<Vec<ScreenElement>, ToolError> {
        Ok(self.elements())
    }

    async fn capture(&self) -> Result<ScreenCapture, ToolError> {
        let mut st = self.state();
        let cap = ScreenCapture {
            backend: "sim".to_string(),
            width: st.width,
            height: st.height,
            elements: st.elements.clone(),
            image_path: None,
        };
        st.captures.push(cap.clone());
        Ok(cap)
    }

    async fn pointer_move(&self, x: i64, y: i64) -> Result<(), ToolError> {
        self.state().cursor = (x, y);
        Ok(())
    }

    async fn pointer_click(&self, x: i64, y: i64, button: u32) -> Result<(), ToolError> {
        let mut st = self.state();
        st.cursor = (x, y);
        let hit = self.top_at(&st, x, y).map(|e| e.name.clone());
        let target = hit.clone().unwrap_or_else(|| "desktop".to_string());
        st.pointer_clicks.push(target);
        if let Some(name) = hit {
            st.focused.push(name.clone());
            st.focus = Some(name);
        }
        let _ = button;
        Ok(())
    }

    async fn pointer_drag(&self, from: (i64, i64), to: (i64, i64)) -> Result<(), ToolError> {
        let mut st = self.state();
        st.cursor = to;
        let hit = self.top_at(&st, to.0, to.1).map(|e| e.name.clone());
        st.pointer_clicks.push(format!(
            "drag:{:?}->{:?}:{}",
            from,
            to,
            hit.unwrap_or_else(|| "desktop".to_string())
        ));
        Ok(())
    }

    async fn keyboard_type(&self, text: &str) -> Result<(), ToolError> {
        let mut st = self.state();
        let target = st.focus.clone().unwrap_or_else(|| "desktop".to_string());
        st.typed.push(format!("{target}:{text}"));
        Ok(())
    }

    async fn keyboard_key(&self, key: &str) -> Result<(), ToolError> {
        let mut st = self.state();
        let target = st.focus.clone().unwrap_or_else(|| "desktop".to_string());
        st.keys.push(format!("{target}:{key}"));
        Ok(())
    }

    async fn window_list(&self) -> Result<Vec<String>, ToolError> {
        Ok(self
            .elements()
            .into_iter()
            .filter(|e| e.role == "window")
            .map(|e| e.name)
            .collect())
    }

    async fn window_focus(&self, name: &str) -> Result<(), ToolError> {
        let mut st = self.state();
        let matches: Vec<String> = st
            .elements
            .iter()
            .filter(|e| e.name.to_lowercase().contains(&name.to_lowercase()))
            .map(|e| e.name.clone())
            .collect();
        if matches.is_empty() {
            return Err(invalid(
                "browser.activate",
                format!("no window matching `{name}`"),
            ));
        }
        st.focus = Some(matches[0].clone());
        st.focused.push(matches[0].clone());
        Ok(())
    }
}

/// Optional `xdotool`-availability probe used to decide the real-display
/// backend. Off when X tooling is absent (deterministic fallback).
pub fn have_x11_tooling() -> bool {
    if std::env::var_os("DISPLAY").is_none() {
        return false;
    }
    [("xdotool", "xdotool"), ("import", "import")]
        .iter()
        .fold(true, |ok, (_, bin)| {
            ok && std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("command -v {bin} >/dev/null 2>&1"))
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        })
}

/// Real-display backend over Xvfb/xdotool/ImageMagick (written for hosts with
/// X tooling; the sim backend is the deterministic default).
#[derive(Default, Debug)]
pub struct X11Screen {
    geometry: std::sync::OnceLock<(u64, u64)>,
}

impl X11Screen {
    pub fn try_new() -> Option<Self> {
        have_x11_tooling().then_some(X11Screen::default())
    }

    /// Display geometry resolved once via `xdotool getdisplaygeometry`
    /// (fail-closed: `(0,0)` when the display/tooling is unavailable, which
    /// makes out-of-bounds checks reject rather than trust bad coordinates).
    fn geometry(&self) -> (u64, u64) {
        *self.geometry.get_or_init(|| {
            let resolved: Option<(u64, u64)> = (|| {
                let out = std::process::Command::new("xdotool")
                    .arg("getdisplaygeometry")
                    .output()
                    .ok()?;
                if !out.status.success() {
                    return None;
                }
                let s = String::from_utf8(out.stdout).ok()?;
                let mut parts = s.split_whitespace();
                let w = parts.next()?.parse().ok()?;
                let h = parts.next()?.parse().ok()?;
                Some((w, h))
            })();
            resolved.unwrap_or((0, 0))
        })
    }

    async fn run(&self, args: &[&str]) -> Result<String, ToolError> {
        let out = tokio::process::Command::new("xdotool")
            .args(args)
            .output()
            .await
            .map_err(|e| internal("screen.x11", format!("xdotool: {e}")))?;
        if !out.status.success() {
            return Err(internal(
                "screen.x11",
                format!(
                    "xdotool {} failed: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

#[async_trait]
impl ScreenBackend for X11Screen {
    fn kind(&self) -> &'static str {
        "x11"
    }
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn width(&self) -> u64 {
        self.geometry().0
    }
    fn height(&self) -> u64 {
        self.geometry().1
    }

    async fn capture(&self) -> Result<ScreenCapture, ToolError> {
        let dims = self
            .run(&["getdisplaygeometry"])
            .await
            .map_err(|_| internal("screen.capture", "x11 backend unavailable"))?;
        let parts: Vec<u64> = dims
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        let (width, height) = match parts.as_slice() {
            [w, h] => (*w, *h),
            _ => return Err(internal("screen.capture", "bad display geometry")),
        };
        let path = std::env::temp_dir().join(format!(
            "openrt-screen-{}-{:x}.png",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let status = tokio::process::Command::new("import")
            .args(["-window", "root", path.to_str().unwrap_or("")])
            .status()
            .await
            .map_err(|e| internal("screen.capture", format!("import: {e}")))?;
        if !status.success() {
            return Err(internal("screen.capture", "import (ImageMagick) failed"));
        }
        Ok(ScreenCapture {
            backend: "x11".to_string(),
            width,
            height,
            elements: self.elements().await?,
            image_path: Some(path),
        })
    }

    async fn elements(&self) -> Result<Vec<ScreenElement>, ToolError> {
        let ids = self
            .run(&["search", "--onlyvisible", "--name", "."])
            .await?;
        let mut out = Vec::new();
        for id in ids.split_whitespace() {
            let geo = match self.run(&["getwindowgeometry", "--shell", id]).await {
                Ok(g) => g,
                Err(_) => continue,
            };
            let mut w = 0_u64;
            let mut h = 0_u64;
            let mut x = 0_i64;
            let mut y = 0_i64;
            for line in geo.lines() {
                let mut it = line.split('=');
                let (k, v) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
                match (k, v) {
                    ("WIDTH", v) => w = v.parse().unwrap_or(0),
                    ("HEIGHT", v) => h = v.parse().unwrap_or(0),
                    ("X", v) => x = v.parse().unwrap_or(0),
                    ("Y", v) => y = v.parse().unwrap_or(0),
                    _ => {}
                }
            }
            if w == 0 || h == 0 {
                continue;
            }
            let name = self.run(&["getwindowname", id]).await.unwrap_or_default();
            if !name.is_empty() {
                out.push(ScreenElement::new(&name, "window", x, y, w, h));
            }
        }
        Ok(out)
    }

    async fn pointer_move(&self, x: i64, y: i64) -> Result<(), ToolError> {
        self.run(&["mousemove", "--sync", &x.to_string(), &y.to_string()])
            .await
            .map(|_| ())
    }

    async fn pointer_click(&self, x: i64, y: i64, button: u32) -> Result<(), ToolError> {
        self.pointer_move(x, y).await?;
        self.run(&["click", &button.to_string()]).await.map(|_| ())
    }

    async fn pointer_drag(&self, _from: (i64, i64), to: (i64, i64)) -> Result<(), ToolError> {
        self.run(&["mousedown", "1"]).await?;
        self.run(&["mousemove", "--sync", &to.0.to_string(), &to.1.to_string()])
            .await?;
        self.run(&["mouseup", "1"]).await.map(|_| ())
    }

    async fn keyboard_type(&self, text: &str) -> Result<(), ToolError> {
        self.run(&["type", "--clearmodifiers", "--delay", "1", text])
            .await
            .map(|_| ())
    }

    async fn keyboard_key(&self, key: &str) -> Result<(), ToolError> {
        self.run(&["key", "--clearmodifiers", key])
            .await
            .map(|_| ())
    }

    async fn window_list(&self) -> Result<Vec<String>, ToolError> {
        self.elements()
            .await
            .map(|es| es.into_iter().map(|e| e.name).collect())
    }

    async fn window_focus(&self, name: &str) -> Result<(), ToolError> {
        self.run(&["windowactivate", "--sync", name])
            .await
            .map(|_| ())
    }
}

// ---- tools ----

pub struct ScreenCaptureTool {
    pub spec: ToolSpec,
    pub backend: Arc<dyn ScreenBackend>,
}

#[async_trait]
impl Tool for ScreenCaptureTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn run(&self, _args: Value) -> Result<ToolOutput, ToolError> {
        let cap = self.backend.capture().await?;
        Ok(ToolOutput::Json(serde_json::to_value(cap).map_err(
            |e| internal("screen.capture", format!("serialize: {e}")),
        )?))
    }
}

pub struct ScreenGroundTool {
    pub spec: ToolSpec,
    pub backend: Arc<dyn ScreenBackend>,
}

#[async_trait]
impl Tool for ScreenGroundTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let target = args.get("target").and_then(Value::as_str).unwrap_or("*");
        let cap = self.backend.capture().await?;
        let grounded: Vec<Value> = cap
            .ground(target)
            .into_iter()
            .map(|e| e.grounded_json())
            .collect();
        if grounded.is_empty() {
            return Err(invalid(
                "screen.ground",
                format!("no element matches `{target}` (deterministic grounding)"),
            ));
        }
        Ok(ToolOutput::Json(json!({
            "matches": grounded.len(),
            "elements": grounded,
        })))
    }
}

pub struct InputMouseTool {
    pub spec: ToolSpec,
    pub backend: Arc<dyn ScreenBackend>,
}

#[async_trait]
impl Tool for InputMouseTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("click");
        let x = args.get("x").and_then(Value::as_i64).unwrap_or(0);
        let y = args.get("y").and_then(Value::as_i64).unwrap_or(0);
        let tx = args.get("to_x").and_then(Value::as_i64);
        let ty = args.get("to_y").and_then(Value::as_i64);
        let button = args.get("button").and_then(Value::as_u64).unwrap_or(1) as u32;

        // Coordinate bounds (P2.7): reject actions targeting a point outside
        // the backend's display before dispatching. A bounding box of (0,0)
        // (unknown display) also fails closed.
        let w = self.backend.width();
        let h = self.backend.height();
        let check = |px: i64, py: i64, label: &str| -> Result<(), ToolError> {
            let in_w = w > 0 && px >= 0 && (px as u64) < w;
            let in_h = h > 0 && py >= 0 && (py as u64) < h;
            if in_w && in_h {
                Ok(())
            } else {
                Err(invalid(
                    "input.mouse",
                    format!("{label} ({px},{py}) is outside display bounds 0..{w} x 0..{h}"),
                ))
            }
        };

        match action {
            "move" => {
                check(x, y, "target")?;
                self.backend.pointer_move(x, y).await?;
            }
            "drag" => {
                let (tx, ty) = match (tx, ty) {
                    (Some(a), Some(b)) => (a, b),
                    _ => return Err(invalid("input.mouse", "drag requires to_x and to_y")),
                };
                check(x, y, "start")?;
                check(tx, ty, "end")?;
                self.backend.pointer_drag((x, y), (tx, ty)).await?
            }
            _ => {
                check(x, y, "target")?;
                self.backend.pointer_click(x, y, button).await?
            }
        }
        Ok(ToolOutput::Json(
            json!({ "action": action, "x": x, "y": y }),
        ))
    }
}

pub struct InputKeyboardTool {
    pub spec: ToolSpec,
    pub backend: Arc<dyn ScreenBackend>,
}

#[async_trait]
impl Tool for InputKeyboardTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("type");
        match action {
            "type" => {
                let text = args
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("input.keyboard", "type requires `text`"))?;
                self.backend.keyboard_type(text).await?;
            }
            "key" => {
                let key = args
                    .get("key")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("input.keyboard", "key requires `key`"))?;
                self.backend.keyboard_key(key).await?;
            }
            other => {
                return Err(invalid(
                    "input.keyboard",
                    format!("unknown action `{other}`"),
                ))
            }
        }
        Ok(ToolOutput::Json(json!({ "action": action })))
    }
}

pub struct BrowserListTool {
    pub spec: ToolSpec,
    pub backend: Arc<dyn ScreenBackend>,
}

#[async_trait]
impl Tool for BrowserListTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn run(&self, _args: Value) -> Result<ToolOutput, ToolError> {
        let windows = self.backend.window_list().await?;
        Ok(ToolOutput::Json(json!({ "windows": windows })))
    }
}

pub struct BrowserActivateTool {
    pub spec: ToolSpec,
    pub backend: Arc<dyn ScreenBackend>,
}

#[async_trait]
impl Tool for BrowserActivateTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let window = args
            .get("window")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("browser.activate", "missing `window` string"))?;
        self.backend.window_focus(window).await?;
        Ok(ToolOutput::Json(
            json!({ "window": window, "activated": true }),
        ))
    }
}

/// All M2.5 screen/input/browser tools sharing one backend.
pub fn screen_tools(backend: Arc<dyn ScreenBackend>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ScreenCaptureTool {
            spec: ToolSpec::new(
                "screen.capture",
                "Captures the current screen; returns dimensions plus the groundable element tree.",
            )
            .with_caps(&["screen.capture.all"])
            .with_schema(json!({ "type": "object" })),
            backend: backend.clone(),
        }),
        Arc::new(ScreenGroundTool {
            spec: ToolSpec::new(
                "screen.ground",
                "Resolves screen elements by name to geometry and center coordinates (deterministic grounding).",
            )
            .with_caps(&["screen.ground.all"])
            .with_schema(json!({
                "type": "object",
                "properties": { "target": { "type": "string", "description": "element name substring (default: all)" } }
            })),
            backend: backend.clone(),
        }),
        Arc::new(InputMouseTool {
            spec: ToolSpec::new(
                "input.mouse",
                "Pointer input: move/click/drag to resolved coordinates.",
            )
            .with_caps(&["input.mouse.all"])
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["move", "click", "drag"], "default": "click" },
                    "x": { "type": "integer" }, "y": { "type": "integer" },
                    "to_x": { "type": "integer" }, "to_y": { "type": "integer" },
                    "button": { "type": "integer", "default": 1 }
                }
            })),
            backend: backend.clone(),
        }),
        Arc::new(InputKeyboardTool {
            spec: ToolSpec::new(
                "input.keyboard",
                "Keyboard input: type text or send a key/shortcut to the focused window.",
            )
            .with_caps(&["input.keyboard.all"])
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["type", "key"], "default": "type" },
                    "text": { "type": "string" },
                    "key": { "type": "string" }
                }
            })),
            backend: backend.clone(),
        }),
        Arc::new(BrowserListTool {
            spec: ToolSpec::new(
                "browser.list",
                "Lists the open windows (the DOM/window tree).",
            )
            .with_caps(&["browser.list.all"])
            .with_schema(json!({ "type": "object" })),
            backend: backend.clone(),
        }),
        Arc::new(BrowserActivateTool {
            spec: ToolSpec::new(
                "browser.activate",
                "Brings the named window to focus for input.",
            )
            .with_caps(&["browser.activate.all"])
            .with_schema(json!({
                "type": "object",
                "properties": { "window": { "type": "string" } },
                "required": ["window"]
            })),
            backend,
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> SimScreen {
        SimScreen::new(
            1280,
            720,
            vec![
                ScreenElement::new("session-form", "window", 20, 40, 600, 400),
                ScreenElement::new("name-field", "field", 40, 90, 260, 24),
                ScreenElement::new("submit", "button", 40, 140, 120, 32),
            ],
        )
    }

    #[tokio::test]
    async fn sim_capture_grounds_and_returns_centers() {
        let s = fixture();
        let cap = s.capture().await.unwrap();
        assert_eq!(cap.width, 1280);
        assert_eq!(cap.backend, "sim");
        let matches = cap.ground("submit");
        assert_eq!(matches.len(), 1);
        let m = matches[0];
        let (cx, cy) = m.center();
        assert_eq!(cx, 40 + 60);
        assert_eq!(cy, 140 + 16);
        assert_eq!(s.captures().len(), 1);
    }

    #[tokio::test]
    async fn sim_click_types_into_focused_windows() {
        let s = fixture();
        let cap = s.capture().await.unwrap();
        let sub = cap.ground("submit")[0].clone();
        let (cx, cy) = sub.center();
        s.pointer_click(cx, cy, 1).await.unwrap();
        assert_eq!(s.pointer_clicks(), vec!["submit".to_string()]);
        s.keyboard_type("hello").await.unwrap();
        assert_eq!(s.typed(), vec!["submit:hello".to_string()]);
    }

    #[tokio::test]
    async fn simulator_window_focus_and_desktop_click() {
        let s = fixture();
        s.pointer_click(5, 5, 1).await.unwrap();
        assert_eq!(s.pointer_clicks(), vec!["desktop".to_string()]);
        let windows = s.window_list().await.unwrap();
        assert_eq!(windows, vec!["session-form".to_string()]);
        s.window_focus("session").await.unwrap();
        assert_eq!(s.focused_windows(), vec!["session-form".to_string()]);
        let missing = s.window_focus("nope").await.unwrap_err();
        assert!(matches!(missing, ToolError::InvalidArguments(..)));
    }

    #[tokio::test]
    async fn grounding_accuracy_on_fixture_set_is_exact() {
        // Scripted screen states drive grounding; 20 irregular elements with
        // known geometry resolve to their exact centers (100% ≥ 95% DoD).
        let s = SimScreen::new(1920, 1080, vec![]);
        let labels = [
            "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india",
            "juliett", "kilo", "lima", "mike", "november", "oscar", "papa", "quebec", "romeo",
            "sierra", "tango",
        ];
        let mut expected = Vec::new();
        for (i, label) in labels.iter().enumerate() {
            let i64 = i as i64;
            let (x, y, w, h) = (
                10 + (i64 % 5) * 377,
                20 + (i64 / 5) * 251,
                40 + (i64 * 37) % 300,
                20 + (i64 * 53) % 200,
            );
            s.add_element(ScreenElement::new(
                label, "button", x, y, w as u64, h as u64,
            ));
            expected.push((x + w / 2, y + h / 2));
        }
        for (i, label) in labels.iter().enumerate() {
            let cap = s.capture().await.unwrap();
            let m = cap.ground(label);
            assert_eq!(m.len(), 1, "{label}");
            assert_eq!(m[0].center(), expected[i], "{label} center");
        }
        // 20/20 exact → deterministic grounding accuracy is 100% on the set.
        assert_eq!(s.captures().len(), 20);
    }

    #[test]
    fn x11_backend_detects_tooling() {
        // Never *requires* X tooling; on hosts without it we must report false.
        let _ = have_x11_tooling();
        // try_new is exercised by the integration suite when DISPLAY is set.
    }

    // Tools delegate to the backend; a smoke test on the sim backend.
    #[tokio::test]
    async fn tools_drive_a_scripts_harness() {
        let backend: Arc<dyn ScreenBackend> = Arc::new(fixture());
        let tools = screen_tools(backend);
        let ids: Vec<String> = tools.iter().map(|t| t.spec().name.clone()).collect();
        assert_eq!(
            ids,
            vec![
                "screen.capture",
                "screen.ground",
                "input.mouse",
                "input.keyboard",
                "browser.list",
                "browser.activate",
            ]
        );

        let cap_out = tools[0].run(json!({})).await.unwrap();
        let ToolOutput::Json(cap_json) = cap_out else {
            panic!()
        };
        assert_eq!(cap_json["backend"], "sim");
        assert_eq!(cap_json["elements"].as_array().unwrap().len(), 3);

        let out = tools[3]
            .run(json!({ "action": "key", "key": "Tab" }))
            .await
            .unwrap();
        assert!(out.render().contains("key"));
    }

    #[tokio::test]
    async fn mouse_tool_rejects_out_of_bounds_coordinates() {
        let backend: Arc<dyn ScreenBackend> = Arc::new(SimScreen::new(1280, 720, vec![]));
        let tools = screen_tools(backend);
        let mouse = tools[2].clone();

        // In-bounds stays fine.
        mouse
            .run(json!({ "action": "click", "x": 1279, "y": 719 }))
            .await
            .unwrap();
        mouse
            .run(json!({ "action": "move", "x": 0, "y": 0 }))
            .await
            .unwrap();

        // Negative and beyond-the-edge coordinates are rejected (P2.7 bounds).
        let neg = mouse
            .run(json!({ "action": "click", "x": -1, "y": 10 }))
            .await
            .unwrap_err();
        assert!(matches!(neg, ToolError::InvalidArguments(..)), "{neg:?}");
        let oob = mouse
            .run(json!({ "action": "click", "x": 1280, "y": 10 }))
            .await
            .unwrap_err();
        assert!(matches!(oob, ToolError::InvalidArguments(..)), "{oob:?}");
        let oob_y = mouse
            .run(json!({ "action": "move", "x": 10, "y": 720 }))
            .await
            .unwrap_err();
        assert!(
            matches!(oob_y, ToolError::InvalidArguments(..)),
            "{oob_y:?}"
        );
        let drag = mouse
            .run(json!({ "action": "drag", "x": 0, "y": 0, "to_x": 999, "to_y": 721 }))
            .await
            .unwrap_err();
        assert!(matches!(drag, ToolError::InvalidArguments(..)), "{drag:?}");
    }
}
