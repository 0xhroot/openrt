//! `openrt-tui` — Phase 5, M5.1 **TUI** (persistent workspace + session UX).
//!
//! This is a **UI adapter** over the wired [`openrt_runtime::Runtime`]: it
//! never opens a second store, never keeps a parallel session history, and
//! never fabricates state. Every list/create/switch/run the user sees is a
//! real call into `Runtime` (or the single foundation store behind it).
//!
//! Surfaces (ROADMAP M5.1: "Persistent workspace + session UX in TUI"):
//!
//! * workspace create / open / reopen (via `openrt-workspace` over `rt.store`)
//! * session create / list / switch / resume (via `Runtime::{create,list}_sessions`)
//! * current-session display + task/run visibility (`rt.store.list_tasks`)
//! * operating-mode status (ADR-003 `ModeManager`)
//! * exit (clean; restores the terminal)
//!
//! The controller ([`App`]) is fully testable without a TTY: production-path
//! tests drive it against a real `Runtime` + `MemoryStore` + mock provider and
//! assert the store round-trips (never hardcoded UI strings).

pub mod agent;
pub mod app;
pub mod design;
pub mod run;
pub mod theme;
pub mod ui;

pub use agent::{run_agent, AgentApp, SwitchableProvider};
pub use app::{
    App, AppError, ComputerRow, ExtensionRow, Focus, OpsTab, RoomRow, SessionRow, TaskRow,
    WorkspaceRow,
};
pub use run::run_tui;
