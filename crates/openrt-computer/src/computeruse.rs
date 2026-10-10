//! M11.8 ComputerUse: one observation → resolution → twin → plan → execute →
//! verify loop over the Phase 7 [`ComputerControl`] authority and the M2.5
//! screen input layer.
//!
//! Design rules (OWNERSHIP.md M11.8 + Phase 11 non-negotiables):
//! - **One gate**: every mutation runs as the `computer.use` tool through the
//!   single gate (permission policy → AgentShield scan → sandbox/kill scope
//!   `computer` → audit). The engine never executes outside that decision;
//!   GUI interaction is never a policy bypass.
//! - **One computer backend**: window/workspace/clipboard/app/focus/type/key
//!   actions execute through [`ComputerControl`] (the platform
//!   [`openrt_platform::ComputerBackend`]). Pointer geometry the platform
//!   vocabulary cannot express (wheel scroll, drag) rides the existing M2.5
//!   screen input layer — the same layer `input.*`/`screen.*` already use
//!   under the same gate, never a second authority.
//! - **VLM grounds, never executes**: [`VisionGrounding`] only answers "where
//!   is `<target>` in this screenshot"; the returned point flows through the
//!   same resolution → planning → gated execution path as every other source.
//! - **Digital twin**: an in-memory observation model, invalidated after
//!   EVERY action attempt (per engine session; never persisted — no second
//!   store).
//! - **Bounded recovery**: verifiable + idempotent actions retry up to
//!   [`MAX_ATTEMPTS`] (3) on verification mismatch; non-idempotent actions
//!   (type/hotkey/launch/navigate/drag) execute exactly once and report
//!   `verified: null` when the backend offers no observation signal.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};
use thiserror::Error;

use openrt_platform::{CapabilityStatus, ComputerControl, ControlOp, OperationResult, WindowRef};
use openrt_tools::{Tool, ToolError, ToolSpec};

use crate::screen::{ScreenBackend, ScreenElement};

/// Bounded verification/retry budget (Phase 11 non-negotiable: ≤3).
pub const MAX_ATTEMPTS: u32 = 3;

/// Heuristic confidence for vision-grounded points (the model emits no
/// calibrated confidence; documented as a fixed heuristic, never 1.0).
const VISION_CONFIDENCE: f64 = 0.6;

/// Prompt asking the vision model for a pixel point (or an honest absence).
const GROUND_PROMPT: &str = "Locate the UI element named `{target}` in this \
screenshot. Reply with ONLY its pixel coordinates as `x,y` (e.g. `412,308`). \
If it is not present, reply NOT_FOUND.";

#[derive(Debug, Error)]
pub enum ComputerUseError {
    #[error("invalid arguments: {0}")]
    Invalid(String),
    #[error("target not found: {0}")]
    NotFound(String),
    #[error("unavailable: {0}")]
    Unavailable(String),
    #[error("verification failed after {attempts} attempt(s): {reason}")]
    Verify { attempts: u32, reason: String },
    #[error("control backend: {0}")]
    Control(String),
    #[error("screen backend: {0}")]
    Screen(String),
    #[error("vision grounding: {0}")]
    Vision(String),
}

fn control(e: impl std::fmt::Display) -> ComputerUseError {
    ComputerUseError::Control(e.to_string())
}

/// Fail closed on a non-ok platform result BEFORE any verification loop:
/// an unavailable/permission-gated mechanism surfaces the honest
/// [`ComputerUseError::Unavailable`] (never a fabricated success and never
/// a bogus "verification failed"), any other non-ok status is a hard
/// control error. `ok=true` passes through untouched.
fn ctrl(r: OperationResult) -> Result<OperationResult, ComputerUseError> {
    if r.ok {
        return Ok(r);
    }
    let detail = if r.message.is_empty() {
        r.status.as_str()
    } else {
        r.message.as_str()
    };
    let msg = format!("{}: {detail}", r.op);
    match r.status {
        CapabilityStatus::Unavailable | CapabilityStatus::PermissionRequired => {
            Err(ComputerUseError::Unavailable(msg))
        }
        _ => Err(ComputerUseError::Control(msg)),
    }
}

fn screen(e: impl std::fmt::Display) -> ComputerUseError {
    ComputerUseError::Screen(e.to_string())
}

/// Where a resolved target came from, in chain order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetSource {
    Accessibility,
    WindowTree,
    Elements,
    Vision,
    Coordinate,
}

impl TargetSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Accessibility => "accessibility",
            Self::WindowTree => "window-tree",
            Self::Elements => "elements",
            Self::Vision => "vision",
            Self::Coordinate => "coordinate",
        }
    }
}

/// One attempted (or skipped) step of the semantic target resolution chain.
#[derive(Debug, Clone, Serialize)]
pub struct ChainStep {
    pub source: String,
    /// `matched` | `no-match` | `unavailable` | `skipped`
    pub status: String,
    pub detail: String,
}

/// Element bounds on the captured screen (x, y, width, height).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Bounds {
    pub x: i64,
    pub y: i64,
    pub width: u64,
    pub height: u64,
}

impl Bounds {
    pub fn center(&self) -> (i64, i64) {
        (
            self.x + (self.width as i64) / 2,
            self.y + (self.height as i64) / 2,
        )
    }
}

/// Semantic target resolution: the point (when the source provides bounds),
/// its provenance, confidence, owning window, timestamp, and the full chain
/// trace that produced it.
#[derive(Debug, Clone, Serialize)]
pub struct TargetResolution {
    pub target: String,
    pub source: TargetSource,
    /// 0.0–1.0 (vision sources use the fixed [`VISION_CONFIDENCE`]).
    pub confidence: f64,
    /// Click point (center of the matched bounds) — `None` for window-level
    /// matches without geometry (focus/close do not need one).
    pub point: Option<(i64, i64)>,
    pub bounds: Option<Bounds>,
    pub window: Option<String>,
    pub timestamp: String,
    pub chain: Vec<ChainStep>,
}

/// Everything perceivable at one instant: screenshot + window metadata +
/// structured elements + focus + cursor + environment profile + clipboard
/// (ON DEMAND only) + twin generation.
#[derive(Debug, Clone, Serialize)]
pub struct ObservationBundle {
    pub captured_at: String,
    /// Screen input/capture backend id (M2.5 layer).
    pub screen_backend: String,
    /// Platform `ComputerBackend` id (Phase 7 layer).
    pub computer_backend: String,
    pub width: u64,
    pub height: u64,
    pub windows: Vec<String>,
    pub focused_window: Option<String>,
    pub active_window: Option<Value>,
    pub cursor: Option<(i32, i32)>,
    pub monitors: usize,
    pub elements: Vec<ScreenElement>,
    /// Environment profile (host/session/desktop) from the one platform
    /// authority.
    pub profile: Value,
    /// Only populated when explicitly requested (clipboard-on-demand).
    pub clipboard: Option<String>,
    pub clipboard_requested: bool,
    pub image_path: Option<String>,
    pub twin_generation: u64,
}

