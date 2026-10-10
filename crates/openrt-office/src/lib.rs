//! Phase 7B office automation: real XLSX/PPTX/DOCX files in pure Rust
//! (deterministic stored ZIP + minimal OOXML) plus render/export to PDF/PNG
//! through a real LibreOffice backend when installed.
//!
//! Design (mission decisions):
//! - create/modify/read are fully implemented here (no GUI macros, no fake
//!   success); written files are self-verified by reading them back.
//! - `ppt.render` / `*.export_pdf` shell out to `soffice --headless` with an
//!   isolated per-call user profile; when LibreOffice is absent the tools
//!   fail closed with `UNAVAILABLE` (meta `SupportState::Partial`).
//! - Every create/write registers an artifact record (sha256 + provenance)
//!   in the `fabric.artifacts` store; render/export record the source digest
//!   so derived files are traceable back to their input.
//! - Render/export use the source file's read cap (`artifact.*.read`) because
//!   they never modify the source; all mutating operations use `artifact.*.write`
//!   and refuse silent overwrite of existing files.

pub mod docx;
pub mod pptx;
pub mod soffice;
pub mod tools;
pub mod xlsx;
pub mod xml;
pub mod zip;

pub use docx::{Block, Document};
pub use pptx::{Deck, Slide};
pub use soffice::Sofice;
pub use tools::{office_metas, office_tools, OfficeCtx};
pub use xlsx::{Sheet, Workbook};

use std::path::Path;
use std::sync::Arc;

/// Resolve the render backend once at wiring time: configured override or
/// PATH probe, with the failure reason kept for structured `UNAVAILABLE`
/// errors at call time.
pub fn detect_soffice(override_bin: Option<&Path>) -> (Option<Sofice>, String) {
    match Sofice::detect(override_bin) {
        Ok(s) => (Some(s), String::new()),
        Err(e) => (None, format!("{} — {}", e.what, e.why)),
    }
}

/// Build the shared office context for tool registration.
pub fn office_ctx(
    artifacts: Arc<openrt_fabric::artifacts::ArtifactStore>,
    override_bin: Option<&Path>,
) -> OfficeCtx {
    let (soffice, soffice_reason) = detect_soffice(override_bin);
    OfficeCtx {
        artifacts,
        soffice,
        soffice_reason,
    }
}
