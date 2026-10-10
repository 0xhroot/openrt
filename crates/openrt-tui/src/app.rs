//! TUI controller: real state over one wired `Runtime` (no second engine).

use std::sync::Arc;

use crate::design::widgets::BadgeTone;
use crate::design::{HitTarget, PanelId};

use openrt_core::{Session, SessionId, Task, TaskStatus};
use openrt_modes::ExecutionMode;
use openrt_runtime::Runtime;
use openrt_workspace::{
    create_workspace, list_workspaces, open_workspace, reopen_current, Workspace,
};

/// Whether a panel's data has arrived, and why it has not.
///
/// A dashboard panel that has never loaded and a panel that loaded and found
/// nothing are different situations and must not look the same: the first means
/// "ask again", the second means "there is genuinely nothing". Collapsing both
/// into "empty" is how an operator ends up believing a store is empty because a
/// read failed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PanelLoad {
    /// A refresh has completed at least once.
    pub loaded: bool,
    /// The failure from the most recent refresh, if it failed.
    pub failed: Option<String>,
}

impl PanelLoad {
    pub fn ok(&mut self) {
        self.loaded = true;
        self.failed = None;
    }

    pub fn err(&mut self, e: &AppError) {
        self.loaded = true;
        self.failed = Some(e.to_string());
    }
}

/// Which panel a refresh outcome belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanelSlot {
    Workspaces,
    Sessions,
    Tasks,
    Ops,
}

/// Which panel currently has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Workspaces,
    Sessions,
    Tasks,
    Ops,
}

/// Which Phase 10/11 sub-view the Ops panel shows (keys 1–6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpsTab {
    /// M10.5 time-machine: the verified history ledger timeline.
    Timeline,
    /// M10.6 skills: installed skill id/activation pairs.
    Skills,
    /// M10.9 AgentShield: recorded security findings.
    Shield,
    /// M10.2 boot recovery: recovery records (interrupted/incomplete/stale).
    Recovery,
    /// M11.6 rooms: workbench agent rooms (status, workflow, links).
    Rooms,
    /// M11.6 extensions: installed extension versions + security status.
    Extensions,
    /// M11.6 computer: backend kinds + honest capability matrix (read-only).
    Computer,
    /// Phase 12 (M12): industrial + trading + learning at-a-glance rows
    /// (real store reads — counts and honest backend statuses, never invented).
    Intel,
}

impl OpsTab {
    pub fn title(&self) -> &'static str {
        match self {
            OpsTab::Timeline => "Ops · Timeline",
            OpsTab::Skills => "Ops · Skills",
            OpsTab::Shield => "Ops · Shield",
            OpsTab::Recovery => "Ops · Recovery",
            OpsTab::Rooms => "Ops · Rooms",
            OpsTab::Extensions => "Ops · Extensions",
            OpsTab::Computer => "Ops · Computer",
            OpsTab::Intel => "Ops · Phase 12",
        }
    }

    pub fn index(&self) -> usize {
        match self {
            OpsTab::Timeline => 0,
            OpsTab::Skills => 1,
            OpsTab::Shield => 2,
            OpsTab::Recovery => 3,
            OpsTab::Rooms => 4,
            OpsTab::Extensions => 5,
            OpsTab::Computer => 6,
            OpsTab::Intel => 7,
        }
    }

    pub fn from_index(i: usize) -> Self {
        match i % 8 {
            0 => OpsTab::Timeline,
            1 => OpsTab::Skills,
            2 => OpsTab::Shield,
            3 => OpsTab::Recovery,
            4 => OpsTab::Rooms,
            5 => OpsTab::Extensions,
            6 => OpsTab::Computer,
            _ => OpsTab::Intel,
        }
    }
}

/// One workspace row rendered in the left pane (derived from a real store read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRow {
    pub address_code: String,
    pub title: String,
    pub description: String,
    pub active: bool,
}

/// One session row (mirrors a real `Session` read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub id: SessionId,
    pub title: String,
    pub status: String,
    pub messages: usize,
    pub active: bool,
}

/// One task row for the current session (mirrors a real `Task` read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    pub id: String,
    pub prompt: String,
    pub status: String,
    pub turns: usize,
}

/// One time-machine timeline row (mirrors a real `HistoryRecord` read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineRow {
    pub seq: u64,
    pub status: String,
    pub event_kind: String,
    pub note: String,
}

/// One installed-skill row (mirrors the real `assets_skill_states` read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRow {
    pub id: String,
    pub state: String,
}

/// One AgentShield finding row (mirrors a real recorded `SecurityFinding`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingRow {
    pub id: String,
    pub severity: String,
    pub category: String,
    pub reason: String,
    pub status: String,
}

/// One boot-recovery record row (mirrors a real `RecoveryRecord` read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryRow {
    pub record_id: String,
    pub interrupted: usize,
    pub incomplete: usize,
    pub stale_sandboxes: usize,
    pub clean: bool,
}

/// One workbench room row (M11.6 — mirrors a real `workbench.rooms` read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomRow {
    pub id: String,
    pub title: String,
    pub workbench: String,
    pub status: String,
    pub agents: usize,
    pub materials: usize,
    pub steer: usize,
    pub active_workflow: Option<String>,
}

/// One installed-extension row (M11.6 — mirrors `extension_states`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionRow {
    pub id: String,
    pub version: String,
    pub activation: String,
    pub security: String,
}

/// One ComputerUse capability-matrix row (M11.6 — mirrors the honest
/// `capability_matrix` the CLI prints; UNAVAILABLE stays UNAVAILABLE).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputerRow {
    pub capability: String,
    pub status: String,
    pub detail: String,
}

/// One Phase 12 at-a-glance row (label → value; every value is a real
/// store read or an honest backend status).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntelRow {
    pub label: String,
    pub value: String,
}