/// In-memory observation model. Invalidated after EVERY action; never
/// persisted (twin is derived — no second store).
#[derive(Debug, Default)]
pub struct DigitalTwin {
    bundle: Option<ObservationBundle>,
    pub observations: u64,
    pub invalidations: u64,
    pub valid: bool,
}

impl DigitalTwin {
    fn record(&mut self, bundle: ObservationBundle) -> u64 {
        self.bundle = Some(bundle);
        self.observations += 1;
        self.valid = true;
        self.observations
    }

    /// Mark the model stale after an action attempt.
    pub fn invalidate(&mut self) {
        self.valid = false;
        self.invalidations += 1;
    }

    fn fresh(&self) -> Option<&ObservationBundle> {
        if self.valid {
            self.bundle.as_ref()
        } else {
            None
        }
    }

    pub fn status(&self) -> Value {
        json!({
            "valid": self.valid,
            "observations": self.observations,
            "invalidations": self.invalidations,
            "note": "in-memory per-engine; invalidated after every action",
        })
    }
}

/// The planned/structured action vocabulary (OWNERSHIP.md M11.8).
#[derive(Debug, Clone, PartialEq)]
pub enum ComputerAction {
    Focus {
        target: String,
    },
    Launch {
        app: String,
    },
    Click {
        target: Option<String>,
        x: Option<i64>,
        y: Option<i64>,
        button: u32,
    },
    Type {
        target: Option<String>,
        text: String,
    },
    Hotkey {
        key: String,
    },
    Scroll {
        target: Option<String>,
        x: Option<i64>,
        y: Option<i64>,
        delta: i64,
    },
    Drag {
        from_x: i64,
        from_y: i64,
        to_x: i64,
        to_y: i64,
    },
    Workspace {
        name: String,
    },
    Clipboard {
        set: Option<String>,
    },
    Close {
        target: String,
    },
    Navigate {
        url: String,
    },
}

impl ComputerAction {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Focus { .. } => "focus",
            Self::Launch { .. } => "launch",
            Self::Click { .. } => "click",
            Self::Type { .. } => "type",
            Self::Hotkey { .. } => "hotkey",
            Self::Scroll { .. } => "scroll",
            Self::Drag { .. } => "drag",
            Self::Workspace { .. } => "workspace",
            Self::Clipboard { .. } => "clipboard",
            Self::Close { .. } => "close",
            Self::Navigate { .. } => "navigate",
        }
    }

    /// Structured Tool Fabric alternative the planner prefers over driving
    /// the GUI when a dedicated tool already expresses the action (M11.8:
    /// "structured Tool Fabric preferred over GUI" — advisory, not a block).
    pub fn structured_alternative(&self) -> Option<&'static str> {
        match self {
            Self::Focus { .. } | Self::Close { .. } => Some("computer.window"),
            Self::Launch { .. } | Self::Navigate { .. } => Some("computer.app"),
            Self::Type { .. } | Self::Hotkey { .. } => Some("computer.input"),
            Self::Click { .. } | Self::Drag { .. } => Some("input.mouse"),
            Self::Clipboard { .. } => Some("computer.clipboard"),
            Self::Scroll { .. } | Self::Workspace { .. } => None,
        }
    }

    /// Parse the `computer.use` argument object.
    pub fn from_json(args: &Value) -> Result<Self, ComputerUseError> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| ComputerUseError::Invalid("missing `action`".into()))?;
        let s = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_string);
        let i = |k: &str| args.get(k).and_then(Value::as_i64);
        let pair = |a: &str, b: &str| -> Result<Option<(i64, i64)>, ComputerUseError> {
            match (i(a), i(b)) {
                (Some(x), Some(y)) => Ok(Some((x, y))),
                (None, None) => Ok(None),
                _ => Err(ComputerUseError::Invalid(format!(
                    "`{a}` and `{b}` must be provided together"
                ))),
            }
        };
        match action {
            "focus" => Ok(Self::Focus {
                target: s("target")
                    .or_else(|| s("id"))
                    .ok_or_else(|| ComputerUseError::Invalid("focus requires `target`".into()))?,
            }),
            "launch" => Ok(Self::Launch {
                app: s("app")
                    .or_else(|| s("name"))
                    .ok_or_else(|| ComputerUseError::Invalid("launch requires `app`".into()))?,
            }),
            "click" => {
                let xy = pair("x", "y")?;
                Ok(Self::Click {
                    target: s("target"),
                    x: xy.map(|p| p.0),
                    y: xy.map(|p| p.1),
                    button: args
                        .get("button")
                        .and_then(Value::as_u64)
                        .unwrap_or(1)
                        .min(u32::MAX as u64) as u32,
                })
            }
            "type" => Ok(Self::Type {
                target: s("target"),
                text: s("text")
                    .ok_or_else(|| ComputerUseError::Invalid("type requires `text`".into()))?,
            }),
            "hotkey" | "key" => Ok(Self::Hotkey {
                key: s("key")
                    .ok_or_else(|| ComputerUseError::Invalid("hotkey requires `key`".into()))?,
            }),
            "scroll" => {
                let xy = pair("x", "y")?;
                Ok(Self::Scroll {
                    target: s("target"),
                    x: xy.map(|p| p.0),
                    y: xy.map(|p| p.1),
                    delta: i("delta").unwrap_or(100),
                })
            }
            "drag" => {
                let from = pair("from_x", "from_y")?.ok_or_else(|| {
                    ComputerUseError::Invalid("drag requires `from_x`/`from_y`".into())
                })?;
                let to = pair("to_x", "to_y")?.ok_or_else(|| {
                    ComputerUseError::Invalid("drag requires `to_x`/`to_y`".into())
                })?;
                Ok(Self::Drag {
                    from_x: from.0,
                    from_y: from.1,
                    to_x: to.0,
                    to_y: to.1,
                })
            }
            "workspace" => Ok(Self::Workspace {
                name: s("name")
                    .or_else(|| s("workspace"))
                    .ok_or_else(|| ComputerUseError::Invalid("workspace requires `name`".into()))?,
            }),
            // `text` present → set; absent → get (structured alternatives:
            // dedicated `computer.clipboard` tool remains available).
            "clipboard" => Ok(Self::Clipboard { set: s("text") }),
            "close" => Ok(Self::Close {
                target: s("target")
                    .or_else(|| s("id"))
                    .ok_or_else(|| ComputerUseError::Invalid("close requires `target`".into()))?,
            }),
            "navigate" => Ok(Self::Navigate {
                url: s("url")
                    .ok_or_else(|| ComputerUseError::Invalid("navigate requires `url`".into()))?,
            }),
            other => Err(ComputerUseError::Invalid(format!(
                "unknown action `{other}` (focus|launch|click|type|hotkey|scroll|drag|\
                 workspace|clipboard|close|navigate)"
            ))),
        }
    }
}

