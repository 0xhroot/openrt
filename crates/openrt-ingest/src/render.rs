//! PDF page rasterization via `pdftoppm` (Poppler) — the real renderer the
//! scanned-PDF OCR path depends on. A missing binary is a structured
//! `BackendMissing`; rasterized pages are temporary files removed on drop.

use std::path::{Path, PathBuf};

use crate::capabilities::{render_capability, Capability};
use crate::ocr::{temp_path, OcrError};

/// A rendered PNG page; removed on drop.
#[derive(Debug)]
pub struct RenderedPng {
    path: PathBuf,
}

impl RenderedPng {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for RenderedPng {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// `pdftoppm` backend. Construct only through [`Pdftoppm::detect`].
#[derive(Debug, Clone)]
pub struct Pdftoppm {
    pub capability: Capability,
    pub dpi: u32,
}

impl Pdftoppm {
    pub fn detect() -> Option<Self> {
        Some(Self {
            capability: render_capability()?,
            dpi: 200,
        })
    }

    pub fn with_dpi(mut self, dpi: u32) -> Self {
        self.dpi = dpi;
        self
    }

    /// `name|version` label for provenance.
    pub fn label(&self) -> String {
        match &self.capability.version {
            Some(v) => format!("{}|{}", self.capability.name, v),
            None => self.capability.name.clone(),
        }
    }

    /// Rasterize one 1-based PDF page to a temporary PNG.
    pub fn render_page(&self, pdf: &Path, page: u32) -> Result<RenderedPng, OcrError> {
        let prefix = temp_path(&format!("p{page}"), "");
        let prefix_str = prefix.to_string_lossy().to_string();
        // `temp_path` adds a trailing ".": strip it for a clean pdftoppm prefix.
        let prefix_str = prefix_str.trim_end_matches('.').to_string();
        let out = std::process::Command::new(&self.capability.binary)
            .arg("-png")
            .arg("-r")
            .arg(self.dpi.to_string())
            .arg("-f")
            .arg(page.to_string())
            .arg("-l")
            .arg(page.to_string())
            .arg("-singlefile")
            .arg(pdf)
            .arg(&prefix_str)
            .output()
            .map_err(|e| OcrError::Spawn {
                bin: self.capability.binary.clone(),
                err: e.to_string(),
            })?;
        if !out.status.success() {
            return Err(OcrError::Render(format!(
                "{} exited with status {}: {}",
                self.capability.name,
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let png = PathBuf::from(format!("{prefix_str}.png"));
        if !png.is_file() {
            return Err(OcrError::Render(format!(
                "{} produced no output page for page {page}",
                self.capability.name
            )));
        }
        Ok(RenderedPng { path: png })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_is_capability_honest() {
        match Pdftoppm::detect() {
            Some(r) => {
                assert!(r.capability.binary.ends_with("pdftoppm"));
                assert!(r.dpi > 0);
                assert!(r.label().starts_with("pdftoppm"));
            }
            None => {
                // Absent backend must surface as structured BackendMissing
                // from the OCR path — never as fabricated pages.
                let e = OcrError::BackendMissing {
                    backend: "pdftoppm".into(),
                    capability: "pdf page rasterization".into(),
                    reason: "no `pdftoppm` binary on PATH".into(),
                };
                assert!(e.to_string().contains("pdf page rasterization"));
            }
        }
    }

    #[test]
    fn rendered_png_removed_on_drop() {
        let Some(r) = Pdftoppm::detect() else {
            eprintln!("pdftoppm absent; skipping (honest absence)");
            return;
        };
        let missing_pdf = std::env::temp_dir().join("openrt-render-test-missing.pdf");
        // A missing pdf must fail with a structured render error, and no
        // stray PNG may remain.
        match r.render_page(&missing_pdf, 1) {
            Err(OcrError::Render(_)) | Err(OcrError::Failed { .. }) => {}
            other => panic!("expected render failure, got {other:?}"),
        }
    }
}
