//! Computer-use tool belt (Phase 2, M2.3 + M2.5).
//!
//! Filesystem `write`/`list`/`move`/`delete`, a `terminal` tool, and the M2.5
//! surface (screen capture, deterministic vision grounding, mouse, keyboard
//! and window-level browser tools, plus the computer-use loop). The
//! filesystem/terminal tools are jail-scoped to an operator-configured root
//! (SEC-01, the same boundary the Phase 1 `filesystem.read` tool uses), and
//! every tool is permission-gated by the runtime.
//!
//! Every tool follows the fail-closed philosophy of
//! [`openrt_tools::ToolRegistry::with_filesystem_root`]: an instance built
//! without a root (e.g. a runtime with no `workspace_dir`) denies **all**
//! filesystem/terminal invocations. The screen/input/browser tools default to
//! the deterministic [`SimScreen`] backend and are deny-by-default in policy.
//! The tools never bypass the gate — they enforce the path/scope boundary
//! again in the tool layer so a correctly-policied setup cannot escape its
//! root through a directory traversal, a symlink, or a sibling absolute path.

mod computeruse;
mod filesystem;
mod loop_;
mod screen;
mod terminal;

use std::path::Path;
use std::sync::Arc;

use openrt_safety::KillSwitch;

pub use computeruse::{
    computer_use_spec, Bounds, ChainStep, ComputerAction, ComputerUse, ComputerUseError,
    ComputerUseReport, ComputerUseTool, DigitalTwin, ObservationBundle, Plan, TargetResolution,
    TargetSource, VisionGrounding, VlmGrounding, MAX_ATTEMPTS,
};
pub use filesystem::{
    FilesystemDelete, FilesystemList, FilesystemMove, FilesystemWrite, DEFAULT_LIST_LIMIT,
    DEFAULT_MAX_WRITE_BYTES,
};
pub use loop_::{ComputerLoopReport, ComputerUseLoop, ToolCall};
pub use screen::{
    have_x11_tooling, screen_tools, BrowserActivateTool, BrowserListTool, InputKeyboardTool,
    InputMouseTool, ScreenBackend, ScreenCapture, ScreenCaptureTool, ScreenElement,
    ScreenGroundTool, SimScreen, X11Screen,
};
pub use terminal::{
    Terminal, TerminalLimits, DEFAULT_MAX_COMMAND_BYTES, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_TIMEOUT,
};

/// Resolve the configured screen backend: `"x11"` opts into the real-display
/// backend (only when X tooling is present), everything else stays on the
/// deterministic [`SimScreen`] harness so behavior never windows on host X
/// availability.
pub fn default_screen_backend(kind: Option<&str>) -> Arc<dyn ScreenBackend> {
    match kind {
        Some("x11") if have_x11_tooling() => Arc::new(X11Screen::default()),
        _ => Arc::new(SimScreen::new(1280, 720, vec![])),
    }
}

/// The M2.3 tool set scoped to `root`. Pass `None` to register the tools in
/// deny-all (fail-closed) mode. `terminal_limits` overrides the terminal's
/// defaults when present; `kill`, when provided, lets the terminal abort a
/// running command the moment the computer-use kill switch engages.
pub fn computer_tools(
    root: Option<&Path>,
    terminal_limits: Option<TerminalLimits>,
    kill: Option<Arc<KillSwitch>>,
) -> Vec<Arc<dyn openrt_tools::Tool>> {
    let mut terminal = Terminal::scoped_or_deny(root);
    if let Some(limits) = terminal_limits {
        terminal.limits = limits;
    }
    terminal.kill = kill;
    vec![
        Arc::new(FilesystemWrite::scoped_or_deny(root)),
        Arc::new(FilesystemList::scoped_or_deny(root)),
        Arc::new(FilesystemMove::scoped_or_deny(root)),
        Arc::new(FilesystemDelete::scoped_or_deny(root)),
        Arc::new(terminal),
    ]
}

/// The full M2.3 + M2.5 tool set: filesystem/terminal plus the screen/input/
/// browser tools over the given backend.
pub fn computer_tools_with_screen(
    root: Option<&Path>,
    terminal_limits: Option<TerminalLimits>,
    kill: Option<Arc<KillSwitch>>,
    screen: Arc<dyn ScreenBackend>,
) -> Vec<Arc<dyn openrt_tools::Tool>> {
    let mut all = computer_tools(root, terminal_limits, kill);
    all.extend(screen_tools(screen));
    all
}