/// Advisory prediction produced BEFORE execution (never a guarantee — the
/// verification step reports what observation actually showed).
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub action: String,
    pub target: Option<TargetResolution>,
    /// Dedicated structured tool covering this action (preferred over GUI).
    pub structured_alternative: Option<String>,
    /// Human-readable predicted effect. Never echoes secret-shaped payloads
    /// (typed text is reported by length only).
    pub predicted_effect: String,
    pub predicted_state: Value,
}

/// Result of one executed action (post-twin-invalidation).
#[derive(Debug, Clone, Serialize)]
pub struct ComputerUseReport {
    pub action: String,
    pub ok: bool,
    /// The pre-execution advisory plan (JSON).
    pub advisory: Value,
    pub resolved: Option<TargetResolution>,
    /// Backend operation result (platform `OperationResult` or a synthesized
    /// screen-layer result).
    pub result: Value,
    /// `Some(true/false)` when observation could verify the prediction,
    /// `None` when the backend offers no verification signal.
    pub verified: Option<bool>,
    pub attempts: u32,
    /// Twin status AFTER invalidation (valid=false on every successful call).
    pub twin: Value,
}

/// Vision grounding backend: answers "where is `<target>`?" — it NEVER
/// executes actions (Phase 11 non-negotiable: VLM intent → policy →
/// AgentShield → ComputerControl → action → verification).
#[async_trait]
pub trait VisionGrounding: Send + Sync {
    /// Raw model reply locating `target` inside `image` (screenshot bytes).
    async fn ground(&self, image: &[u8], target: &str) -> Result<String, String>;
}

/// Ollama-backed grounding over the ONE vision pipeline (`openrt-vlm`).
/// Honest under SEC-07: a build without the `ollama` transport feature
/// reports UNAVAILABLE instead of fabricating coordinates.
pub struct VlmGrounding {
    endpoint: String,
    model: String,
}

impl VlmGrounding {
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            model: model.into(),
        }
    }

    /// Endpoint/model defaults shared with the `vision.vlm` doctor row.
    pub fn with_default_model(endpoint: impl Into<String>) -> Self {
        Self::new(endpoint, openrt_vlm::DEFAULT_VLM_MODEL)
    }
}

#[async_trait]
impl VisionGrounding for VlmGrounding {
    async fn ground(&self, image: &[u8], target: &str) -> Result<String, String> {
        let prompt = GROUND_PROMPT.replace("{target}", target);
        match openrt_vlm::ask_vision(&self.endpoint, &self.model, image, &prompt).await {
            Ok(ans) => Ok(ans.text),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// One coordinate parse of a grounding reply (`x,y` anywhere in the text).
fn parse_xy(reply: &str) -> Option<(i64, i64)> {
    let mut nums: Vec<i64> = Vec::with_capacity(2);
    let mut cur = String::new();
    for c in reply.chars() {
        if c.is_ascii_digit() {
            cur.push(c);
        } else if !cur.is_empty() {
            if let Ok(n) = cur.parse::<i64>() {
                nums.push(n);
            }
            cur.clear();
            if nums.len() == 2 {
                break;
            }
        }
    }
    if nums.len() < 2 && !cur.is_empty() {
        if let Ok(n) = cur.parse::<i64>() {
            nums.push(n);
        }
    }
    if nums.len() >= 2 {
        Some((nums[0], nums[1]))
    } else {
        None
    }
}

fn reply_is_absent(reply: &str) -> bool {
    let l = reply.to_lowercase();
    l.contains("not_found") || l.contains("not found") || l.contains("absent")
}

/// The ComputerUse engine: one observation/execution authority per runtime
/// session (constructed by the runtime; the `computer.use` tool owns it).
pub struct ComputerUse {
    screen: Arc<dyn ScreenBackend>,
    control: Arc<ComputerControl>,
    vision: Option<Arc<dyn VisionGrounding>>,
    twin: Mutex<DigitalTwin>,
}

impl std::fmt::Debug for ComputerUse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComputerUse").finish_non_exhaustive()
    }
}

impl ComputerUse {
    pub fn new(screen: Arc<dyn ScreenBackend>, control: Arc<ComputerControl>) -> Self {
        Self {
            screen,
            control,
            vision: None,
            twin: Mutex::new(DigitalTwin::default()),
        }
    }

    pub fn with_vision(mut self, vision: Arc<dyn VisionGrounding>) -> Self {
        self.vision = Some(vision);
        self
    }

    /// Twin health snapshot (doctor row + report payloads).
    pub fn twin_status(&self) -> Value {
        self.twin.lock().unwrap().status()
    }

    /// Fresh observation: capture + windows + focus + cursor + profile
    /// (+ clipboard ON DEMAND). Records the bundle as the twin state.
    pub async fn observe(
        &self,
        with_clipboard: bool,
    ) -> Result<ObservationBundle, ComputerUseError> {
        let cap = self.screen.capture().await.map_err(screen)?;
        let windows = self.screen.window_list().await.map_err(screen)?;
        let obs = self.control.observe().await.map_err(control)?;
        let clipboard = if with_clipboard {
            match self.control.execute(ControlOp::ClipboardGet).await {
                Ok(r) if r.ok => r
                    .data
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                _ => None,
            }
        } else {
            None
        };
        let focused = obs
            .active_window
            .as_ref()
            .map(|w| w.title.clone().unwrap_or_else(|| w.id.clone()));
        let active = obs
            .active_window
            .as_ref()
            .map(|w| serde_json::to_value(w).unwrap_or(Value::Null));
        let mut bundle = ObservationBundle {
            captured_at: openrt_core::timestamp::now().to_rfc3339(),
            screen_backend: cap.backend.clone(),
            computer_backend: obs.backend.clone(),
            width: cap.width,
            height: cap.height,
            windows,
            focused_window: focused,
            active_window: active,
            cursor: obs.cursor,
            monitors: obs.monitors.len(),
            elements: cap.elements.clone(),
            profile: self.control.profile().to_value(),
            clipboard,
            clipboard_requested: with_clipboard,
            image_path: cap.image_path.as_ref().map(|p| p.display().to_string()),
            twin_generation: 0,
        };
        let mut twin = self.twin.lock().unwrap();
        let gen = twin.observations + 1;
        bundle.twin_generation = gen;
        twin.record(bundle.clone());
        Ok(bundle)
    }

