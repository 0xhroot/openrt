//! Real document-render/export backend: LibreOffice (`soffice`) in headless
//! mode. Used for `ppt.render` / `*.export_pdf`. When LibreOffice is absent,
//! callers get a structured UNAVAILABLE error — never a fake success.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use openrt_fabric::FabricError;

const CONVERT_TIMEOUT: Duration = Duration::from_secs(90);

/// Resolved `soffice` binary.
#[derive(Debug, Clone)]
pub struct Sofice {
    pub bin: PathBuf,
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

impl Sofice {
    /// Probe for a real LibreOffice binary (`soffice` override or PATH).
    pub fn detect(override_bin: Option<&Path>) -> Result<Self, FabricError> {
        let bin = match override_bin {
            Some(p) if p.is_file() => p.to_path_buf(),
            Some(p) => {
                return Err(FabricError::unavailable(
                    "office render backend",
                    format!("configured soffice path `{}` does not exist", p.display()),
                    "office.render/export",
                    "fix fabric.office.soffice or remove the override to probe PATH",
                ))
            }
            None => which("soffice").or_else(|| which("libreoffice")).ok_or_else(|| {
                FabricError::unavailable(
                    "office render backend",
                    "no `soffice`/`libreoffice` binary found on PATH",
                    "office.render/export",
                    "install LibreOffice (only then can ppt.render/ppt.export_pdf run); create/read still work without it",
                )
            })?,
        };
        // Real probe: `soffice --version` must succeed.
        let out = Command::new(&bin)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| {
                FabricError::unavailable(
                    "office render backend",
                    format!("cannot execute `{}`: {e}", bin.display()),
                    "office.render/export",
                    "install/repair LibreOffice or update fabric.office.soffice",
                )
            })?;
        if !out.status.success() {
            return Err(FabricError::unavailable(
                "office render backend",
                format!(
                    "`{} --version` failed: {}",
                    bin.display(),
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
                "office.render/export",
                "repair the LibreOffice installation",
            ));
        }
        Ok(Self { bin })
    }

    pub fn version(&self) -> String {
        Command::new(&self.bin)
            .arg("--version")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "unknown".into())
    }

    /// Convert `input` to `format` (`pdf`, `png`, …) inside `out_dir`.
    /// Returns the produced file path. Uses an isolated per-call user profile
    /// so a concurrent desktop LibreOffice instance is never disturbed.
    pub fn convert(
        &self,
        input: &Path,
        out_dir: &Path,
        format: &str,
    ) -> Result<PathBuf, FabricError> {
        if !input.is_file() {
            return Err(FabricError::not_found(
                "office input file",
                format!("`{}` does not exist", input.display()),
                "pass an existing file path",
            ));
        }
        std::fs::create_dir_all(out_dir).map_err(|e| {
            FabricError::backend_failure(
                "office render backend",
                format!("cannot create out dir `{}`: {e}", out_dir.display()),
                "office.render/export",
                "use a writable output directory",
            )
        })?;
        let profile = std::env::temp_dir().join(format!("openrt-lo-{}", uuid::Uuid::new_v4()));
        let profile_url = format!("-env:UserInstallation=file://{}", profile.display());
        let mut child = Command::new(&self.bin)
            .arg("--headless")
            .arg(&profile_url)
            .arg("--convert-to")
            .arg(format)
            .arg("--outdir")
            .arg(out_dir)
            .arg(input)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                FabricError::backend_failure(
                    "office render backend",
                    format!("spawn soffice: {e}"),
                    "office.render/export",
                    "check LibreOffice installation",
                )
            })?;
        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if start.elapsed() > CONVERT_TIMEOUT => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = std::fs::remove_dir_all(&profile);
                    return Err(FabricError::timeout(
                        "office conversion",
                        format!("soffice exceeded {CONVERT_TIMEOUT:?}"),
                        "office.render/export",
                        "simplify the document or raise the timeout; retry",
                    ));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = std::fs::remove_dir_all(&profile);
                    return Err(FabricError::backend_failure(
                        "office render backend",
                        format!("wait soffice: {e}"),
                        "office.render/export",
                        "retry; check system health",
                    ));
                }
            }
        };
        let _ = std::fs::remove_dir_all(&profile);
        let stem = input
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let produced = out_dir.join(format!("{stem}.{format}"));
        if !status.success() {
            return Err(FabricError::backend_failure(
                "office render backend",
                format!(
                    "soffice --convert-to {format} failed for `{}`",
                    input.display()
                ),
                "office.render/export",
                "open the file manually to inspect it; the document may be malformed",
            ));
        }
        if !produced.is_file() {
            return Err(FabricError::verification_failed(
                "office conversion output",
                format!("expected `{}` was not produced", produced.display()),
                "check soffice filters; try another output format",
            ));
        }
        let size = std::fs::metadata(&produced).map(|m| m.len()).unwrap_or(0);
        if size == 0 {
            return Err(FabricError::verification_failed(
                "office conversion output",
                format!("`{}` is empty", produced.display()),
                "the conversion produced no content; inspect the source file",
            ));
        }
        Ok(produced)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_reports_honestly() {
        // Real host probe: either a working soffice or structured UNAVAILABLE.
        match Sofice::detect(None) {
            Ok(s) => {
                assert!(s.version().to_ascii_lowercase().contains("libreoffice"));
            }
            Err(e) => {
                assert_eq!(e.code, openrt_fabric::ErrorCode::Unavailable);
            }
        }
    }

    #[test]
    fn missing_override_is_unavailable() {
        let err = Sofice::detect(Some(Path::new("/nonexistent/soffice"))).unwrap_err();
        assert_eq!(err.code, openrt_fabric::ErrorCode::Unavailable);
    }
}
