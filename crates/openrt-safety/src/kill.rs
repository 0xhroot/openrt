//! Kill-scope classification and the kill-switch latch (ADR-008).

use std::sync::{Arc, RwLock};

/// The safety scopes a HALT may target (ADR-008). Each subsystem subscribes
/// only to its own scope; a `global` operator abort (Phase 6) fans out
/// per-scope but each subsystem still runs its own mitigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KillScope {
    /// Computer-use (mouse/keyboard/terminal/browser/python), Phase 2.
    Computer,
    /// Trading order placement / capital actions (ADR-008). Separate from
    /// `Computer`: a trading HALT never stops computer tools and vice versa.
    /// Scoped tools are registered by the optional trading subsystem; none
    /// exist until that subsystem ships (scope reserved, optionality per
    /// ROADMAP §8).
    Trading,
}

impl KillScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Computer => "computer",
            Self::Trading => "trading",
        }
    }
}

/// Classify a tool id into the kill scope it belongs to (or `None`).
///
/// This is the scoping-discipline guard from ADR-008: a HALT may only ever
/// stop tools whose scope it targets. Anything unlisted (e.g. `echo`,
/// `filesystem.read`) is invisible to a computer-scope HALT.
pub fn tool_scope(tool: &str) -> Option<KillScope> {
    match tool {
        "terminal.run" | "python.run" | "input.mouse" | "input.keyboard" | "screen.capture"
        | "screen.ground" => Some(KillScope::Computer),
        // P7 Tool Fabric: every `computer.*` platform tool is computer-scoped
        // (ADR-008 — scopes never cross; a computer HALT halts these too).
        t if t.starts_with("computer.") => Some(KillScope::Computer),
        // M7B: the whole `browser.*` family (window-level 7A tools + the
        // in-page 7B backend tools) plus every side-effecting fabric tool.
        // Read-only discovery (`tools.*`, `artifact.*`) stays unscoped: a
        // computer HALT must not blind the operator's inspection surface.
        t if t.starts_with("browser.")
            || t.starts_with("database.")
            || t.starts_with("excel.")
            || t.starts_with("ppt.")
            || t.starts_with("docx.")
            || t.starts_with("mcp.")
            || t.starts_with("application.") =>
        {
            Some(KillScope::Computer)
        }
        t if t.starts_with("trading.") => Some(KillScope::Trading),
        _ => None,
    }
}

/// An operator-only, tamper-resistant kill-switch latch.
///
/// `trip()` latches the switch open with an immutable reason; there is no way
/// for a tool or agent to arm, disarm, or clear it (the runtime's operator
/// surface owns construction). `reset()` exists for recovery after an
/// operator-verified all-clear and for tests.
#[derive(Debug, Clone, Default)]
pub struct KillSwitch {
    inner: Arc<RwLock<Option<String>>>,
}

impl KillSwitch {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(None)),
        }
    }

    /// Latch the switch open. Returns `true` if this call tripped it (first
    /// tripper), `false` if it was already tripped (reason preserved).
    pub fn trip(&self, reason: impl Into<String>) -> bool {
        let mut g = self.inner.write().unwrap_or_else(|p| p.into_inner());
        if g.is_none() {
            *g = Some(reason.into());
            true
        } else {
            false
        }
    }

    /// Whether the switch is engaged; the stored reason when engaged.
    pub fn engaged(&self) -> Option<String> {
        self.inner.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Clear the latch (operator recovery only). Returns whether it was
    /// engaged before clearing.
    pub fn reset(&self) -> bool {
        let mut g = self.inner.write().unwrap_or_else(|p| p.into_inner());
        g.take().is_some()
    }
}

/// Wait until the computer-use kill switch engages (or return immediately if
/// it already is), yielding the operator's halt reason. Long-running tools
/// select on this so a HALT aborts them mid-action (ADR-008).
pub async fn halt_reason(kill: Option<Arc<KillSwitch>>) -> String {
    loop {
        if let Some(k) = &kill {
            if let Some(reason) = k.engaged() {
                return reason;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_scope_classifies_computer_tools() {
        for tool in [
            "terminal.run",
            "python.run",
            "input.mouse",
            "input.keyboard",
            "screen.capture",
            "screen.ground",
            "browser.list",
            "browser.activate",
            "computer.env",
            "computer.window",
            "computer.screen",
            "computer.input",
            "computer.clipboard",
            "computer.app",
            "browser.navigate",
            "browser.screenshot",
            "database.query",
            "excel.create",
            "ppt.render",
            "docx.read",
            "mcp.servers",
            "mcp.discover",
            "application.verify",
        ] {
            assert_eq!(tool_scope(tool), Some(KillScope::Computer), "{tool}");
        }
        for tool in ["echo", "filesystem.read", "filesystem.write", "system.info"] {
            assert_eq!(tool_scope(tool), None, "{tool}");
        }
    }

    #[test]
    fn tool_scope_classifies_trading_tools_separately() {
        for tool in [
            "trading.order.place",
            "trading.order.cancel",
            "trading.position.close",
        ] {
            assert_eq!(tool_scope(tool), Some(KillScope::Trading), "{tool}");
        }
        // Computer tools stay computer-scoped (ADR-008: scopes never cross).
        assert_eq!(tool_scope("terminal.run"), Some(KillScope::Computer));
        assert_eq!(KillScope::Trading.as_str(), "trading");
        assert_eq!(KillScope::Computer.as_str(), "computer");
    }

    #[test]
    fn trip_latches_with_reason() {
        let k = KillSwitch::new();
        assert_eq!(k.engaged(), None);
        assert!(k.trip("operator emergency"));
        assert_eq!(k.engaged().as_deref(), Some("operator emergency"));
        // Second tripper does not overwrite the reason.
        assert!(!k.trip("other"));
        assert_eq!(k.engaged().as_deref(), Some("operator emergency"));
    }

    #[test]
    fn reset_clears_and_reports_previous_state() {
        let k = KillSwitch::new();
        assert!(!k.reset());
        assert!(k.trip("done"));
        assert!(k.reset());
        assert_eq!(k.engaged(), None);
        assert!(k.trip("again"));
    }

    #[test]
    fn clones_share_the_latch() {
        let a = KillSwitch::new();
        let b = a.clone();
        assert!(a.trip("shared"));
        assert_eq!(b.engaged().as_deref(), Some("shared"));
    }

    #[tokio::test]
    async fn halt_reason_returns_on_engage() {
        let kill = KillSwitch::new();
        let k2 = kill.clone();
        let handle = tokio::spawn(async move { halt_reason(Some(Arc::new(k2))).await });
        tokio::task::yield_now().await;
        kill.trip("stop now");
        assert_eq!(handle.await.unwrap(), "stop now");
    }
}