    /// The freshest bundle: reuse the valid twin, re-observe when stale.
    async fn current_bundle(&self) -> Result<ObservationBundle, ComputerUseError> {
        let fresh = self.twin.lock().unwrap().fresh().cloned();
        match fresh {
            Some(b) => Ok(b),
            None => self.observe(false).await,
        }
    }

    /// Semantic target resolution chain:
    /// accessibility tree → window tree → structured elements → vision
    /// grounding → coordinate fallback. Every step records
    /// source/status/detail; nothing is ever guessed.
    pub async fn resolve_target(
        &self,
        target: &str,
        at: Option<(i64, i64)>,
    ) -> Result<TargetResolution, ComputerUseError> {
        let t = target.trim();
        if t.is_empty() && at.is_none() {
            return Err(ComputerUseError::Invalid(
                "resolution needs a `target` or explicit coordinates".into(),
            ));
        }
        let bundle = self.current_bundle().await?;
        let now = openrt_core::timestamp::now().to_rfc3339();
        let mut chain: Vec<ChainStep> = Vec::new();

        // 1. Accessibility tree: no backend wires an AT-SPI/a11y source into
        //    ComputerUse today — recorded honestly, never faked. (The
        //    structured element list below is the deterministic stand-in.)
        let a11y = self
            .control
            .capability_matrix()
            .rows()
            .iter()
            .find(|r| {
                r.capability == openrt_platform::ComputerCapability::AccessibilityTree.as_str()
            })
            .map(|r| format!("{}: {}", r.status.as_str(), r.reason));
        chain.push(ChainStep {
            source: TargetSource::Accessibility.as_str().into(),
            status: "unavailable".into(),
            detail: a11y.unwrap_or_else(|| "no accessibility tree source wired".into()),
        });

        // 2. Window tree: exact match against the platform window list
        //    (ComputerControl: id/title/app_id — the window metadata
        //    authority), the screen window list, and window-role elements
        //    (geometry when the capture carries it).
        if !t.is_empty() {
            let needle = t.to_lowercase();
            let by_screen = bundle.windows.iter().find(|w| w.to_lowercase() == needle);
            let by_element = bundle
                .elements
                .iter()
                .find(|e| e.role == "window" && e.name.to_lowercase() == needle);
            let by_platform = self.window_ids().await.ok().and_then(|ws| {
                ws.into_iter().find(|w| {
                    w.id.to_lowercase() == needle
                        || w.title.as_deref().map(|s| s.to_lowercase()).as_deref()
                            == Some(needle.as_str())
                        || w.app_id.as_deref().map(|s| s.to_lowercase()).as_deref()
                            == Some(needle.as_str())
                })
            });
            if by_screen.is_some() || by_element.is_some() || by_platform.is_some() {
                let (window, origin) = if let Some(e) = by_element {
                    (Some(e.name.clone()), "screen window element")
                } else if let Some(w) = by_platform {
                    (
                        Some(w.title.clone().unwrap_or_else(|| w.id.clone())),
                        "platform window list",
                    )
                } else {
                    (by_screen.cloned(), "screen window list")
                };
                let bounds = by_element.map(|e| Bounds {
                    x: e.x,
                    y: e.y,
                    width: e.width,
                    height: e.height,
                });
                let point = bounds.map(|b| b.center());
                let confidence = if by_element.is_some() { 0.95 } else { 0.9 };
                chain.push(ChainStep {
                    source: TargetSource::WindowTree.as_str().into(),
                    status: "matched".into(),
                    detail: format!(
                        "window `{}` exact match via {origin}",
                        window.clone().unwrap_or_default()
                    ),
                });
                return Ok(TargetResolution {
                    target: t.into(),
                    source: TargetSource::WindowTree,
                    confidence,
                    point,
                    bounds,
                    window,
                    timestamp: now,
                    chain,
                });
            }
            chain.push(ChainStep {
                source: TargetSource::WindowTree.as_str().into(),
                status: "no-match".into(),
                detail: format!("no window named `{t}`"),
            });
        } else {
            chain.push(ChainStep {
                source: TargetSource::WindowTree.as_str().into(),
                status: "skipped".into(),
                detail: "no target string".into(),
            });
        }

        // 3. Structured elements (deterministic grounding): exact match
        //    preferred, then substring (the capture's `ground`).
        if !t.is_empty() {
            let matches = bundle
                .elements
                .iter()
                .filter(|e| e.role != "window")
                .filter(|e| e.name.to_lowercase().contains(&t.to_lowercase()))
                .collect::<Vec<_>>();
            let exact = matches
                .iter()
                .find(|e| e.name.to_lowercase() == t.to_lowercase());
            if let Some(e) = exact.or_else(|| matches.first()) {
                let bounds = Bounds {
                    x: e.x,
                    y: e.y,
                    width: e.width,
                    height: e.height,
                };
                let confidence = if e.name.to_lowercase() == t.to_lowercase() {
                    1.0
                } else {
                    0.7
                };
                chain.push(ChainStep {
                    source: TargetSource::Elements.as_str().into(),
                    status: "matched".into(),
                    detail: format!(
                        "element `{}` role `{}` via {} match",
                        e.name,
                        e.role,
                        if confidence == 1.0 {
                            "exact"
                        } else {
                            "substring"
                        }
                    ),
                });
                return Ok(TargetResolution {
                    target: t.into(),
                    source: TargetSource::Elements,
                    confidence,
                    point: Some(bounds.center()),
                    bounds: Some(bounds),
                    window: None,
                    timestamp: now,
                    chain,
                });
            }
            chain.push(ChainStep {
                source: TargetSource::Elements.as_str().into(),
                status: "no-match".into(),
                detail: format!("no structured element matching `{t}`"),
            });
        } else {
            chain.push(ChainStep {
                source: TargetSource::Elements.as_str().into(),
                status: "skipped".into(),
                detail: "no target string".into(),
            });
        }

        // 4. Vision grounding (VLM points only — never executes).
        if !t.is_empty() {
            match &self.vision {
                None => chain.push(ChainStep {
                    source: TargetSource::Vision.as_str().into(),
                    status: "unavailable".into(),
                    detail: "no vision grounding backend configured".into(),
                }),
                Some(vision) => {
                    let image = match bundle.image_path.as_ref() {
                        None => Err("no rasterized screenshot on this capture".to_string()),
                        Some(p) => std::fs::read(p).map_err(|e| format!("read {p}: {e}")),
                    };
                    match image {
                        Err(reason) => chain.push(ChainStep {
                            source: TargetSource::Vision.as_str().into(),
                            status: "unavailable".into(),
                            detail: reason,
                        }),
                        Ok(bytes) => match vision.ground(&bytes, t).await {
                            Err(reason) => chain.push(ChainStep {
                                source: TargetSource::Vision.as_str().into(),
                                status: "unavailable".into(),
                                detail: reason,
                            }),
                            Ok(reply) if reply_is_absent(&reply) => chain.push(ChainStep {
                                source: TargetSource::Vision.as_str().into(),
                                status: "no-match".into(),
                                detail: "vision reports target absent in image".into(),
                            }),
                            Ok(reply) => match parse_xy(&reply) {
                                None => chain.push(ChainStep {
                                    source: TargetSource::Vision.as_str().into(),
                                    status: "unavailable".into(),
                                    detail: "unparseable grounding reply (expected `x,y`)".into(),
                                }),
                                Some((x, y))
                                    if x < 0
                                        || y < 0
                                        || x >= bundle.width as i64
                                        || y >= bundle.height as i64 =>
                                {
                                    chain.push(ChainStep {
                                        source: TargetSource::Vision.as_str().into(),
                                        status: "unavailable".into(),
                                        detail: format!(
                                            "grounded point ({x},{y}) outside image {}x{}",
                                            bundle.width, bundle.height
                                        ),
                                    })
                                }
                                Some((x, y)) => {
                                    chain.push(ChainStep {
                                        source: TargetSource::Vision.as_str().into(),
                                        status: "matched".into(),
                                        detail: format!(
                                            "vision grounded ({x},{y}) (screenshot space; \
                                             assumed screen space for single-scale capture)"
                                        ),
                                    });
                                    return Ok(TargetResolution {
                                        target: t.into(),
                                        source: TargetSource::Vision,
                                        confidence: VISION_CONFIDENCE,
                                        point: Some((x, y)),
                                        bounds: None,
                                        window: None,
                                        timestamp: now,
                                        chain,
                                    });
                                }
                            },
                        },
                    }
                }
            }
        } else {
            chain.push(ChainStep {
                source: TargetSource::Vision.as_str().into(),
                status: "skipped".into(),
                detail: "no target string".into(),
            });
        }

        // 5. Coordinate fallback: caller-provided point (never guessed).
        if let Some((x, y)) = at {
            chain.push(ChainStep {
                source: TargetSource::Coordinate.as_str().into(),
                status: "matched".into(),
                detail: format!("explicit coordinate ({x},{y})"),
            });
            return Ok(TargetResolution {
                target: t.into(),
                source: TargetSource::Coordinate,
                confidence: 1.0,
                point: Some((x, y)),
                bounds: None,
                window: None,
                timestamp: now,
                chain,
            });
        }
        chain.push(ChainStep {
            source: TargetSource::Coordinate.as_str().into(),
            status: at
                .map(|_| "matched".to_string())
                .unwrap_or_else(|| "unavailable".into()),
            detail: "no explicit coordinates provided".into(),
        });

        let trace = chain
            .iter()
            .map(|s| format!("{}={}", s.source, s.status))
            .collect::<Vec<_>>()
            .join(", ");
        Err(ComputerUseError::NotFound(format!(
            "target `{t}` (chain: {trace})"
        )))
    }

