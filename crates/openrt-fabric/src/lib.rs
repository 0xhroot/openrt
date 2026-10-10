//! Phase 7B Tool Fabric core (M7B.1).
//!
//! One metadata/discovery/error/artifact layer shared by every 7B subsystem.
//! It **enriches** the existing `ToolRegistry`/`GateExecutor` — it is never a
//! second registry, permission system, or execution authority:
//!
//! * [`meta`] — authoritative tool metadata (`FabricMeta`) + `tools.list`.
//! * [`artifacts`] — generated-artifact records over the one store (ADR-006).
//! * [`error`] — normalized error codes with WHAT/WHY/WHERE/FIX.
//! * [`net_guard`] — one network-policy entry point (modes + netpol).
//! * [`adapters`] — application-adapter framework + `application.*` tools.

pub mod adapters;
pub mod artifacts;
pub mod error;
pub mod meta;
pub mod net_guard;

use std::sync::Arc;

pub use adapters::{application_tools, ApplicationAdapter, ApplicationRegistry};
pub use artifacts::{artifact_list_tool, ArtifactRecord, ArtifactStore, ARTIFACT_NS};
pub use error::{fail, ErrorCode, FabricError};
pub use meta::{FabricCatalog, FabricMeta, RiskLevel, SupportState, ToolCategory, ToolsListTool};

use openrt_persistence::PersistenceStore;

/// Shared fabric state owned by the runtime: metadata catalog + artifact
/// store. Subsystem managers (browser, MCP, database) hold their own handles.
pub struct FabricState {
    pub catalog: Arc<FabricCatalog>,
    pub artifacts: Arc<ArtifactStore>,
    pub applications: std::sync::Mutex<ApplicationRegistry>,
}

impl FabricState {
    pub fn new(store: Arc<dyn PersistenceStore>) -> Arc<Self> {
        Arc::new(Self {
            catalog: Arc::new(FabricCatalog::new()),
            artifacts: Arc::new(ArtifactStore::new(store)),
            applications: std::sync::Mutex::new(ApplicationRegistry::new()),
        })
    }

    pub fn register_adapter(&self, adapter: ApplicationAdapter) {
        self.applications
            .lock()
            .expect("application registry poisoned")
            .register(adapter);
    }

    pub fn applications(&self) -> Arc<ApplicationRegistry> {
        Arc::new(ApplicationRegistry::from_vec(
            self.applications
                .lock()
                .expect("application registry poisoned")
                .list()
                .to_vec(),
        ))
    }
}

/// Metadata for the frozen Phase 7A `computer.*` tools (declared here so the
/// catalog covers every registered tool without touching the frozen crate).
pub fn computer_metas(host: &str) -> Vec<FabricMeta> {
    use RiskLevel::*;
    use SupportState::*;
    use ToolCategory::*;
    vec![
        FabricMeta::new("computer.env", Discovery, Low)
            .with_platform(host, Green)
            .observe()
            .verify()
            .side_effects(&["reads environment config (read-only)"]),
        FabricMeta::new("computer.window", Computer, Medium)
            .with_platform(host, Partial)
            .observe()
            .verify()
            .side_effects(&["focus/close windows"])
            .rollback("refocus previous window (manual)"),
        FabricMeta::new("computer.screen", Computer, Medium)
            .with_platform(host, Green)
            .observe()
            .verify()
            .side_effects(&["captures screen content"])
            .rollback("none (capture is read-only)"),
        FabricMeta::new("computer.input", Computer, High)
            .with_platform(host, Partial)
            .observe()
            .verify()
            .side_effects(&["synthesizes keyboard/pointer input"])
            .rollback("none (input cannot be unsent)"),
        FabricMeta::new("computer.clipboard", Computer, Medium)
            .with_platform(host, Green)
            .observe()
            .side_effects(&["reads/writes system clipboard"])
            .rollback("restore previous clipboard (manual)"),
        FabricMeta::new("computer.app", Application, Medium)
            .with_platform(host, Green)
            .observe()
            .verify()
            .side_effects(&["launches processes"])
            .rollback("close launched application"),
        // Phase 11 M11.8: the universal computer-use planner/executor.
        FabricMeta::new("computer.use", Computer, High)
            .with_platform(host, Partial)
            .observe()
            .verify()
            .side_effects(&[
                "synthesizes focus/keyboard/pointer/window/workspace/clipboard \
                             input through the single computer gate",
            ])
            .rollback("none (input cannot be unsent); refocus/deselect manually"),
        // Phase 2 window-level browser tools (owned by openrt-computer).
        FabricMeta::new("browser.list", Browser, Low)
            .with_platform(host, Partial)
            .observe()
            .side_effects(&["none (window listing)"]),
        FabricMeta::new("browser.activate", Browser, Medium)
            .with_platform(host, Partial)
            .verify()
            .side_effects(&["activates a browser window"])
            .rollback("refocus previous window (manual)"),
    ]
}

/// Metadata for the discovery/utility tools this crate registers.
pub fn fabric_metas(host: &str) -> Vec<FabricMeta> {
    use RiskLevel::*;
    use SupportState::*;
    use ToolCategory::*;
    vec![
        FabricMeta::new("tools.list", Discovery, Low)
            .with_platform(host, Green)
            .side_effects(&["none (read-only discovery)"]),
        FabricMeta::new("artifact.list", Discovery, Low)
            .with_platform(host, Green)
            .observe()
            .verify()
            .side_effects(&["none (read-only); verify recomputes digests)"]),
        FabricMeta::new("application.list", Application, Low)
            .with_platform(host, Green)
            .side_effects(&["none (read-only)"]),
        FabricMeta::new("application.verify", Application, Low)
            .with_platform(host, Partial)
            .observe()
            .verify()
            .side_effects(&["reads desktop entries and window tree (read-only)"]),
    ]
}
