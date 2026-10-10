//! Agent Mode application state: chat stream, task lifecycle, approvals,
//! plan, and per-view data. Every field is either operator input or a view
//! over the ONE Runtime — nothing here invents activity.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use openrt_core::{DomainEvent, EventKind, MessageRole, Session, SessionId, Task, TaskId};
use openrt_runtime::Runtime;

use crate::agent::provider::SwitchableProvider;
use crate::agent::setup::SetupState;
use crate::agent::sink::{PendingApproval, TuiResolver};
use crate::app::{App, AppError};
use crate::design::focus::{FocusStop, FocusTarget, OverlayKeys, OverlayScope};
use crate::design::modals::{confirm_target, permission_target, GRANT_BUTTONS};

/// Which surface owns the terminal right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiMode {
    Agent,
    Workbench,
}

/// Which Settings sub-section holds the keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsFocus {
    /// Read-only runtime/config dump.
    Info,
    /// Theme picker: the Phase 1 editable surface.
    Theme,
}

impl SettingsFocus {
    pub const ALL: [SettingsFocus; 2] = [SettingsFocus::Info, SettingsFocus::Theme];
}

/// A semantic group of navigation-rail destinations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavGroup {
    Workspace,
    Build,
    Platform,
    System,
}

impl NavGroup {
    pub const ALL: [NavGroup; 4] = [
        NavGroup::Workspace,
        NavGroup::Build,
        NavGroup::Platform,
        NavGroup::System,
    ];

    /// Rail heading. Kept short so every group label fits a 10-column rail.
    pub fn label(self) -> &'static str {
        match self {
            NavGroup::Workspace => "WORKSPACE",
            NavGroup::Build => "BUILD",
            NavGroup::Platform => "PLATFORM",
            NavGroup::System => "SYSTEM",
        }
    }
}

/// Left-rail destination (Agent Mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Chat,
    Files,
    Search,
    Tasks,
    Sessions,
    Agents,
    Git,
    Terminal,
    Tools,
    Artifacts,
    Models,
    Mcp,
    Extensions,
    Security,
    Settings,
}

impl View {
    pub const ALL: [View; 15] = [
        View::Chat,
        View::Files,
        View::Search,
        View::Tasks,
        View::Sessions,
        View::Agents,
        View::Git,
        View::Terminal,
        View::Tools,
        View::Artifacts,
        View::Models,
        View::Mcp,
        View::Extensions,
        View::Security,
        View::Settings,
    ];

    /// Semantic groups for the navigation rail. Grouping is presentation
    /// only — every destination keeps its own key, index, and `View::ALL`
    /// position, so navigation order and shortcuts are unchanged.
    pub const GROUPS: [(NavGroup, &[View]); 4] = [
        (
            NavGroup::Workspace,
            &[
                View::Chat,
                View::Files,
                View::Search,
                View::Tasks,
                View::Sessions,
                View::Agents,
            ],
        ),
        (
            NavGroup::Build,
            &[
                View::Git,
                View::Terminal,
                View::Tools,
                View::Artifacts,
                View::Models,
            ],
        ),
        (
            NavGroup::Platform,
            &[View::Mcp, View::Extensions, View::Security],
        ),
        (NavGroup::System, &[View::Settings]),
    ];

    /// The group a destination belongs to, in rail order.
    pub fn group(self) -> NavGroup {
        Self::GROUPS
            .iter()
            .find(|(_, views)| views.contains(&self))
            .map(|(group, _)| *group)
            // Unreachable for the current table; fall back to the last group
            // rather than panicking if a destination is ever added.
            .unwrap_or(NavGroup::System)
    }

    pub fn label(self) -> &'static str {
        match self {
            View::Chat => "CHAT",
            View::Files => "FILES",
            View::Search => "SEARCH",
            View::Tasks => "TASKS",
            View::Sessions => "SESSIONS",
            View::Agents => "AGENTS",
            View::Git => "GIT",
            View::Terminal => "TERMINAL",
            View::Tools => "TOOLS",
            View::Artifacts => "ARTIFACTS",
            View::Models => "MODELS",
            View::Mcp => "MCP",
            View::Extensions => "EXTENSIONS",
            View::Security => "SECURITY",
            View::Settings => "SETTINGS",
        }
    }

    pub fn idx(self) -> usize {
        View::ALL.iter().position(|v| *v == self).unwrap_or(0)
    }

    pub fn from_idx(i: usize) -> View {
        View::ALL[i.min(View::ALL.len() - 1)]
    }

    pub fn next(self) -> View {
        View::from_idx((self.idx() + 1) % View::ALL.len())
    }

    pub fn prev(self) -> View {
        View::from_idx((self.idx() + View::ALL.len() - 1) % View::ALL.len())
    }
}

/// Modal layer above the current view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    Help,
    Palette,
    Model,
    Sessions,
    Preview,
    /// First-run setup wizard (system report → provider → models → review).
    Setup,
    /// Operating-mode control (operator-only; a switch always confirms).
    Mode,
}

/// Compact MODEL ERROR card (PART 15/20): a provider/model failure surfaced
/// as structured state — sanitized reason (never a raw HTTP dump), the
/// provider and exact model id involved, and the recovery actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelErrorCard {
    /// One-line headline, e.g. "Could not parse the tool call."
    pub headline: String,
    /// Sanitized reason shown on the card (redacted, truncated, single-line).
    pub reason: String,
    /// Provider display name ("Ollama", "llama.cpp", …).
    pub provider: String,
    /// Exact model id the request used (or "(default)").
    pub model: String,
    /// Full sanitized reason kept for `[v] view` (may wrap to more lines).
    pub detail: String,
}

/// One operator action offered by a RESOURCE LIMIT card.
///
/// Mirrors `openrt_resources::Remediation` at the UI boundary: the TUI shows
/// what the gate actually offers and nothing more, so a disabled override is
/// never rendered as a dead key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceAction {
    Retry,
    Wait,
    SwitchModel,
    Details,
    /// Operator bypass — only ever present when the deployment permits it.
    Override,
    Cancel,
}

impl ResourceAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Retry => "Retry",
            Self::Wait => "Wait",
            Self::SwitchModel => "Switch model",
            Self::Details => "Resource details",
            Self::Override => "Override once",
            Self::Cancel => "Cancel",
        }
    }

    /// Single-key shortcut shown next to the label.
    pub fn key(self) -> char {
        match self {
            Self::Retry => 'r',
            Self::Wait => 'w',
            Self::SwitchModel => 'm',
            Self::Details => 'v',
            Self::Override => 'o',
            Self::Cancel => '\u{1b}',
        }
    }

    pub fn hint(self) -> String {
        format!("[{}] {}", self.key(), self.label())
    }
}

/// Structured RESOURCE LIMIT card: a host admission refusal with real numbers
/// and the operator's options. Distinct from `ModelErrorCard` on purpose — a
/// resource refusal says nothing about the model or the tool being broken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceCard {
    /// Dimension label, e.g. "Host memory" / "Free disk".
    pub kind: String,
    /// Plain-language cause from the runtime.
    pub reason: String,
    /// "9.5 GiB free of 15.9 GiB" — already human-readable.
    pub current_human: String,
    /// The floor that was not met, human-readable.
    pub limit_human: String,
    /// Percentage line ("60.2% free / 15.0% required").
    pub percent_line: String,
    /// Options in the order the gate offered them.
    pub actions: Vec<ResourceAction>,
    /// Selected action (Tab/arrows move, Enter runs it).
    pub selected: usize,
    /// Full multi-line detail for `[v] view`.
    pub detail: String,
    /// True while a bounded WAITING FOR RESOURCES retry is in flight.
    pub waiting: Option<ResourceWaitProgress>,
}

/// Bounded-wait progress. Never a spinner that can run forever: `attempt` and
/// `max_attempts` are both shown so the operator can see the budget shrinking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceWaitProgress {
    pub attempt: u32,
    pub max_attempts: u32,
    /// Seconds remaining until the next re-check.
    pub next_check_secs: u64,
    /// Total budget for the whole wait.
    pub total_budget_ms: u64,
}

/// Destructive-action confirmation (y = proceed, anything else = cancel).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    DeleteSession {
        id: SessionId,
        title: String,
    },
    /// Security preset: append N allow rules to the versioned policy doc
    /// (append-only + audited; `full` = every registered tool/capability,
    /// otherwise read-shaped capabilities only).
    PolicyPreset {
        label: String,
        full: bool,
    },
    /// One custom rule typed in the Security view.
    PolicyAppend {
        pattern: String,
        action: String,
        scope: Option<String>,
    },
    /// Operating-mode switch (y = transition through the ONE ModeManager —
    /// audited, idempotent; the agent/model has no path to this).
    Mode {
        to: String,
    },
}

/// Honest per-model capability probe results (never inferred from the model
/// name — `None` means the transport could not say). `status_ok` carries the
/// transport reachability: the provider status row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelProbe {
    pub native_tools: Option<bool>,
    pub native_tools_err: Option<String>,
    pub vision: Option<bool>,
    pub vision_err: Option<String>,
    /// `Some(Ok(()))` = the last probe round-trip succeeded (provider
    /// reachable), `Some(Err(_))` = it failed, `None` = never probed.
    pub status: Option<Result<(), String>>,
    /// The tag this probe actually asked about, which is not always the tag
    /// that was requested: `default` probes whatever the provider resolves it
    /// to.
    pub tag: String,
}

/// Result payload of the single-flight background view refresh
/// (`refresh_join`) — applied in `finalize_joins` without ever blocking
/// the draw/key loop.
pub enum RefreshData {
    Files(Vec<FileRow>),
    Search {
        hits: Vec<SearchHit>,
        status: String,
    },
    Git(Result<GitInfo, String>),
    Artifacts(Result<(serde_json::Value, String), String>),
    Models {
        cards: Vec<openrt_registry::RegistryCard>,
        probe: ModelProbe,
        status: String,
        /// Per-artifact resource profiles gathered in the same background
        /// task. Cached on the app so the render path never reads a file.
        profiles: std::collections::BTreeMap<String, openrt_models::artifact::ModelResourceProfile>,
    },
    ModelSearch {
        query: String,
        results: Vec<openrt_registry::SearchResult>,
        status: String,
    },
    /// Hugging Face marketplace rows for the `[Hugging Face]` section
    /// (background; the Sovereign gate runs before any socket is opened —
    /// a blocked fetch lands as an honest status, zero rows).
    HubSearch {
        query: String,
        rows: Vec<openrt_registry::HfModelLite>,
        status: String,
    },
    Preview {
        title: String,
        lines: Vec<String>,
    },
    /// Wizard Hugging Face search: rows built in the background (an HF
    /// round-trip for sizes must never freeze the setup flow).
    SetupSearch {
        rows: Vec<crate::agent::setup::SetupModel>,
        fetched: usize,
        local_hits: usize,
        network_allowed: bool,
    },
}

/// State of one tool card in the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolState {
    /// Gate asked / call in flight.
    Running { since: Instant },
    /// Completed through the gate. `took_ms` is measured once, at
    /// completion — renders must never re-derive it from the clock.
    Done {
        ok: bool,
        detail: String,
        took_ms: u128,
    },
    /// Denied by policy, shield, resources, or the operator.
    Denied { reason: String, since: Instant },
}

/// One chat-stream entry.
#[derive(Debug, Clone)]
pub struct Msg {
    pub at: String,
    pub kind: MsgKind,
}

#[derive(Debug, Clone)]
pub enum MsgKind {
    User(String),
    Assistant(String),
    /// Real gate observation: name + invocation args + live state.
    Tool {
        tool: String,
        args: String,
        state: ToolState,
    },
    /// Compiled workflow plan rendered into the stream (live statuses are
    /// resolved against `app.plan` at draw time).
    Plan {
        id: String,
        goal: String,
        steps: Vec<(String, PlanStatus)>,
    },
    /// A `/plan` the compiler rejected — a STRUCTURED result inside the chat
    /// stream (goal preserved, real reason, real fix). The session, the
    /// stream, and the process all stay alive; `r`/`e`/`Esc` act on it.
    PlanFailed {
        goal: String,
        /// Machine-readable code of the first error (`unsupported_goal`, …).
        code: String,
        /// Full rendered diagnostic (what/why/where/fix per diagnostic).
        reason: String,
        /// The compiler's own remediation (first error's `how_to_fix`).
        fix: String,
    },
    /// System/process note (session created, task boundary, …).
    Note(String),
    /// Error surfaced from the runtime (task failure, tool denial, …).
    Err(String),
}

/// One plan step (compiled workflow, executed by `/run`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanStatus {
    Pending,
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone)]
pub struct PlanStep {
    pub id: String,
    pub label: String,
    pub status: PlanStatus,
}

/// Workspace-relative file row (read-only explorer).
#[derive(Debug, Clone)]
pub struct FileRow {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// A content search hit (real bytes read from disk).
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub path: String,
    pub line: usize,
    pub text: String,
}

/// `git` panel snapshot (read-only porcelain/log/diff-stat subprocess).
#[derive(Debug, Clone, Default)]
pub struct GitInfo {
    pub branch: String,
    pub status: Vec<String>,
    pub log: Vec<String>,
    pub stat: Vec<String>,
}

/// One terminal-panel line.
#[derive(Debug, Clone)]
pub struct TermLine {
    pub kind: &'static str, // "in" | "out" | "err" | "note"
    pub text: String,
}

/// File preview / diff payload for the Preview overlay.
#[derive(Debug, Clone)]
pub struct PreviewData {
    pub title: String,
    pub lines: Vec<String>,
}

/// Real Hugging Face metadata for one hub row. Every field comes from the
/// hub's `model_info` response — a `None` is an honest "the hub didn't say",
/// never an estimate.
#[derive(Debug, Clone)]
pub struct HfSize {
    /// Sum of sibling files with a declared size (real bytes).
    pub total_bytes: Option<u64>,
    /// Hub marks the model gated (access requires approval).
    pub gated: bool,
    /// Fetch failure (network/gate) — shown instead of a silent blank.
    pub error: Option<String>,
}

/// The four Models-view sections (Tab / BackTab cycles them in this order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSection {
    Local,
    Hub,
    Installed,
    Recommended,
}

impl ModelSection {
    pub const ALL: [ModelSection; 4] = [
        ModelSection::Local,
        ModelSection::Hub,
        ModelSection::Installed,
        ModelSection::Recommended,
    ];

    pub fn title(&self) -> &'static str {
        match self {
            Self::Local => "Local",
            Self::Hub => "Hugging Face",
            Self::Installed => "Installed",
            Self::Recommended => "Recommended",
        }
    }

    pub fn idx(&self) -> usize {
        Self::ALL.iter().position(|s| s == self).unwrap_or(0)
    }

    pub fn cycle(self, delta: i32) -> Self {
        let n = Self::ALL.len() as i32;
        let next = (self.idx() as i32 + delta).rem_euclid(n);
        Self::ALL[next as usize]
    }
}

impl Default for ModelSection {
    fn default() -> Self {
        Self::Local
    }
}

/// Client-side Hub filter dimensions (`f` in the `[Hugging Face]` section).
/// Values are derived ONLY from the rows actually loaded — a dimension the
/// loaded rows cannot answer shows `no data in loaded rows` instead of a
/// fabricated value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubFilterDim {
    Task,
    Provider,
    Parameters,
    Quantization,
    Context,
    Vision,
    ToolCalling,
    Format,
    Runtime,
}

impl HubFilterDim {
    pub const ALL: [HubFilterDim; 9] = [
        Self::Task,
        Self::Provider,
        Self::Parameters,
        Self::Quantization,
        Self::Context,
        Self::Vision,
        Self::ToolCalling,
        Self::Format,
        Self::Runtime,
    ];

    pub fn title(&self) -> &'static str {
        match self {
            Self::Task => "Task",
            Self::Provider => "Provider",
            Self::Parameters => "Parameters",
            Self::Quantization => "Quantization",
            Self::Context => "Context",
            Self::Vision => "Vision",
            Self::ToolCalling => "Tool calling",
            Self::Format => "Format",
            Self::Runtime => "Runtime",
        }
    }
}

/// Local GPU facts from `nvidia-smi` (background probe). Absent = never
/// probed or unavailable — rendered as "n/a", never as an invented device.
#[derive(Debug, Clone)]
pub struct GpuHw {
    pub name: String,
    pub total_mib: u64,
    pub used_mib: u64,
}

// ---- formatting helpers for the Models browser (facts only) ----