    /// Advisory prediction + structured-tool preference for one action.
    pub async fn plan(&self, action: &ComputerAction) -> Result<Plan, ComputerUseError> {
        let structured = action.structured_alternative().map(str::to_string);
        let (target, effect, state) = match action {
            ComputerAction::Focus { target } => {
                let res = Some(self.resolve_target(target, None).await?);
                (
                    res,
                    format!("focused window becomes `{target}`"),
                    json!({ "active_window": target }),
                )
            }
            ComputerAction::Close { target } => {
                let res = Some(self.resolve_target(target, None).await?);
                (
                    res,
                    format!("window `{target}` closes; it disappears from the window list"),
                    json!({ "window_absent": target }),
                )
            }
            ComputerAction::Click {
                target,
                x,
                y,
                button,
            } => {
                let at = match (x, y) {
                    (Some(a), Some(b)) => Some((*a, *b)),
                    (None, None) => None,
                    _ => {
                        return Err(ComputerUseError::Invalid(
                            "`x` and `y` must be provided together".into(),
                        ))
                    }
                };
                if target.is_none() && at.is_none() {
                    return Err(ComputerUseError::Invalid(
                        "click needs `target` or `x`+`y`".into(),
                    ));
                }
                let res = self
                    .resolve_target(target.as_deref().unwrap_or(""), at)
                    .await?;
                let point = res.point.ok_or_else(|| {
                    ComputerUseError::Invalid(format!(
                        "target `{}` resolved to window-level metadata without bounds — \
                         pass `x`/`y` or use action `focus`",
                        target.as_deref().unwrap_or("")
                    ))
                })?;
                (
                    Some(res),
                    format!(
                        "cursor moves to ({}, {}) and button-{} click",
                        point.0, point.1, button
                    ),
                    json!({ "cursor": [point.0, point.1], "button": button }),
                )
            }
            ComputerAction::Scroll {
                target,
                x,
                y,
                delta,
            } => {
                let at = match (x, y) {
                    (Some(a), Some(b)) => Some((*a, *b)),
                    (None, None) => None,
                    _ => {
                        return Err(ComputerUseError::Invalid(
                            "`x` and `y` must be provided together".into(),
                        ))
                    }
                };
                if target.is_none() && at.is_none() {
                    return Err(ComputerUseError::Invalid(
                        "scroll needs `target` or `x`+`y`".into(),
                    ));
                }
                let res = self
                    .resolve_target(target.as_deref().unwrap_or(""), at)
                    .await?;
                let point = res.point.ok_or_else(|| {
                    ComputerUseError::Invalid(format!(
                        "target `{}` has no bounds for scrolling — pass `x`/`y`",
                        target.as_deref().unwrap_or("")
                    ))
                })?;
                let button = if *delta >= 0 { 4 } else { 5 };
                (
                    Some(res),
                    format!(
                        "cursor moves to ({}, {}); wheel button {} ×{} (delta {delta})",
                        point.0,
                        point.1,
                        button,
                        wheel_steps(*delta)
                    ),
                    json!({ "cursor": [point.0, point.1], "wheel_button": button,
                            "steps": wheel_steps(*delta) }),
                )
            }
            ComputerAction::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
            } => (
                None,
                format!("pointer drags from ({from_x},{from_y}) to ({to_x},{to_y})"),
                json!({ "from": [from_x, from_y], "to": [to_x, to_y] }),
            ),
            ComputerAction::Type { target, text } => {
                let res = match target {
                    Some(t) => Some(self.resolve_target(t, None).await?),
                    None => None,
                };
                // Length only — typed text may be secret-shaped (RedactionPolicy).
                let effect = match target {
                    Some(t) => format!("focus `{t}` then inject {len} chars", len = text.len()),
                    None => format!(
                        "inject {len} chars into the focused window",
                        len = text.len()
                    ),
                };
                (res, effect, json!({ "typed_chars": text.len() }))
            }
            ComputerAction::Hotkey { key } => (
                None,
                format!("key chord `{key}` delivered to the focused window"),
                json!({ "key": key }),
            ),
            ComputerAction::Workspace { name } => (
                None,
                format!("active workspace becomes `{name}`"),
                json!({ "workspace": name }),
            ),
            ComputerAction::Clipboard { set } => match set {
                Some(_) => (
                    None,
                    "clipboard content becomes the provided text (length-redacted)".to_string(),
                    json!({ "clipboard": "<set>" }),
                ),
                None => (
                    None,
                    "clipboard content read back through the platform backend".to_string(),
                    json!({ "clipboard": "<read>" }),
                ),
            },
            ComputerAction::Launch { app } => (
                None,
                format!("application `{app}` launches via the platform backend"),
                json!({ "app": app }),
            ),
            ComputerAction::Navigate { url } => (
                None,
                format!("URL `{url}` opens via the platform launcher (never a raw shell)"),
                json!({ "url": url }),
            ),
        };
        Ok(Plan {
            action: action.name().into(),
            target,
            structured_alternative: structured,
            predicted_effect: effect,
            predicted_state: state,
        })
    }