/// Errors surfaced to the operator (fail-closed; never auto-repaired).
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("runtime: {0}")]
    Runtime(#[from] openrt_runtime::Error),
    #[error("workspace: {0}")]
    Workspace(#[from] openrt_workspace::WorkspaceError),
    #[error("mode: {0}")]
    Mode(#[from] openrt_modes::ModeError),
    #[error("persistence: {0}")]
    Persistence(#[from] openrt_persistence::PersistenceError),
    #[error("shield: {0}")]
    Shield(#[from] openrt_shield::store::ShieldError),
    #[error("{0}")]
    Invalid(String),
}

/// The TUI application state. Holds the one runtime; every mutation goes
/// through it (or the foundation store it owns).
pub struct App {
    pub runtime: Arc<Runtime>,
    /// THE theme. One handle for the whole process — Agent Mode, Workbench and
    /// every overlay resolve their colors from here, so a switch is visible
    /// everywhere at once and there is never a second palette to drift.
    pub theme: crate::theme::Theme,
    /// Where [`Self::theme`] is persisted, resolved once at startup.
    ///
    /// Holding the path instead of re-deriving it on every save means a commit
    /// cannot be redirected by a later environment change, and a test can
    /// redirect it without touching the developer's real preference file.
    pub theme_path: Option<std::path::PathBuf>,
    pub focus: Focus,
    pub workspace: Option<Workspace>,
    pub workspaces: Vec<WorkspaceRow>,
    pub sessions: Vec<SessionRow>,
    pub current_session: Option<SessionId>,
    pub tasks: Vec<TaskRow>,
    pub mode: ExecutionMode,
    pub status: String,
    pub should_quit: bool,
    /// Selected row within each panel.
    ///
    /// `Option`, not a sentinel index: a panel with no rows must have *no*
    /// selection, so arrow keys, `Enter`, and the drawn highlight all agree that
    /// there is nothing selected. A `usize::MAX` sentinel is silently clamped
    /// by every bounds check in turn, which is how a read-only panel ends up
    /// showing row 0 highlighted with nothing there.
    pub sel_workspace: Option<usize>,
    pub sel_session: Option<usize>,
    pub sel_task: Option<usize>,
    /// Per-panel data availability, for honest empty/loading/failed states.
    pub load_workspaces: PanelLoad,
    pub load_sessions: PanelLoad,
    pub load_tasks: PanelLoad,
    pub load_ops: PanelLoad,
    /// Phase 10 Ops panel: which sub-view + its real store data.
    pub ops_tab: OpsTab,
    pub ops_timeline: Vec<TimelineRow>,
    pub ops_skills: Vec<SkillRow>,
    pub ops_findings: Vec<FindingRow>,
    pub ops_recovery: Vec<RecoveryRow>,
    /// Phase 11 (M11.6): rooms + extensions + computer sub-views (real
    /// reads from the one Runtime — views only).
    pub ops_rooms: Vec<RoomRow>,
    pub ops_extensions: Vec<ExtensionRow>,
    pub ops_computer: Vec<ComputerRow>,
    /// Phase 12: industrial + trading + learning at-a-glance (real reads).
    pub ops_intel: Vec<IntelRow>,
    /// Selection within the Rooms list (pause/resume/steer act on it).
    pub sel_room: Option<usize>,
    /// Where a read-only Ops sub-view's selection would go. Never drawn and
    /// never read; it exists so the accessor above can hand back a real slot
    /// instead of a dangling reference. See [`App::sel`].
    sel_readonly_sink: Option<usize>,
    /// Steer text-input buffer (`Some` while the operator is typing).
    pub steer_input: Option<String>,

    // ---- interaction ----
    /// Workbench hit regions, rewritten every frame by [`crate::ui::draw`].
    ///
    /// The registry is the *only* place input coordinates are resolved, and it is
    /// populated from the same `Rect`s the renderer drew into. That is the whole
    /// point: a second copy of the layout for input handling is a second copy to
    /// forget to update, and the symptom is a click that activates a control
    /// which has moved or vanished.
    pub hits: std::cell::RefCell<crate::design::HitTargets>,
    /// Last known pointer position (a terminal reports no standalone
    /// "where is the cursor" event).
    pub pointer: (u16, u16),
    /// The target a press armed, released on the next button-up.
    ///
    /// Workbench mirrors the Agent's contract exactly: a press only arms, a
    /// release over the same target activates, and dragging off disarms. Two
    /// surfaces with two different click contracts is how a permission dialog
    /// ends up firing twice.
    pub pressed: Option<HitTarget>,
}

impl From<Focus> for PanelId {
    fn from(f: Focus) -> Self {
        match f {
            Focus::Workspaces => Self::Workspaces,
            Focus::Sessions => Self::Sessions,
            Focus::Tasks => Self::Tasks,
            Focus::Ops => Self::Ops,
        }
    }
}

impl From<PanelId> for Focus {
    fn from(p: PanelId) -> Self {
        match p {
            PanelId::Workspaces => Self::Workspaces,
            PanelId::Sessions => Self::Sessions,
            PanelId::Tasks => Self::Tasks,
            PanelId::Ops => Self::Ops,
        }
    }
}

impl App {
    /// The semantic tokens for the committed theme.
    pub fn tokens(&self) -> crate::design::tokens::Tokens {
        self.theme.tokens()
    }

    /// The focused panel's selection, read/write through one place.
    ///
    /// Four `match` arms in four different methods is how the four panels drift
    /// apart; this is the single point where "which list is focused" is decided.
    pub fn sel(&self) -> &Option<usize> {
        match self.focus {
            Focus::Workspaces => &self.sel_workspace,
            Focus::Sessions => &self.sel_session,
            Focus::Tasks => &self.sel_task,
            Focus::Ops if self.ops_tab == OpsTab::Rooms => &self.sel_room,
            // Every other Ops sub-view is a read-only scroll view with no row
            // selection, so it reports `None` rather than borrowing another
            // panel's index.
            Focus::Ops => &None,
        }
    }

    fn sel_mut(&mut self) -> &mut Option<usize> {
        match self.focus {
            Focus::Workspaces => &mut self.sel_workspace,
            Focus::Sessions => &mut self.sel_session,
            Focus::Tasks => &mut self.sel_task,
            Focus::Ops if self.ops_tab == OpsTab::Rooms => &mut self.sel_room,
            Focus::Ops => {
                // A read-only view has no selection to store. Returning a
                // detached value would drop the write silently, so this is a
                // real location that is never drawn and never read.
                &mut self.sel_readonly_sink
            }
        }
    }

    /// How many selectable rows the focused panel currently has.
    pub fn selectable_rows(&self) -> usize {
        match self.focus {
            Focus::Workspaces => self.workspaces.len(),
            Focus::Sessions => self.sessions.len(),
            Focus::Tasks => self.tasks.len(),
            Focus::Ops if self.ops_tab == OpsTab::Rooms => self.ops_rooms.len(),
            Focus::Ops => 0,
        }
    }

    /// Re-clamp the focused panel's selection after its rows changed.
    ///
    /// A selection that survives the row it pointed at is a selection on a row
    /// that no longer exists: it draws nothing, or draws the wrong row.
    pub fn reconcile_selection(&mut self) {
        let n = self.selectable_rows();
        let slot = self.sel_mut();
        *slot = match (*slot, n) {
            (_, 0) => None,
            (None, _) => Some(0),
            (Some(i), _) if i < n => Some(i),
            (Some(_), _) => Some(n - 1),
        };
    }

    /// Move the focused panel's selection by `delta`, wrapping.
    ///
    /// Does nothing when the panel has no rows: an arrow key on an empty panel
    /// must not manufacture a selection out of nothing.
    pub fn step_selection(&mut self, delta: isize) -> Option<usize> {
        let n = self.selectable_rows();
        if n == 0 {
            self.sel_mut().take();
            return None;
        }
        // With no current selection, "down" means the first row and "up" means
        // the last. Treating "nothing selected" as row 0 and then adding the
        // delta skipped row 0 entirely: the first Down on a freshly loaded panel
        // jumped to the second row, and the second row is where a new operator's
        // attention was least likely to be.
        let next = match *self.sel() {
            None => {
                if delta < 0 {
                    n - 1
                } else {
                    0
                }
            }
            Some(cur) => (cur as isize + delta).rem_euclid(n as isize) as usize,
        };
        *self.sel_mut() = Some(next);
        Some(next)
    }
}

impl App {
    /// Attach to a live runtime and load the initial view (real store reads).
    pub async fn new(runtime: Arc<Runtime>) -> Result<Self, AppError> {
        let mut app = Self {
            runtime,
            theme: crate::theme::load_theme(),
            theme_path: crate::theme::theme_config_path(),
            focus: Focus::Sessions,
            workspace: None,
            workspaces: Vec::new(),
            sessions: Vec::new(),
            current_session: None,
            tasks: Vec::new(),
            mode: ExecutionMode::Sovereign,
            status: "ready".into(),
            hits: std::cell::RefCell::new(crate::design::HitTargets::default()),
            pointer: (0, 0),
            pressed: None,
            should_quit: false,
            sel_workspace: None,
            sel_session: None,
            sel_task: None,
            load_workspaces: PanelLoad::default(),
            load_sessions: PanelLoad::default(),
            load_tasks: PanelLoad::default(),
            load_ops: PanelLoad::default(),
            ops_tab: OpsTab::Timeline,
            ops_timeline: Vec::new(),
            ops_skills: Vec::new(),
            ops_findings: Vec::new(),
            ops_recovery: Vec::new(),
            ops_rooms: Vec::new(),
            ops_extensions: Vec::new(),
            ops_computer: Vec::new(),
            ops_intel: Vec::new(),
            sel_room: None,
            sel_readonly_sink: None,
            steer_input: None,
        };
        app.refresh().await?;
        // Reopen the durable active workspace pointer if one exists.
        match reopen_current(&*app.runtime.store, &now()).await {
            Ok(ws) => {
                app.status = format!("reopened workspace {}", ws.address_code);
                app.workspace = Some(ws);
            }
            Err(openrt_workspace::WorkspaceError::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
        }
        app.refresh_workspaces().await?;
        Ok(app)
    }

    /// Reload mode + workspace list + sessions (+ tasks for the current
    /// session) + the Phase 10 Ops panel data.
    pub async fn refresh(&mut self) -> Result<(), AppError> {
        self.mode = self.runtime.modes.current().await?.mode;
        // Each panel records its own outcome and the others still refresh, then
        // the first failure is returned.
        //
        // The previous version short-circuited on the first `?`, so one failing
        // read left every later panel showing whatever it had last seen — and
        // with no error anywhere on screen, stale rows were indistinguishable
        // from fresh ones. Recording per panel keeps a failure visible on the
        // panel that failed while the error still reaches the caller, so nothing
        // that handled it before stops being told.
        let mut first_err = None;
        // Written out rather than looped: the array literal would hold four
        // `&mut self` borrows alive across four `.await`s, which is both a
        // borrow error and a promise the future cannot keep.
        let ws = self.refresh_workspaces().await;
        if let Some(e) = self.record_refresh(PanelSlot::Workspaces, ws) {
            first_err.get_or_insert(e);
        }
        let sessions = self.refresh_sessions().await;
        if let Some(e) = self.record_refresh(PanelSlot::Sessions, sessions) {
            first_err.get_or_insert(e);
        }
        let tasks = self.refresh_tasks().await;
        if let Some(e) = self.record_refresh(PanelSlot::Tasks, tasks) {
            first_err.get_or_insert(e);
        }
        let ops = self.refresh_ops().await;
        if let Some(e) = self.record_refresh(PanelSlot::Ops, ops) {
            first_err.get_or_insert(e);
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Record one panel's refresh outcome; returns the error so the caller can
    /// keep the first one and still propagate it.
    fn record_refresh(
        &mut self,
        slot: PanelSlot,
        result: Result<(), AppError>,
    ) -> Option<AppError> {
        let load = match slot {
            PanelSlot::Workspaces => &mut self.load_workspaces,
            PanelSlot::Sessions => &mut self.load_sessions,
            PanelSlot::Tasks => &mut self.load_tasks,
            PanelSlot::Ops => &mut self.load_ops,
        };
        match result {
            Ok(()) => {
                load.ok();
                None
            }
            Err(e) => {
                load.err(&e);
                Some(e)
            }
        }
    }

    /// Reload all four Ops sub-views from the real authorities (M10.5
    /// timeline ledger, M10.6 asset skills, M10.9 shield findings, M10.2
    /// recovery records). Every row is a store read — never invented.
    pub async fn refresh_ops(&mut self) -> Result<(), AppError> {
        // M10.5: verified timeline over the one history ledger, scoped to
        // the current session when one is open.
        let opts = openrt_runtime::TimelineOptions {
            session: self.current_session.map(|id| id.to_string()),
            ..Default::default()
        };
        let timeline = self.runtime.history_timeline(&opts).await?;
        self.ops_timeline = timeline
            .records
            .iter()
            .map(|r| TimelineRow {
                seq: r.seq,
                status: r.status.clone(),
                event_kind: r.event_kind.clone(),
                note: r.note.clone(),
            })
            .collect();

        // M10.6: installed skill id/activation pairs from the AssetStore.
        let skills = self.runtime.assets_skill_states().await?;
        self.ops_skills = skills
            .into_iter()
            .map(|(id, state)| SkillRow { id, state })
            .collect();

        // M10.9: recorded AgentShield findings (newest first), same store
        // the gate writes to.
        let findings = openrt_shield::store::list_findings(&*self.runtime.store, None).await?;
        self.ops_findings = findings
            .into_iter()
            .map(|f| FindingRow {
                id: f.id,
                severity: f.severity.as_str().to_string(),
                category: f.category.as_str().to_string(),
                reason: f.reason,
                status: f.status.as_str().to_string(),
            })
            .collect();

        // M10.2: boot-recovery records (interrupted/incomplete/stale).
        let records = self.runtime.recovery_records().await?;
        self.ops_recovery = records
            .iter()
            .map(|r| RecoveryRow {
                record_id: r.record_id.clone(),
                interrupted: r.interrupted_tasks.len(),
                incomplete: r.incomplete_runs.len(),
                stale_sandboxes: r.stale_sandboxes.len(),
                clean: r.is_clean(),
            })
            .collect();

        // M11.6: workbench rooms — the same `workbench.rooms` records the
        // CLI writes (views only; no UI-local authoritative state).
        let rooms = self.runtime.list_rooms().await?;
        self.ops_rooms = rooms
            .iter()
            .map(|r| RoomRow {
                id: r.id.clone(),
                title: r.title.clone(),
                workbench: r.workbench_code.clone(),
                status: r.status.to_string(),
                agents: r.agents.len(),
                materials: r.materials.len(),
                steer: r.steer.len(),
                active_workflow: r.active_workflow.clone(),
            })
            .collect();
        self.sel_room = match (self.sel_room, self.ops_rooms.len()) {
            (_, 0) => None,
            (None, _) => Some(0),
            (Some(i), n) if i < n => Some(i),
            (Some(_), n) => Some(n - 1),
        };

        // M11.6: installed extension versions (activation + security status
        // straight from the ONE registry — imported never arrives Active).
        let states = self.runtime.extension_states().await?;
        self.ops_extensions = states
            .iter()
            .map(|s| ExtensionRow {
                id: s["id"].as_str().unwrap_or("?").to_string(),
                version: s["version"].to_string(),
                activation: s["activation"].as_str().unwrap_or("?").to_string(),
                security: s["security"].as_str().unwrap_or("?").to_string(),
            })
            .collect();

        // M11.6: computer capability matrix — read-only snapshot from the
        // same ComputerControl authority the CLI prints (no UI-local state).
        let env = self.runtime.computer_env_snapshot();
        self.ops_computer = env["capabilities"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .map(|r| ComputerRow {
                        capability: r["capability"].as_str().unwrap_or("?").to_string(),
                        status: r["status"].as_str().unwrap_or("?").to_string(),
                        detail: r["reason"].as_str().unwrap_or("").to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Phase 12 (M12): at-a-glance rows over the ONE store — record
        // counts from the same Runtime methods the CLI calls, plus honest
        // backend statuses (research-only refusal, no-training-backend probe).
        let assets = self
            .runtime
            .industrial_assets(&openrt_industrial::AssetFilter::default())
            .await?;
        let instruments = self.runtime.trading_instruments().await?;
        let datasets = self.runtime.trading_datasets().await?;
        let strategies = self.runtime.trading_strategies().await?;
        let episodes = self.runtime.learn_episodes().await?;
        let promotions = self.runtime.learn_promotions().await?;
        let signals = self.runtime.learn_routing_signals().await?;
        let cache = self.runtime.cache_state().await?;
        let checks = self.runtime.phase12_doctor_checks().await;
        let mut pass = 0usize;
        let mut warn = 0usize;
        let mut unavailable = 0usize;
        for c in &checks {
            match c.status.as_str() {
                "PASS" => pass += 1,
                "WARN" => warn += 1,
                _ => unavailable += 1,
            }
        }
        self.ops_intel = vec![
            IntelRow {
                label: "industrial assets".into(),
                value: assets.len().to_string(),
            },
            IntelRow {
                label: "trading instruments".into(),
                value: instruments.len().to_string(),
            },
            IntelRow {
                label: "trading datasets".into(),
                value: datasets.len().to_string(),
            },
            IntelRow {
                label: "trading strategies".into(),
                value: strategies.len().to_string(),
            },
            IntelRow {
                label: "trading execute".into(),
                value: "DENIED (research only)".into(),
            },
            IntelRow {
                label: "experience episodes".into(),
                value: episodes.len().to_string(),
            },
            IntelRow {
                label: "promotion decisions".into(),
                value: promotions.len().to_string(),
            },
            IntelRow {
                label: "routing signals".into(),
                value: signals.len().to_string(),
            },
            IntelRow {
                label: "model cache entries".into(),
                value: cache.entries.len().to_string(),
            },
            IntelRow {
                label: "phase12 doctor".into(),
                value: format!("{pass} pass / {warn} warn / {unavailable} other"),
            },
        ];
        Ok(())
    }

    /// Switch the Ops panel sub-view (keys 1–8).
    ///
    /// Focus follows the switch. Changing the sub-view of a panel the operator
    /// is not looking at produces no visible change at all — the keys appear to
    /// do nothing — so the switch brings Ops into focus and highlights row 0
    /// when there is one.
    pub fn set_ops_tab(&mut self, tab: OpsTab) {
        self.ops_tab = tab;
        self.focus = Focus::Ops;
        if tab == OpsTab::Rooms {
            self.sel_room = match (self.sel_room, self.ops_rooms.len()) {
                (_, 0) => None,
                (None, _) => Some(0),
                (Some(i), n) if i < n => Some(i),
                (Some(_), n) => Some(n - 1),
            };
        }
        self.status = format!("ops view: {}", tab.title());
    }

    /// M10.5 time machine: fork a what-if branch from the current head.
    /// The branch name derives from the store tail (store-derived — never
    /// UI-local bookkeeping), and failures land on the status line instead
    /// of tearing the TUI down.
    pub async fn ops_time_machine_fork(&mut self) {
        let outcome = match self.runtime.history_tail().await {
            Err(e) => format!("fork failed: {e}"),
            Ok(tail) => {
                let branch = format!("tui-fork-{tail}");
                match self.runtime.history_fork(&branch, "").await {
                    Err(e) => format!("fork failed: {e}"),
                    Ok(()) => format!("forked branch `{branch}` from head {tail}"),
                }
            }
        };
        self.status = outcome;
        let _ = self.refresh_ops().await;
    }

    /// M10.5 time machine: divergence verdict for the newest branch in the
    /// store's fork catalog (read-only; parent ledger untouched).
    pub async fn ops_time_machine_compare(&mut self) {
        self.status = match self.runtime.history_forks().await {
            Err(e) => format!("compare failed: {e}"),
            Ok(forks) => match forks.last() {
                None => "no fork branches to compare".to_string(),
                Some(f) => match self.runtime.history_branch_compare(&f.branch).await {
                    Err(e) => format!("compare failed: {e}"),
                    Ok(c) => format!(
                        "branch `{}` base={} shared={} parent_tail={} fork_verified={} entries={}",
                        c.branch,
                        c.base_seq,
                        c.shared_prefix_len,
                        c.parent_tail_seq,
                        c.fork_records_verified,
                        c.entries.len()
                    ),
                },
            },
        };
    }

    /// M10.5 time machine: leave the what-if view — tail + fork catalog of
    /// the parent ledger.
    pub async fn ops_time_machine_return(&mut self) {
        self.status = match self.runtime.history_return_to_current().await {
            Err(e) => format!("return failed: {e}"),
            Ok(v) => format!(
                "returned to current head (tail={} {} fork(s))",
                v.tail_seq,
                v.forks.len()
            ),
        };
    }

    /// M10.5 time machine: state-as-of — materialize the ledger at the
    /// newest fork's base sequence (head when no fork exists), read-only.
    pub async fn ops_time_machine_state(&mut self) {
        let mut as_of = match self.runtime.history_forks().await {
            Ok(forks) => forks.last().map(|f| f.base_seq).unwrap_or(0),
            Err(_) => 0,
        };
        if as_of == 0 {
            as_of = self.runtime.history_tail().await.unwrap_or(0);
        }
        self.status = match self.runtime.history_state_as_of(as_of).await {
            Err(e) => format!("state-as-of failed: {e}"),
            Ok(s) => format!(
                "state as-of seq {}: {} record(s), {} session(s)",
                s.as_of,
                s.records.len(),
                s.sessions.len()
            ),
        };
    }

    pub async fn refresh_workspaces(&mut self) -> Result<(), AppError> {
        let active_id = self.workspace.as_ref().map(|w| w.address_code.clone());
        let mut rows = Vec::new();
        for (key, value) in list_workspaces(&*self.runtime.store).await? {
            // Keys are `w.<id>`; address lives in the record.
            let address = value
                .get("address_code")
                .and_then(|v| v.as_str())
                .unwrap_or(&key)
                .to_string();
            let title = value
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let description = value
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            rows.push(WorkspaceRow {
                active: active_id.as_deref() == Some(address.as_str()),
                address_code: address,
                title,
                description,
            });
        }
        rows.sort_by(|a, b| a.address_code.cmp(&b.address_code));
        self.workspaces = rows;
        self.sel_workspace = match (self.sel_workspace, self.workspaces.len()) {
            (_, 0) => None,
            (None, _) => Some(0),
            (Some(i), n) if i < n => Some(i),
            (Some(_), n) => Some(n - 1),
        };
        Ok(())
    }

    pub async fn refresh_sessions(&mut self) -> Result<(), AppError> {
        let sessions = self.runtime.list_sessions().await?;
        self.sessions = sessions.iter().map(session_row).collect();
        // Keep selection valid; default to the active current session if any.
        if let Some(cur) = self.current_session {
            if let Some(pos) = self.sessions.iter().position(|s| s.id == cur) {
                self.sel_session = Some(pos);
            } else {
                self.current_session = None;
            }
        }
        if self.sessions.is_empty() {
            self.current_session = None;
            self.sel_session = None;
        } else if self.current_session.is_none() {
            // Resume the most recently listed session (real order from store).
            let id = self.sessions.last().map(|s| s.id);
            self.current_session = id;
            self.sel_session = Some(self.sessions.len() - 1);
        } else if self.sel_session.is_none_or(|i| i >= self.sessions.len()) {
            self.sel_session = Some(self.sessions.len() - 1);
        }
        Ok(())
    }

    pub async fn refresh_tasks(&mut self) -> Result<(), AppError> {
        self.tasks.clear();
        if let Some(sid) = self.current_session {
            let tasks: Vec<Task> = self.runtime.store.list_tasks(Some(sid)).await?;
            self.tasks = tasks.iter().map(task_row).collect();
        }
        self.sel_task = match (self.sel_task, self.tasks.len()) {
            (_, 0) => None,
            (None, _) => Some(0),
            (Some(i), n) if i < n => Some(i),
            (Some(_), n) => Some(n - 1),
        };
        Ok(())
    }

    /// Create a durable workspace and make it current (store round-trip).
    pub async fn create_workspace(
        &mut self,
        address_code: &str,
        title: &str,
        description: &str,
    ) -> Result<(), AppError> {
        if address_code.trim().is_empty() {
            return Err(AppError::Invalid(
                "workspace address code must not be empty".into(),
            ));
        }
        create_workspace(
            &*self.runtime.store,
            address_code,
            title,
            description,
            &now(),
        )
        .await?;
        self.workspace = Some(open_workspace(&*self.runtime.store, address_code, &now()).await?);
        self.status = format!("created workspace {address_code}");
        self.refresh_workspaces().await?;
        Ok(())
    }

    /// Open an existing workspace by address code (fail-closed NotFound).
    pub async fn open_workspace(&mut self, address_code: &str) -> Result<(), AppError> {
        let ws = open_workspace(&*self.runtime.store, address_code, &now()).await?;
        self.status = format!("opened workspace {address_code}");
        self.workspace = Some(ws);
        self.refresh_workspaces().await?;
        Ok(())
    }

    /// Create a session through the runtime (durable + SessionCreated event).
    pub async fn create_session(&mut self, title: &str) -> Result<(), AppError> {
        if title.trim().is_empty() {
            return Err(AppError::Invalid("session title must not be empty".into()));
        }
        let session = self.runtime.create_session(title).await?;
        self.current_session = Some(session.id);
        self.status = format!("created session {}", session.id);
        self.refresh_sessions().await?;
        self.refresh_tasks().await?;
        self.refresh_ops().await?;
        Ok(())
    }

    /// Switch the current session by id (must exist in the store).
    pub async fn switch_session(&mut self, id: SessionId) -> Result<(), AppError> {
        let session = self.runtime.load_session(id).await?;
        self.current_session = Some(session.id);
        self.status = format!("switched to session {}", session.id);
        self.refresh_sessions().await?;
        self.refresh_tasks().await?;
        // The Ops timeline is session-scoped — re-read on every switch.
        self.refresh_ops().await?;
        Ok(())
    }

    /// Move selection within the focused list and commit the switch (sessions).
    ///
    /// The step itself is delegated to [`App::step_selection`] so every panel
    /// gets identical wrap-around and identical "no rows means no selection"
    /// behaviour; only the session panel has a side effect to add.
    pub async fn move_selection(&mut self, delta: isize) -> Result<(), AppError> {
        let Some(next) = self.step_selection(delta) else {
            // An empty panel says so rather than leaving the operator pressing a
            // key that visibly does nothing.
            self.status = match self.focus {
                Focus::Workspaces => "no workspaces to select".into(),
                Focus::Sessions => "no sessions to select".into(),
                Focus::Tasks => "no tasks to select".into(),
                Focus::Ops => format!("{} has no rows to select", self.ops_tab.title()),
            };
            return Ok(());
        };
        if self.focus == Focus::Sessions {
            if let Some(row) = self.sessions.get(next) {
                let id = row.id;
                self.switch_session(id).await?;
            }
        }
        Ok(())
    }

    /// Open the currently highlighted workspace (keyboard Enter on Workspaces).
    pub async fn activate_workspace_selection(&mut self) -> Result<(), AppError> {
        if let Some(row) = self.sel_workspace.and_then(|i| self.workspaces.get(i)) {
            let code = row.address_code.clone();
            self.open_workspace(&code).await?;
        }
        Ok(())
    }

    /// Run a prompt in the current session (real pipeline; waits for completion).
    pub async fn run_prompt(&mut self, prompt: &str) -> Result<(), AppError> {
        if prompt.trim().is_empty() {
            return Err(AppError::Invalid("prompt must not be empty".into()));
        }
        let sid = match self.current_session {
            Some(id) => id,
            None => {
                let s = self.runtime.create_session("tui").await?;
                self.current_session = Some(s.id);
                self.refresh_sessions().await?;
                s.id
            }
        };
        let task = self.runtime.run_task(sid, prompt).await?;
        // The short id, because this line shares the status bar with the key map
        // at 80 columns and a 36-character UUID pushes the bindings off the end.
        self.status = format!("task {:?}: {}", task.status, short_id(task.id.to_string()));
        self.refresh_sessions().await?;
        self.refresh_tasks().await?;
        self.refresh_ops().await?;
        Ok(())
    }

    /// Compile the current session's latest task prompt (or a fallback goal)
    /// into a validated workflow through the production compiler (M6).
    pub async fn compile_workflow_from_context(&mut self) -> Result<(), AppError> {
        let goal = self
            .tasks
            .last()
            .map(|t| t.prompt.clone())
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| "Create a file containing the current project status".into());
        let out = self
            .runtime
            .compile_workflow(&goal, openrt_compiler::CompileMode::Compile)
            .await?;
        self.status = format!(
            "compiled {} ({} steps, persisted={})",
            out.workflow_id,
            out.workflow.steps.len(),
            out.persisted
        );
        self.refresh_tasks().await?;
        Ok(())
    }

    /// The room under the Rooms-panel selection (None when the list is empty).
    fn selected_room(&self) -> Option<&RoomRow> {
        self.sel_room.and_then(|i| self.ops_rooms.get(i))
    }

    /// M11.6: pause the selected room's active workflow — cooperative, via
    /// the ONE runtime; a room without a run fails closed on the status line
    /// (the TUI never fabricates a pause).
    pub async fn room_pause(&mut self) {
        self.status = match self.selected_room().and_then(|r| r.active_workflow.clone()) {
            None => "no active workflow on the selected room".into(),
            Some(wf) => match self.runtime.workflow_pause(&wf, TUI_OPERATOR).await {
                Ok(()) => format!("pause requested for workflow {wf} (drains in-flight steps)"),
                Err(e) => format!("pause failed: {e}"),
            },
        };
        let _ = self.refresh_ops().await;
    }

    /// M11.6: resume the selected room's paused workflow — checkpoints are
    /// the run authority; the TUI only asks the runtime.
    pub async fn room_resume(&mut self) {
        self.status = match self.selected_room().and_then(|r| r.active_workflow.clone()) {
            None => "no active workflow on the selected room".into(),
            Some(wf) => match self.runtime.workflow_resume(&wf, TUI_OPERATOR).await {
                Ok(report) => format!(
                    "resumed {wf}: {} ({} step report(s))",
                    report.status,
                    report.steps.len()
                ),
                Err(e) => format!("resume failed: {e}"),
            },
        };
        let _ = self.refresh_ops().await;
    }

    /// M11.6: open the steer text-input line for the selected room.
    pub fn room_steer_begin(&mut self) {
        let selected = self
            .selected_room()
            .map(|r| (r.id.clone(), r.title.clone()));
        match selected {
            None => self.status = "no room selected".into(),
            Some((_id, title)) => {
                self.steer_input = Some(String::new());
                self.status = format!("steer room {title} — Enter: submit, Esc: cancel");
            }
        }
    }

    /// M11.6: append the typed steer directive (append-only, audited through
    /// the same runtime path the CLI uses; empty input appends nothing).
    pub async fn room_steer_submit(&mut self) {
        let Some(buffer) = self.steer_input.take() else {
            return;
        };
        let Some(room) = self.selected_room().map(|r| r.id.clone()) else {
            self.status = "no room selected".into();
            return;
        };
        if buffer.trim().is_empty() {
            self.status = "empty steer — nothing appended".into();
            return;
        }
        self.status = match self.runtime.room_steer(&room, TUI_OPERATOR, &buffer).await {
            Ok(entry) => format!("steer appended at {} (append-only)", entry.at),
            Err(e) => format!("steer failed: {e}"),
        };
        let _ = self.refresh_ops().await;
    }

    /// Cycle focus forward (Tab).
    /// Advance focus one panel forward, wrapping.
    ///
    /// The ring covers exactly the four panels and nothing else. It used to
    /// include a fifth stop for the status bar, which drew nothing, owned no
    /// rows, and had nothing to activate — so Tab could park the operator on
    /// invisible focus with every subsequent key reporting "nothing to do".
    /// Focus must always correspond to something rendered.
    pub fn cycle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Workspaces => Focus::Sessions,
            Focus::Sessions => Focus::Tasks,
            Focus::Tasks => Focus::Ops,
            Focus::Ops => Focus::Workspaces,
        };
    }

    /// Focus backward, wrapping — `Shift+Tab`.
    pub fn cycle_focus_back(&mut self) {
        self.focus = match self.focus {
            Focus::Workspaces => Focus::Ops,
            Focus::Sessions => Focus::Workspaces,
            Focus::Tasks => Focus::Sessions,
            Focus::Ops => Focus::Tasks,
        };
    }

    /// Request exit (the run loop restores the terminal).
    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    /// Activate the current selection in the focused panel (Enter).
    ///
    /// Panel-specific: opening a workspace, switching to a session, or
    /// selecting an Ops row. A panel with nothing to activate says so in the
    /// status line rather than silently doing nothing, which is the difference
    /// between "my keypress was ignored" and "the app told me why".
    pub async fn activate_focused(&mut self) -> Result<(), AppError> {
        match self.focus {
            Focus::Workspaces => self.activate_workspace_selection().await,
            // Sessions already switch on selection (see `move_selection`), so
            // Enter is a no-op rather than a redundant second switch.
            Focus::Sessions => {
                self.status = match self.current_session {
                    Some(id) => format!("session {} already active", id),
                    None => "no session selected".into(),
                };
                Ok(())
            }
            // Tasks are a read-only history: there is no task to open.
            Focus::Tasks => {
                self.status = match self.sel_task.and_then(|i| self.tasks.get(i)) {
                    Some(t) => format!(
                        "task {} [{}] — {}",
                        short_id(t.id.to_string()),
                        t.status,
                        head_text(&t.prompt)
                    ),
                    None => "no task selected".into(),
                };
                Ok(())
            }
            // Ops: Rooms is the only actionable sub-view.
            Focus::Ops if self.ops_tab == OpsTab::Rooms => {
                match self.selected_room() {
                    Some(r) => {
                        let name = r.title.clone();
                        let wf = r.active_workflow.clone().unwrap_or_else(|| "idle".into());
                        self.status = format!("room {name} — {wf}");
                    }
                    None => self.status = "no room selected".into(),
                }
                Ok(())
            }
            Focus::Ops => {
                self.status = format!("{} is read-only", self.ops_tab.title());
                Ok(())
            }
        }
    }

    /// Handle one pointer event in Workbench mode.
    ///
    /// The contract is the same one the Agent mode implements, deliberately:
    ///
    /// * `Down` only *arms*. Nothing happens until a release.
    /// * `Up` activates only if the release lands on the same target that armed.
    /// * Dragging off the armed target disarms — the common cause of "I clicked
    ///   the delete row and it deleted a different one".
    /// * `Drag` keeps the press armed while it stays inside the original target,
    ///   so a sloppy click inside a row still works.
    /// * Anything other than the left button is ignored outright. Right-click
    ///   must never activate a destructive row.
    ///
    /// Resolution goes through `hits`, which the renderer repopulated from the
    /// geometry it actually drew, so a click cannot land on a control that is no
    /// longer on screen.
    pub async fn handle_mouse(&mut self, m: crossterm::event::MouseEvent) -> Result<(), AppError> {
        use crossterm::event::{MouseButton, MouseEventKind};

        let col = m.column;
        let row = m.row;
        self.pointer = (col, row);

        match m.kind {
            MouseEventKind::Moved | MouseEventKind::Drag(_) => {
                // A drag that leaves the armed target cancels the press. A drag
                // that stays inside does not, so the release can still land.
                if let Some(armed) = self.pressed.clone() {
                    if self.hits.borrow().hit(col, row) != armed {
                        self.pressed = None;
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let hit = self.hits.borrow().hit(col, row);
                self.pressed = (hit != HitTarget::None).then_some(hit);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let armed = match self.pressed.take() {
                    // Release with no armed press: a release arriving from a
                    // drag the terminal reported as `Up` (some terminals emit
                    // `Up` where they mean `Drag`). Never activate on this.
                    None => return Ok(()),
                    Some(a) => a,
                };
                if self.hits.borrow().hit(col, row) != armed {
                    return Ok(());
                }
                self.activate_hit(armed).await?;
            }
            // A press must not survive a wheel scroll: the content under the
            // pointer is about to move, and arming against geometry that no
            // longer describes the screen is how the wrong row gets activated.
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => self.pressed = None,
            _ => {}
        }
        Ok(())
    }

    /// Act on a resolved hit target.
    async fn activate_hit(&mut self, target: HitTarget) -> Result<(), AppError> {
        match target {
            HitTarget::WorkbenchPanel { panel } => {
                self.focus = panel.into();
            }
            HitTarget::WorkbenchRow { panel, index } => {
                self.focus = panel.into();
                match panel {
                    PanelId::Workspaces => {
                        if index < self.workspaces.len() {
                            self.sel_workspace = Some(index);
                            self.activate_workspace_selection().await?;
                        }
                    }
                    PanelId::Sessions => {
                        if let Some(row) = self.sessions.get(index) {
                            let id = row.id;
                            if self.sel_session != Some(index) {
                                self.sel_session = Some(index);
                                self.switch_session(id).await?;
                            }
                        }
                    }
                    PanelId::Tasks => {
                        if index < self.tasks.len() {
                            self.sel_task = Some(index);
                        }
                    }
                    PanelId::Ops => {
                        // Ops rows are only actionable in the Rooms sub-view;
                        // elsewhere a click is a read-only selection.
                        if self.ops_tab == OpsTab::Rooms && index < self.ops_rooms.len() {
                            self.sel_room = Some(index);
                        }
                    }
                }
            }
            // Not reachable: the Workbench registry never holds these. Handled
            // explicitly so a future variant addition is a compile error rather
            // than a silent fallthrough that activates an Agent surface.
            _ => {}
        }
        Ok(())
    }

    /// Drop every registered region and any armed press.
    ///
    /// Called on every mode transition. A registry that survives a mode switch
    /// is a registry whose coordinates mean something else by the time a click
    /// arrives — the Workbench equivalent of the Agent's stale-mode hit bug.
    pub fn clear_interaction(&mut self) {
        self.hits.borrow_mut().clear();
        self.pressed = None;
    }

    /// The posture badge: what this session is allowed to do, and what that
    /// means.
    ///
    /// Returns a shared tone rather than a colour so both modes read one
    /// vocabulary. The header line already names the mode in words, so the badge
    /// is the redundant half of the pair — which is the point, because the two
    /// survive a monochrome terminal.
    pub fn mode_badge(&self) -> (&'static str, BadgeTone) {
        match self.mode {
            // Sovereign runs tools without a permission gate: warn, do not
            // paint it green.
            ExecutionMode::Sovereign => ("SOVEREIGN", BadgeTone::Warning),
            ExecutionMode::Provisioning => ("PROVISIONING", BadgeTone::Success),
        }
    }

    /// Counts worth watching, taken from data already loaded.
    ///
    /// Every value is a `len()` of a vector this app filled from the store, so
    /// the header can never claim a count the panels disagree with. Nothing is
    /// derived or estimated, and a panel that failed to load contributes its
    /// real length rather than a guess.
    pub fn header_meters(&self) -> Vec<(&'static str, String)> {
        let ops_rows = match self.ops_tab {
            OpsTab::Timeline => self.ops_timeline.len(),
            OpsTab::Skills => self.ops_skills.len(),
            OpsTab::Shield => self.ops_findings.len(),
            OpsTab::Recovery => self.ops_recovery.len(),
            OpsTab::Rooms => self.ops_rooms.len(),
            OpsTab::Extensions => self.ops_extensions.len(),
            OpsTab::Computer => self.ops_computer.len(),
            OpsTab::Intel => self.ops_intel.len(),
        };
        vec![
            ("ws", self.workspaces.len().to_string()),
            ("sess", self.sessions.len().to_string()),
            ("tasks", self.tasks.len().to_string()),
            ("ops", ops_rows.to_string()),
        ]
    }

    /// What the status text means, as a shared tone.
    ///
    /// The status line is free text written by whichever action last ran, so the
    /// meaning is matched from a small closed vocabulary. Anything unrecognised
    /// is `Neutral`: a tone for a phrase nobody classified would be a guess, and
    /// a wrong tone is worse than no tone.
    pub fn status_tone(&self) -> BadgeTone {
        let text = self.status.trim().to_ascii_lowercase();
        if text.contains("fail")
            || text.contains("error")
            || text.contains("refus")
            || text.contains("denied")
            || text.contains("cannot")
            || text.contains("no such")
        {
            BadgeTone::Danger
        } else if text.contains("cancel")
            || text.contains("paused")
            || text.contains("interrupted")
            || text.contains("could not")
            || text.contains("unavailable")
            || text.contains("stale")
        {
            BadgeTone::Warning
        } else if text.contains("await") || text.contains("pending") || text.contains("loading") {
            BadgeTone::Pending
        } else {
            BadgeTone::Neutral
        }
    }

    /// A short human line for the header (never invents data).
    ///
    /// The mode is deliberately absent: it is already on screen as a posture
    /// badge from [`App::mode_badge`]. It used to be spelled out here as
    /// `mode=sovereign` *and* badged as `! SOVEREIGN`, which spent header width
    /// repeating one fact and left the operator with two renderings of it to
    /// keep in sync. What remains is what the badges do not say: which
    /// workspace, which session, and how to get back to Agent Mode — the last
    /// being the only route out of Workbench and therefore the one binding that
    /// cannot be left to the status bar, which ellipsizes at 80 columns.
    pub fn header_line(&self) -> String {
        self.header_line_for(true)
    }

    /// The header subtitle, spelled out or compact depending on the width.
    ///
    /// At 80 columns the long form plus the title and the posture badge overflow
    /// the single header row, and the overflow lands on the badge — so the badge
    /// read "SOVERE" on one line and "IGN" on the next. Dropping the `key=`
    /// prefixes is what buys the room, and it costs nothing: the values keep their
    /// own labels in the panels below.
    pub fn header_line_for(&self, wide: bool) -> String {
        if wide {
            return self.header_line_long();
        }
        let ws = self
            .workspace
            .as_ref()
            .map(|w| w.address_code.as_str())
            .unwrap_or("(none)");
        let sess = self
            .current_session
            .map(|id| short_id(id.to_string()))
            .unwrap_or_else(|| "(none)".into());
        format!("{ws} · {sess} · Ctrl+W agent")
    }

    /// The long spelling, for terminals with room for it.
    fn header_line_long(&self) -> String {
        let ws = self
            .workspace
            .as_ref()
            .map(|w| w.address_code.as_str())
            .unwrap_or("(none)");
        // The session's first id segment, not the whole UUID: a UUID is 36
        // characters of noise at the right edge of a header that already has a
        // title, a badge and the route back to Agent Mode to fit.
        let sess = self
            .current_session
            .map(|id| short_id(id.to_string()))
            .unwrap_or_else(|| "(none)".into());
        // Kept as short as it can be while still reading as words: at 80 columns
        // this shares the header line with the title and the posture badge, and
        // whatever does not fit is ellipsized from the right — which would take
        // the Ctrl+W route out first.
        format!("workspace={ws} · session={sess} · Ctrl+W agent")
    }
}

/// A short, single-line rendering of a task prompt for the status bar.
///
/// The bar is one row shared with the key map, so a multi-line prompt would push
/// the bindings off the end; whitespace is collapsed and the head is kept,
/// because the first words are what identify the task.
fn head_text(s: &str) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= 32 {
        return flat;
    }
    let mut out: String = flat.chars().take(31).collect();
    out.push('…');
    out
}

/// The first segment of a session id — enough to tell two sessions apart, short
/// enough to leave the header room for what it has to say.
fn short_id(full: String) -> String {
    full.split('-')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(&full)
        .to_string()
}

fn session_row(s: &Session) -> SessionRow {
    SessionRow {
        id: s.id,
        title: s.title.clone(),
        status: format!("{:?}", s.status),
        messages: s.content.len(),
        active: false,
    }
}

fn task_row(t: &Task) -> TaskRow {
    let status: &'static str = match t.status {
        TaskStatus::Created => "Created",
        TaskStatus::Queued => "Queued",
        TaskStatus::Running => "Running",
        TaskStatus::Waiting => "Waiting",
        TaskStatus::Completed => "Completed",
        TaskStatus::Failed => "Failed",
        TaskStatus::Cancelled => "Cancelled",
        TaskStatus::Interrupted => "Interrupted",
        TaskStatus::Paused => "Paused",
    };
    TaskRow {
        id: t.id.to_string(),
        prompt: t.prompt.clone(),
        status: status.to_string(),
        turns: t.turns.len(),
    }
}

fn now() -> String {
    chrono_like_now()
}

/// Operator identity recorded in audit rows for TUI-originated actions.
const TUI_OPERATOR: &str = "tui";

/// RFC-3339-ish UTC stamp without pulling chrono into the TUI dep graph.
fn chrono_like_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Format as `YYYY-MM-DDTHH:MM:SSZ` via civil-from-days (Howard Hinnant).
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod p5_tui_tests {
    use super::*;
    use openrt_models::MockProvider;
    use openrt_persistence::MemoryStore;

    async fn app() -> App {
        let rt = openrt_runtime::RuntimeBuilder::new()
            .config(openrt_config::Config::default())
            .store(Arc::new(MemoryStore::new()))
            .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
            .bus_capacity(32)
            .build()
            .await
            .unwrap();
        App::new(rt).await.unwrap()
    }

    /// Production path: create workspace + session through the App; assert the
    /// foundation store round-trips (IDs come from the store, not hardcoded UI).
    #[tokio::test]
    async fn workspace_and_session_round_trip_is_real() {
        let mut a = app().await;
        a.create_workspace("acct/tui", "TUI ws", "m51")
            .await
            .unwrap();
        assert_eq!(a.workspaces.len(), 1);
        assert_eq!(a.workspaces[0].address_code, "acct/tui");
        assert!(a.workspaces[0].active);
        assert!(a.workspace.is_some());

        a.create_session("hello tui").await.unwrap();
        let sid = a.current_session.expect("session selected");
        assert_eq!(a.sessions.len(), 1);
        assert_eq!(a.sessions[0].title, "hello tui");

        // A second create must appear in the real store list, not overwrite.
        a.create_session("second").await.unwrap();
        assert_eq!(a.sessions.len(), 2);

        // Switch back to the first session by id — load_session is real.
        a.switch_session(sid).await.unwrap();
        assert_eq!(a.current_session, Some(sid));
        assert_eq!(a.sessions[a.sel_session.unwrap()].id, sid);

        // Reopen after a simulated restart: same store, pointer intact.
        let store = a.runtime.store.clone();
        let reopened = reopen_current(&*store, &now()).await.unwrap();
        assert_eq!(reopened.address_code, "acct/tui");
        assert!(openrt_workspace::verify_workspace(&*store, &reopened.id)
            .await
            .unwrap());
    }

    /// Running a prompt through the TUI controller hits the real pipeline and
    /// leaves a durable task row for the current session.
    #[tokio::test]
    async fn run_prompt_produces_real_task_row() {
        let mut a = app().await;
        a.create_session("run").await.unwrap();
        a.run_prompt("echo from tui").await.unwrap();
        assert_eq!(a.tasks.len(), 1);
        assert_eq!(a.tasks[0].status, "Completed");
        assert!(
            a.tasks[0].prompt.contains("echo from tui"),
            "prompt must be the real submitted text"
        );
    }

    /// Fail-closed: empty title / empty address never reach the store.
    #[tokio::test]
    async fn invalid_inputs_fail_closed() {
        let mut a = app().await;
        assert!(matches!(
            a.create_session("  ").await,
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(
            a.create_workspace("", "x", "").await,
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(
            a.open_workspace("acct/ghost").await,
            Err(AppError::Workspace(_))
        ));
        assert!(matches!(a.run_prompt(" ").await, Err(AppError::Invalid(_))));
    }

    /// Header never invents workspace/session names when none are open.
    #[tokio::test]
    async fn header_uses_real_state_only() {
        let a = app().await;
        let h = a.header_line();
        assert!(h.contains("workspace=(none)"), "got: {h}");
        assert!(h.contains("session=(none)"), "got: {h}");
        // The mode is not spelled out here any more: it is on screen once, as
        // the posture badge. Two renderings of one fact is one too many, and the
        // header is the line that runs out of room first.
        assert!(!h.contains("mode=sovereign"), "got: {h}");
        assert_eq!(a.mode_badge().0, "SOVEREIGN", "the badge names it instead");
    }
}

#[cfg(test)]
mod p10_tui_tests {
    use super::*;
    use openrt_models::MockProvider;
    use openrt_persistence::MemoryStore;

    async fn app() -> App {
        let rt = openrt_runtime::RuntimeBuilder::new()
            .config(openrt_config::Config::default())
            .store(Arc::new(MemoryStore::new()))
            .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
            .bus_capacity(32)
            .build()
            .await
            .unwrap();
        App::new(rt).await.unwrap()
    }

    /// M10.5: the Ops timeline renders REAL ledger rows — seq chain strictly
    /// increasing, every row carries a real event kind, session-scoped.
    #[tokio::test]
    async fn ops_timeline_reflects_real_ledger_records() {
        let mut a = app().await;
        assert_eq!(a.ops_tab, OpsTab::Timeline, "default sub-view");
        a.create_session("ops-timeline").await.unwrap();
        a.run_prompt("echo ops timeline").await.unwrap();
        // HistorySink lands rows asynchronously — poll like phase5 does.
        for _ in 0..100 {
            if a.runtime.history_tail().await.unwrap() >= 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        a.refresh_ops().await.unwrap();
        assert!(
            !a.ops_timeline.is_empty(),
            "task lifecycle events must land in the verified ledger"
        );
        for w in a.ops_timeline.windows(2) {
            assert!(w[0].seq < w[1].seq, "timeline must be chain-ordered");
        }
        assert!(a.ops_timeline.iter().all(|r| !r.event_kind.is_empty()));
    }

    /// M10.5: the time-machine keys drive real Runtime APIs over the ONE
    /// ledger — fork appears in the store's catalog, compare/return/state
    /// report store-derived facts on the status line (never UI-local).
    #[tokio::test]
    async fn time_machine_ops_actions_surface_store_data() {
        let mut a = app().await;
        a.create_session("tui time machine").await.unwrap();
        a.run_prompt("seed rows for the time machine")
            .await
            .unwrap();
        for _ in 0..100 {
            if a.runtime.history_tail().await.unwrap() >= 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        a.ops_time_machine_fork().await;
        assert!(
            a.status.starts_with("forked branch `tui-fork-"),
            "{}",
            a.status
        );
        let forks = a.runtime.history_forks().await.unwrap();
        assert!(
            forks.iter().any(|f| f.branch.starts_with("tui-fork-")),
            "the fork lands in the store catalog: {forks:?}"
        );

        a.ops_time_machine_compare().await;
        assert!(a.status.starts_with("branch `"), "{}", a.status);
        assert!(a.status.contains("fork_verified="), "{}", a.status);

        a.ops_time_machine_state().await;
        assert!(a.status.contains("state as-of seq"), "{}", a.status);

        a.ops_time_machine_return().await;
        assert!(
            a.status.starts_with("returned to current head"),
            "{}",
            a.status
        );

        // The parent chain still verifies — forking never rewrites it.
        let verified = a.runtime.history_verify().await.unwrap();
        assert!(verified >= 3, "parent chain intact: {verified}");
    }

    /// M10.9: the Ops shield view lists findings recorded through the SAME
    /// store the gate writes to — real id/severity/status, never invented.
    #[tokio::test]
    async fn ops_shield_lists_recorded_finding() {
        let mut a = app().await;
        let finding = openrt_shield::SecurityFinding::new(
            openrt_shield::FindingCategory::NetworkEscalation,
            openrt_shield::Severity::High,
            "tui.ops.test",
            "tool.exec",
            "seeded finding for the ops view",
            serde_json::json!({"seed": true}),
        )
        .with_action(openrt_shield::ShieldAction::Deny, "test:deny->DENY");
        let report =
            openrt_shield::ShieldReport::from_findings("tui.ops.test", "tool.exec", vec![finding]);
        openrt_shield::store::record_enforced(
            &*a.runtime.store,
            &report,
            openrt_shield::FindingStatus::Denied,
        )
        .await
        .unwrap();

        a.refresh().await.unwrap();
        assert_eq!(a.ops_findings.len(), 1);
        let row = &a.ops_findings[0];
        assert_eq!(row.severity, "HIGH");
        assert_eq!(row.status, "denied");
        assert_eq!(row.category, "network_escalation");
        assert!(row.reason.contains("seeded finding"));

        a.set_ops_tab(OpsTab::Shield);
        assert_eq!(a.ops_tab, OpsTab::Shield);
        assert!(a.status.contains("Ops · Shield"));
    }

    /// M10.6: the Ops skills view mirrors `assets_skill_states` exactly —
    /// an empty AssetStore renders zero rows (honest, never invented).
    #[tokio::test]
    async fn ops_skills_mirror_asset_store() {
        let mut a = app().await;
        let expected = a.runtime.assets_skill_states().await.unwrap();
        a.refresh().await.unwrap();
        assert_eq!(a.ops_skills.len(), expected.len());
        for (row, (id, state)) in a.ops_skills.iter().zip(expected.iter()) {
            assert_eq!(&row.id, id);
            assert_eq!(&row.state, state);
        }
        a.set_ops_tab(OpsTab::Skills);
        assert_eq!(a.ops_tab, OpsTab::Skills);
    }

    /// M10.2: every boot persists a recovery record — the Ops recovery view
    /// shows the same records the runtime reads (clean boot marked ok).
    #[tokio::test]
    async fn ops_recovery_lists_boot_records() {
        let mut a = app().await;
        let records = a.runtime.recovery_records().await.unwrap();
        assert!(!records.is_empty(), "every boot records evidence");
        a.refresh().await.unwrap();
        assert_eq!(a.ops_recovery.len(), records.len());
        assert!(a.ops_recovery.iter().all(|r| r.clean));
        a.set_ops_tab(OpsTab::Recovery);
        assert_eq!(a.ops_tab, OpsTab::Recovery);
    }

    /// Focus ring includes Ops (5-panel ring) and sub-view switching is
    /// total over the four tabs.
    #[tokio::test]
    async fn focus_ring_and_ops_tabs_cover_all_views() {
        let mut a = app().await;
        let mut seen = vec![a.focus];
        for _ in 0..4 {
            a.cycle_focus();
            seen.push(a.focus);
        }
        assert_eq!(seen[0], seen[4], "ring must close on itself");
        assert!(seen.contains(&Focus::Ops));
        // App::new starts focused on Sessions. The ring is four panels deep:
        // the status bar is chrome, not a panel — it draws nothing, owns no
        // rows and has nothing to activate, so a fifth stop would park focus
        // somewhere invisible.
        assert_eq!(
            seen,
            vec![
                Focus::Sessions,
                Focus::Tasks,
                Focus::Ops,
                Focus::Workspaces,
                Focus::Sessions,
            ]
        );
        // Shift+Tab walks the same ring the other way and closes on itself.
        let mut back = Vec::new();
        a.focus = Focus::Sessions;
        for _ in 0..4 {
            a.cycle_focus_back();
            back.push(a.focus);
        }
        assert_eq!(
            back,
            vec![Focus::Workspaces, Focus::Ops, Focus::Tasks, Focus::Sessions,],
            "Shift+Tab must be the reverse lap of Tab"
        );
        for (i, tab) in [
            OpsTab::Timeline,
            OpsTab::Skills,
            OpsTab::Shield,
            OpsTab::Recovery,
            OpsTab::Rooms,
            OpsTab::Extensions,
        ]
        .into_iter()
        .enumerate()
        {
            a.set_ops_tab(tab);
            assert_eq!(a.ops_tab.index(), i);
            assert_eq!(OpsTab::from_index(i), tab);
        }
    }
}

#[cfg(test)]
mod p11_tui_tests {
    use super::*;
    use openrt_models::MockProvider;
    use openrt_persistence::MemoryStore;

    async fn app() -> App {
        let rt = openrt_runtime::RuntimeBuilder::new()
            .config(openrt_config::Config::default())
            .store(Arc::new(MemoryStore::new()))
            .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
            .bus_capacity(32)
            .build()
            .await
            .unwrap();
        App::new(rt).await.unwrap()
    }

    /// M11.6: the Rooms and Extensions sub-views mirror the ONE store —
    /// rows come from `list_rooms` / `extension_states`, never UI-local.
    #[tokio::test]
    async fn rooms_and_extensions_tabs_read_real_state() {
        let mut a = app().await;
        a.create_workspace("acct/rooms", "Rooms ws", "m116")
            .await
            .unwrap();
        a.runtime
            .create_room("acct/rooms", "Standup", "operator")
            .await
            .unwrap();
        a.refresh().await.unwrap();
        assert_eq!(a.ops_rooms.len(), 1);
        assert_eq!(a.ops_rooms[0].title, "Standup");
        assert_eq!(a.ops_rooms[0].workbench, "acct/rooms");
        assert_eq!(a.ops_rooms[0].status, "active");
        assert!(a.ops_rooms[0].active_workflow.is_none());

        // Extensions mirror the registry exactly (honest empty on a fresh
        // store — imported extensions never arrive Active).
        let expected = a.runtime.extension_states().await.unwrap();
        assert_eq!(a.ops_extensions.len(), expected.len());

        a.set_ops_tab(OpsTab::Rooms);
        assert_eq!(a.ops_tab, OpsTab::Rooms);
        assert!(a.status.contains("Ops · Rooms"));
        a.set_ops_tab(OpsTab::Extensions);
        assert_eq!(a.ops_tab, OpsTab::Extensions);
    }

    /// M11.6: pause/resume/steer act THROUGH the one Runtime — a room with
    /// no live run fails closed on the status line, and a steer directive
    /// appends exactly once (append-only, same path as the CLI).
    #[tokio::test]
    async fn rooms_pause_resume_steer_go_through_runtime() {
        let mut a = app().await;
        a.create_workspace("acct/keys", "Keys ws", "m116k")
            .await
            .unwrap();
        let room = a
            .runtime
            .create_room("acct/keys", "Ops", "operator")
            .await
            .unwrap();
        a.refresh().await.unwrap();
        assert_eq!(a.sel_room, Some(0));

        // No active workflow → pause/resume refuse (fail-closed messaging).
        a.room_pause().await;
        assert_eq!(a.status, "no active workflow on the selected room");
        a.room_resume().await;
        assert_eq!(a.status, "no active workflow on the selected room");

        // Point the room at a workflow id that has no live run: the TUI
        // surfaces the REAL runtime error (it never fabricates success).
        a.runtime
            .set_room_workflow(&room.id, Some("wf-tui-none"), "operator")
            .await
            .unwrap();
        a.refresh().await.unwrap();
        a.room_pause().await;
        assert!(
            a.status.starts_with("pause failed:") && a.status.contains("no active run"),
            "must route through runtime.workflow_pause: {}",
            a.status
        );
        a.room_resume().await;
        assert!(
            a.status.starts_with("resume failed:"),
            "must route through runtime.workflow_resume: {}",
            a.status
        );

        // Steer: begin → type → submit appends exactly one directive.
        a.room_steer_begin();
        assert_eq!(a.steer_input.as_deref(), Some(""));
        for c in "prefer summaries".chars() {
            if let Some(buf) = a.steer_input.as_mut() {
                buf.push(c);
            }
        }
        a.room_steer_submit().await;
        assert!(a.steer_input.is_none());
        assert!(a.status.starts_with("steer appended at"), "{}", a.status);
        let room_after = a.runtime.room(&room.id).await.unwrap();
        assert_eq!(room_after.steer.len(), 1);
        assert_eq!(room_after.steer[0].input, "prefer summaries");
        assert_eq!(room_after.steer[0].operator, "tui");

        // Empty steer appends nothing (fail-closed).
        a.room_steer_begin();
        a.room_steer_submit().await;
        assert_eq!(a.status, "empty steer — nothing appended");
        let room_after = a.runtime.room(&room.id).await.unwrap();
        assert_eq!(room_after.steer.len(), 1, "append-only, nothing added");

        // The refresh reflects the appended directive (view-only state).
        a.refresh().await.unwrap();
        assert_eq!(a.ops_rooms[0].steer, 1);
    }
}

#[cfg(test)]
mod p12_tui_tests {
    use super::*;
    use openrt_models::MockProvider;
    use openrt_persistence::MemoryStore;

    async fn app() -> App {
        let rt = openrt_runtime::RuntimeBuilder::new()
            .config(openrt_config::Config::default())
            .store(Arc::new(MemoryStore::new()))
            .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
            .bus_capacity(32)
            .build()
            .await
            .unwrap();
        App::new(rt).await.unwrap()
    }

    /// Phase 12 ops sub-view roundtrips like every other Ops tab.
    #[tokio::test]
    async fn intel_tab_roundtrip() {
        let mut a = app().await;
        a.set_ops_tab(OpsTab::Intel);
        assert_eq!(a.ops_tab, OpsTab::Intel);
        assert_eq!(a.ops_tab.index(), 7);
        assert_eq!(OpsTab::from_index(7), OpsTab::Intel);
        assert_eq!(a.ops_tab.title(), "Ops · Phase 12");
        assert!(a.status.contains("Phase 12"), "status: {}", a.status);
    }

    /// Phase 12 rows are real store reads — counts change when the store
    /// changes, and the research-only refusal is stated honestly.
    #[tokio::test]
    async fn intel_rows_reflect_real_store_state() {
        let mut a = app().await;
        a.refresh_ops().await.unwrap();
        let get = |rows: &[IntelRow], label: &str| -> String {
            rows.iter()
                .find(|r| r.label == label)
                .map(|r| r.value.clone())
                .unwrap_or_else(|| panic!("missing row `{label}`"))
        };
        assert_eq!(get(&a.ops_intel, "industrial assets"), "0");
        assert_eq!(get(&a.ops_intel, "trading datasets"), "0");
        assert_eq!(
            get(&a.ops_intel, "trading execute"),
            "DENIED (research only)"
        );
        assert!(get(&a.ops_intel, "phase12 doctor").contains("pass"));
        assert_eq!(get(&a.ops_intel, "model cache entries"), "0");

        // A real store write moves the count — no UI-local bookkeeping.
        let asset = openrt_industrial::Asset::new(
            "pump.1",
            openrt_industrial::AssetKind::Equipment,
            "Feed pump",
            openrt_industrial::Provenance::derived("test", openrt_core::now().to_rfc3339()),
        );
        a.runtime.industrial_asset_put(asset, "op1").await.unwrap();
        a.refresh_ops().await.unwrap();
        assert_eq!(get(&a.ops_intel, "industrial assets"), "1");
    }

    /// Phase 12 doctor rows surfaced into the TUI: no FAIL ever counts as
    /// pass, and every row carries non-empty detail (rendered by the UI).
    #[tokio::test]
    async fn intel_doctor_rows_have_details() {
        let a = app().await;
        let checks = a.runtime.phase12_doctor_checks().await;
        assert!(
            checks.len() >= 15,
            "expected phase12 doctor rows, got {}",
            checks.len()
        );
        for c in &checks {
            assert_ne!(c.status.as_str(), "FAIL", "{}", c.name);
            assert!(!c.detail.is_empty(), "{} needs a detail", c.name);
        }
    }
}

#[cfg(test)]
mod interaction_tests {
    use super::*;
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use openrt_models::MockProvider;
    use openrt_persistence::MemoryStore;
    use ratatui::Terminal;

    async fn app() -> App {
        let rt = openrt_runtime::RuntimeBuilder::new()
            .config(openrt_config::Config::default())
            .store(Arc::new(MemoryStore::new()))
            .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
            .bus_capacity(32)
            .build()
            .await
            .unwrap();
        App::new(rt).await.unwrap()
    }

    /// Draw one Workbench frame so the hit registry reflects real geometry.
    ///
    /// Clicking without a drawn frame would test nothing: the registry is
    /// populated by the renderer, so an un-drawn app has no regions and every
    /// assertion below would pass for the wrong reason.
    async fn drawn(width: u16, height: u16) -> App {
        let mut a = app().await;
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut a)).unwrap();
        a
    }

    fn down(x: u16, y: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    fn up(x: u16, y: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: x,
            row: y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    fn moved(x: u16, y: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Moved,
            column: x,
            row: y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    /// Every panel's frame rect must be registered.
    ///
    /// A panel that renders but does not register is a dead zone: the operator
    /// clicks it and nothing happens, with no way to tell that apart from a bug.
    #[tokio::test]
    async fn a_frame_registers_every_panel() {
        let a = drawn(160, 50).await;
        let hits = a.hits.borrow();
        for panel in PanelId::ALL {
            let registered = hits
                .all()
                .iter()
                .any(|t| matches!(t, HitTarget::WorkbenchPanel { panel: p } if *p == panel));
            assert!(registered, "{panel:?} rendered but registered no region");
        }
    }

    #[tokio::test]
    async fn a_press_alone_does_not_activate() {
        let mut a = drawn(120, 40).await;
        a.focus = Focus::Workspaces;
        // Aim at the Tasks panel: pressing must arm, not move focus.
        let target = a.hits.borrow().hit(95, 20);
        assert!(
            matches!(target, HitTarget::WorkbenchPanel { .. }),
            "expected a panel region at that point, got {target:?}"
        );
        a.handle_mouse(down(95, 20)).await.unwrap();
        assert_eq!(a.focus, Focus::Workspaces, "press must not act");
        assert!(a.pressed.is_some(), "press must arm");
    }

    #[tokio::test]
    async fn a_release_on_the_armed_target_activates() {
        let mut a = drawn(120, 40).await;
        a.handle_mouse(down(95, 20)).await.unwrap();
        a.handle_mouse(up(95, 20)).await.unwrap();
        assert!(a.pressed.is_none(), "release must consume the armed press");
    }

    #[tokio::test]
    async fn a_release_elsewhere_does_not_activate_the_armed_target() {
        let mut a = drawn(120, 40).await;
        a.focus = Focus::Workspaces;
        // Arm on the Tasks panel, release on Workspaces: nothing may activate.
        a.handle_mouse(down(95, 20)).await.unwrap();
        a.handle_mouse(up(20, 20)).await.unwrap();
        assert_eq!(
            a.focus,
            Focus::Workspaces,
            "a release off-target must not activate the armed target"
        );
    }

    #[tokio::test]
    async fn a_release_without_a_press_never_activates() {
        let mut a = drawn(120, 40).await;
        a.focus = Focus::Workspaces;
        a.handle_mouse(up(95, 20)).await.unwrap();
        assert_eq!(a.focus, Focus::Workspaces, "up-only must not activate");
    }

    #[tokio::test]
    async fn dragging_off_the_target_cancels_the_press() {
        let mut a = drawn(120, 40).await;
        a.focus = Focus::Workspaces;
        a.handle_mouse(down(95, 20)).await.unwrap();
        a.handle_mouse(moved(20, 20)).await.unwrap();
        assert!(
            a.pressed.is_none(),
            "leaving the armed target must cancel the press"
        );
        // And the later release back over the original target must stay inert.
        a.handle_mouse(up(95, 20)).await.unwrap();
        assert_eq!(a.focus, Focus::Workspaces);
    }

    #[tokio::test]
    async fn a_drag_that_stays_inside_the_target_keeps_the_press_armed() {
        let mut a = drawn(120, 40).await;
        a.handle_mouse(down(95, 20)).await.unwrap();
        // Same panel, different row: still the same target for a panel frame.
        a.handle_mouse(moved(96, 21)).await.unwrap();
        assert!(
            a.pressed.is_some(),
            "a drag within the armed target must keep the press"
        );
        a.handle_mouse(up(96, 21)).await.unwrap();
        assert!(a.pressed.is_none());
    }

    #[tokio::test]
    async fn the_right_button_never_arms_or_activates() {
        let mut a = drawn(120, 40).await;
        a.focus = Focus::Workspaces;
        for kind in [
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Up(MouseButton::Right),
            MouseEventKind::Down(MouseButton::Middle),
            MouseEventKind::Up(MouseButton::Middle),
        ] {
            a.handle_mouse(MouseEvent {
                kind,
                column: 95,
                row: 20,
                modifiers: crossterm::event::KeyModifiers::NONE,
            })
            .await
            .unwrap();
            assert!(a.pressed.is_none(), "{kind:?} must not arm a press");
        }
        assert_eq!(a.focus, Focus::Workspaces, "right-click must not act");
    }

    #[tokio::test]
    async fn a_scroll_cancels_an_armed_press() {
        let mut a = drawn(120, 40).await;
        a.handle_mouse(down(95, 20)).await.unwrap();
        a.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 95,
            row: 20,
            modifiers: crossterm::event::KeyModifiers::NONE,
        })
        .await
        .unwrap();
        assert!(
            a.pressed.is_none(),
            "the content under the pointer is about to move; a stale press must not survive it"
        );
    }

    #[tokio::test]
    async fn a_double_click_is_two_ordinary_clicks() {
        let mut a = drawn(120, 40).await;
        a.handle_mouse(down(95, 20)).await.unwrap();
        a.handle_mouse(up(95, 20)).await.unwrap();
        a.handle_mouse(down(95, 20)).await.unwrap();
        a.handle_mouse(up(95, 20)).await.unwrap();
        // No special double-click path exists, so two clicks means two clicks.
        assert!(a.pressed.is_none());
    }

    #[tokio::test]
    async fn a_click_on_empty_space_activates_nothing() {
        let mut a = drawn(120, 40).await;
        a.focus = Focus::Workspaces;
        // Far below every panel, in a region no registry entry covers.
        a.hits.borrow_mut().clear();
        a.handle_mouse(down(0, 39)).await.unwrap();
        a.handle_mouse(up(0, 39)).await.unwrap();
        assert_eq!(a.focus, Focus::Workspaces);
        assert!(a.pressed.is_none());
    }

    #[tokio::test]
    async fn a_mode_transition_drops_every_region_and_press() {
        let mut a = drawn(120, 40).await;
        a.handle_mouse(down(95, 20)).await.unwrap();
        assert!(a.pressed.is_some());
        a.clear_interaction();
        assert!(a.pressed.is_none(), "the armed press must not survive");
        assert_eq!(
            a.hits.borrow().hit(95, 20),
            HitTarget::None,
            "a stale registry would resolve clicks against the old surface"
        );
    }

    #[tokio::test]
    async fn every_acceptance_size_stays_interactive() {
        for (w, h) in [(80u16, 24u16), (100, 30), (120, 40), (160, 50), (180, 50)] {
            let a = drawn(w, h).await;
            let hits = a.hits.borrow();
            assert!(!hits.all().is_empty(), "{w}x{h}: nothing registered");
            // Assert on the registry rather than a probe point: (0,0) is the
            // header at every size, so "hit(0,0) is not None" would be a test
            // of the header, not of the panels.
            for panel in PanelId::ALL {
                assert!(
                    hits.all().iter().any(
                        |t| matches!(t, HitTarget::WorkbenchPanel { panel: p } if *p == panel)
                    ),
                    "{w}x{h}: {panel:?} rendered but registered no region"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_degenerate_size_panics_neither_on_draw_nor_on_click() {
        for (w, h) in [(1u16, 1u16), (3, 1), (10, 2), (20, 3)] {
            let mut a = drawn(w, h).await;
            a.handle_mouse(down(0, 0)).await.unwrap();
            a.handle_mouse(up(0, 0)).await.unwrap();
        }
    }

    #[tokio::test]
    async fn tab_cycles_the_focus_ring_and_returns_home() {
        let mut a = app().await;
        let start = a.focus;
        // Four panels, so four Tab presses is one lap. The old count was five,
        // which passed on a five-stop ring and would have failed on a four-stop
        // one — the assertion was tied to a number nobody restated when the
        // phantom status stop was removed.
        for _ in 0..4 {
            a.cycle_focus();
        }
        assert_eq!(a.focus, start, "the focus ring must be closed");

        // One lap backward lands in the same place, so Shift+Tab is not stuck.
        for _ in 0..4 {
            a.cycle_focus_back();
        }
        assert_eq!(a.focus, start, "the backward ring must be closed too");

        // Both directions visit all four panels and no fifth one.
        let mut forward = Vec::new();
        for _ in 0..4 {
            a.cycle_focus();
            forward.push(a.focus);
        }
        let mut backward = Vec::new();
        for _ in 0..4 {
            a.cycle_focus_back();
            backward.push(a.focus);
        }
        for ring in [&forward, &backward] {
            let mut sorted = ring.clone();
            sorted.sort_by_key(|f| match f {
                Focus::Workspaces => 0,
                Focus::Sessions => 1,
                Focus::Tasks => 2,
                Focus::Ops => 3,
            });
            sorted.dedup();
            assert_eq!(sorted.len(), 4, "a lap must visit four panels: {ring:?}");
        }
    }

    #[tokio::test]
    async fn a_panel_click_moves_focus_without_selecting_a_row() {
        let mut a = drawn(160, 50).await;
        // Find a panel frame region away from any row: use the panel's own
        // rect via a hit on its border row.
        //
        // The match is asserted rather than guarded with `if let`: guarded, the
        // test passed whenever the probe missed, which is the most likely way
        // for it to fail — a layout change moving the panel it was aiming at.
        let panel = a.hits.borrow().hit(0, 39).clone();
        let HitTarget::WorkbenchPanel { panel } = panel else {
            panic!("expected a panel frame at (0,39), got {panel:?}");
        };
        let before = *a.sel();
        a.handle_mouse(down(0, 39)).await.unwrap();
        a.handle_mouse(up(0, 39)).await.unwrap();
        assert_eq!(a.focus, Focus::from(panel));
        assert_eq!(
            *a.sel(),
            before,
            "clicking a frame focuses; it must not also select a row"
        );
    }
}

/// Phase 5: Workbench-as-first-class-surface invariants.
#[cfg(test)]
mod p5_workbench_tests {
    use super::*;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use openrt_models::MockProvider;
    use openrt_persistence::MemoryStore;
    use ratatui::Terminal;

    async fn app() -> App {
        let rt = openrt_runtime::RuntimeBuilder::new()
            .config(openrt_config::Config::default())
            .store(Arc::new(MemoryStore::new()))
            .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
            .bus_capacity(32)
            .build()
            .await
            .unwrap();
        App::new(rt).await.unwrap()
    }

    async fn drawn(w: u16, h: u16) -> App {
        let mut a = app().await;
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut a)).unwrap();
        a
    }

    fn down(x: u16, y: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn up(x: u16, y: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// How many selectable rows each panel really has.
    fn real_rows(a: &App, panel: PanelId) -> usize {
        match panel {
            PanelId::Workspaces => a.workspaces.len(),
            PanelId::Sessions => a.sessions.len(),
            PanelId::Tasks => a.tasks.len(),
            PanelId::Ops => match a.ops_tab {
                OpsTab::Rooms => a.ops_rooms.len(),
                _ => 0,
            },
        }
    }

    /// The first point in the frame that resolves to this exact target.
    ///
    /// Locating the point instead of hard-coding one keeps the test honest when
    /// the layout moves: a guessed coordinate that lands on empty space resolves
    /// to `None` and the test would otherwise assert on the wrong target.
    fn point_for(a: &App, want: &HitTarget) -> Option<(u16, u16)> {
        let w = 160u16;
        let h = 50u16;
        (0..h).find_map(|y| {
            (0..w)
                .find(|x| a.hits.borrow().hit(*x, y) == *want)
                .map(|x| (x, y))
        })
    }

    /// A Workbench with rows that the runtime will actually accept, because a
    /// click on a Sessions row *activates* it and activation switches sessions —
    /// so fabricated ids fail the very switch the click is meant to trigger.
    async fn live(w: u16, h: u16) -> App {
        let mut a = app().await;
        a.create_workspace("acct/keys", "Keys ws", "m116k")
            .await
            .unwrap();
        for i in 0..3 {
            a.create_session(&format!("tui-{i}")).await.unwrap();
        }
        a.runtime
            .create_room("acct/keys", "Standup", "operator")
            .await
            .unwrap();
        a.ops_tab = OpsTab::Rooms;
        a.focus = Focus::Workspaces;
        a.reconcile_selection();
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut a)).unwrap();
        a
    }

    #[tokio::test]
    async fn a_row_click_resolves_to_that_row_and_not_to_the_panel_frame() {
        let mut a = live(160, 50).await;
        a.focus = Focus::Workspaces;

        let want = HitTarget::WorkbenchRow {
            panel: PanelId::Workspaces,
            index: 0,
        };
        let (x, y) = point_for(&a, &want).expect("a populated panel must register row 0");

        // The row sits inside the panel rect, so both regions contain this
        // point and only the registration order decides which one wins.
        assert_eq!(
            a.hits.borrow().hit(x, y),
            want,
            "a click inside a row must resolve to the row, not the panel frame"
        );

        a.handle_mouse(down(x, y)).await.unwrap();
        a.handle_mouse(up(x, y)).await.unwrap();
        assert_eq!(a.focus, Focus::Workspaces);
        assert_eq!(a.sel_workspace, Some(0), "the click selects that row");
    }

    #[tokio::test]
    async fn no_panel_ever_registers_a_row_it_does_not_have() {
        // Every acceptance size, audited from the frame's own registry rather
        // than by probing coordinates: a region somewhere the test did not
        // think to click is exactly what a point probe misses.
        for (w, h) in [(80u16, 24u16), (100, 30), (120, 40), (160, 50), (180, 50)] {
            for (name, a) in [
                ("empty", drawn(w, h).await),
                ("populated", populated(w, h).await),
            ] {
                for target in a.hits.borrow().all() {
                    if let HitTarget::WorkbenchRow { panel, index } = target {
                        let real = real_rows(&a, panel);
                        assert!(
                            index < real,
                            "{name} {w}x{h}: {panel:?} registered row {index} of {real}"
                        );
                    }
                }
            }
        }
    }

    async fn populated(w: u16, h: u16) -> App {
        let mut a = app().await;
        for i in 0..3 {
            a.workspaces.push(WorkspaceRow {
                address_code: format!("acct/ws{i}"),
                title: format!("Workspace {i}"),
                description: String::new(),
                active: i == 0,
            });
        }
        for i in 0..4 {
            a.sessions.push(SessionRow {
                id: openrt_core::SessionId::new(),
                title: format!("Session {i}"),
                status: "idle".into(),
                messages: i,
                active: i == 0,
            });
        }
        for i in 0..5 {
            a.tasks.push(TaskRow {
                id: format!("task-{i}"),
                prompt: format!("task {i}"),
                status: "running".into(),
                turns: i,
            });
        }
        for i in 0..3 {
            a.ops_rooms.push(RoomRow {
                id: format!("room-{i}"),
                title: format!("Room {i}"),
                workbench: "acct/ws0".into(),
                status: "idle".into(),
                agents: i,
                materials: 0,
                steer: 0,
                active_workflow: None,
            });
        }
        a.ops_tab = OpsTab::Rooms;
        a.sel_workspace = Some(0);
        a.sel_session = Some(0);
        a.sel_task = Some(0);
        a.sel_room = Some(0);
        a.load_workspaces.ok();
        a.load_sessions.ok();
        a.load_tasks.ok();
        a.load_ops.ok();
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut a)).unwrap();
        a
    }

    #[tokio::test]
    async fn an_empty_panel_has_no_selection_and_arrow_keys_cannot_invent_one() {
        let mut a = app().await;
        assert_eq!(a.sel_session, None, "a fresh app selects nothing");
        a.focus = Focus::Sessions;
        assert_eq!(a.step_selection(1), None);
        assert_eq!(a.sel_session, None, "Down on an empty list must not select");
        a.step_selection(-1);
        assert_eq!(a.sel_session, None, "Up on an empty list must not select");
        assert_eq!(a.selectable_rows(), 0);

        // Once rows exist, selection is real and bounded.
        for i in 0..2 {
            a.sessions.push(SessionRow {
                id: openrt_core::SessionId::new(),
                title: format!("Session {i}"),
                status: "idle".into(),
                messages: 0,
                active: false,
            });
        }
        assert_eq!(a.step_selection(1), Some(0));
        assert_eq!(a.step_selection(1), Some(1));
        assert_eq!(a.step_selection(1), Some(0), "wraps forward");
        assert_eq!(a.step_selection(-1), Some(1), "wraps backward");
    }

    #[tokio::test]
    async fn a_selection_that_outlives_its_row_is_reconciled_not_left_dangling() {
        let mut a = app().await;
        for i in 0..3 {
            a.sessions.push(SessionRow {
                id: openrt_core::SessionId::new(),
                title: format!("Session {i}"),
                status: "idle".into(),
                messages: 0,
                active: false,
            });
        }
        a.focus = Focus::Sessions;
        a.sel_session = Some(2);
        a.sessions.truncate(1);
        a.reconcile_selection();
        assert_eq!(a.sel_session, Some(0), "clamped to the last surviving row");

        a.sessions.clear();
        a.reconcile_selection();
        assert_eq!(a.sel_session, None, "no rows means no selection at all");
    }

    #[tokio::test]
    async fn ops_tab_keys_move_focus_to_ops_so_the_switch_is_visible() {
        let mut a = app().await;
        a.focus = Focus::Workspaces;
        crate::run::workbench_key(&mut a, key(KeyCode::Char('5')))
            .await
            .unwrap();
        assert_eq!(a.ops_tab, OpsTab::Rooms);
        assert_eq!(
            a.focus,
            Focus::Ops,
            "switching the sub-view of an unfocused panel is invisible"
        );
        assert!(
            a.status.contains("Ops"),
            "the status line names the new view: {}",
            a.status
        );
    }

    #[tokio::test]
    async fn every_panel_reports_its_own_load_state_and_a_failure_does_not_hide_the_others() {
        let mut a = app().await;
        // A fresh app has read from the store, so every panel is loaded — even
        // the empty ones, which is the whole point of tracking `loaded`.
        assert!(a.load_workspaces.loaded);
        assert!(a.load_sessions.loaded);
        assert!(a.load_tasks.loaded);
        assert!(a.load_ops.loaded);
        assert!(a.load_sessions.failed.is_none());

        a.load_sessions.failed = Some("store read failed".into());
        let state = crate::design::states::for_collection(
            a.load_sessions.loaded,
            a.load_sessions.failed.as_deref(),
            a.sessions.len(),
            "sessions",
        );
        assert_eq!(
            state.kind,
            crate::design::states::StateKind::Error,
            "a failed read must not render as 'no sessions yet'"
        );
        let state = crate::design::states::for_collection(false, None, 0, "sessions");
        assert_eq!(
            state.kind,
            crate::design::states::StateKind::Loading,
            "a panel that has never read must not render as empty"
        );
    }

    #[tokio::test]
    async fn the_header_states_the_mode_once_and_names_the_real_context() {
        let a = app().await;
        let (posture, tone) = a.mode_badge();
        assert_eq!(posture, "SOVEREIGN");
        let line = a.header_line();
        assert!(
            !line.contains("mode=sovereign"),
            "the badge already carries the posture: {line}"
        );
        assert!(
            line.contains("Ctrl+W agent"),
            "the way out is named: {line}"
        );
        assert_ne!(tone, crate::design::widgets::BadgeTone::Success);

        // Meters are counts of what is actually loaded, never estimates.
        let meters = a.header_meters();
        let names: Vec<&str> = meters.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, ["ws", "sess", "tasks", "ops"]);
        for (name, value) in &meters {
            assert_eq!(
                value,
                &real_rows(
                    &a,
                    PanelId::ALL[meters.iter().position(|(n, _)| n == name).unwrap()]
                )
                .to_string()
            );
        }
    }

    #[tokio::test]
    async fn status_tone_states_meaning_and_leaves_unknown_words_neutral() {
        let mut a = app().await;
        use crate::design::widgets::BadgeTone as T;
        for (text, want) in [
            ("refresh failed", T::Danger),
            ("permission denied", T::Danger),
            ("steer cancelled", T::Warning),
            ("session paused", T::Warning),
            ("awaiting approval", T::Pending),
            ("ready", T::Neutral),
            ("tui-7 opened", T::Neutral),
        ] {
            a.status = text.into();
            assert_eq!(a.status_tone(), want, "{text:?} must read as {want:?}");
        }
    }

    /// The rendered text of one frame, for assertions about what an operator can
    /// actually read rather than about internal state.
    fn frame_text(a: &mut App, w: u16, h: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &a)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[tokio::test]
    async fn escape_is_contextual_and_never_quits_the_app() {
        let mut a = live(120, 40).await;
        a.status = "ready".into();

        // Nothing layered to unwind: say so, do not quit.
        crate::run::workbench_key(&mut a, key(KeyCode::Esc))
            .await
            .unwrap();
        assert_eq!(a.status, "nothing to cancel");
        assert!(!a.should_quit, "Esc must never quit a live session");

        // A steer line is a real layer, and Esc unwinds it and nothing else.
        a.refresh_ops().await.unwrap();
        a.focus = Focus::Ops;
        a.set_ops_tab(OpsTab::Rooms);
        assert_eq!(a.sel_room, Some(0), "precondition: a room is selected");
        a.room_steer_begin();
        assert!(a.steer_input.is_some(), "precondition: steering");
        crate::run::workbench_key(&mut a, key(KeyCode::Esc))
            .await
            .unwrap();
        assert_eq!(a.steer_input, None, "Esc cancels the buffer it opened");
        assert!(
            a.status.contains("nothing appended"),
            "and says the directive was not sent: {}",
            a.status
        );
    }

    #[tokio::test]
    async fn enter_on_an_empty_panel_explains_itself_and_runs_nothing() {
        let mut a = app().await;
        a.focus = Focus::Sessions;
        a.status = "ready".into();
        crate::run::workbench_key(&mut a, key(KeyCode::Enter))
            .await
            .unwrap();
        assert!(
            !a.should_quit,
            "Enter on an empty panel must not run or exit anything"
        );
        assert!(
            a.status.contains("no session selected") || a.status.contains("nothing"),
            "an empty activation must say why it did nothing: {}",
            a.status
        );
    }

    #[tokio::test]
    async fn only_the_left_button_can_arm_or_activate_anything() {
        let mut a = live(160, 50).await;
        let want = HitTarget::WorkbenchRow {
            panel: PanelId::Workspaces,
            index: 0,
        };
        let (x, y) = point_for(&a, &want).expect("a populated panel must register row 0");
        a.focus = Focus::Tasks;
        let before = a.focus;

        for button in [MouseButton::Right, MouseButton::Middle] {
            a.handle_mouse(MouseEvent {
                kind: MouseEventKind::Down(button),
                column: x,
                row: y,
                modifiers: KeyModifiers::NONE,
            })
            .await
            .unwrap();
            a.handle_mouse(MouseEvent {
                kind: MouseEventKind::Up(button),
                column: x,
                row: y,
                modifiers: KeyModifiers::NONE,
            })
            .await
            .unwrap();
            assert!(a.pressed.is_none(), "{button:?} must not arm anything");
            assert_eq!(a.focus, before, "{button:?} must not move focus");
        }
    }

    #[tokio::test]
    async fn a_wheel_scroll_cannot_leave_a_press_armed_against_moved_content() {
        let mut a = live(160, 50).await;
        let want = HitTarget::WorkbenchRow {
            panel: PanelId::Workspaces,
            index: 0,
        };
        let (x, y) = point_for(&a, &want).unwrap();
        a.handle_mouse(down(x, y)).await.unwrap();
        assert!(a.pressed.is_some(), "precondition: armed");
        a.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        })
        .await
        .unwrap();
        assert!(
            a.pressed.is_none(),
            "the row under the pointer is about to move; the arm must not survive"
        );
        // And the release that follows must activate nothing.
        let status = a.status.clone();
        a.handle_mouse(up(x, y)).await.unwrap();
        assert_eq!(
            a.status, status,
            "a release after a scroll activates nothing"
        );
    }

    #[tokio::test]
    async fn regions_never_outlive_the_frame_they_were_measured_in() {
        let mut a = live(160, 50).await;
        let (w, h) = (160u16, 50u16);
        // Shrink hard, then grow again. Every frame rebuilds the registry, so a
        // region from a wider viewport must not survive into a narrower one —
        // that is what lets a click activate a row that is no longer on screen.
        for (nw, nh) in [(40u16, 10u16), (80, 24), (180, 50), (30, 6)] {
            let text = frame_text(&mut a, nw, nh);
            for target in a.hits.borrow().all() {
                let panel = match target {
                    HitTarget::WorkbenchPanel { panel } => Some(panel),
                    HitTarget::WorkbenchRow { panel, .. } => Some(panel),
                    _ => None,
                };
                assert!(
                    panel.is_some(),
                    "Workbench must register only its own targets, got {target:?}"
                );
            }
            // Whatever survived must be inside this frame.
            for (rect, target) in a.hits.borrow().iter_regions() {
                assert!(
                    rect.x + rect.width <= nw && rect.y + rect.height <= nh,
                    "{target:?} at {rect:?} escapes the {nw}x{nh} frame"
                );
            }
            assert!(text.contains("OpenRT Workbench"), "{nw}x{nh}");
        }
        assert_eq!(w, 160, "sanity: the original size");
        assert_eq!(h, 50);
    }

    #[tokio::test]
    async fn the_focused_panel_says_so_in_words_not_only_in_colour() {
        let mut a = populated(120, 40).await;
        a.focus = Focus::Tasks;
        let text = frame_text(&mut a, 120, 40);
        assert!(
            text.contains("▸ Tasks"),
            "the focused panel must be named in text: {text}"
        );

        // And it moves when focus moves.
        a.focus = Focus::Workspaces;
        let text = frame_text(&mut a, 120, 40);
        assert!(text.contains("▸ Workspaces"));
        assert!(
            !text.contains("▸ Tasks"),
            "only one panel may claim focus: {text}"
        );
    }

    #[tokio::test]
    async fn every_panel_state_says_something_different_on_screen() {
        let mut a = app().await;
        // Loading: read has not happened.
        a.load_sessions = Default::default();
        let text = frame_text(&mut a, 100, 30);
        assert!(text.contains("LOADING"), "an unread panel says so: {text}");

        // Empty: read happened, nothing there.
        a.load_sessions.ok();
        let text = frame_text(&mut a, 100, 30);
        assert!(text.contains("no sessions yet"), "{text}");
        assert!(!text.contains("LOADING"), "empty is not loading: {text}");

        // Failed: the read did not happen, which is not the same as empty.
        a.load_sessions.failed = Some("store read failed".into());
        let text = frame_text(&mut a, 100, 30);
        // Title and body use one vocabulary, so the same word appears in both
        // places rather than the operator learning that two spellings mean one
        // thing.
        assert!(
            text.contains("FAILED"),
            "the title names the failure: {text}"
        );
        assert!(
            !text.contains("no sessions yet"),
            "a failure must never read as emptiness: {text}"
        );
        assert!(text.contains("store read failed"), "and shows why: {text}");
    }

    #[tokio::test]
    async fn panels_never_overlap_and_never_collapse_at_any_acceptance_size() {
        for (w, h) in [(80u16, 24u16), (100, 30), (120, 40), (160, 50), (180, 50)] {
            let a = live(w, h).await;
            let regions: Vec<_> = a
                .hits
                .borrow()
                .iter_regions()
                .filter(|(_, t)| matches!(t, HitTarget::WorkbenchPanel { .. }))
                .map(|(r, t)| (*r, t.clone()))
                .collect();
            assert_eq!(regions.len(), 4, "{w}x{h}: four panel regions");
            for (i, (ri, ti)) in regions.iter().enumerate() {
                assert!(
                    ri.width >= 8 && ri.height >= 3,
                    "{w}x{h}: {ti:?} is too small to read: {ri:?}"
                );
                for (rj, tj) in regions.iter().skip(i + 1) {
                    let overlap = ri.x < rj.x + rj.width
                        && rj.x < ri.x + ri.width
                        && ri.y < rj.y + rj.height
                        && rj.y < ri.y + ri.height;
                    assert!(!overlap, "{w}x{h}: {ti:?} overlaps {tj:?}");
                }
            }
        }
    }

    #[tokio::test]
    async fn a_row_stays_inside_its_own_panel() {
        // A row target that escaped its panel would let a click on one panel
        // select a row of another — two surfaces, two meanings for "row 3".
        for (w, h) in [(80u16, 24u16), (100, 30), (160, 50)] {
            let a = live(w, h).await;
            let regions = a
                .hits
                .borrow()
                .iter_regions()
                .map(|(r, t)| (*r, t.clone()))
                .collect::<Vec<_>>();
            let panel_of = |p: PanelId| {
                regions
                    .iter()
                    .find(|(_, t)| *t == HitTarget::WorkbenchPanel { panel: p })
                    .map(|(r, _)| *r)
                    .unwrap_or_else(|| panic!("no panel region for {p:?} at {w}x{h}"))
            };
            for (rect, target) in &regions {
                let HitTarget::WorkbenchRow { panel, .. } = target else {
                    continue;
                };
                let panel = panel_of(*panel);
                assert!(
                    rect.x >= panel.x
                        && rect.x + rect.width <= panel.x + panel.width
                        && rect.y >= panel.y
                        && rect.y + rect.height <= panel.y + panel.height,
                    "{w}x{h}: row {rect:?} escapes its panel {panel:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn every_theme_draws_every_panel_without_losing_the_grid() {
        let mut a = populated(120, 40).await;
        for kind in crate::theme::ThemeKind::ALL {
            let name = kind.name();
            a.theme.set(kind);
            let backend = ratatui::backend::TestBackend::new(120, 40);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| crate::ui::draw(f, &mut a)).unwrap();
            let buf = terminal.backend().buffer().clone();
            let text: String = buf.content().iter().map(|c| c.symbol()).collect();
            for panel in ["Workspaces", "Sessions", "Tasks", "Ops"] {
                assert!(
                    text.contains(panel),
                    "{name}: the {panel} panel title must render"
                );
            }
            // Geometry survives the switch: the registry still has one panel
            // region per panel.
            let panels: std::collections::HashSet<_> = a
                .hits
                .borrow()
                .all()
                .into_iter()
                .filter_map(|t| match t {
                    HitTarget::WorkbenchPanel { panel } => Some(panel),
                    _ => None,
                })
                .collect();
            assert_eq!(panels.len(), 4, "{name}: all four panels must register");
        }
    }
}