/// `key` padded to a fixed column, value truncated to `width`.
fn fmt_kv(key: &str, value: &str, width: usize) -> String {
    let head = format!("{key:<8}");
    let room = width.saturating_sub(head.len()).max(6);
    format!("{head}{}", truncate(value, room))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// `7050000000` → `7.05B`.
fn fmt_params(n: u64) -> String {
    const B: u64 = 1_000_000_000;
    const M: u64 = 1_000_000;
    if n >= B {
        format!("{}.{}B", n / B, (n % B) / (B / 100))
    } else if n >= M {
        format!("{}.{}M", n / M, (n % M) / (M / 100))
    } else {
        n.to_string()
    }
}

/// `1234567` → `1.2M`.
fn fmt_count(n: u64) -> String {
    const K: u64 = 1_000;
    const M: u64 = 1_000_000;
    if n >= M {
        format!("{}.{}M", n / M, (n % M) / (M / 100))
    } else if n >= K {
        format!("{}.{}k", n / K, (n % K) / (K / 10))
    } else {
        n.to_string()
    }
}

/// `15393050624` → `14.3 GiB`.
fn fmt_bytes(n: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if n >= GIB {
        format!("{}.{} GiB", n / GIB, (n % GIB) * 10 / GIB)
    } else if n >= MIB {
        format!("{}.{} MiB", n / MIB, (n % MIB) * 10 / MIB)
    } else {
        format!("{n} B")
    }
}

/// Weight-file extensions present in the repo's file list.
fn format_exts(siblings: &[openrt_registry::HfSibling]) -> String {
    const EXTS: [&str; 8] = [
        "safetensors",
        "bin",
        "gguf",
        "pt",
        "pth",
        "onnx",
        "npz",
        "h5",
    ];
    let mut found: Vec<&str> = siblings
        .iter()
        .filter_map(|s| {
            let f = s.rfilename.rsplit('/').next().unwrap_or(&s.rfilename);
            f.rsplit_once('.')
                .map(|(_, e)| e.to_ascii_lowercase())
                .as_deref()
                .and_then(|e| EXTS.iter().find(|x| **x == e).copied())
        })
        .collect();
    found.sort_unstable();
    found.dedup();
    if found.is_empty() {
        String::new()
    } else {
        found.join(",")
    }
}

/// Quantization marker from real hub tags (first match, e.g. `4-bit`,
/// `awq`, `gptq`, `gguf`). `None` = no marker declared.
fn quant_from_tags(tags: &[String]) -> Option<String> {
    const MARKERS: [&str; 6] = ["4-bit", "8-bit", "awq", "gptq", "gguf", "bitsandbytes"];
    MARKERS
        .iter()
        .find(|m| tags.iter().any(|t| t.eq_ignore_ascii_case(m)))
        .map(|m| m.to_string())
}

/// Vision capability of a hub row from its REAL metadata: a vision pipeline
/// or a vision tag → `Some(true)`; a non-vision pipeline (or tags) that say
/// nothing about vision → `Some(false)`; no metadata at all → `None`
/// (rendered as UNKNOWN — never inferred from the model name).
pub fn row_vision(row: &openrt_registry::HfModelLite) -> Option<bool> {
    const VISION_PIPELINES: [&str; 13] = [
        "image-classification",
        "image-to-text",
        "image-to-image",
        "text-to-image",
        "image-segmentation",
        "object-detection",
        "depth-estimation",
        "visual-question-answering",
        "document-question-answering",
        "video-classification",
        "text-to-video",
        "zero-shot-image-classification",
        "image-text-to-text",
    ];
    const VISION_TAGS: [&str; 7] = ["vision", "clip", "llava", "vlm", "visual", "siglip", "vit"];
    if let Some(p) = row.pipeline_tag.as_deref() {
        if VISION_PIPELINES.contains(&p) {
            return Some(true);
        }
    }
    if row
        .tags
        .iter()
        .any(|t| VISION_TAGS.iter().any(|v| t.eq_ignore_ascii_case(v)))
    {
        return Some(true);
    }
    if row.pipeline_tag.is_some() || !row.tags.is_empty() {
        Some(false)
    } else {
        None
    }
}

/// Tool-calling capability from the REAL tag list (`tool-calling`, `tools`,
/// `function-calling`, …). No tags at all → `None` = UNKNOWN.
pub fn row_tool_calling(row: &openrt_registry::HfModelLite) -> Option<bool> {
    const TOOL_TAGS: [&str; 5] = [
        "tool-calling",
        "tool_calling",
        "function-calling",
        "tools",
        "tooluse",
    ];
    if row.tags.is_empty() {
        return None;
    }
    Some(
        row.tags
            .iter()
            .any(|t| TOOL_TAGS.iter().any(|v| t.eq_ignore_ascii_case(v))),
    )
}

/// One row of the MCP connectivity table — straight from `mcp_states`.
#[derive(Debug, Clone)]
pub struct McpRow {
    pub name: String,
    pub trust: String,
    pub enabled: bool,
    pub security: String,
    pub tools: i64,
    pub source: String,
}

/// Operator actions on the selected MCP server (all run through the ONE
/// runtime registry — the TUI never edits MCP state itself).
#[derive(Debug, Clone)]
pub enum McpOp {
    Enable,
    Disable,
    Trust(String),
    Scan,
    Health,
    Inspect,
}

/// One row of the extension inventory (from `extension_states`).
#[derive(Debug, Clone)]
pub struct ExtRow {
    pub id: String,
    pub version: u32,
    pub name: String,
    pub activation: String,
    pub security: String,
}

/// Operator actions on extensions — every one goes through the ONE runtime
/// asset store (install → validate → scan → activate are separate gates).
#[derive(Debug, Clone)]
pub enum ExtOp {
    Validate,
    Scan,
    Activate,
    Deactivate,
    Invalidate,
    Inspect,
    /// Install a package from a local path or an (egress-gated) https URL —
    /// the GitHub/raw.githubusercontent browser path.
    Install {
        source: String,
    },
}

pub type EventRx = tokio::sync::mpsc::UnboundedReceiver<DomainEvent>;
type TaskJoin = tokio::task::JoinHandle<Result<Task, openrt_runtime::Error>>;
type WorkflowJoin =
    tokio::task::JoinHandle<Result<openrt_orch::WorkflowRunReport, openrt_runtime::Error>>;
type TermJoin = tokio::task::JoinHandle<Vec<TermLine>>;

/// The Agent Mode controller.
pub struct AgentApp {
    // ---- authorities & wiring ----
    pub runtime: Arc<Runtime>,
    pub workbench: App,
    pub switcher: Option<Arc<SwitchableProvider>>,
    pub resolver: Arc<TuiResolver>,
    pub rx: EventRx,

    // ---- shell ----
    /// Focus ring and hit registry, repopulated every frame by the renderer.
    ///
    /// These are the *only* record of what is clickable and what owns the
    /// keyboard, and they are filled in during draw rather than guessed in the
    /// event loop. That ordering is deliberate: a target that was not drawn
    /// cannot be clicked or focused, which makes "phantom controls" a
    /// representable impossibility instead of a bug.
    ///
    /// `RefCell` keeps `draw(&AgentApp)` immutable for every reader, including
    /// the ~50 render tests. Rendering is single-threaded and happens between
    /// input events, so the borrow never outlives a frame.
    pub(crate) focus: std::cell::RefCell<crate::design::FocusModel>,
    pub(crate) hits: std::cell::RefCell<crate::design::HitTargets>,
    /// Target a mouse press landed on, cleared on release. Drives the
    /// `Pressed` render state so a click is visible, not just acted upon.
    pub(crate) pressed: std::cell::RefCell<Option<crate::design::FocusTarget>>,
    /// Last known pointer position, in terminal cells.
    ///
    /// Kept here because a terminal reports motion and drag coordinates but not a
    /// "current position" — a `Drag` event that leaves the button held still needs
    /// somewhere to read the last position from to decide whether the pointer has
    /// moved off the pressed control.
    pub(crate) pointer: (u16, u16),
    /// Transient Settings preview. `None` means "render the committed theme".
    ///
    /// The committed theme itself is NOT stored here: it lives on
    /// [`App::theme`], which `AgentApp` embeds. One owner, so a switch in one
    /// mode is instantly visible in the other and the two can never disagree.
    pub theme_preview: Option<crate::theme::ThemeKind>,
    /// Settings sub-section the operator is on. `Info` is the read-only
    /// config dump; `Theme` is the one editable surface in Phase 1.
    pub settings_focus: SettingsFocus,
    /// Index into [`crate::theme::ThemeKind::ALL`] for the theme row the
    /// operator is pointing at. Navigation moves this and previews live;
    /// `Enter` commits, `Esc` cancels.
    pub theme_sel: usize,
    pub mode: UiMode,
    pub view: View,
    pub overlay: Option<Overlay>,
    pub should_quit: bool,
    pub status: String,
    pub clock: String,
    /// Cached operating mode (refreshed periodically — render is sync).
    pub exec_mode: String,
    pub net_allowed: bool,
    pub mode_tick: u32,

    // ---- chat ----
    pub stream: Vec<Msg>,
    pub input: String,
    /// Chat draft stashed while the Models view owns the input line.
    pub draft: String,
    /// Rows the chat view is scrolled UP from the newest message
    /// (0 = pinned to the bottom — the default, following new entries).
    pub chat_scroll: u16,
    pub open_tool: Option<usize>,
    pub approvals: Vec<PendingApproval>,

    // ---- sessions & tasks ----
    pub current_session: Option<SessionId>,
    pub sessions: Vec<Session>,
    pub tasks: Vec<Task>,
    pub current_task: Option<TaskId>,
    pub task_join: Option<TaskJoin>,

    // ---- plan ----
    pub plan: Vec<PlanStep>,
    pub plan_goal: Option<String>,
    /// Last `/plan` goal the compiler rejected. The failure is a STRUCTURED
    /// in-chat result (never a process exit) — `r` retries, `e` edits, Esc
    /// dismisses while the session stays alive.
    pub plan_failure: Option<String>,
    pub compiled: Option<openrt_orch::Workflow>,
    pub workflow_join: Option<WorkflowJoin>,

    // ---- palette ----
    pub palette: String,
    pub palette_sel: usize,
    pub preview_scroll: u16,
    /// Scroll offset for the Help overlay (rows skipped from the top —
    /// the key table outgrows a 24-row terminal).
    pub help_scroll: u16,

    // ---- universal list filter (Ctrl+F in Tools/Tasks/Sessions) ----
    pub filter_active: bool,
    pub list_filter: String,
    /// Top-row offset for scroll-only views (Git/Agents/Artifacts/MCP/…).
    pub view_scroll: u16,
    /// Rows the terminal log is scrolled UP from the newest output
    /// (0 = pinned to the bottom — output is always visible).
    pub term_scroll: u16,

    // ---- first-run setup wizard (None once finished/dismissed) ----
    pub setup: Option<SetupState>,
    /// Pending destructive action awaiting an explicit `y`.
    pub confirm: Option<Confirm>,

    // ---- per-view selections (one list cursor per view) ----
    pub sel: usize,

    // ---- view data ----
    pub files_root: PathBuf,
    pub files: Vec<FileRow>,
    pub preview: Option<PreviewData>,
    pub search: String,
    pub search_hits: Vec<SearchHit>,
    pub git: Option<GitInfo>,
    pub git_error: Option<String>,
    pub term_log: Vec<TermLine>,
    /// Background `terminal.run` execution — the UI never blocks on it.
    pub term_join: Option<TermJoin>,
    pub tools: Vec<openrt_tools::ToolSpec>,
    pub artifacts: serde_json::Value,
    pub artifacts_note: String,
    pub model_cards: Vec<openrt_registry::RegistryCard>,
    /// Resource profile per artifact, gathered off the render path. An absent
    /// entry means "not probed yet", which the UI says rather than guessing.
    pub model_profiles:
        std::collections::BTreeMap<String, openrt_models::artifact::ModelResourceProfile>,
    pub model_input: String,
    /// A model override waiting on its probe before it is committed.
    ///
    /// The active model is not overwritten on request: a switch that cannot
    /// reach the provider must leave the previous model in place, so the commit
    /// happens here, when the probe lands, rather than when Enter is pressed.
    pub pending_switch: Option<String>,
    /// Ctrl+F model search: query mode owns the input line while active.
    pub model_search_active: bool,
    pub model_query: String,
    pub model_results: Vec<openrt_registry::SearchResult>,
    /// The query `model_results` was produced for — Enter re-runs when the
    /// typed query moved on, applies the selection when it did not.
    pub model_results_query: String,
    /// Real Hugging Face metadata per hub row (fetched in the background,
    /// only while the network mode allows it — never guessed).
    pub hf_sizes: std::collections::HashMap<String, HfSize>,
    pub hf_size_join: Option<tokio::task::JoinHandle<Vec<(String, HfSize)>>>,
    /// Local-file import workflow: `i` picks the artifact, the input line
    /// becomes the file path, Enter runs the import on the runtime store.
    pub import_mode: bool,
    pub import_id: String,
    pub import_join: Option<tokio::task::JoinHandle<std::result::Result<String, String>>>,
    // ---- Models browser sections (Tab / BackTab) ----
    /// Active section: `[Local] [Hugging Face] [Installed] [Recommended]`.
    pub model_section: ModelSection,
    /// Marketplace rows from the hub (metadata only — never weights).
    pub hub_rows: Vec<openrt_registry::HfModelLite>,
    /// Query `hub_rows` was fetched for ("" = hub's recent listing).
    pub hub_query: String,
    /// Honest hub status (hits / Sovereign block / transport error).
    pub hub_status: String,
    /// Pipeline facet over `hub_rows` (`None` = all rows).
    pub hub_facet: Option<String>,
    /// A HubSearch has landed at least once (first Tab auto-fetches).
    pub hub_loaded: bool,
    /// Hub API page offset (`[` / `]` — 10 rows per page).
    pub hub_skip: usize,
    /// `f` filter overlay in the Hub section (client-side over loaded rows).
    pub hub_filter_active: bool,
    /// Selected dimension cursor inside the filter UI.
    pub hub_filter_sel: usize,
    /// Active filter per dimension (`None` = all).
    pub hub_filters: Vec<(HubFilterDim, Option<String>)>,
    /// Structured import failure card (MODEL IMPORT FAILED / Reason: …).
    pub import_error: Option<String>,
    /// Structured model/provider failure card (PART 15/20): compact,
    /// sanitized, never a raw HTTP dump. Actions: r/m/v/Esc.
    pub model_error: Option<ModelErrorCard>,
    /// `[v] view` toggle: expand the card to the full sanitized detail.
    pub model_error_detail: bool,
    /// Structured RESOURCE LIMIT card (host admission refusal). Rendered above
    /// the stream with real numbers and operator actions; the session survives.
    pub resource_card: Option<ResourceCard>,
    /// In-flight bounded WAITING FOR RESOURCES retry (background, cancellable).
    pub resource_wait:
        Option<tokio::task::JoinHandle<openrt_runtime::resource_retry::ResourceWaitOutcome>>,
    /// The prompt that was refused — `[r]` on the card re-runs it once the
    /// operator frees resources, so a refusal never loses the user's work.
    pub resource_prompt: Option<String>,
    /// `[v] view` toggle for the resource card's multi-line detail.
    pub resource_detail: bool,
    /// Cached host resource posture for the header strip (background refresh;
    /// never a blocking `/proc` read on the render path).
    pub resource_header: Option<openrt_resources::preflight::ResourcePreflight>,
    pub resource_header_join:
        Option<tokio::task::JoinHandle<openrt_resources::preflight::ResourcePreflight>>,
    pub resource_tick: u32,
    /// Last prompt sent — `[r]` on the MODEL ERROR card re-sends it.
    pub last_prompt: Option<String>,
    /// Full `model_info` for the selected hub row (background fetch).
    pub hf_detail: Option<openrt_registry::HfModelInfo>,
    /// Hub row id `hf_detail` belongs to (stale details are ignored).
    pub hf_detail_id: String,
    /// Detail fetch failure (honest note, replaces a stale blank).
    pub hf_detail_err: Option<String>,
    pub hf_detail_join:
        Option<tokio::task::JoinHandle<(String, Result<openrt_registry::HfModelInfo, String>)>>,
    /// Local GPU facts (background `nvidia-smi` probe).
    pub hw: Option<GpuHw>,
    pub hw_err: Option<String>,
    pub hw_join: Option<tokio::task::JoinHandle<Result<Option<GpuHw>, String>>>,
    /// Honest per-model native tool support: `Some(true/false)` from a real
    /// transport probe, `None` = unknown (never inferred from the name).
    pub native_tools: Option<bool>,
    /// Last probe failure (rendered instead of a silent "unknown").
    pub native_tools_err: Option<String>,
    /// Honest per-model vision support from the same transport probe.
    pub vision: Option<bool>,
    pub vision_err: Option<String>,
    /// Provider reachability from the last probe round-trip.
    pub provider_status: Option<Result<(), String>>,
    pub mcp: serde_json::Value,
    /// Parsed rows of the MCP connectivity table (from `mcp_states`).
    pub mcp_rows: Vec<McpRow>,
    /// Operator id recorded on every MCP/extension action + audit row.
    pub operator_id: String,
    /// `o` prompt: the input line edits the operator id while true.
    pub op_edit: bool,
    /// Background operator action (MCP scan/enable/…, extension
    /// validate/activate/install/… — the UI never blocks).
    pub op_join: Option<tokio::task::JoinHandle<Result<(String, Option<Vec<String>>), String>>>,
    /// Single-flight background view refresh (files / search / git /
    /// artifacts / models / preview): the draw and key loops never wait on
    /// directory walks, `git` subprocesses, or provider probes.
    pub refresh_join: Option<tokio::task::JoinHandle<Result<RefreshData, String>>>,
    /// Single-flight capability probe (native tools + vision + reachability
    /// for the current tag). PART 29: the probe is two HTTP round-trips —
    /// it must never run inline in the key handler (a dead provider would
    /// freeze the TUI for the full client timeout).
    pub probe_join: Option<tokio::task::JoinHandle<ModelProbe>>,
    /// Parsed extension inventory rows (from `extension_states`).
    pub ext_rows: Vec<ExtRow>,
    /// `i` prompt: the input line holds a local package path or an https URL.
    pub ext_install: bool,
    /// Security view: the input line edits a custom policy rule while true
    /// (`pattern <allow|deny|ask> [scope]`).
    pub policy_edit: bool,
    pub exts: Vec<serde_json::Value>,
    pub findings: Vec<crate::app::FindingRow>,
    pub policy_rows: Vec<(String, String)>,
    pub recovery_rows: Vec<crate::app::RecoveryRow>,

    pub started: Instant,
    pub seq: usize,
}

/// Subsequence fuzzy score for the command palette: every needle char must
/// appear in order; consecutive runs and early hits score higher. `None` =
/// not a subsequence (no match). Case-insensitive (lowercase both sides).
pub fn fuzzy_score(needle: &str, hay: &str) -> Option<i64> {
    if needle.is_empty() {
        return Some(0);
    }
    let mut hay_chars = hay.chars().enumerate();
    let mut last: Option<usize> = None;
    let mut score = 0i64;
    for nc in needle.chars() {
        let mut found = false;
        while let Some((idx, hc)) = hay_chars.next() {
            if hc == nc {
                score += match last {
                    Some(l) if l + 1 == idx => 4,
                    _ => 1,
                };
                if idx == 0 {
                    score += 2;
                }
                last = Some(idx);
                found = true;
                break;
            }
        }
        if !found {
            return None;
        }
    }
    Some(score)
}

impl AgentApp {
    /// Attach to a live runtime (with the UI sink + resolver already wired at
    /// build time) and load the initial view state.
    pub async fn new(
        runtime: Arc<Runtime>,
        workbench: App,
        switcher: Option<Arc<SwitchableProvider>>,
        resolver: Arc<TuiResolver>,
        rx: EventRx,
    ) -> Result<Self, AppError> {
        let files_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let tools = runtime.registry.specs();
        let mut app = Self {
            runtime,
            workbench,
            switcher,
            resolver,
            rx,
            focus: std::cell::RefCell::new(crate::design::FocusModel::new()),
            hits: std::cell::RefCell::new(crate::design::HitTargets::new()),
            pressed: std::cell::RefCell::new(None),
            pointer: (0, 0),
            theme_preview: None,
            settings_focus: SettingsFocus::Info,
            theme_sel: 0,
            mode: UiMode::Agent,
            view: View::Chat,
            overlay: None,
            should_quit: false,
            status: "ready — ask OpenRT anything (F1 for help)".into(),
            clock: now_hms(),
            exec_mode: "…".into(),
            net_allowed: false,
            mode_tick: 0,
            stream: Vec::new(),
            input: String::new(),
            draft: String::new(),
            chat_scroll: 0,
            open_tool: None,
            approvals: Vec::new(),
            current_session: None,
            sessions: Vec::new(),
            tasks: Vec::new(),
            current_task: None,
            task_join: None,
            plan: Vec::new(),
            plan_goal: None,
            plan_failure: None,
            compiled: None,
            workflow_join: None,
            palette: String::new(),
            palette_sel: 0,
            preview_scroll: 0,
            help_scroll: 0,
            filter_active: false,
            list_filter: String::new(),
            view_scroll: 0,
            term_scroll: 0,
            setup: None,
            confirm: None,
            sel: 0,
            files_root,
            files: Vec::new(),
            preview: None,
            search: String::new(),
            search_hits: Vec::new(),
            git: None,
            git_error: None,
            term_log: Vec::new(),
            term_join: None,
            tools,
            artifacts: serde_json::Value::Null,
            artifacts_note: "not loaded yet — Ctrl+R refresh".into(),
            model_cards: Vec::new(),
            model_profiles: std::collections::BTreeMap::new(),
            model_input: String::new(),
            model_search_active: false,
            model_query: String::new(),
            model_results: Vec::new(),
            model_results_query: String::new(),
            hf_sizes: std::collections::HashMap::new(),
            hf_size_join: None,
            import_mode: false,
            import_id: String::new(),
            import_join: None,
            model_section: ModelSection::Local,
            hub_rows: Vec::new(),
            hub_query: String::new(),
            hub_status: "not loaded — Tab to [Hugging Face]".into(),
            hub_facet: None,
            hub_loaded: false,
            hub_skip: 0,
            hub_filter_active: false,
            hub_filter_sel: 0,
            hub_filters: HubFilterDim::ALL.iter().map(|d| (*d, None)).collect(),
            import_error: None,
            model_error: None,
            model_error_detail: false,
            resource_card: None,
            resource_wait: None,
            resource_prompt: None,
            resource_detail: false,
            resource_header: None,
            resource_header_join: None,
            resource_tick: 0,
            last_prompt: None,
            hf_detail: None,
            hf_detail_id: String::new(),
            hf_detail_err: None,
            hf_detail_join: None,
            hw: None,
            hw_err: None,
            hw_join: None,
            native_tools: None,
            native_tools_err: None,
            vision: None,
            vision_err: None,
            provider_status: None,
            mcp: serde_json::Value::Null,
            mcp_rows: Vec::new(),
            operator_id: "tui.operator".into(),
            op_edit: false,
            op_join: None,
            refresh_join: None,
            probe_join: None,
            pending_switch: None,
            ext_rows: Vec::new(),
            ext_install: false,
            policy_edit: false,
            exts: Vec::new(),
            findings: Vec::new(),
            policy_rows: Vec::new(),
            recovery_rows: Vec::new(),
            started: Instant::now(),
            seq: 0,
        };
        app.refresh_sessions().await?;
        app.load_git().await;
        // Drain the startup background git read BEFORE the draw loop starts:
        // the first frame ships real git data, and the status messages below
        // are not clobbered later by a landing join.
        if let Some(h) = app.refresh_join.take() {
            if let Ok(Ok(data)) = h.await {
                app.apply_refresh(data).await;
            }
        }
        if let Ok(m) = app.runtime.modes.current().await {
            app.exec_mode = m.mode.as_str().to_string();
            app.net_allowed = m.mode.network_allowed();
        }
        // First run with no usable model: open the setup wizard (detects the
        // host, probes the provider, recommends models — nothing invented).
        let switched = app.switcher.as_ref().is_some_and(|s| s.is_switched());
        let has_default = app
            .runtime
            .provider
            .info()
            .default_model
            .as_deref()
            .is_some_and(|m| !m.is_empty());
        if !switched && !has_default {
            // First-run setup persists its completion (ADR-006 record) —
            // subsequent startups with a finished setup must NOT replay the
            // wizard (mission: setup persistence). A store read error fails
            // open to the wizard: it grants nothing and detects honestly.
            let setup_done = matches!(
                app.runtime.store.get_record("tui.setup", "state").await,
                Ok(Some(v))
                    if v.get("completed").and_then(|c| c.as_bool()).unwrap_or(false)
            );
            if setup_done {
                app.status =
                    "ready — ask OpenRT anything (F1 for help) · no default model — Ctrl+K → models"
                        .into();
            } else {
                app.status =
                    "first run — setup wizard (Enter advances, ↑/↓ selects, Esc closes)".into();
                app.setup_begin().await;
            }
        } else {
            // Startup refreshes have drained; keep the friendly ready line.
            app.status = "ready — ask OpenRT anything (F1 for help)".into();
        }
        // Boot recovery surfaced IN-SESSION: RuntimeBuilder already ran the
        // fail-closed boot scan before serving anything (never auto-resumes).
        // A previous session left behind — and the recovery rows live in the
        // SECURITY view — must be visible at once, not buried in the CLI.
        if let Ok(records) = app.runtime.recovery_records().await {
            let dirty: Vec<&_> = records.iter().filter(|r| !r.is_clean()).collect();
            if !dirty.is_empty() {
                let interrupted: usize = dirty.iter().map(|r| r.interrupted_tasks.len()).sum();
                let incomplete: usize = dirty.iter().map(|r| r.incomplete_runs.len()).sum();
                let stale: usize = dirty.iter().map(|r| r.stale_sandboxes.len()).sum();
                let push = |app: &mut Self, text: String| {
                    app.stream.push(Msg {
                        at: now_hms(),
                        kind: MsgKind::Note(text),
                    });
                };
                push(
                    &mut app,
                    "SESSION INTERRUPTED — boot recovery found work left behind".into(),
                );
                push(
                    &mut app,
                    format!(
                        "{interrupted} interrupted task(s) · {incomplete} incomplete run(s) · {stale} stale sandbox(es)"
                    ),
                );
                push(&mut app, "  nothing was auto-resumed (fail-closed)".into());
                for t in dirty.iter().flat_map(|r| &r.interrupted_tasks).take(3) {
                    push(
                        &mut app,
                        format!(
                            "  {} was {:?} at last boot (session {})",
                            t.task_id, t.previous_status, t.session_id
                        ),
                    );
                }
                if incomplete > 0 {
                    push(
                        &mut app,
                        "  resume: `openrt workflow resume` · review: SECURITY view / `openrt recovery list`"
                            .into(),
                    );
                }
                if app.setup.is_none() {
                    app.status =
                        "session interrupted — boot recovery found work left behind".into();
                }
            }
        }
        Ok(app)
    }

    // ------------------------------------------------------------------
    // event pump
    // ------------------------------------------------------------------

    /// Drain bus events, approval requests, and finished background joins.
    /// Called every loop tick (and from tests).
    pub async fn pump(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            self.apply_event(ev).await;
        }
        for pending in self.resolver.drain() {
            let tool = pending.request.tool.clone();
            self.status = format!("approval required for {tool}");
            self.approvals.push(pending);
        }
        self.finalize_resource().await;
        self.finalize_joins().await;
        self.clock = now_hms();
        self.mode_tick = self.mode_tick.wrapping_add(1);
        if self.mode_tick % 30 == 1 {
            if let Ok(m) = self.runtime.modes.current().await {
                self.exec_mode = m.mode.as_str().to_string();
                self.net_allowed = m.mode.network_allowed();
            }
        }
        // Header posture refresh: one background probe at a time, every ~3s.
        // Never a blocking read on the render path.
        if self.mode_tick % 30 == 1 {
            self.refresh_resource_header();
        }
        self.poll_resource_header().await;
    }

    /// Land a finished bounded resource wait. A wait that ends while the card
    /// is gone (dismissed, or a new prompt sent) is dropped silently: the
    /// operator moved on and a stale resume would be worse than silence.
    async fn finalize_resource(&mut self) {
        let finished = self
            .resource_wait
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished);
        if !finished {
            return;
        }
        let Some(h) = self.resource_wait.take() else {
            return;
        };
        match h.await {
            Ok(outcome) => {
                if self.resource_card.is_some() {
                    self.finish_resource_wait(&outcome);
                }
            }
            Err(_) => {
                // The wait task was aborted: that is the Esc path, already
                // reflected in the status line.
                if let Some(c) = self.resource_card.as_mut() {
                    c.waiting = None;
                }
            }
        }
    }

    async fn finalize_joins(&mut self) {
        // Background capability probe (native tools / vision / reachability):
        // land the result when the round-trip finishes — the key handler
        // never waited on it (PART 29).
        let mut probe_done: Option<ModelProbe> = None;
        let mut probe_cancelled = false;
        if let Some(h) = &mut self.probe_join {
            if h.is_finished() {
                let res = h.await;
                self.probe_join = None;
                match res {
                    Ok(p) => probe_done = Some(p),
                    // An aborted probe is the *normal* path when a newer one
                    // supersedes it — but it still has to retire the switch that
                    // was staged for it. Dropping the error here used to leave
                    // `pending_switch` set and the status line reading "probing
                    // before it takes effect" forever, with no switch left that
                    // could ever complete. Silence would be worse than either.
                    Err(e) if e.is_cancelled() => probe_cancelled = true,
                    Err(e) => {
                        self.native_tools_err = Some(format!("model probe crashed: {e}"));
                        self.provider_status = Some(Err(format!("model probe crashed: {e}")));
                    }
                }
            }
        }
        if probe_cancelled {
            if let Some(candidate) = self.pending_switch.take() {
                self.status =
                    format!("switch to {candidate} cancelled — superseded before it took effect");
            }
        }
        if let Some(p) = probe_done {
            // Commit (or refuse) a waiting switch before anything else reads
            // the status, so the operator's line and the provider's actual
            // model can never disagree.
            if let Some(candidate) = self.pending_switch.take() {
                // The answer has to be about the model that was staged. A probe
                // that lands for some *other* tag — the shape a superseded probe
                // takes when it completes in the window between two switches —
                // must not be read as confirmation for this candidate: that is
                // precisely "every later request carried a model nobody had
                // confirmed to exist".
                if p.tag == candidate {
                    self.commit_switch(&candidate, &p);
                } else {
                    self.status = format!(
                        "switch to {candidate} not applied: the probe that landed answered \
                         for {} instead",
                        if p.tag.is_empty() { "nothing" } else { &p.tag }
                    );
                }
            }
            self.native_tools = p.native_tools;
            self.native_tools_err = p.native_tools_err;
            self.vision = p.vision;
            self.vision_err = p.vision_err;
            self.provider_status = p.status.clone();
            // Setup wizard: the review note promised "probing …" — replace
            // it with the real label now that the answer landed.
            let label = crate::agent::render::native_tools_label(self);
            if let Some(st) = self.setup.as_mut() {
                if st.note.contains("native tools: probing") {
                    st.note = format!("model → {} · native tools: {label}", self.model_input);
                }
            }
        }
        // Background view refresh (files/search/git/models/preview): land
        // the result when the walk/probe finishes — never before.
        let mut refresh_done: Option<Result<RefreshData, String>> = None;
        if let Some(h) = &mut self.refresh_join {
            if h.is_finished() {
                let res = h.await;
                self.refresh_join = None;
                refresh_done = Some(res.unwrap_or_else(|e| Err(format!("task crashed: {e}"))));
            }
        }
        if let Some(res) = refresh_done {
            match res {
                Ok(data) => {
                    let is_model_search = matches!(&data, RefreshData::ModelSearch { .. });
                    self.apply_refresh(data).await;
                    if is_model_search {
                        // Continue in the background: real HF byte sizes for
                        // the top rows (own join, gated by the network mode).
                        self.probe_hf_sizes().await;
                    }
                }
                Err(e) => self.status = format!("refresh failed: {e}"),
            }
        }
        // Background HF size probe: real bytes from hub metadata.
        if let Some(h) = &mut self.hf_size_join {
            if h.is_finished() {
                let res = h.await;
                self.hf_size_join = None;
                if let Ok(entries) = res {
                    for (id, info) in entries {
                        self.hf_sizes.insert(id, info);
                    }
                }
            }
        }
        // Background hub detail fetch (`model_info` for the selected row).
        // Rendering only trusts details whose id matches the selection.
        if let Some(h) = &mut self.hf_detail_join {
            if h.is_finished() {
                let res = h.await;
                self.hf_detail_join = None;
                if let Ok((id, info)) = res {
                    match info {
                        Ok(i) => {
                            self.hf_detail = Some(i);
                            self.hf_detail_id = id;
                            self.hf_detail_err = None;
                        }
                        Err(e) => {
                            self.hf_detail = None;
                            self.hf_detail_id = id;
                            self.hf_detail_err = Some(e);
                        }
                    }
                }
            }
        }
        // Background local GPU probe (`nvidia-smi`): lands or notes failure —
        // the browser never displays a device that was not observed.
        if let Some(h) = &mut self.hw_join {
            if h.is_finished() {
                let res = h.await;
                self.hw_join = None;
                match res {
                    Ok(Ok(Some(g))) => {
                        self.hw = Some(g);
                        self.hw_err = None;
                    }
                    Ok(Ok(None)) => {
                        self.hw_err = Some("nvidia-smi not available".into());
                    }
                    Ok(Err(e)) => {
                        self.hw_err = Some(e);
                    }
                    Err(e) => {
                        self.hw_err = Some(format!("gpu probe task crashed: {e}"));
                    }
                }
            }
        }
        // Background local import: report through the status line, never
        // through a frozen UI.
        if let Some(h) = &mut self.import_join {
            if h.is_finished() {
                let res = h.await;
                self.import_join = None;
                match res {
                    Ok(Ok(msg)) => {
                        self.status = msg;
                        self.import_error = None;
                        self.import_mode = false;
                        self.import_id.clear();
                        let _ = self.refresh_models().await;
                    }
                    Ok(Err(msg)) => {
                        // Keep the import prompt open so the path can be fixed;
                        // the structured MODEL IMPORT FAILED card shows the reason.
                        self.status = msg.clone();
                        self.import_error = Some(msg);
                    }
                    Err(join) => {
                        let m = format!("import task crashed: {join}");
                        self.status = m.clone();
                        self.import_error = Some(m);
                    }
                }
            }
        }
        // Background operator action (MCP + extensions): report through the
        // status line; inspect/scan payloads open the Preview overlay.
        if let Some(h) = &mut self.op_join {
            if h.is_finished() {
                let res = h.await;
                self.op_join = None;
                match res {
                    Ok(Ok((msg, preview))) => {
                        // Refresh first, then report — the refresh's own
                        // "view refreshed" status must not clobber the
                        // operator action's real outcome.
                        let _ = self.refresh_view().await;
                        self.status = msg.clone();
                        if let Some(lines) = preview {
                            self.preview = Some(PreviewData {
                                title: msg.clone(),
                                lines,
                            });
                            self.preview_scroll = 0;
                            // Only claim the screen when the operator has not.
                            // A result that arrives while they are working inside
                            // another modal used to replace it, destroying the
                            // context they were typing in with no way back. The
                            // data is still there, so the next `p` finds it.
                            match self.overlay {
                                None => self.overlay = Some(Overlay::Preview),
                                Some(open) => {
                                    self.status =
                                        format!("{msg} · preview ready (p) — {open:?} is in front");
                                }
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        let _ = self.refresh_view().await;
                        self.status = e;
                    }
                    Err(join) => {
                        self.status = format!("operator action crashed: {join}");
                    }
                }
            }
        }
        // Background terminal command: append its output when it finishes.
        if let Some(h) = &mut self.term_join {
            if h.is_finished() {
                let res = h.await;
                self.term_join = None;
                match res {
                    Ok(lines) => {
                        self.term_log.extend(lines);
                        self.status = "terminal · command finished".into();
                    }
                    Err(join) => {
                        self.term_log.push(TermLine {
                            kind: "err",
                            text: format!("command task crashed: {join}"),
                        });
                    }
                }
            }
        }
        let mut finished: Option<Result<Task, openrt_runtime::Error>> = None;
        if let Some(h) = &mut self.task_join {
            if h.is_finished() {
                match h.await {
                    Ok(res) => finished = Some(res),
                    Err(join) => {
                        self.push(MsgKind::Err(format!("task task crashed: {join}")));
                        self.task_join = None;
                        self.current_task = None;
                    }
                }
            }
        }
        if let Some(res) = finished {
            self.task_join = None;
            self.current_task = None;
            match res {
                Ok(task) => {
                    // Events already rendered the stream; this is the
                    // authoritative store read for the status + task list.
                    self.status = format!("task {:?} · {}", task.status, task.id);
                }
                Err(e) => {
                    self.push(MsgKind::Err(format!("task failed: {e}")));
                    self.status = format!("task failed: {e}");
                }
            }
            let _ = self.refresh_tasks().await;
        }
        if let Some(h) = &mut self.workflow_join {
            if h.is_finished() {
                let res = h.await;
                self.workflow_join = None;
                match res {
                    Ok(Ok(report)) => {
                        self.push(MsgKind::Note(format!(
                            "workflow {} finished: {}",
                            report.workflow_id, report.status
                        )));
                        self.status = format!("workflow {}", report.status);
                    }
                    Ok(Err(e)) => {
                        self.push(MsgKind::Err(format!("workflow failed: {e}")));
                        self.status = format!("workflow failed: {e}");
                    }
                    Err(join) => {
                        self.push(MsgKind::Err(format!("workflow task crashed: {join}")));
                    }
                }
            }
        }
    }

    fn push(&mut self, kind: MsgKind) {
        self.seq += 1;
        self.stream.push(Msg { at: now_hm(), kind });
    }

    /// Append a message directly, for seeding a render in a test.
    ///
    /// `push` is private because production code should go through the typed
    /// helpers above; a palette-compliance test needs every message variant on
    /// screen at once, which those helpers cannot express.
    #[cfg(test)]
    pub fn push_for_test(&mut self, kind: MsgKind) {
        self.push(kind);
    }

    /// Surface an unexpected key-path error INSIDE the session instead of
    /// letting it reach the event loop as a fatal `io::Error` (TUI
    /// resilience: only an explicit quit stops the process).
    pub fn record_error(&mut self, msg: &str) {
        self.push(MsgKind::Err(msg.to_string()));
        self.status = if msg.chars().count() > 160 {
            format!("{}…", msg.chars().take(160).collect::<String>())
        } else {
            msg.to_string()
        };
    }

    /// Classify a task error as a **host resource** refusal and build the
    /// structured RESOURCE LIMIT card.
    ///
    /// Returns `None` for everything else, so a provider/model failure still
    /// gets the MODEL ERROR card and a permission failure keeps its own
    /// dialog. A resource refusal is its own state: it means "this host cannot
    /// start the work yet", never "the model or the tool is broken".
    fn resource_card(&self, error: &str) -> Option<ResourceCard> {
        use openrt_resources::preflight::{PreflightVerdict, ResourceStatus};
        use openrt_resources::{Remediation, ResourcePreflight};

        // `Error::ResourceBlocked` renders as "resource limit: {denial}".
        let marker = error
            .strip_prefix("resource limit: ")
            .or_else(|| error.strip_prefix("error: resource limit: "))?;

        // Re-derive the structure from the live gate so the card shows the real
        // dimensions, not a re-parse of prose. The prose above only tells us
        // *that* this is a resource refusal.
        let gate = &self.runtime.resource_gate;
        let snapshot = gate.current();
        let preflight = ResourcePreflight::evaluate(&snapshot, gate.limits());

        let (kind_label, current_human, limit_human, percent_line, reason) = match &preflight.denial
        {
            Some(d) => {
                let limit_human = match d.limit {
                    Some(l) => humanize_bytes(l),
                    None => "unconfigured".to_string(),
                };
                (
                    d.kind.label().to_string(),
                    match d.current {
                        Some(c) => humanize_bytes(c),
                        None => "unmeasured".to_string(),
                    },
                    limit_human,
                    match (d.used_pct, d.headroom) {
                        (Some(u), Some(_)) => format!("{u:.1}% in use"),
                        _ => "usage unmeasured".to_string(),
                    },
                    d.reason.clone(),
                )
            }
            // A refusal whose structured detail is gone (e.g. the snapshot
            // already recovered by the time we render) still deserves an
            // honest card, not a blank one.
            None => (
                "Host resources".to_string(),
                format!(
                    "{} free of {}",
                    humanize_bytes(snapshot.mem.available_kb.saturating_mul(1024)),
                    humanize_bytes(snapshot.mem.total_kb.saturating_mul(1024))
                ),
                format!(
                    "{}% must stay free",
                    (gate.limits().min_headroom_ratio * 100.0).round()
                ),
                match preflight.memory.used_pct {
                    Some(u) => format!("{u:.1}% in use"),
                    None => "usage unmeasured".to_string(),
                },
                marker.to_string(),
            ),
        };

        // Offer exactly the actions the gate offered, in its order.
        let offered = preflight
            .denial
            .as_ref()
            .map(|d| d.remediations.clone())
            .unwrap_or_else(|| {
                vec![
                    Remediation::Retry,
                    Remediation::Wait,
                    Remediation::SwitchModel,
                    Remediation::OpenDetails,
                    Remediation::Cancel,
                ]
            });
        let actions: Vec<ResourceAction> = offered
            .iter()
            .filter_map(|r| match r {
                Remediation::Retry => Some(ResourceAction::Retry),
                Remediation::Wait => Some(ResourceAction::Wait),
                Remediation::SwitchModel => Some(ResourceAction::SwitchModel),
                Remediation::OpenDetails => Some(ResourceAction::Details),
                Remediation::OperatorOverride => Some(ResourceAction::Override),
                Remediation::Cancel => Some(ResourceAction::Cancel),
            })
            .collect();

        let verdict = preflight.verdict();
        let detail = match &verdict {
            PreflightVerdict::Admitted | PreflightVerdict::Constrained(_) => {
                let status = if preflight.disk.status == ResourceStatus::Blocked {
                    "disk floor not met".to_string()
                } else {
                    "memory floor not met".to_string()
                };
                format!(
                    "{status}\nhost memory   {:>10} free of {:>10}\nfree disk     {:>10}\ncpu cores     {:>10}\nvram          {:>10}\n\nrule: {}\nThis is a host capacity limit, not a model or tool failure. The session is intact.",
                    humanize_bytes(snapshot.mem.available_kb.saturating_mul(1024)),
                    humanize_bytes(snapshot.mem.total_kb.saturating_mul(1024)),
                    humanize_bytes(snapshot.disk.avail_bytes),
                    snapshot.cpu.count,
                    if preflight.vram.unknown_reason.is_empty() {
                        "measured".to_string()
                    } else {
                        preflight.vram.unknown_reason.clone()
                    },
                    describe_rule(gate.limits()),
                )
            }
            PreflightVerdict::Denied(_) => marker.to_string(),
        };

        Some(ResourceCard {
            kind: kind_label,
            reason,
            current_human,
            limit_human,
            percent_line,
            actions,
            selected: 0,
            detail,
            waiting: None,
        })
    }

    /// Dismiss the resource card and stop any bounded retry in flight.
    pub fn clear_resource_card(&mut self) {
        self.resource_card = None;
        self.resource_detail = false;
        if let Some(h) = self.resource_wait.take() {
            h.abort();
        }
    }

    /// Start a bounded WAITING FOR RESOURCES retry against the one gate.
    ///
    /// Never blocks the render loop: the wait runs on a background task and
    /// reports back into the card. The operator can cancel at any time.
    pub fn start_resource_wait(&mut self) {
        use openrt_runtime::resource_retry::{await_admission, ResourceRetryPolicy};
        let policy = ResourceRetryPolicy::default();
        let gate = self.runtime.resource_gate.clone();
        let token = openrt_core::CancellationToken::new();
        if let Some(h) = self.resource_wait.take() {
            h.abort();
        }
        let budget_ms = policy.max_total_wait().as_millis() as u64;
        if let Some(card) = self.resource_card.as_mut() {
            card.waiting = Some(ResourceWaitProgress {
                attempt: 0,
                max_attempts: policy.max_attempts,
                next_check_secs: (policy.initial_delay_ms as f64 / 1000.0).ceil() as u64,
                total_budget_ms: budget_ms,
            });
        }
        self.resource_wait = Some(tokio::spawn(async move {
            await_admission(&gate, policy, &token).await
        }));
        self.status = format!("waiting for resources — up to {budget_ms}ms, Esc cancels");
    }

    /// Adopt the result of a bounded wait: admitted resumes the prompt,
    /// exhausted/cancelled leaves the card in place with an honest status.
    pub fn finish_resource_wait(
        &mut self,
        outcome: &openrt_runtime::resource_retry::ResourceWaitOutcome,
    ) {
        self.resource_wait = None;
        let admitted = outcome.admitted();
        if let Some(card) = self.resource_card.as_mut() {
            card.waiting = None;
        }
        if admitted {
            let prompt = self.resource_prompt.clone();
            self.clear_resource_card();
            self.status = "resources available — resuming".into();
            if let Some(p) = prompt.filter(|p| !p.is_empty()) {
                self.input = p;
            }
        } else {
            self.status = match outcome.denial() {
                Some(d) => format!("still waiting — {}", d.to_line()),
                None => "wait cancelled".into(),
            };
        }
    }

    /// Refresh the cached header posture off the render path.
    pub fn refresh_resource_header(&mut self) {
        if self
            .resource_header_join
            .as_ref()
            .is_some_and(|h| !h.is_finished())
        {
            return;
        }
        let gate = self.runtime.resource_gate.clone();
        self.resource_header_join = Some(tokio::spawn(async move {
            use openrt_resources::preflight::ResourcePreflight;
            gate.refresh().await;
            ResourcePreflight::evaluate(&gate.current(), gate.limits())
        }));
    }

    /// Adopt a finished header probe and advance the refresh tick.
    ///
    /// Never blocks: an unfinished probe is left alone and picked up on a
    /// later tick, so the render path stays free of `/proc` reads.
    pub async fn poll_resource_header(&mut self) {
        self.resource_tick = self.resource_tick.wrapping_add(1);
        let finished = self
            .resource_header_join
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished);
        if !finished {
            return;
        }
        if let Some(h) = self.resource_header_join.take() {
            if let Ok(pf) = h.await {
                self.resource_header = Some(pf);
            }
        }
    }

    /// One-line posture for the header strip, always honest about unknowns.
    pub fn resource_status_line(&self) -> String {
        let Some(pf) = self.resource_header.as_ref() else {
            return "AGENT READY".to_string();
        };
        if pf.denial.is_some() {
            return "RESOURCE BLOCKED".to_string();
        }
        if pf.verdict().is_constrained() {
            return "RESOURCE TIGHT".to_string();
        }
        if self.running() {
            return "AGENT RUNNING".to_string();
        }
        "AGENT READY".to_string()
    }

    /// PART 15/20: classify a task error as a provider/model failure and
    /// build the sanitized MODEL ERROR card. Returns `None` for unrelated
    /// failures (they keep the plain stream error).
    fn model_error_card(&self, error: &str) -> Option<ModelErrorCard> {
        // AgentError::Model renders as "model provider error: {ModelError}".
        // Everything else (tool gate, cancelled, internal) is not ours.
        let inner = error
            .strip_prefix("model provider error: ")
            .or_else(|| error.strip_prefix("error: model provider error: "))?;

        let headline = if inner.contains("unparsable response")
            || inner.contains("Expected 'id'")
            || inner.contains("finish_reason `tool_calls`")
            || inner.contains("MODEL_TOOL_CALL_PARSE_ERROR")
            || inner.contains("empty assistant response")
        {
            "Could not parse the tool call.".to_string()
        } else if inner.contains("transport error") {
            "Provider unreachable.".to_string()
        } else if inner.contains("request timed out") {
            "Provider request timed out.".to_string()
        } else if inner.contains("not found on provider") {
            "Model not found on provider.".to_string()
        } else if inner.contains("not configured") {
            "Provider is not configured.".to_string()
        } else if inner.contains("HTTP error") {
            "Provider returned an HTTP error.".to_string()
        } else if inner.contains("egress refused") {
            "Provider blocked by egress policy.".to_string()
        } else {
            "Model request failed.".to_string()
        };

        let detail = sanitize_provider_error(inner);
        let reason = if detail.chars().count() > 160 {
            format!("{}…", detail.chars().take(160).collect::<String>())
        } else {
            detail.clone()
        };

        let info = self.runtime.provider.info();
        let provider = info.display_name.clone();
        let model = self
            .switcher
            .as_ref()
            .and_then(|s| s.selected())
            .filter(|m| !m.is_empty())
            .or(info.default_model.filter(|m| !m.is_empty()))
            .unwrap_or_else(|| "(default)".to_string());

        Some(ModelErrorCard {
            headline,
            reason,
            provider,
            model,
            detail,
        })
    }

    /// Session/task correlation: only our session's lifecycle events render;
    /// plan events render when a plan exists; everything else is other
    /// surfaces' traffic and must not pretend to be our conversation.
    fn relevant(&self, ev: &DomainEvent) -> bool {
        if matches!(
            ev.kind,
            EventKind::WorkflowStarted
                | EventKind::WorkflowStepStarted
                | EventKind::WorkflowStepCompleted
                | EventKind::WorkflowStepFailed
                | EventKind::WorkflowCompleted
                | EventKind::WorkflowPaused
                | EventKind::SafetyHalt
        ) {
            return !self.plan.is_empty() || matches!(ev.kind, EventKind::SafetyHalt);
        }
        match (self.current_session, ev.session_id) {
            (Some(me), Some(theirs)) => me == theirs,
            (Some(_), None) => false,
            (None, _) => false,
        }
    }

    /// Translate one runtime event into stream/status/plan mutations.
    pub async fn apply_event(&mut self, ev: DomainEvent) {
        if !self.relevant(&ev) {
            return;
        }
        let data = &ev.data;
        let tool = data
            .get("tool")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        match ev.kind {
            EventKind::SessionCreated => {
                self.push(MsgKind::Note("session created".into()));
                let _ = self.refresh_sessions().await;
            }
            EventKind::SessionLoaded => {}
            EventKind::TaskCreated => {
                self.status = "task created".into();
                let _ = self.refresh_tasks().await;
            }
            EventKind::TaskStarted | EventKind::AgentStarted => {
                self.status = "agent running…".into();
            }
            EventKind::PermissionRequested => {
                let args = data
                    .get("args")
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "{}".into());
                self.open_tool = Some(self.stream.len());
                self.push(MsgKind::Tool {
                    tool,
                    args,
                    state: ToolState::Running {
                        since: Instant::now(),
                    },
                });
            }
            EventKind::PermissionGranted | EventKind::PermissionApproved => {}
            EventKind::PermissionDenied => {
                let reason = data
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("denied by policy")
                    .to_string();
                self.close_tool(ToolState::Denied {
                    reason,
                    since: Instant::now(),
                });
            }
            EventKind::ToolInvocationStarted => {
                if self.open_tool.is_none() {
                    self.open_tool = Some(self.stream.len());
                    self.push(MsgKind::Tool {
                        tool,
                        args: String::new(),
                        state: ToolState::Running {
                            since: Instant::now(),
                        },
                    });
                }
            }
            EventKind::ToolInvocationCompleted => {
                let ok = data.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                let detail = match data.get("error").and_then(|v| v.as_str()) {
                    Some(e) => e.to_string(),
                    None => "ok".into(),
                };
                // Measure duration once, here: start = when the card opened
                // (permission asked / invocation started), end = now.
                let started = self
                    .open_tool
                    .and_then(|i| self.stream.get(i))
                    .and_then(|m| match &m.kind {
                        MsgKind::Tool {
                            state: ToolState::Running { since },
                            ..
                        } => Some(*since),
                        _ => None,
                    })
                    .unwrap_or_else(Instant::now);
                let took_ms = Instant::now().duration_since(started).as_millis();
                self.close_tool(ToolState::Done {
                    ok,
                    detail,
                    took_ms,
                });
            }
            EventKind::TaskCompleted => {
                if let Some(text) = data.get("result").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        self.push(MsgKind::Assistant(text.to_string()));
                    }
                }
                self.status = "task completed".into();
                let _ = self.refresh_tasks().await;
            }
            EventKind::TaskFailed => {
                let reason = data
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("failed")
                    .to_string();
                let error = data
                    .get("error")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let msg = match &error {
                    Some(e) if !e.is_empty() => format!("{reason}: {e}"),
                    _ => reason.clone(),
                };
                // A host resource refusal is its own state: RESOURCE LIMIT with
                // numbers and operator actions, never a MODEL ERROR card. The
                // session and its transcript stay intact either way.
                if reason == "error" {
                    if let Some(e) = error.as_deref().filter(|e| !e.is_empty()) {
                        if let Some(card) = self.resource_card(e) {
                            self.resource_card = Some(card);
                            self.resource_detail = false;
                        } else if let Some(card) = self.model_error_card(e) {
                            self.model_error = Some(card);
                        }
                    }
                }
                self.push(MsgKind::Err(msg.clone()));
                self.status = msg;
                let _ = self.refresh_tasks().await;
            }
            EventKind::WorkflowStarted => {
                let steps = data.get("steps").and_then(|v| v.as_u64()).unwrap_or(0);
                self.push(MsgKind::Note(format!("workflow started · {steps} steps")));
            }
            EventKind::WorkflowStepStarted => {
                if let Some(step) = data.get("step").and_then(|v| v.as_str()) {
                    self.mark_step(step, PlanStatus::Running);
                }
            }
            EventKind::WorkflowStepCompleted => {
                if let Some(step) = data.get("step").and_then(|v| v.as_str()) {
                    self.mark_step(step, PlanStatus::Done);
                }
            }
            EventKind::WorkflowStepFailed => {
                if let Some(step) = data.get("step").and_then(|v| v.as_str()) {
                    self.mark_step(step, PlanStatus::Failed);
                }
            }
            EventKind::WorkflowCompleted => {
                let st = data
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("finished")
                    .to_string();
                self.push(MsgKind::Note(format!("workflow {st}")));
            }
            EventKind::SafetyHalt => {
                let reason = data
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("halt")
                    .to_string();
                self.push(MsgKind::Err(format!("HALT: {reason}")));
                self.status = format!("HALT: {reason}");
            }
            EventKind::ResourceCheck => {
                let allowed = data.get("allowed").and_then(|v| v.as_bool());
                if allowed == Some(false) {
                    let reason = data
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("denied")
                        .to_string();
                    self.push(MsgKind::Err(format!("resource gate: {reason}")));
                }
            }
            EventKind::ErrorOccurred => {
                let msg = data
                    .get("error")
                    .or_else(|| data.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("runtime error")
                    .to_string();
                self.push(MsgKind::Err(msg));
            }
            _ => {}
        }
    }

    fn close_tool(&mut self, state: ToolState) {
        if let Some(idx) = self.open_tool.take() {
            if let Some(msg) = self.stream.get_mut(idx) {
                if let MsgKind::Tool { state: s, .. } = &mut msg.kind {
                    *s = state;
                }
            }
        }
    }

    fn mark_step(&mut self, id: &str, status: PlanStatus) {
        if let Some(step) = self.plan.iter_mut().find(|s| s.id == id) {
            step.status = status;
        }
    }

    // ------------------------------------------------------------------
    // chat / task lifecycle
    // ------------------------------------------------------------------

    pub fn running(&self) -> bool {
        self.task_join.is_some() || self.workflow_join.is_some()
    }

    /// Send the current input (chat message or `/command`).
    pub async fn send(&mut self) -> Result<(), AppError> {
        let line = self.input.trim().to_string();
        if line.is_empty() {
            self.status = "type a message first".into();
            return Ok(());
        }
        if line.starts_with('/') {
            self.input.clear();
            self.command(&line).await?;
            return Ok(());
        }
        if self.running() {
            self.status = "a task is running — Esc cancels it".into();
            return Ok(());
        }
        // Fail fast when the provider cannot resolve any model: the request
        // would die inside the provider with an opaque error. Keep the input
        // so the operator can fix the model first.
        let has_model = self
            .runtime
            .provider
            .info()
            .default_model
            .as_deref()
            .is_some_and(|m| !m.is_empty())
            || self
                .switcher
                .as_ref()
                .and_then(|s| s.selected())
                .is_some_and(|m| !m.is_empty());
        if !has_model {
            self.status =
                "no model — Ctrl+K → models, type a provider tag (e.g. llama3.1:8b), Enter".into();
            return Ok(());
        }
        self.input.clear();
        let sid = match self.current_session {
            Some(id) => id,
            None => {
                let s = self.runtime.create_session("agent").await?;
                self.current_session = Some(s.id);
                let _ = self.refresh_sessions().await;
                s.id
            }
        };
        self.push(MsgKind::User(line.clone()));
        // PART 20: keep the prompt so the MODEL ERROR card's `[r] retry`
        // can re-send it; a fresh send clears any stale card.
        self.last_prompt = Some(line.clone());
        self.model_error = None;
        self.model_error_detail = false;
        // A new send supersedes any resource refusal/wait: the operator moved
        // on, so the old card and its bounded retry must not linger.
        self.clear_resource_card();
        self.resource_prompt = Some(line.clone());
        let handle = self.runtime.spawn_task(sid, &line).await?;
        self.current_task = Some(handle.task_id());
        self.task_join = Some(tokio::spawn(handle.execute()));
        self.status = "task running…".into();
        self.refresh_tasks().await?;
        Ok(())
    }

    /// Cancel the in-flight task through the runtime's live-handle registry.
    pub async fn cancel(&mut self) {
        if let Some(id) = self.current_task {
            match self.runtime.task_cancel(id).await {
                Ok(t) => {
                    self.push(MsgKind::Note(format!("cancelled {}", t.id)));
                    self.status = format!("cancelled {}", t.id);
                }
                Err(e) => {
                    self.status = format!("cancel failed: {e}");
                }
            }
        } else {
            self.status = "nothing to cancel".into();
        }
    }

    /// Part C: cooperatively pause the in-flight task (stops at the next turn
    /// boundary; persists as `Paused`, never terminal).
    pub async fn pause(&mut self) {
        if let Some(id) = self.current_task {
            match self.runtime.task_pause(id).await {
                Ok(t) => {
                    self.push(MsgKind::Note(format!("pause signalled {}", t.id)));
                    self.status = format!(
                        "pause signalled {} (cooperative; persists at the next turn boundary)",
                        t.id
                    );
                }
                Err(e) => {
                    self.status = format!("pause failed: {e}");
                }
            }
        } else {
            self.status = "nothing to pause".into();
        }
    }

    /// Part C: resume a `Paused` task (clears a live pause signal, or re-runs
    /// a durably paused task over its preserved transcript).
    pub async fn resume(&mut self) {
        let id = match self.current_task {
            Some(id) => id,
            None => {
                self.status = "nothing to resume (no active task id)".into();
                return;
            }
        };
        match self.runtime.task_resume(id).await {
            Ok(t) => {
                self.push(MsgKind::Note(format!("resumed {}", t.id)));
                match t.status {
                    openrt_core::TaskStatus::Completed => {
                        self.status = format!("resumed {}: {}", t.id, t.result.unwrap_or_default());
                    }
                    s => self.status = format!("resumed {}: {s:?}", t.id),
                }
            }
            Err(e) => {
                self.status = format!("resume failed: {e}");
            }
        }
    }

    // ------------------------------------------------------------------
    // slash commands
    // ------------------------------------------------------------------

    async fn command(&mut self, line: &str) -> Result<(), AppError> {
        let mut parts = line[1..].splitn(2, ' ');
        let cmd = parts.next().unwrap_or("").trim().to_lowercase();
        let arg = parts.next().unwrap_or("").trim();
        match cmd.as_str() {
            "help" | "?" => self.overlay = Some(Overlay::Help),
            "setup" => self.setup_begin().await,
            "clear" => {
                self.stream.clear();
                self.open_tool = None;
                self.chat_scroll = 0;
                self.status = "stream cleared".into();
            }
            "new" | "session" => {
                self.new_session("agent").await?;
            }
            "sessions" | "switch" => {
                let _ = self.refresh_sessions().await;
                self.overlay = Some(Overlay::Sessions);
            }
            "cancel" => self.cancel().await,
            "pause" => self.pause().await,
            "resume" => self.resume().await,
            "plan" => self.compile_plan(arg).await?,
            "run" => self.start_plan().await?,
            "model" => {
                self.refresh_models().await;
                self.overlay = Some(Overlay::Model);
            }
            // Same route as the palette entry, for the same reason: a slash
            // command must not be a second way to leave a stale hit registry
            // behind.
            "workbench" | "wb" => self.enter_workbench().await,
            // PART 21: operator debug path — show the raw body of the last
            // model response with secrets redacted (never printed raw).
            "debug" => match arg {
                "model-response" | "response" => {
                    let raw = self.runtime.provider.last_raw_response();
                    match raw {
                        Some(body) => {
                            let redacted =
                                openrt_core::RedactionPolicy::default().redact_text(&body);
                            self.push(MsgKind::Note("last model response (redacted):".into()));
                            for l in redacted.lines() {
                                self.push(MsgKind::Note(l.to_string()));
                            }
                            self.status =
                                "last model response shown (redacted) in the stream".into();
                        }
                        None => {
                            self.status =
                                "no captured response — this transport does not store one (run a task first)"
                                    .into();
                        }
                    }
                }
                _ => {
                    self.status =
                        "usage: /debug model-response — redacted last response body".into();
                }
            },
            "refresh" | "r" => self.refresh_view().await?,
            "status" => {
                let sess = self
                    .current_session
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "(none)".into());
                let task = self
                    .current_task
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "(none)".into());
                self.status = format!(
                    "session {sess} · task {task} · mode {}",
                    self.fetch_mode().await
                );
            }
            "quit" | "exit" => self.should_quit = true,
            other => {
                self.status = format!("unknown command /{other} — /help lists commands");
            }
        }
        Ok(())
    }

    pub async fn new_session(&mut self, title: &str) -> Result<(), AppError> {
        if self.running() {
            self.status = "finish the running task first".into();
            return Ok(());
        }
        let s = self.runtime.create_session(title).await?;
        self.current_session = Some(s.id);
        self.stream.clear();
        self.open_tool = None;
        self.plan.clear();
        self.compiled = None;
        self.plan_goal = None;
        self.plan_failure = None;
        self.refresh_sessions().await?;
        self.refresh_tasks().await?;
        self.status = format!("new session {} ({title})", s.id);
        Ok(())
    }

    /// Select a session and load its REAL persisted transcript.
    pub async fn select_session(&mut self, idx: usize) -> Result<(), AppError> {
        if self.running() {
            self.status = "finish the running task first".into();
            return Ok(());
        }
        let Some(s) = self.sessions.get(idx) else {
            return Ok(());
        };
        let id = s.id;
        let loaded = self.runtime.load_session(id).await?;
        self.current_session = Some(id);
        self.stream = loaded
            .content
            .iter()
            .map(|m| {
                let kind = match m.role {
                    MessageRole::User => MsgKind::User(m.content.clone()),
                    MessageRole::Assistant => MsgKind::Assistant(m.content.clone()),
                    MessageRole::System => MsgKind::Note(m.content.clone()),
                    MessageRole::Tool => MsgKind::Note(format!("tool: {}", m.content.clone())),
                };
                Msg {
                    at: String::new(),
                    kind,
                }
            })
            .collect();
        self.open_tool = None;
        self.overlay = None;
        self.refresh_sessions().await?;
        self.refresh_tasks().await?;
        self.status = format!("session {} · {} messages", id, loaded.content.len());
        Ok(())
    }

    /// `d` in the Sessions view (or session switcher): arm the delete
    /// confirmation for the selected session. Nothing is written yet.
    pub fn sessions_delete_request(&mut self) {
        if self.running() {
            self.status = "finish the running task before deleting a session".into();
            return;
        }
        let idx = self.filtered_sessions().get(self.sel).copied();
        let Some(s) = idx.and_then(|i| self.sessions.get(i)) else {
            self.status = "no session selected".into();
            return;
        };
        let (id, title) = (s.id, s.title.clone());
        self.confirm = Some(Confirm::DeleteSession { id, title });
        self.status = format!("delete “{}”? y = delete · any other key cancels", s.title);
    }

    /// Explicit `y`: run the confirmed action through the ONE runtime.
    pub async fn confirm_accept(&mut self) -> std::result::Result<(), AppError> {
        match self.confirm.take() {
            None => Ok(()),
            Some(Confirm::DeleteSession { id, title }) => {
                if self.running() {
                    self.status = "task running — delete cancelled".into();
                    return Ok(());
                }
                match self.runtime.delete_session(id).await {
                    Ok(()) => {
                        let was_current = self.current_session == Some(id);
                        if was_current {
                            self.current_session = None;
                            self.stream.clear();
                            self.open_tool = None;
                            self.plan.clear();
                            self.compiled = None;
                            self.plan_goal = None;
                            self.plan_failure = None;
                            self.push(MsgKind::Note(format!(
                                "deleted session “{title}” (transcript removed; task history kept)"
                            )));
                        }
                        let _ = self.refresh_sessions().await;
                        if self.sel >= self.sessions.len() {
                            self.sel = self.sessions.len().saturating_sub(1);
                        }
                        self.status = format!("deleted session “{title}”");
                    }
                    Err(e) => self.status = format!("delete failed: {e}"),
                }
                Ok(())
            }
            Some(Confirm::PolicyPreset { label, full }) => {
                if self.op_join.is_some() {
                    self.status =
                        "another operator action is running — retry after it finishes".into();
                    return Ok(());
                }
                let rules = self.preset_rules(full);
                let n = rules.len();
                if n == 0 {
                    self.status = "no capabilities registered — nothing to append".into();
                    return Ok(());
                }
                let rt = self.runtime.clone();
                let actor = self.operator_id.trim().to_string();
                let note = format!("tui preset: {label}");
                self.status = format!("appending {n} allow rules (preset `{label}`) …");
                self.op_join = Some(tokio::spawn(async move {
                    let mut v = rt
                        .policy_document_ensure()
                        .await
                        .map_err(|e| e.to_string())?
                        .version;
                    for pattern in &rules {
                        v = rt
                            .policy_append_rule(
                                &actor,
                                &note,
                                pattern,
                                openrt_permissions::RuleAction::Allow,
                                None,
                            )
                            .await
                            .map_err(|e| e.to_string())?
                            .version;
                    }
                    let res: Result<(String, Option<Vec<String>>), String> = Ok((
                        format!(
                            "preset `{label}` applied · {n} allow rules appended · policy doc v{v} (enforced from the next runtime build; the live gate keeps its current policy)"
                        ),
                        None,
                    ));
                    res
                }));
                Ok(())
            }
            Some(Confirm::PolicyAppend {
                pattern,
                action,
                scope,
            }) => {
                let act = match action.as_str() {
                    "allow" => openrt_permissions::RuleAction::Allow,
                    "deny" => openrt_permissions::RuleAction::Deny,
                    "ask" => openrt_permissions::RuleAction::Ask,
                    other => {
                        self.status = format!("unknown action `{other}` (allow|deny|ask)");
                        return Ok(());
                    }
                };
                if self.op_join.is_some() {
                    self.status =
                        "another operator action is running — retry after it finishes".into();
                    return Ok(());
                }
                let rt = self.runtime.clone();
                let actor = self.operator_id.trim().to_string();
                let p = pattern.clone();
                let note = format!("tui rule: {action} {p}");
                self.status = format!("appending rule `{p}` → {action} …");
                self.op_join = Some(tokio::spawn(async move {
                    rt.policy_document_ensure()
                        .await
                        .map_err(|e| e.to_string())?;
                    let doc = rt
                        .policy_append_rule(&actor, &note, &p, act, scope)
                        .await
                        .map_err(|e| e.to_string())?;
                    let res: Result<(String, Option<Vec<String>>), String> = Ok((
                        format!(
                            "rule `{p}` → {action} appended · policy doc v{} (enforced from the next runtime build)",
                            doc.version
                        ),
                        None,
                    ));
                    res
                }));
                Ok(())
            }
            Some(Confirm::Mode { to }) => {
                let target = match to.as_str() {
                    "provisioning" => openrt_modes::ExecutionMode::Provisioning,
                    _ => openrt_modes::ExecutionMode::Sovereign,
                };
                let actor = self.operator_id.trim().to_string();
                let actor = if actor.is_empty() {
                    "operator".into()
                } else {
                    actor
                };
                match self
                    .runtime
                    .modes
                    .transition(&actor, target, "operator switch from the TUI")
                    .await
                {
                    Ok(_) => {
                        self.exec_mode = target.as_str().to_string();
                        self.net_allowed = target.network_allowed();
                        // The hub status note explains the LAST attempt's
                        // network posture — it goes stale the moment the
                        // mode moves. Re-point it at the not-loaded state so
                        // the pane can't claim "blocked in sovereign mode"
                        // while egress is allowed (rows already fetched stay).
                        if self.hub_status.contains("blocked in sovereign")
                            || self.hub_status.contains("not contacted")
                        {
                            self.hub_status = "not loaded — Tab to [Hugging Face]".into();
                        }
                        self.push(MsgKind::Note(format!(
                            "operating mode → {} (audited · network {})",
                            target.as_str(),
                            if self.net_allowed {
                                "allowed"
                            } else {
                                "blocked"
                            }
                        )));
                        self.status = format!(
                            "mode {} · network {} — refreshing…",
                            target.as_str(),
                            if self.net_allowed { "on" } else { "off" }
                        );
                        let _ = self.refresh_view().await;
                    }
                    Err(e) => self.status = format!("mode switch failed: {e}"),
                }
                Ok(())
            }
        }
    }

    pub fn confirm_cancel(&mut self) {
        if self.confirm.take().is_some() {
            self.status = "cancelled".into();
        }
    }

    // ------------------------------------------------------------------
    // plan
    // ------------------------------------------------------------------

    pub(crate) async fn compile_plan(&mut self, goal: &str) -> Result<(), AppError> {
        let goal = if !goal.is_empty() {
            goal.to_string()
        } else if let Some(u) = self.stream.iter().rev().find_map(|m| match &m.kind {
            MsgKind::User(u) => Some(u.clone()),
            _ => None,
        }) {
            u
        } else {
            self.status = "usage: /plan <goal>".into();
            return Ok(());
        };
        match self
            .runtime
            .compile_workflow(&goal, openrt_compiler::CompileMode::Compile)
            .await
        {
            Ok(out) => {
                self.plan = out
                    .workflow
                    .steps
                    .iter()
                    .map(|s| PlanStep {
                        id: s.id.clone(),
                        label: format!("{} · {}", s.kind, s.id),
                        status: PlanStatus::Pending,
                    })
                    .collect();
                self.plan_goal = Some(goal.clone());
                self.plan_failure = None;
                self.compiled = Some(out.workflow.clone());
                // The full plan IS the chat output — steps stay live-bound
                // through `mark_step` (statuses resolve against `app.plan`).
                self.push(MsgKind::Plan {
                    id: out.workflow_id.clone(),
                    goal,
                    steps: self
                        .plan
                        .iter()
                        .map(|s| (s.label.clone(), PlanStatus::Pending))
                        .collect(),
                });
                self.status = format!(
                    "compiled {} · {} steps — /run to execute",
                    out.workflow_id,
                    self.plan.len()
                );
                Ok(())
            }
            Err(e) => {
                // A compiler/gate rejection is a STRUCTURED result inside the
                // session — it must never reach the event loop as a fatal
                // error (the process stays alive; the session stays open).
                let (code, reason, fix) = match &e {
                    openrt_runtime::Error::Compile(diags) => {
                        let first = diags.iter().find(|d| d.is_error()).or(diags.first());
                        let code = first
                            .map(|d| d.code.as_str().to_string())
                            .unwrap_or_else(|| "compile_error".into());
                        let reason = diags
                            .iter()
                            .map(|d| {
                                format!(
                                    "[{}] {}\n     why: {}\n     at:  {}\n     fix: {}",
                                    d.code.as_str(),
                                    d.what,
                                    d.why,
                                    d.where_,
                                    d.how_to_fix
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        let fix = first
                            .map(|d| d.how_to_fix.clone())
                            .unwrap_or_else(|| "restate the goal".into());
                        (code, reason, fix)
                    }
                    other => (
                        "plan_failed".to_string(),
                        other.to_string(),
                        "fix the reported condition and press r to retry".to_string(),
                    ),
                };
                self.plan_goal = None;
                self.compiled = None;
                self.plan.clear();
                self.plan_failure = Some(goal.clone());
                self.push(MsgKind::PlanFailed {
                    goal,
                    code,
                    reason,
                    fix,
                });
                self.status = "PLAN FAILED — r = retry · e = edit · Esc = dismiss".into();
                Ok(())
            }
        }
    }

    async fn start_plan(&mut self) -> Result<(), AppError> {
        if self.running() {
            self.status = "a task is running — wait or Esc-cancel it".into();
            return Ok(());
        }
        let Some(wf) = self.compiled.clone() else {
            self.status = "no compiled plan — /plan <goal> first".into();
            return Ok(());
        };
        let sid = match self.current_session {
            Some(id) => id,
            None => {
                let s = self.runtime.create_session("agent-plan").await?;
                self.current_session = Some(s.id);
                let _ = self.refresh_sessions().await;
                s.id
            }
        };
        let rt = self.runtime.clone();
        self.workflow_join = Some(tokio::spawn(async move { rt.run_workflow(sid, wf).await }));
        self.status = "workflow running…".into();
        Ok(())
    }

    /// Expand the newest tool card into the Preview overlay: full args
    /// (never truncated at capture time) + the live gate state.
    pub fn expand_last_tool(&mut self) {
        let idx = self
            .stream
            .iter()
            .rposition(|m| matches!(m.kind, MsgKind::Tool { .. }));
        let Some(msg) = idx.and_then(|i| self.stream.get(i).cloned()) else {
            self.status = "no tool calls in this stream".into();
            return;
        };
        let MsgKind::Tool { tool, args, state } = msg.kind else {
            return;
        };
        let state_line = match &state {
            ToolState::Running { since } => format!("running · {}s", since.elapsed().as_secs()),
            ToolState::Done {
                ok,
                detail,
                took_ms,
            } => {
                format!(
                    "{} · {detail} · {took_ms}ms",
                    if *ok { "ok" } else { "failed" },
                )
            }
            ToolState::Denied { reason, .. } => format!("denied · {reason}"),
        };
        let mut lines = vec![
            format!("tool    {tool}"),
            format!("state   {state_line}"),
            String::new(),
            "args (full)".into(),
        ];
        lines.extend(args.split('\n').map(str::to_string));
        self.preview = Some(PreviewData {
            title: format!("tool · {tool}"),
            lines,
        });
        self.preview_scroll = 0;
        self.overlay = Some(Overlay::Preview);
    }

    // ------------------------------------------------------------------
    // view plumbing
    // ------------------------------------------------------------------

    pub fn set_view(&mut self, v: View) {
        if self.view == v {
            return;
        }
        // The input line is the model-id field while the Models view is
        // active: stash any chat draft on entry, drop unapplied ids on exit
        // and restore the draft everywhere else.
        if self.view == View::Models {
            self.input.clear();
        } else if v == View::Models && !self.input.is_empty() {
            self.draft = std::mem::take(&mut self.input);
        }
        if v != View::Models && self.input.is_empty() && !self.draft.is_empty() {
            self.input = std::mem::take(&mut self.draft);
        }
        // Ctrl+F search state is Models-local; the universal list filter
        // belongs to the view it was opened in.
        if v != View::Models || self.view != View::Models {
            self.model_search_active = false;
        }
        if v != self.view {
            self.filter_active = false;
            self.list_filter.clear();
            self.view_scroll = 0;
            self.term_scroll = 0;
        }
        self.view = v;
        self.sel = 0;
    }

    /// Toggle the Ctrl+F model search (Models view only). Query text is
    /// stashed so leaving and re-entering search keeps the operator's input.
    pub fn toggle_model_search(&mut self) {
        if self.view != View::Models {
            return;
        }
        self.model_search_active = !self.model_search_active;
        self.status = if self.model_search_active {
            "model search: type a query, Enter · Esc back".into()
        } else {
            "search closed".into()
        };
        self.sel = 0;
    }

    /// Palette entries: every rail view, the Workbench switch, the slash
    /// commands, and the two operator overlays (fuzzy-matched — no invented
    /// actions; every entry executes a real command/overlay).
    pub fn palette_items(&self) -> Vec<String> {
        let mut items: Vec<String> = View::ALL.iter().map(|v| v.label().to_string()).collect();
        items.push("WORKBENCH".into());
        for c in [
            "/help",
            "/plan <goal>",
            "/run",
            "/model",
            "/setup",
            "/new session",
            "/sessions",
            "/status",
            "/refresh",
            "/cancel",
            "/clear",
        ] {
            items.push(format!("command  {c}"));
        }
        items.push("open  operating mode".into());
        items
    }

    /// Indices into [`Self::palette_items`] matching the current query —
    /// subsequence fuzzy match, best scores first (empty query = source order).
    pub fn palette_matches(&self) -> Vec<usize> {
        let q = self.palette.trim().to_lowercase();
        let items = self.palette_items();
        if q.is_empty() {
            return (0..items.len()).collect();
        }
        let mut scored: Vec<(i64, usize)> = items
            .iter()
            .enumerate()
            .filter_map(|(i, label)| fuzzy_score(&q, &label.to_lowercase()).map(|s| (s, i)))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        scored.into_iter().map(|(_, i)| i).collect()
    }

    /// Enter Workbench from any mode or overlay.
    ///
    /// One door, because a mode change is a *geometry* change: the two modes own
    /// different hit registries with different meanings for the same
    /// coordinates. Leaving a stale registry behind lets a click answer a control
    /// that is no longer on screen — a permission card in one mode, a panel row in
    /// the other.
    ///
    /// The palette jump used to set `self.mode = Workbench` inline, skipping all
    /// of this: jumping to WORKBENCH from the palette left the Agent's registry
    /// and any armed press in place, so the first click in Workbench resolved
    /// against rectangles the Agent had drawn. Every path now goes through here.
    pub async fn enter_workbench(&mut self) {
        self.clear_hits();
        self.workbench.clear_interaction();
        self.overlay = None;
        self.mode = UiMode::Workbench;
        let _ = self.workbench.refresh().await;
        self.status = "workbench mode — Ctrl+W back".into();
    }

    /// Return to Agent Mode from Workbench.
    ///
    /// The mirror of [`Self::enter_workbench`], and for the same reason: the
    /// Workbench registry describes panels, and dispatching Agent clicks against
    /// it would need a guess about which panel the coordinate belonged to.
    pub fn enter_agent(&mut self) {
        self.workbench.clear_interaction();
        self.clear_hits();
        self.overlay = None;
        self.mode = UiMode::Agent;
        self.status = "agent mode".into();
    }

    /// Jump to a palette selection (view, Workbench, slash command, or the
    /// operating-mode control).
    pub async fn palette_jump(&mut self, item: usize) {
        let items = self.palette_items();
        let Some(label) = items.get(item).cloned() else {
            self.overlay = None;
            return;
        };
        let n_nav = View::ALL.len();
        if item < n_nav {
            self.set_view(View::from_idx(item));
            self.overlay = None;
        } else if label == "WORKBENCH" {
            self.enter_workbench().await;
        } else if label == "open  operating mode" {
            self.overlay = Some(Overlay::Mode);
            self.status = "↑/↓ select mode · Enter switches (confirm) · Esc closes".into();
        } else if let Some(cmd) = label.strip_prefix("command  ") {
            self.overlay = None;
            if cmd.contains('<') {
                // Parameterized command (e.g. `/plan <goal>`): prefill only.
                self.input = cmd.to_string();
            } else {
                self.input = cmd.to_string();
                let _ = self.send().await;
            }
        } else {
            self.overlay = None;
        }
    }

    pub async fn refresh_sessions(&mut self) -> Result<(), AppError> {
        self.sessions = self.runtime.list_sessions().await?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // universal list filter (Tools / Tasks / Sessions / MCP)
    // ------------------------------------------------------------------

    /// Toggle the Ctrl+F filter for the current list view.
    pub fn toggle_list_filter(&mut self) {
        match self.view {
            View::Files
            | View::Tools
            | View::Tasks
            | View::Sessions
            | View::Mcp
            | View::Extensions
            | View::Security => {
                self.filter_active = !self.filter_active;
                if !self.filter_active {
                    self.list_filter.clear();
                    self.sel = 0;
                } else {
                    self.filter_sel_reset();
                }
                self.status = if self.filter_active {
                    if self.view == View::Files {
                        "filter: type to narrow · Enter opens · Esc closes".into()
                    } else {
                        "filter: type to narrow · Enter keeps focus · Esc closes".into()
                    }
                } else {
                    "filter closed".into()
                };
            }
            _ => {}
        }
    }

    /// Selection after any filter edit: always the FIRST VISIBLE row — on
    /// Files the sel is an original index, so landing on row 0 would leave
    /// the highlight (and Enter) on a file the filter just hid.
    pub fn filter_sel_reset(&mut self) {
        self.sel = if self.view == View::Files && self.filter_active && !self.list_filter.is_empty()
        {
            self.filtered_files().first().copied().unwrap_or(0)
        } else {
            0
        };
    }

    fn filter_matches(&self, hay: &str) -> bool {
        if !self.filter_active || self.list_filter.is_empty() {
            return true;
        }
        hay.to_lowercase()
            .contains(&self.list_filter.to_lowercase())
    }

    /// Does task `i` survive the active filter?
    ///
    /// The predicate is public and the [`Self::filtered_tasks`] vector is derived
    /// from it, so a repaint can ask about rows without materialising an index
    /// vector. Keyboard handling still uses the vector: it needs "how many survive"
    /// to clamp a movement, and it runs once per keypress rather than once per
    /// frame.
    /// Number of survivors for a predicate, without materialising an index vector.
    ///
    /// Purely O(n) in time and O(1) in space, which is what a header count needs.
    pub fn count_matching(&self, len: usize, matches: &dyn Fn(usize) -> bool) -> usize {
        (0..len).filter(|i| matches(*i)).count()
    }

    pub fn task_matches(&self, i: usize) -> bool {
        let t = &self.tasks[i];
        self.filter_matches(&t.prompt)
            || self.filter_matches(&t.id.to_string())
            || self.filter_matches(&format!("{:?}", t.status))
    }

    /// Indices into `tasks` that survive the active filter.
    pub fn filtered_tasks(&self) -> Vec<usize> {
        (0..self.tasks.len())
            .filter(|i| self.task_matches(*i))
            .collect()
    }

    /// Does session `i` survive the active filter? See [`Self::task_matches`].
    pub fn session_matches(&self, i: usize) -> bool {
        let s = &self.sessions[i];
        self.filter_matches(&s.title) || self.filter_matches(&s.id.to_string())
    }

    /// Indices into `sessions` that survive the active filter.
    pub fn filtered_sessions(&self) -> Vec<usize> {
        (0..self.sessions.len())
            .filter(|i| self.session_matches(*i))
            .collect()
    }

    /// Does tool `i` survive the active filter? See [`Self::task_matches`].
    pub fn tool_matches(&self, i: usize) -> bool {
        let t = &self.tools[i];
        self.filter_matches(&t.name)
            || t.required_caps
                .iter()
                .any(|c| self.filter_matches(c.as_str()))
    }

    /// Indices into `tools` that survive the active filter.
    pub fn filtered_tools(&self) -> Vec<usize> {
        (0..self.tools.len())
            .filter(|i| self.tool_matches(*i))
            .collect()
    }

    /// Does MCP server `i` survive the active filter? See [`Self::task_matches`].
    pub fn mcp_matches(&self, i: usize) -> bool {
        let r = &self.mcp_rows[i];
        self.filter_matches(&r.name)
            || self.filter_matches(&r.trust)
            || self.filter_matches(&r.security)
            || self.filter_matches(&r.source)
    }

    /// Indices into `mcp_rows` that survive the active filter.
    pub fn filtered_mcp(&self) -> Vec<usize> {
        (0..self.mcp_rows.len())
            .filter(|i| self.mcp_matches(*i))
            .collect()
    }

    /// Does extension `i` survive the active filter? See [`Self::task_matches`].
    pub fn ext_matches(&self, i: usize) -> bool {
        let e = &self.ext_rows[i];
        self.filter_matches(&e.id)
            || self.filter_matches(&e.name)
            || self.filter_matches(&e.activation)
            || self.filter_matches(&e.security)
    }

    /// Indices into `ext_rows` that survive the active filter.
    pub fn filtered_ext(&self) -> Vec<usize> {
        (0..self.ext_rows.len())
            .filter(|i| self.ext_matches(*i))
            .collect()
    }

    /// How many rows the Security view has: live rules + shield findings +
    /// recovery records (every row a real read — nothing synthesized).
    ///
    /// This is arithmetic, not a build. Pair with [`Self::security_row`], which
    /// formats exactly the row asked for.
    pub fn security_row_count(&self) -> usize {
        let findings = if self.findings.is_empty() {
            1 // "(none recorded)"
        } else {
            self.findings.len()
        };
        let recovery = if self.recovery_rows.is_empty() {
            1 // "(none recorded)"
        } else {
            self.recovery_rows.len()
        };
        self.policy_rows.len() + 1 + findings + 1 + recovery
    }

    /// The `i`-th Security row, built on demand.
    ///
    /// [`Self::security_rows`] used to assemble the whole list — one `format!` per
    /// finding and recovery record — on every repaint, so a long-lived session
    /// with many findings paid for all of them to show a screenful. The section
    /// layout is identical; only the construction is lazy.
    pub fn security_row(&self, i: usize) -> (String, String) {
        let findings_start = self.policy_rows.len();
        let findings_len = if self.findings.is_empty() {
            1
        } else {
            self.findings.len()
        };
        let recovery_start = findings_start + 1 + findings_len;

        if let Some(row) = self.policy_rows.get(i) {
            return row.clone();
        }
        if i == findings_start {
            return ("shield findings".into(), String::new());
        }
        if i < recovery_start {
            // An empty section still gets one row, and it is a literal — so the
            // empty case has to be answered before indexing.
            if let Some(f) = self.findings.get(i - findings_start - 1) {
                return (
                    format!("[{}] {}", f.severity, f.category),
                    format!("{} · {}", f.reason, f.status),
                );
            }
            return ("(none recorded)".into(), String::new());
        }
        if i == recovery_start {
            return ("recovery".into(), String::new());
        }
        match self.recovery_rows.get(i - recovery_start - 1) {
            Some(r) => (
                format!("record {}", r.record_id),
                format!(
                    "{} interrupted · {} incomplete · {} stale sandboxes · clean={}",
                    r.interrupted, r.incomplete, r.stale_sandboxes, r.clean
                ),
            ),
            None => ("(none recorded)".into(), String::new()),
        }
    }

    /// Indices into the Security row list that survive the active filter.
    ///
    /// Used by key handling, which needs a count to clamp movement against.
    /// Repaints use [`Self::security_row`] + [`Self::security_matches`] instead.
    pub fn filtered_security(&self) -> Vec<usize> {
        (0..self.security_row_count())
            .filter(|i| self.security_matches_lazy(*i))
            .collect()
    }

    /// Does Security row `i` survive the active filter?
    ///
    /// Formats the row to answer the question, which is why the repaint path uses
    /// it through [`crate::design::collections::FilteredWindow`]: the predicate is
    /// asked about every row, but only the visible ones are painted or hit-tested.
    pub fn security_matches(&self, i: usize) -> bool {
        let (k, v) = self.security_row(i);
        self.filter_matches(&k) || self.filter_matches(&v)
    }

    /// [`Self::security_matches`] without formatting the row.
    ///
    /// For the *empty* filter, where the answer is known without reading the row.
    /// A non-empty filter still needs the text, so it goes through
    /// [`Self::security_matches`].
    pub fn security_matches_lazy(&self, i: usize) -> bool {
        if self.list_filter.is_empty() {
            return true;
        }
        self.security_matches(i)
    }

    /// Does file `i` survive the active Ctrl+F filter? See [`Self::task_matches`].
    pub fn file_matches(&self, i: usize) -> bool {
        self.filter_matches(&self.files[i].name)
    }

    /// Files rows (original indices) matching the active Ctrl+F filter.
    pub fn filtered_files(&self) -> Vec<usize> {
        (0..self.files.len())
            .filter(|i| self.file_matches(*i))
            .collect()
    }

    // ------------------------------------------------------------------
    // Models browser sections (PART G/H — accessors, pure data only)
    // ------------------------------------------------------------------

    /// Hub rows matching the active pipeline facet AND every active `f`
    /// filter (insertion order).
    pub fn hub_indices(&self) -> Vec<usize> {
        self.hub_rows
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                (match &self.hub_facet {
                    None => true,
                    Some(f) => r.pipeline_tag.as_deref() == Some(f.as_str()),
                }) && self.hub_filter_matches(r)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// True when `row` matches every active hub filter (a `None` value on a
    /// dimension is "all"). Dimensions the loaded rows cannot answer never
    /// filter anything out (their value can only be `(all)`).
    pub fn hub_filter_matches(&self, row: &openrt_registry::HfModelLite) -> bool {
        self.hub_filters.iter().all(|(dim, val)| {
            let Some(v) = val else {
                return true;
            };
            match dim {
                HubFilterDim::Task => row.pipeline_tag.as_deref() == Some(v.as_str()),
                HubFilterDim::Provider => row.author.as_deref() == Some(v.as_str()),
                HubFilterDim::Parameters => {
                    row.params.map(|p| fmt_params(p) == *v).unwrap_or(false)
                }
                HubFilterDim::Quantization => {
                    quant_from_tags(&row.tags).as_deref() == Some(v.as_str())
                }
                HubFilterDim::Vision => {
                    row_vision(row).map(|b| if b { "yes" } else { "no" }) == Some(v.as_str())
                }
                HubFilterDim::ToolCalling => {
                    row_tool_calling(row).map(|b| if b { "yes" } else { "no" }) == Some(v.as_str())
                }
                // No per-row data in the loaded listing — nothing to match.
                HubFilterDim::Context | HubFilterDim::Format | HubFilterDim::Runtime => true,
            }
        })
    }

    /// Distinct filter values for a dimension, derived from the loaded rows
    /// in listing order. Empty = the rows cannot answer this dimension.
    pub fn hub_filter_values(&self, dim: HubFilterDim) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut push = |v: String| {
            if !v.is_empty() && !out.contains(&v) {
                out.push(v);
            }
        };
        for r in &self.hub_rows {
            match dim {
                HubFilterDim::Task => {
                    if let Some(t) = &r.pipeline_tag {
                        push(t.clone());
                    }
                }
                HubFilterDim::Provider => {
                    if let Some(a) = &r.author {
                        push(a.clone());
                    }
                }
                HubFilterDim::Parameters => {
                    if let Some(p) = r.params {
                        push(fmt_params(p));
                    }
                }
                HubFilterDim::Quantization => {
                    if let Some(q) = quant_from_tags(&r.tags) {
                        push(q);
                    }
                }
                HubFilterDim::Vision => {
                    if let Some(b) = row_vision(r) {
                        push(if b { "yes" } else { "no" }.to_string());
                    }
                }
                HubFilterDim::ToolCalling => {
                    if let Some(b) = row_tool_calling(r) {
                        push(if b { "yes" } else { "no" }.to_string());
                    }
                }
                // Not derivable from the lite listing (needs per-row detail).
                HubFilterDim::Context | HubFilterDim::Format | HubFilterDim::Runtime => {}
            }
        }
        out
    }

    /// The hub row under the cursor (faceted index → real row).
    pub fn hub_selected(&self) -> Option<&openrt_registry::HfModelLite> {
        self.hub_indices()
            .get(self.sel)
            .copied()
            .map(|i| &self.hub_rows[i])
    }

    /// Registry cards with real bytes on disk (`installed_bytes > 0`).
    pub fn installed_cards(&self) -> Vec<&openrt_registry::RegistryCard> {
        self.model_cards
            .iter()
            .filter(|c| c.installed_bytes > 0)
            .collect()
    }

    /// Curated catalog cards (`source == Curated`) — the "Recommended"
    /// section shows exactly what the offline catalog contains, nothing else.
    pub fn recommended_cards(&self) -> Vec<&openrt_registry::RegistryCard> {
        self.model_cards
            .iter()
            .filter(|c| c.source == openrt_registry::SourceLabel::Curated)
            .collect()
    }

    /// Row count of the active section (drives list navigation bounds).
    pub fn models_section_len(&self) -> usize {
        match self.model_section {
            ModelSection::Local => self.model_cards.len(),
            ModelSection::Hub => self.hub_indices().len(),
            ModelSection::Installed => self.installed_cards().len(),
            ModelSection::Recommended => self.recommended_cards().len(),
        }
    }

    /// The registry card under the cursor for card-backed sections
    /// (Local / Installed / Recommended). Hub rows go through
    /// [`Self::hub_selected`] instead.
    pub fn section_card(&self) -> Option<&openrt_registry::RegistryCard> {
        match self.model_section {
            ModelSection::Local => self.model_cards.get(self.sel),
            ModelSection::Installed => self.installed_cards().get(self.sel).copied(),
            ModelSection::Recommended => self.recommended_cards().get(self.sel).copied(),
            ModelSection::Hub => None,
        }
    }

    /// The selected card's resource profile, if it has been probed.
    pub fn selected_profile(
        &self,
    ) -> Option<(&String, &openrt_models::artifact::ModelResourceProfile)> {
        let card = self.section_card()?;
        self.model_profiles
            .get_key_value(&card.artifact_id)
            .map(|(k, v)| (k, v))
    }

    /// Id of the hub row under the cursor ("" when not on a hub row).
    pub fn hub_selected_id(&self) -> String {
        self.hub_selected()
            .map(|r| r.id.clone())
            .unwrap_or_default()
    }

    /// Detail lines for the right pane — only real hub/config facts.
    /// Unknown fields display `UNKNOWN` (with why) — never a guessed value.
    /// Returns `None` when there is no detail for the selection yet.
    pub fn hf_detail_lines(&self, max_width: usize) -> Option<Vec<String>> {
        let row = self.hub_selected()?;
        let w = max_width.max(12);
        let mut out: Vec<String> = Vec::new();
        out.push(fmt_kv("id", &row.id, w));
        let detail = self.hf_detail.as_ref().filter(|d| d.id == row.id);
        let author = detail
            .and_then(|d| d.author.clone())
            .or_else(|| row.author.clone());
        out.push(fmt_kv("author", author.as_deref().unwrap_or("UNKNOWN"), w));
        let pipeline = row
            .pipeline_tag
            .clone()
            .or_else(|| detail.and_then(|d| d.pipeline_tag.clone()));
        out.push(fmt_kv(
            "pipeline",
            pipeline.as_deref().unwrap_or("UNKNOWN"),
            w,
        ));
        let license = row
            .license
            .clone()
            .or_else(|| detail.and_then(|d| d.license.clone()));
        out.push(fmt_kv(
            "license",
            license.as_deref().unwrap_or("UNKNOWN"),
            w,
        ));
        let params = row.params.or_else(|| detail.and_then(|d| d.params));
        out.push(fmt_kv(
            "params",
            &params
                .map(fmt_params)
                .unwrap_or_else(|| "UNKNOWN".to_string()),
            w,
        ));
        if row.tags.is_empty() {
            out.push(fmt_kv("tags", "UNKNOWN", w));
        } else {
            let mut tags = row.tags.clone();
            let extra = tags.len().saturating_sub(5);
            tags.truncate(5);
            let mut joined = tags.join(", ");
            if extra > 0 {
                joined.push_str(&format!(" +{extra} more"));
            }
            out.push(fmt_kv("tags", &joined, w));
        }
        let (ctx, gated, sha, files, formats, quant) = match detail {
            Some(d) => (
                d.context_length,
                Some(d.gated),
                d.sha
                    .as_deref()
                    .map(|s| s.chars().take(7).collect::<String>()),
                Some(d.siblings.len()),
                format_exts(&d.siblings),
                quant_from_tags(&row.tags),
            ),
            None => (
                None,
                None,
                None,
                None,
                String::new(),
                quant_from_tags(&row.tags),
            ),
        };
        // Size is its own probe (hf_sizes) — real bytes even when the full
        // detail fetch was refused; its own error is reported verbatim.
        let size_entry = self.hf_sizes.get(&row.id);
        let size = size_entry.and_then(|h| h.total_bytes);
        match ctx {
            Some(c) => out.push(fmt_kv("context", &c.to_string(), w)),
            None => out.push(fmt_kv(
                "context",
                if detail.is_some() {
                    "UNKNOWN (not reported by the hub)"
                } else {
                    "UNKNOWN (detail not loaded)"
                },
                w,
            )),
        }
        if let Some(d) = row.downloads {
            out.push(fmt_kv("downloads", &fmt_count(d), w));
        }
        if let Some(l) = row.likes {
            out.push(fmt_kv("likes", &fmt_count(l), w));
        }
        match gated {
            Some(true) => out.push(fmt_kv("gated", "yes", w)),
            Some(false) => out.push(fmt_kv("gated", "no", w)),
            None => out.push(fmt_kv("gated", "UNKNOWN (detail not loaded)", w)),
        }
        if let Some(s) = sha {
            out.push(fmt_kv("sha", &s, w));
        }
        match files {
            Some(n) => out.push(fmt_kv("files", &n.to_string(), w)),
            None => out.push(fmt_kv("files", "UNKNOWN (detail not loaded)", w)),
        }
        if !formats.is_empty() {
            out.push(fmt_kv("format", &formats, w));
        } else {
            out.push(fmt_kv(
                "format",
                if detail.is_some() {
                    "UNKNOWN (no weight files listed)"
                } else {
                    "UNKNOWN (detail not loaded)"
                },
                w,
            ));
        }
        match quant {
            Some(q) => out.push(fmt_kv("quant", &q, w)),
            None => out.push(fmt_kv("quant", "UNKNOWN (not tagged)", w)),
        }
        // Vision / tool calling — only from real pipeline/tags metadata.
        match row_vision(row) {
            Some(true) => out.push(fmt_kv("vision", "yes (vision pipeline/tags)", w)),
            Some(false) => out.push(fmt_kv("vision", "no (no vision pipeline/tags)", w)),
            None => out.push(fmt_kv("vision", "UNKNOWN (no metadata)", w)),
        }
        match row_tool_calling(row) {
            Some(true) => out.push(fmt_kv("tools", "yes (tagged tool-calling)", w)),
            Some(false) => out.push(fmt_kv("tools", "no (not in tags)", w)),
            None => out.push(fmt_kv("tools", "UNKNOWN (no tags)", w)),
        }
        out.push(fmt_kv("runtime", "UNKNOWN (no installer metadata)", w));
        // Size line: real bytes when known; honest states otherwise.
        let size_line = match (
            size_entry.and_then(|h| h.error.clone()),
            size,
            self.hf_detail_err.clone(),
            self.hf_detail_join.is_some(),
        ) {
            (Some(e), _, _, _) | (None, None, Some(e), _) => {
                format!(
                    "size    fetch failed: {}",
                    truncate(&e, w.saturating_sub(9))
                )
            }
            (None, Some(b), _, _) => fmt_kv("size", &fmt_bytes(b), w),
            (None, None, None, true) => "size    probing…".to_string(),
            (None, None, None, false) => "size    UNKNOWN (metadata not fetched)".to_string(),
        };
        out.push(size_line);
        // Install status: never claim weight download.
        out.push(
            "install metadata only — Remote: UNAVAILABLE (no downloader) · i = import local".into(),
        );
        // Local GPU (real probe) + factual fit line only when BOTH known.
        out.push(self.hw_line(w));
        if let (Some(sz), Some(hw)) = (size, self.hw.as_ref()) {
            let vram = hw.total_mib.saturating_mul(1024 * 1024);
            if sz > vram {
                out.push(format!(
                    "fit     {} > local VRAM ({} MiB) — not runnable on this GPU",
                    fmt_bytes(sz),
                    hw.total_mib
                ));
            } else {
                out.push(format!(
                    "fit     {} ≤ local VRAM ({} MiB) — size check only, not a guarantee",
                    fmt_bytes(sz),
                    hw.total_mib
                ));
            }
        }
        Some(out)
    }

    /// One honest local-GPU line.
    pub fn hw_line(&self, w: usize) -> String {
        if let Some(h) = &self.hw {
            fmt_kv(
                "gpu",
                &format!("{} · {}/{} MiB used", h.name, h.used_mib, h.total_mib),
                w,
            )
        } else if let Some(e) = &self.hw_err {
            format!("gpu     probe failed: {}", truncate(e, w.saturating_sub(9)))
        } else if self.hw_join.is_some() {
            "gpu     probing…".into()
        } else {
            "gpu     n/a (not probed)".into()
        }
    }

    /// Capability/tool patterns a Security preset appends — built from the
    /// tools ACTUALLY registered in this runtime (never a fixed guess list).
    /// `full` = every tool id + capability; otherwise only read-shaped names
    /// (deterministic substring heuristic, shown honestly as a heuristic).
    pub fn preset_rules(&self, full: bool) -> Vec<String> {
        let read_shaped = |s: &str| {
            let s = s.to_ascii_lowercase();
            [
                "read", "list", "search", "show", "get", "inspect", "describe", "status", "log",
                "query",
            ]
            .iter()
            .any(|k| s.contains(k))
                && ![
                    "write", "delete", "exec", "run", "send", "create", "edit", "post", "put",
                    "kill", "halt",
                ]
                .iter()
                .any(|k| s.contains(k))
        };
        let mut out: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for spec in self.runtime.registry.specs() {
            for c in &spec.required_caps {
                let cap = c.as_str().to_string();
                if full || read_shaped(&cap) {
                    out.insert(cap);
                }
            }
            if full || read_shaped(&spec.name) {
                out.insert(spec.name.clone());
            }
        }
        out.into_iter().collect()
    }

    pub async fn refresh_tasks(&mut self) -> Result<(), AppError> {
        self.tasks = self
            .runtime
            .store
            .list_tasks(self.current_session)
            .await
            .map_err(AppError::Persistence)?;
        Ok(())
    }

    /// Spawn the single-flight background view refresh (latest wins: a
    /// newer request aborts an in-flight one so the UI always converges on
    /// what the operator asked for most recently).
    pub fn spawn_refresh(
        &mut self,
        loading: &str,
        task: impl std::future::Future<Output = Result<RefreshData, String>> + Send + 'static,
    ) {
        if let Some(old) = self.refresh_join.take() {
            old.abort();
        }
        self.status = loading.to_string();
        self.refresh_join = Some(tokio::spawn(task));
    }

    /// Apply a finished background refresh result to the view state.
    async fn apply_refresh(&mut self, data: RefreshData) {
        match data {
            RefreshData::Files(rows) => {
                self.files = rows;
                if self.sel >= self.files.len() {
                    self.sel = 0;
                }
                self.status = format!("{} files · {}", self.files.len(), self.files_root.display());
            }
            RefreshData::Search { hits, status } => {
                self.search_hits = hits;
                self.sel = 0;
                self.status = status;
            }
            RefreshData::Git(Ok(info)) => {
                self.git = Some(info);
                self.git_error = None;
                self.status = "git refreshed".into();
            }
            RefreshData::Git(Err(msg)) => {
                self.git = None;
                self.git_error = Some(msg.clone());
                self.status = msg;
            }
            RefreshData::Artifacts(Ok((value, note))) => {
                self.artifacts = value;
                self.artifacts_note = note;
            }
            RefreshData::Artifacts(Err(e)) => {
                self.artifacts = serde_json::Value::Null;
                self.artifacts_note = e;
            }
            RefreshData::Models {
                cards,
                probe,
                status,
                profiles,
            } => {
                let n = cards.len();
                self.model_cards = cards;
                for (id, profile) in profiles {
                    self.model_profiles.insert(id, profile);
                }
                if self.model_input.is_empty() {
                    self.model_input = self
                        .switcher
                        .as_ref()
                        .and_then(|s| s.selected())
                        .unwrap_or_else(|| {
                            self.runtime
                                .provider
                                .info()
                                .default_model
                                .clone()
                                .unwrap_or_default()
                        });
                }
                self.native_tools = probe.native_tools;
                self.native_tools_err = probe.native_tools_err;
                self.vision = probe.vision;
                self.vision_err = probe.vision_err;
                self.provider_status = probe.status;
                self.status = if status.is_empty() {
                    format!("{n} registry cards")
                } else {
                    status
                };
            }
            RefreshData::ModelSearch {
                query,
                results,
                status,
            } => {
                self.model_results = results;
                self.model_results_query = query;
                self.sel = 0;
                self.status = status;
            }
            RefreshData::HubSearch {
                query,
                rows,
                status,
            } => {
                let n = rows.len();
                self.hub_rows = rows;
                self.hub_query = query;
                self.hub_loaded = true;
                self.hub_status = if status.is_empty() {
                    format!("{n} hub row(s)")
                } else {
                    status
                };
                self.sel = 0;
                // Size + detail probes only make sense with rows in hand;
                // both land honestly (error text) if the mode blocks them.
                self.probe_hf_sizes().await;
                if self.model_section == ModelSection::Hub {
                    self.spawn_hub_detail().await;
                }
                self.status = self.hub_status.clone();
            }
            RefreshData::Preview { title, lines } => {
                self.preview = Some(PreviewData { title, lines });
                self.preview_scroll = 0;
                self.overlay = Some(Overlay::Preview);
            }
            RefreshData::SetupSearch {
                rows,
                fetched,
                local_hits,
                network_allowed,
            } => {
                if let Some(st) = self.setup.as_mut() {
                    for row in rows {
                        if !st.models.iter().any(|m| m.display == row.display) {
                            st.models.push(row);
                        }
                    }
                    let hf_count = st
                        .models
                        .iter()
                        .filter(|m| m.origin == "hugging face")
                        .count();
                    st.note = if network_allowed {
                        format!(
                            "{hf_count} HF row(s) from this query (+{local_hits} local) · \
                             {fetched} sized from real metadata"
                        )
                    } else {
                        "network refused by current mode — local results only (never faked)".into()
                    };
                    st.hf_typing = false;
                }
            }
        }
    }

    pub async fn refresh_view(&mut self) -> Result<(), AppError> {
        if let Ok(m) = self.runtime.modes.current().await {
            self.exec_mode = m.mode.as_str().to_string();
            self.net_allowed = m.mode.network_allowed();
        }
        match self.view {
            View::Chat => {
                self.refresh_sessions().await?;
                self.refresh_tasks().await?;
                self.load_git().await;
            }
            View::Files => self.load_files().await,
            View::Search => self.run_search().await?,
            View::Tasks => self.refresh_tasks().await?,
            View::Sessions => self.refresh_sessions().await?,
            View::Git => self.load_git().await,
            View::Terminal => {}
            View::Tools => self.tools = self.runtime.registry.specs(),
            View::Artifacts => self.load_artifacts().await,
            View::Models => {
                self.refresh_models().await;
                // Auto-init the drop-in category dirs (create only, never
                // delete) — a fresh machine never sees "No such file or
                // directory" for `models/`.
                if let Err(e) =
                    crate::agent::views::ensure_model_dirs(&self.files_root.join("models"))
                {
                    self.status = format!("model dirs: {e}");
                }
            }
            View::Mcp => self.load_mcp().await,
            View::Extensions => self.load_extensions().await,
            View::Security => self.load_security().await?,
            View::Settings => {}
            View::Agents => {
                self.refresh_tasks().await?;
            }
        }
        if self.refresh_join.is_none() {
            self.status = format!("{} refreshed", self.view.label());
        }
        Ok(())
    }

    pub async fn fetch_mode(&self) -> String {
        self.runtime
            .modes
            .current()
            .await
            .map(|m| m.mode.as_str().to_string())
            .unwrap_or_else(|_| "unknown".into())
    }

    /// Estimated context use for the active session (chars/4 vs the provider's
    /// advertised window). Labelled as an estimate — never a fake exact count.
    pub fn ctx_estimate(&self) -> Option<(usize, usize)> {
        let window = self.runtime.provider.capabilities().context_window?;
        let mut chars = self.input.chars().count();
        if let Some(sid) = self.current_session {
            if let Some(s) = self.sessions.iter().find(|x| x.id == sid) {
                chars += s
                    .content
                    .iter()
                    .map(|m| m.content.chars().count())
                    .sum::<usize>();
            }
        }
        let est = chars / 4;
        Some((est, window))
    }

    /// Exit path: release every pending approval (deny-by-drop is already
    /// fail-closed, but answering keeps the gate's audit trail explicit).
    pub fn shutdown(&mut self) {
        for p in self.approvals.drain(..) {
            p.answer(openrt_permissions::PermissionDecision::Deny);
        }
    }
}

// ----------------------------------------------------------------------
// helpers
// ----------------------------------------------------------------------

/// PART 15/20: sanitize a provider error for display on the MODEL ERROR
/// card. Collapses newlines, truncates over-long bodies (an `ModelError::Http`
/// dump can be a full HTML page), and runs the same redaction boundary used
/// for tool output so secrets never render in the TUI.
/// Human-readable bytes. Resource numbers in raw bytes (`6399004128`) are
/// unreadable, and an unreadable number is a number the operator ignores.
pub(crate) fn humanize_bytes(b: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = MIB * 1024;
    if b >= GIB {
        format!("{:.1} GiB", b as f64 / GIB as f64)
    } else if b >= MIB {
        format!("{:.0} MiB", b as f64 / MIB as f64)
    } else if b >= 1024 {
        format!("{:.0} KiB", b as f64 / 1024.0)
    } else {
        format!("{b} B")
    }
}

/// State the admission rule in force, so the card never hides *why* it applied.
pub(crate) fn describe_rule(limits: &openrt_resources::ResourceLimits) -> String {
    if let Some(floor) = limits.min_free_memory_bytes {
        return format!("{floor} absolute bytes must stay available");
    }
    if limits.min_headroom_ratio > 0.0 {
        return format!(
            "{:.1}% of total memory must stay available (tight below {:.1}%)",
            limits.min_headroom_ratio * 100.0,
            limits.tight_headroom_ratio * 100.0
        );
    }
    format!(
        "legacy absolute rule: used memory below {:.1}%",
        limits.max_mem_used_ratio * 100.0
    )
}

/// Redact a provider error down to one safe line...
fn sanitize_provider_error(raw: &str) -> String {
    let flat = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    // Hard cap: the card is a compact diagnostic, not a response viewer.
    let capped: String = if flat.chars().count() > 600 {
        format!("{}…", flat.chars().take(600).collect::<String>())
    } else {
        flat
    };
    openrt_core::RedactionPolicy::default().redact_text(&capped)
}

fn now_hm() -> String {
    let secs = unix_secs();
    let (h, m, _) = hms(secs);
    format!("{h:02}:{m:02}")
}

fn now_hms() -> String {
    let secs = unix_secs();
    let (h, m, s) = hms(secs);
    format!("{h:02}:{m:02}:{s:02}")
}

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn hms(secs: u64) -> (u64, u64, u64) {
    (secs / 3600 % 24, secs / 60 % 60, secs % 60)
}

// ----------------------------------------------------------------------
// Theme access
// ----------------------------------------------------------------------

/// The semantic tokens for the theme currently being rendered.
///
/// Resolution order: a live preview (Settings) wins over the committed theme,
/// so a theme can be evaluated before it is committed. This is the only
/// place Agent Mode asks for colors — a renderer never decides a palette.
impl AgentApp {
    /// Tokens for the theme in effect right now.
    pub fn tokens(&self) -> crate::design::tokens::Tokens {
        crate::design::tokens::Tokens::from_theme(self.tokens_theme_kind())
    }

    /// Whether to *ask* the layout for a contextual side panel.
    ///
    /// This is a request, not a decision: `ShellLayout::build` grants the panel
    /// only when the chat column still clears its minimum readable width. The
    /// renderer therefore has no width threshold of its own, which is what keeps
    /// the side panel appearing at the same terminal size in every mode.
    pub fn wants_context_panel(&self) -> bool {
        self.view == View::Chat && self.overlay.is_none() && self.approvals.is_empty()
    }

    // ------------------------------------------------------------------
    // Focus / pointer lifecycle
    //
    // Rendering fills these in; input reads them. The four calls below are the
    // entire contract, so there is exactly one frame ordering to get right:
    //
    //     begin_frame_ui()   ->  render (pushes stops)  ->  end_frame_ui()
    //
    // ------------------------------------------------------------------

    /// Start a frame: clear the ring so this frame describes only what is on
    /// screen now.
    pub fn begin_frame_ui(&self) {
        self.focus.borrow_mut().begin_frame();
        self.hits.borrow_mut().clear();
    }

    /// Resolve Ctrl+C.
    ///
    /// One policy for the whole surface, layered from the most specific
    /// context outward. Cancellation and quitting are different things and are
    /// routed differently: the first Ctrl+C stops whatever is *stoppable* in the
    /// current context, and only a context with nothing to stop quits.
    ///
    /// The order is the security-relevant part:
    ///
    /// 1. **A pending approval is denied.** Ctrl+C can never grant, and it
    ///    cannot leave the gate blocked either — the waiting task is released
    ///    with a refusal so the process does not hang on exit.
    /// 2. **A destructive confirmation is cancelled.** The safe answer.
    /// 3. **A modal is cancelled**, as `Esc` would.
    /// 4. **A running task is cancelled.** The operator asked for it to stop.
    /// 5. **A resource wait is stopped**, so a retry loop cannot outlive the
    ///    operator's intent.
    /// 6. **Unsaved composer text refuses to quit** and says how to discard it.
    /// 7. **Otherwise the app quits.**
    ///
    /// Returns whether the app should quit.
    /// The escape hatch the composer guard advertises: quit even with unsaved
    /// text in the composer.
    ///
    /// This is deliberately *not* `handle_interrupt` with the last step removed.
    /// Every layer that protects something outside the operator's own screen is
    /// still honoured — a pending permission is denied rather than abandoned
    /// with the gate blocked, a running task is cancelled, a resource wait is
    /// stopped. Only unsent composer text is treated as disposable, because
    /// unlike the others it cannot leave a task hung or a gate open; it is the
    /// operator's own typing, and an explicit "quit anyway" key is exactly the
    /// consent to lose it.
    ///
    /// The previous code printed "Ctrl+Q quits anyway" from step 6 of
    /// [`Self::handle_interrupt`] and implemented no Ctrl+Q anywhere. Found on a
    /// real terminal: type something, press Ctrl+C, press Ctrl+Q, nothing
    /// happens, and the only way out is to discard the text.
    pub async fn handle_quit_now(&mut self) -> bool {
        if !self.approvals.is_empty() {
            use openrt_permissions::PermissionDecision as Pd;
            let p = self.approvals.remove(0);
            let tool = p.request.tool.clone();
            p.answer(Pd::Deny);
            self.status = format!("denied {tool} — quitting");
        }
        if self.confirm.is_some() {
            self.confirm_cancel();
        }
        self.overlay = None;
        if self.is_running() {
            self.cancel_running().await;
        }
        if self.resource_card.is_some() {
            self.clear_resource_card();
        }
        true
    }

    pub async fn handle_interrupt(&mut self) -> bool {
        // 1. A permission prompt is a security boundary: Ctrl+C resolves it the
        // only way it may resolve it — by denial.
        if !self.approvals.is_empty() {
            use openrt_permissions::PermissionDecision as Pd;
            let p = self.approvals.remove(0);
            let tool = p.request.tool.clone();
            p.answer(Pd::Deny);
            self.status = format!("denied {tool} — interrupted");
            return false;
        }
        // 2. A destructive confirmation defaults to the safe answer.
        if self.confirm.is_some() {
            self.confirm_cancel();
            return false;
        }
        // 3. A modal is dismissed exactly as Esc would dismiss it.
        if self.overlay.is_some() {
            self.overlay = None;
            self.status = "closed — interrupted".into();
            return false;
        }
        // 4. An in-flight task is cancelled before the app is considered quit.
        if self.is_running() {
            self.cancel_running().await;
            return false;
        }
        // 5. A bounded resource retry must not keep polling after an interrupt.
        if self.resource_card.is_some() {
            self.clear_resource_card();
            self.status = "stopped waiting for resources".into();
            return false;
        }
        // 6. Composer text is unsaved work. Quitting here would destroy it with
        //    no warning and no undo, so refuse and say what to press instead.
        if !self.input.trim().is_empty() {
            self.status = "interrupted — Esc discards the composer, Ctrl+Q quits anyway".into();
            return false;
        }
        // 7. Nothing left to stop.
        true
    }

    /// Whether a task is currently running.
    pub fn is_running(&self) -> bool {
        self.task_join.is_some()
    }

    /// Cancel the running task, if any.
    ///
    /// Goes through the runtime's live-handle registry rather than dropping a
    /// local handle: a join handle dropped without cancelling leaves the task
    /// running against a session nobody is watching.
    pub async fn cancel_running(&mut self) {
        if self.current_task.is_some() {
            self.cancel().await;
            self.task_join = None;
            self.current_task = None;
        } else {
            self.status = "nothing to cancel".into();
        }
    }

    /// The interaction scope for the modal currently on top.
    ///
    /// This is the single place that answers "what is focusable and what do
    /// keys mean right now". It resolves, in priority order:
    ///
    /// 1. a pending permission approval (a security boundary),
    /// 2. a destructive confirmation,
    /// 3. a content overlay,
    /// 4. nothing — the view itself owns the keyboard.
    ///
    /// Returning `None` for the background is what makes the trap structural:
    /// there is no separate "escape the dialog" path to get wrong, because a
    /// background key handler is only reachable when no scope is open.
    pub fn modal_scope(&self) -> Option<OverlayScope> {
        if !self.approvals.is_empty() {
            let stops: Vec<FocusStop> = crate::design::modals::GRANT_BUTTONS
                .iter()
                .enumerate()
                .map(|(i, id)| FocusStop::in_group(permission_target(*id), i, GRANT_BUTTONS.len()))
                .collect();
            return Some(
                OverlayScope::new("permission", OverlayKeys::security_prompt())
                    .security()
                    .with_stops(stops),
            );
        }
        if self.confirm.is_some() {
            let stops = vec![
                FocusStop::new(confirm_target("confirm.yes")),
                FocusStop::new(confirm_target("confirm.no")),
            ];
            return Some(
                OverlayScope::new("confirm", OverlayKeys::confirmation()).with_stops(stops),
            );
        }
        match self.overlay {
            None => None,
            // Read-only: nothing to focus, so the ring is empty and Tab has
            // nowhere to go. Arrows scroll, Esc closes.
            Some(Overlay::Help) => Some(OverlayScope::new("help", OverlayKeys::scrollable())),
            Some(Overlay::Preview) => Some(OverlayScope::new("preview", OverlayKeys::scrollable())),
            // A text field with one implicit stop: the field itself.
            Some(Overlay::Model) => Some(
                OverlayScope::new("model", OverlayKeys::text_entry())
                    .with_stops(vec![FocusStop::new(FocusTarget::ModelItem(0))]),
            ),
            Some(Overlay::Palette) => {
                let matches = self.palette_matches();
                let len = matches.len();
                let stops = (0..len)
                    .map(|i| FocusStop::in_group(FocusTarget::PaletteItem(i), i, len))
                    .collect();
                Some(OverlayScope::new("palette", OverlayKeys::picker()).with_stops(stops))
            }
            Some(Overlay::Sessions) => {
                let len = self.sessions.len();
                let stops = (0..len)
                    .map(|i| FocusStop::in_group(FocusTarget::Row(i), i, len))
                    .collect();
                Some(OverlayScope::new("sessions", OverlayKeys::picker()).with_stops(stops))
            }
            Some(Overlay::Setup) => Some(OverlayScope::new("setup", OverlayKeys::wizard())),
            // Switching operating mode always confirms first, so Enter here
            // *proposes*; it never switches.
            Some(Overlay::Mode) => Some(OverlayScope::new("mode", OverlayKeys::picker())),
        }
    }

    /// Whether a modal owns the keyboard.
    ///
    /// Every background action is gated on this. A single predicate means a new
    /// modal cannot be added without inheriting the protection.
    pub fn modal_open(&self) -> bool {
        self.modal_scope().is_some()
    }

    /// Drop every registered region and any armed press.
    ///
    /// Used on mode transitions. `begin_frame_ui` clears the registry too, but
    /// only once a frame is drawn — this makes the switch immediate, so a click
    /// in the window between the keypress and the next frame cannot resolve
    /// against the previous surface.
    pub fn clear_hits(&mut self) {
        self.hits.borrow_mut().clear();
        *self.pressed.borrow_mut() = None;
        self.focus.borrow_mut().set_pressed(None);
    }

    /// Finish a frame: re-home focus onto whatever is actually drawn.
    ///
    /// `reconcile` is what keeps focus valid across a view switch, an overlay
    /// opening, a resize, or a row being filtered away — the previous target is
    /// reused when it survived, otherwise the first interactive stop takes over.
    pub fn end_frame_ui(&self) {
        self.focus.borrow_mut().reconcile();
    }

    /// What is currently reachable by `Tab`, in tab order.
    pub fn focus_stops(&self) -> Vec<crate::design::FocusStop> {
        self.focus.borrow().stops().to_vec()
    }

    /// Move keyboard focus. Returns whether it moved.
    pub fn move_focus(&self, mv: crate::design::Move) -> bool {
        self.focus.borrow_mut().apply(mv)
    }

    /// Focus a specific target, if it is on screen.
    pub fn focus_target(&self, target: &crate::design::FocusTarget) -> bool {
        self.focus.borrow_mut().focus(target)
    }

    /// The target a pointer at `(x, y)` would activate.
    pub fn hit_test(&self, x: u16, y: u16) -> Option<crate::design::FocusTarget> {
        self.hits.borrow().at(x, y).map(|h| match h {
            crate::design::HitTarget::None => crate::design::FocusTarget::ModalScrim,
            other => crate::design::FocusTarget::from(&other),
        })
    }

    /// A pointer moved. Updates hover so the element under it renders as hovered,
    /// and returns the target so the caller can redraw.
    pub fn hover_at(&self, x: u16, y: u16) -> Option<crate::design::FocusTarget> {
        let t = self.hit_test(x, y);
        self.focus.borrow_mut().set_hovered(t.clone());
        t
    }

    /// A mouse button went down. Focuses the target and marks it pressed.
    pub fn press_at(&self, x: u16, y: u16) -> Option<crate::design::FocusTarget> {
        let t = self.hit_test(x, y);
        if let Some(t) = &t {
            self.focus.borrow_mut().set_pressed(Some(t.clone()));
            // A press moves focus only if it landed on something focusable;
            // clicking a non-focusable region (a log body, a message) must not
            // rip focus out of the dialog the operator is answering.
            self.focus.borrow_mut().focus(t);
        }
        *self.pressed.borrow_mut() = t.clone();
        t
    }

    /// A mouse button came up. Clears the pressed state and returns the target
    /// that was pressed, so the caller can activate it.
    ///
    /// Activation happens on *release* rather than press, matching every desktop
    /// toolkit: pressing a button then dragging off it must not fire it.
    pub fn release_at(&self, x: u16, y: u16) -> Option<crate::design::FocusTarget> {
        let armed = self.pressed.borrow_mut().take();
        self.focus.borrow_mut().set_pressed(None);
        let armed = armed?;
        // A release only activates if it lands on the target that armed.
        //
        // Without this check a press on "Deny" followed by a release one row
        // away still answers with whatever was armed — the operator's cursor
        // and the acted-upon control are then different things, and a
        // permission grant is the worst possible place for that mismatch.
        // Comparing resolved targets rather than raw coordinates is what lets a
        // sloppy press inside the same button still count.
        if self.hit_test(x, y).as_ref() == Some(&armed) {
            Some(armed)
        } else {
            None
        }
    }

    /// The destination that currently owns the keyboard, if any.
    pub fn focus_target_visible(&self) -> Option<crate::design::FocusTarget> {
        self.focus.borrow().focused_target()
    }

    /// The target currently held down, if any. Read by the drag-cancel check.
    pub fn pressed_target(&self) -> Option<crate::design::FocusTarget> {
        self.pressed.borrow().clone()
    }

    /// Record a pointer position reported by the terminal.
    pub fn set_pointer(&mut self, x: u16, y: u16) {
        self.pointer = (x, y);
    }

    /// Focus the composer.
    ///
    /// Clicking the composer takes the keyboard, which matters because the input
    /// line is always live: a click elsewhere must not leave the operator typing
    /// into a field they can no longer see.
    pub fn focus_composer(&self) {
        self.focus
            .borrow_mut()
            .focus(&crate::design::FocusTarget::Composer);
    }

    /// Focus a Workbench operations tab.
    pub fn select_workbench_tab(&self, index: usize) {
        self.focus
            .borrow_mut()
            .focus(&crate::design::FocusTarget::Tab(index));
    }

    /// Abort a press without activating it (the pointer dragged off the control).
    pub fn cancel_press(&self) {
        self.focus.borrow_mut().set_pressed(None);
        *self.pressed.borrow_mut() = None;
    }

    /// Render one frame so hit regions and focus stops are populated.
    ///
    /// Tests that exercise pointer input need real rectangles: a hit test
    /// against an empty registry trivially "passes" by returning `None`, which
    /// is how a broken mouse path can look correct in a test.
    #[cfg(test)]
    pub fn begin_frame_for_test(&self) {
        let size = ratatui::backend::TestBackend::new(120, 44);
        let mut term = ratatui::Terminal::new(size).expect("test terminal");
        term.draw(|f| {
            super::render::draw(f, self);
        })
        .expect("test draw");
    }

    /// The on-screen rectangle registered for a hit target, if it is registered.
    ///
    /// Input handling and rendering must agree on geometry, so tests locate
    /// their click target by asking the registry the renderer wrote, rather than
    /// recomputing coordinates and hoping they match.
    #[cfg(test)]
    pub fn rect_of_hit(&self, target: crate::design::layout::HitTarget) -> (u16, u16) {
        let hits = self.hits.borrow();
        for (rect, t) in hits.iter_regions() {
            if *t == target {
                return (rect.x + rect.width / 2, rect.y);
            }
        }
        panic!("hit target {target:?} was never registered by the renderer");
    }

    /// The rectangle registered for a hit target, or `None` if it is absent.
    ///
    /// Lets a test distinguish "registered at the wrong place" from "never
    /// registered", which a panic cannot tell apart.
    #[cfg(test)]
    pub fn rect_of_hit_or_none(
        &self,
        target: crate::design::layout::HitTarget,
    ) -> Option<(u16, u16)> {
        let hits = self.hits.borrow();
        for (rect, t) in hits.iter_regions() {
            if *t == target {
                return Some((rect.x + rect.width / 2, rect.y));
            }
        }
        None
    }

    /// Select a row in the active view.
    ///
    /// Selection only — a click never activates. Activating stays a separate,
    /// explicit act (`Enter`), because a stray click while scrolling a list must
    /// not be able to run a command or open a file for editing.
    ///
    /// The index is the *absolute* row index, which is what the shared collection
    /// renderers register, so a click on a row inside a scrolled window selects
    /// the same row the `Up`/`Down` keys would reach.
    pub fn select_row(&mut self, index: usize) {
        let i = index.min(self.selectable_rows().saturating_sub(1));
        // A modal owns selection while it is open. Falling through to the view
        // behind a dialog is how a click ends up acting on something invisible.
        if self.overlay.is_some() || self.confirm.is_some() || !self.approvals.is_empty() {
            return;
        }
        if let Some(c) = self.resource_card.as_mut() {
            c.selected = i.min(c.actions.len().saturating_sub(1));
            return;
        }
        match self.view {
            View::Settings => {
                self.theme_sel = i.min(crate::theme::ThemeKind::ALL.len().saturating_sub(1))
            }
            View::Models => {
                if self.hub_filter_active {
                    self.hub_filter_sel = i.min(HubFilterDim::ALL.len().saturating_sub(1));
                } else {
                    // Palette rows are live search matches, so the bound is the
                    // match count — not a fixed enum size.
                    let n = self.palette_matches().len();
                    self.palette_sel = i.min(n.saturating_sub(1));
                }
            }
            _ => self.sel = i,
        }
    }

    /// How many rows the active view can select.
    ///
    /// Used to clamp a click so an index from a stale frame cannot select past
    /// the end of the list.
    pub fn selectable_rows(&self) -> usize {
        if self.resource_card.is_some() {
            return self.resource_card.as_ref().map_or(0, |c| c.actions.len());
        }
        match self.view {
            View::Chat | View::Terminal => 0,
            View::Files => self.files.len(),
            View::Tasks => self.tasks.len(),
            View::Sessions => self.sessions.len(),
            View::Settings => crate::theme::ThemeKind::ALL.len(),
            View::Models => {
                if self.hub_filter_active {
                    HubFilterDim::ALL.len()
                } else {
                    self.palette_matches().len()
                }
            }
            // These views paint a selection cursor on their rows, so they must
            // report a row count. Reporting `0` here while drawing a cursor is
            // what pins the selection at row 0 and makes the cursor a lie: the
            // arrows and the mouse both clamp to `selectable_rows() - 1`.
            // The counts come from the same filtered helpers the renderer
            // iterates, so the navigable range and the painted rows agree.
            View::Tools => self.filtered_tools().len(),
            View::Mcp => self.filtered_mcp().len(),
            View::Extensions => self.filtered_ext().len(),
            View::Security => self.filtered_security().len(),
            View::Search => self.search_hits.len(),
            // These three render a scrollable text block with no row cursor,
            // so there is genuinely nothing for a click to land on.
            View::Artifacts | View::Git | View::Agents => 0,
        }
    }

    /// The interaction state to render `target` in. Components call this instead
    /// of tracking hover/press themselves.
    pub fn interaction_of(
        &self,
        target: &crate::design::FocusTarget,
    ) -> crate::design::Interaction {
        self.focus.borrow().state_of(target)
    }

    /// The theme a render would use right now.
    pub fn active_theme(&self) -> crate::theme::Theme {
        crate::theme::Theme::new(self.tokens_theme_kind())
    }

    /// The committed theme (never the preview).
    pub fn committed_theme(&self) -> crate::theme::Theme {
        self.workbench.theme
    }

    /// Which theme the renderer resolves: preview first, then committed.
    fn tokens_theme_kind(&self) -> crate::theme::ThemeKind {
        self.theme_preview.unwrap_or(self.workbench.theme.kind)
    }

    /// Commit a theme: switch immediately and persist.
    ///
    /// Persistence is attempted before the in-memory switch is reported as
    /// done, and a failure is returned rather than swallowed — the operator
    /// gets the theme they asked for in this session plus a visible warning
    /// that the preference did not survive a restart.
    pub fn set_theme(&mut self, kind: crate::theme::ThemeKind) -> Result<(), String> {
        self.theme_preview = None;
        self.workbench.theme.set(kind);
        self.theme_sel = crate::theme::ThemeKind::ALL
            .iter()
            .position(|k| *k == kind)
            .unwrap_or(0);
        crate::theme::save_theme_to(self.workbench.theme_path.as_deref(), self.workbench.theme)
    }

    /// Move the theme cursor by `delta` and preview the result live.
    ///
    /// Returns `false` at the ends of the list: a theme list cannot scroll
    /// past its end into a wrapped-around surprise.
    pub fn preview_theme(&mut self, delta: isize) -> bool {
        let all = crate::theme::ThemeKind::ALL;
        let cur = self.theme_sel as isize;
        let next = cur + delta;
        if next < 0 || next >= all.len() as isize {
            return false;
        }
        self.theme_sel = next as usize;
        self.theme_preview = Some(all[next as usize]);
        true
    }

    /// Preview an exact theme (used by direct row selection and tests).
    pub fn preview_theme_kind(&mut self, kind: crate::theme::ThemeKind) {
        if let Some(idx) = crate::theme::ThemeKind::ALL.iter().position(|k| *k == kind) {
            self.theme_sel = idx;
        }
        self.theme_preview = Some(kind);
    }

    /// Point the theme cursor without previewing.
    pub fn select_theme(&mut self, idx: usize) {
        self.theme_sel = idx.min(crate::theme::ThemeKind::ALL.len().saturating_sub(1));
    }

    /// The theme the cursor currently points at.
    pub fn selected_theme(&self) -> crate::theme::ThemeKind {
        crate::theme::ThemeKind::ALL[self.theme_sel.min(crate::theme::ThemeKind::ALL.len() - 1)]
    }

    /// Leave preview mode. Returns the theme that was actually in effect, so
    /// the caller can report which theme was committed.
    pub fn commit_theme_preview(&mut self) -> Result<crate::theme::ThemeKind, String> {
        let kind = self.tokens_theme_kind();
        self.set_theme(kind).map(|()| kind)
    }

    /// Discard preview mode and return to the committed theme.
    pub fn cancel_theme_preview(&mut self) {
        let committed = self.workbench.theme.kind;
        self.theme_preview = None;
        self.theme_sel = crate::theme::ThemeKind::ALL
            .iter()
            .position(|k| *k == committed)
            .unwrap_or(0);
    }

    pub fn is_previewing_theme(&self) -> bool {
        self.theme_preview.is_some()
    }

    /// Rows for the Settings theme list: name, and whether it is live.
    pub fn theme_rows(&self) -> Vec<crate::theme::ThemeKind> {
        crate::theme::ThemeKind::ALL.to_vec()
    }

    /// Move the Settings cursor between its sub-sections.
    pub fn cycle_settings_focus(&mut self, forward: bool) {
        self.settings_focus = if forward {
            match self.settings_focus {
                SettingsFocus::Info => SettingsFocus::Theme,
                SettingsFocus::Theme => SettingsFocus::Info,
            }
        } else {
            match self.settings_focus {
                SettingsFocus::Info => SettingsFocus::Theme,
                SettingsFocus::Theme => SettingsFocus::Info,
            }
        };
    }
}