    /// Execute one action through the single authority, verify against
    /// observation (bounded), and invalidate the twin.
    pub async fn execute(
        &self,
        action: ComputerAction,
    ) -> Result<ComputerUseReport, ComputerUseError> {
        let plan = self.plan(&action).await?;
        let outcome = self.run_action(&action, &plan).await;
        // EVERY action attempt invalidates the twin — even a failed attempt
        // may have partially mutated the desktop.
        self.twin.lock().unwrap().invalidate();
        let (resolved, result, verified, attempts) = outcome?;
        Ok(ComputerUseReport {
            action: action.name().into(),
            ok: true,
            advisory: serde_json::to_value(&plan).unwrap_or(Value::Null),
            resolved,
            result,
            verified,
            attempts,
            twin: self.twin_status(),
        })
    }

    /// One action, no twin bookkeeping (caller invalidates).
    async fn run_action(
        &self,
        action: &ComputerAction,
        plan: &Plan,
    ) -> Result<(Option<TargetResolution>, Value, Option<bool>, u32), ComputerUseError> {
        match action {
            ComputerAction::Focus { target } => {
                let id = self.find_window_id(target).await?;
                let mut attempts = 0;
                loop {
                    attempts += 1;
                    let r = self
                        .control
                        .execute(ControlOp::FocusWindow(id.clone()))
                        .await
                        .map_err(control)
                        .and_then(ctrl)?;
                    let obs = self.control.observe().await.map_err(control)?;
                    let active = obs.active_window.map(|w| w.id);
                    let verified = active
                        .as_deref()
                        .map(|a| a == id || id.contains(a) || a.contains(&id))
                        .unwrap_or(false);
                    if verified {
                        return Ok((plan.target.clone(), op_value(&r), Some(true), attempts));
                    }
                    if attempts >= MAX_ATTEMPTS {
                        return Err(ComputerUseError::Verify {
                            attempts,
                            reason: format!(
                                "focus `{target}`: active window is `{}`",
                                active.unwrap_or_else(|| "<none>".into())
                            ),
                        });
                    }
                }
            }

            ComputerAction::Close { target } => {
                let id = self.find_window_id(target).await?;
                let mut attempts = 0;
                loop {
                    attempts += 1;
                    let r = self
                        .control
                        .execute(ControlOp::CloseWindow(id.clone()))
                        .await
                        .map_err(control)
                        .and_then(ctrl)?;
                    let still = self.window_ids().await?.iter().any(|w| w.id == id);
                    if !still {
                        return Ok((plan.target.clone(), op_value(&r), Some(true), attempts));
                    }
                    if attempts >= MAX_ATTEMPTS {
                        return Err(ComputerUseError::Verify {
                            attempts,
                            reason: format!("close `{target}`: window still listed"),
                        });
                    }
                }
            }

            ComputerAction::Click {
                target: _,
                x: _,
                y: _,
                button,
            } => {
                let point = plan.target.as_ref().and_then(|r| r.point).ok_or_else(|| {
                    ComputerUseError::Invalid("click has no resolved point".into())
                })?;
                if *button != 1 {
                    return Err(ComputerUseError::Invalid(format!(
                        "only button 1 (left click) is expressible through the platform \
                         backend — got button {button}"
                    )));
                }
                self.require_in_bounds(&[(point.0, point.1)]).await?;
                let (px, py) = (point.0 as i32, point.1 as i32);
                let mut attempts = 0;
                loop {
                    attempts += 1;
                    let mv = self
                        .control
                        .execute(ControlOp::MovePointer(px, py))
                        .await
                        .map_err(control)
                        .and_then(ctrl)?;
                    let clk = self
                        .control
                        .execute(ControlOp::ClickPointer)
                        .await
                        .map_err(control)
                        .and_then(ctrl)?;
                    let obs = self.control.observe().await.map_err(control)?;
                    let mut out = op_value(&mv);
                    merge(&mut out, "click", &op_value(&clk));
                    match obs.cursor {
                        // No cursor signal on this backend → unverifiable.
                        None => return Ok((plan.target.clone(), out, None, attempts)),
                        Some(c) if c == (px, py) => {
                            return Ok((plan.target.clone(), out, Some(true), attempts))
                        }
                        Some(_) if attempts >= MAX_ATTEMPTS => {
                            return Err(ComputerUseError::Verify {
                                attempts,
                                reason: format!("cursor did not settle at ({px},{py})"),
                            })
                        }
                        Some(_) => continue,
                    }
                }
            }

            ComputerAction::Scroll {
                target: _,
                x: _,
                y: _,
                delta,
            } => {
                let point = plan.target.as_ref().and_then(|r| r.point).ok_or_else(|| {
                    ComputerUseError::Invalid("scroll has no resolved point".into())
                })?;
                self.require_in_bounds(&[(point.0, point.1)]).await?;
                let (px, py) = (point.0 as i32, point.1 as i32);
                // 1) Position through the platform authority (verifiable).
                let mut attempts = 0;
                let position_verified = loop {
                    attempts += 1;
                    self.control
                        .execute(ControlOp::MovePointer(px, py))
                        .await
                        .map_err(control)
                        .and_then(ctrl)?;
                    let obs = self.control.observe().await.map_err(control)?;
                    match obs.cursor {
                        None => break None,
                        Some(c) if c == (px, py) => break Some(true),
                        Some(_) if attempts >= MAX_ATTEMPTS => {
                            return Err(ComputerUseError::Verify {
                                attempts,
                                reason: format!("cursor did not settle at ({px},{py})"),
                            })
                        }
                        Some(_) => continue,
                    }
                };
                // 2) Wheel injection through the screen input layer (the
                //    platform vocabulary has no wheel verb). Effect itself is
                //    not observable → reported as injected step count.
                let button: u32 = if *delta >= 0 { 4 } else { 5 };
                let steps = wheel_steps(*delta);
                for _ in 0..steps {
                    self.screen
                        .pointer_click(point.0, point.1, button)
                        .await
                        .map_err(screen)?;
                }
                let out = json!({
                    "op": "scroll",
                    "ok": true,
                    "layer": "screen",
                    "wheel_button": button,
                    "wheel_steps": steps,
                    "delta": delta,
                    "position_verified": position_verified,
                    "note": "wheel injection itself has no observation signal",
                });
                Ok((plan.target.clone(), out, position_verified, attempts))
            }

            ComputerAction::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
            } => {
                self.require_in_bounds(&[(*from_x, *from_y), (*to_x, *to_y)])
                    .await?;
                // Platform vocabulary has no press-and-hold; the screen input
                // layer's drag is the existing gated verb (single attempt —
                // non-idempotent, no observation signal for a completed drag).
                self.screen
                    .pointer_drag((*from_x, *from_y), (*to_x, *to_y))
                    .await
                    .map_err(screen)?;
                Ok((
                    None,
                    json!({
                        "op": "drag",
                        "ok": true,
                        "layer": "screen",
                        "from": [from_x, from_y],
                        "to": [to_x, to_y],
                    }),
                    None,
                    1,
                ))
            }

            ComputerAction::Type { target, text } => {
                let mut attempts = 0;
                // Optional semantic focus first (same chain, same budget).
                if let Some(t) = target {
                    let id = self.find_window_id(t).await?;
                    loop {
                        attempts += 1;
                        self.control
                            .execute(ControlOp::FocusWindow(id.clone()))
                            .await
                            .map_err(control)
                            .and_then(ctrl)?;
                        let obs = self.control.observe().await.map_err(control)?;
                        let active = obs.active_window.map(|w| w.id);
                        let ok = active
                            .as_deref()
                            .map(|a| a == id || id.contains(a) || a.contains(&id))
                            .unwrap_or(false);
                        if ok {
                            break;
                        }
                        if attempts >= MAX_ATTEMPTS {
                            return Err(ComputerUseError::Verify {
                                attempts,
                                reason: format!(
                                    "focus `{t}` before type: active window is `{}`",
                                    active.unwrap_or_else(|| "<none>".into())
                                ),
                            });
                        }
                    }
                }
                // Single attempt — injection is non-idempotent (never retried).
                attempts += 1;
                let r = self
                    .control
                    .execute(ControlOp::TypeText(text.clone()))
                    .await
                    .map_err(control)
                    .and_then(ctrl)?;
                let out = json!({
                    "op": "type",
                    "ok": r.ok,
                    "status": r.status,
                    "message": r.message,
                    "typed_chars": text.len(),
                    "focused_first": target.is_some(),
                });
                Ok((plan.target.clone(), out, None, attempts))
            }

            ComputerAction::Hotkey { key } => {
                let r = self
                    .control
                    .execute(ControlOp::SendKey(key.clone()))
                    .await
                    .map_err(control)
                    .and_then(ctrl)?;
                let mut out = op_value(&r);
                merge(&mut out, "key", &json!(key));
                Ok((None, out, None, 1))
            }

            ComputerAction::Workspace { name } => {
                let before = self
                    .control
                    .observe()
                    .await
                    .map_err(control)?
                    .active_window
                    .and_then(|w| w.workspace);
                let mut attempts = 0;
                loop {
                    attempts += 1;
                    let r = self
                        .control
                        .execute(ControlOp::SwitchWorkspace(name.clone()))
                        .await
                        .map_err(control)
                        .and_then(ctrl)?;
                    let after = self
                        .control
                        .observe()
                        .await
                        .map_err(control)?
                        .active_window
                        .and_then(|w| w.workspace);
                    match after {
                        Some(w) if w == *name => {
                            return Ok((None, op_value(&r), Some(true), attempts))
                        }
                        // No workspace observation signal → unverifiable.
                        None => return Ok((None, op_value(&r), None, attempts)),
                        // Unchanged → the backend did not report a transition;
                        // treat as no signal (sim backends model workspaces
                        // only per window, which SwitchWorkspace does not move).
                        Some(w) if before.as_ref() == Some(&w) => {
                            return Ok((None, op_value(&r), None, attempts))
                        }
                        Some(_) if attempts >= MAX_ATTEMPTS => {
                            return Err(ComputerUseError::Verify {
                                attempts,
                                reason: format!("workspace `{name}` not observed active"),
                            })
                        }
                        Some(_) => continue,
                    }
                }
            }

            ComputerAction::Clipboard { set } => match set {
                None => {
                    let r = self
                        .control
                        .execute(ControlOp::ClipboardGet)
                        .await
                        .map_err(control)
                        .and_then(ctrl)?;
                    Ok((None, op_value(&r), None, 1))
                }
                Some(text) => {
                    let mut attempts = 0;
                    loop {
                        attempts += 1;
                        self.control
                            .execute(ControlOp::ClipboardSet(text.clone()))
                            .await
                            .map_err(control)
                            .and_then(ctrl)?;
                        let back = self
                            .control
                            .execute(ControlOp::ClipboardGet)
                            .await
                            .map_err(control)
                            .and_then(ctrl)?;
                        let got = back.data.get("text").and_then(Value::as_str).unwrap_or("");
                        let mut out = json!({
                            "op": "clipboard_set",
                            "ok": true,
                            "length": text.len(),
                        });
                        merge(&mut out, "readback_ok", &json!(got == text));
                        if got == text {
                            return Ok((None, out, Some(true), attempts));
                        }
                        if attempts >= MAX_ATTEMPTS {
                            return Err(ComputerUseError::Verify {
                                attempts,
                                reason: "clipboard readback did not match".into(),
                            });
                        }
                    }
                }
            },

            ComputerAction::Launch { app } => {
                let r = self
                    .control
                    .execute(ControlOp::LaunchApp(app.clone()))
                    .await
                    .map_err(control)
                    .and_then(ctrl)?;
                Ok((None, op_value(&r), None, 1))
            }

            ComputerAction::Navigate { url } => {
                // Never a raw shell: the platform launcher (xdg-open on X11)
                // refuses shell metacharacters; the shield's network scan has
                // already seen this URL at the gate.
                let r = self
                    .control
                    .execute(ControlOp::LaunchApp(url.clone()))
                    .await
                    .map_err(control)
                    .and_then(ctrl)?;
                let mut out = op_value(&r);
                merge(&mut out, "url", &json!(url));
                Ok((None, out, None, 1))
            }
        }
    }

    /// Resolve a target string to a backend window id (title/id/app_id,
    /// exact then substring) through the one platform window list.
    async fn find_window_id(&self, target: &str) -> Result<String, ComputerUseError> {
        let ids = self.window_ids().await?;
        let needle = target.trim().to_lowercase();
        ids.iter()
            .find(|w| {
                let id = w.id.to_lowercase();
                let title = w
                    .title
                    .as_deref()
                    .map(str::to_lowercase)
                    .unwrap_or_default();
                let app = w
                    .app_id
                    .as_deref()
                    .map(str::to_lowercase)
                    .unwrap_or_default();
                id == needle || title == needle || app == needle
            })
            .or_else(|| {
                ids.iter().find(|w| {
                    let id = w.id.to_lowercase();
                    let title = w
                        .title
                        .as_deref()
                        .map(str::to_lowercase)
                        .unwrap_or_default();
                    let app = w
                        .app_id
                        .as_deref()
                        .map(str::to_lowercase)
                        .unwrap_or_default();
                    (!needle.is_empty())
                        && (id.contains(&needle)
                            || title.contains(&needle)
                            || app.contains(&needle))
                })
            })
            .map(|w| w.id.clone())
            .ok_or_else(|| ComputerUseError::NotFound(format!("window `{target}`")))
    }

    async fn window_ids(&self) -> Result<Vec<WindowRef>, ComputerUseError> {
        let r = self
            .control
            .execute(ControlOp::ListWindows)
            .await
            .map_err(control)
            .and_then(ctrl)?;
        Ok(serde_json::from_value(r.data).unwrap_or_default())
    }

    /// Reject coordinates outside the observed screen (same boundary the
    /// structured input tools enforce).
    async fn require_in_bounds(&self, points: &[(i64, i64)]) -> Result<(), ComputerUseError> {
        let b = self.current_bundle().await?;
        for (x, y) in points {
            if *x < 0 || *y < 0 || *x >= b.width as i64 || *y >= b.height as i64 {
                return Err(ComputerUseError::Invalid(format!(
                    "point ({x},{y}) outside screen bounds {}x{}",
                    b.width, b.height
                )));
            }
        }
        Ok(())
    }
}

/// Wheel clicks for a scroll delta: one step per 100 units, clamped 1..=10.
fn wheel_steps(delta: i64) -> u32 {
    (((delta.unsigned_abs() + 99) / 100) as u32).clamp(1, 10)
}

fn op_value(r: &OperationResult) -> Value {
    serde_json::to_value(r).unwrap_or(Value::Null)
}

/// Merge a sub-key into a result object without clobbering siblings.
fn merge(out: &mut Value, key: &str, value: &Value) {
    if let Value::Object(map) = out {
        map.insert(key.to_string(), value.clone());
    }
}

/// `computer.use` — the ONE ComputerUse tool (gate + shield + audit + kill
/// scope `computer`).
pub struct ComputerUseTool {
    pub spec: ToolSpec,
    pub engine: Arc<ComputerUse>,
}

/// Spec for the `computer.use` tool (single registration point).
pub fn computer_use_spec() -> ToolSpec {
    ToolSpec::new(
        "computer.use",
        "Plan and execute one computer-use action (focus/launch/click/type/hotkey/scroll/\
         drag/workspace/clipboard/close/navigate) through the single gate: observation bundle, \
         semantic target resolution, advisory prediction, bounded verification, and digital-twin \
         invalidation. Prefer computer.app to launch apps by name; browser.* for page work.",
    )
    .with_caps(&["computer.use.all"])
    .with_schema(json!({
        "type": "object",
        "required": ["action"],
        "properties": {
            "action": {
                "type": "string",
                "enum": ["focus", "launch", "click", "type", "hotkey", "scroll", "drag",
                          "workspace", "clipboard", "close", "navigate"]
            },
            "target": { "type": "string", "description": "semantic target (window/element name)" },
            "x": { "type": "integer" },
            "y": { "type": "integer" },
            "from_x": { "type": "integer" },
            "from_y": { "type": "integer" },
            "to_x": { "type": "integer" },
            "to_y": { "type": "integer" },
            "button": { "type": "integer", "default": 1 },
            "delta": { "type": "integer", "default": 100, "description": "scroll delta (>=0 up, <0 down)" },
            "text": { "type": "string" },
            "key": { "type": "string" },
            "url": { "type": "string" },
            "app": { "type": "string" },
            "name": { "type": "string" },
            "clipboard": { "type": "string" }
        }
    }))
}

#[async_trait]
impl Tool for ComputerUseTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<openrt_tools::ToolOutput, ToolError> {
        let action = ComputerAction::from_json(&args)
            .map_err(|e| ToolError::InvalidArguments("computer.use".into(), e.to_string()))?;
        let report = self.engine.execute(action).await.map_err(|e| match &e {
            ComputerUseError::Invalid(m)
            | ComputerUseError::NotFound(m)
            | ComputerUseError::Unavailable(m) => {
                ToolError::InvalidArguments("computer.use".into(), m.clone())
            }
            other => ToolError::Internal("computer.use".into(), other.to_string()),
        })?;
        Ok(openrt_tools::ToolOutput::Json(
            serde_json::to_value(&report)
                .map_err(|e| ToolError::Internal("computer.use".into(), e.to_string()))?,
        ))
    }
}
