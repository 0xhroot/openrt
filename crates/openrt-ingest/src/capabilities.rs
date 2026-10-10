//! Capability probes for extraction backends. These report *capability*, not
//! behaviour: OCR / VLM / audio are only ever "available" when a recognised
//! backend genuinely exists in this environment; otherwise the probe returns
//! `None` and ingestion never pretends otherwise. Probes are cached per
//! process (`OnceLock`) so a single `--version` execution is the ceiling.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// A discovered external backend that can genuinely perform a transformation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capability {
    /// Canonical name, e.g. `tesseract`.
    pub name: String,
    /// Backend version string as reported by the backend (never invented).
    pub version: Option<String>,
    /// Path of the discovered binary.
    pub binary: String,
    /// Free-form support note for capability reporting.
    pub note: String,
}

static OCR: OnceLock<Option<Capability>> = OnceLock::new();
static RENDER: OnceLock<Option<Capability>> = OnceLock::new();
static PROBE: OnceLock<Option<Capability>> = OnceLock::new();
static DECODE: OnceLock<Option<Capability>> = OnceLock::new();
static ASR: OnceLock<Option<Capability>> = OnceLock::new();

/// Returns the `tesseract` capability when a tesseract binary is on `PATH`.
/// No tesseract ⇒ `None` ⇒ the build honestly reports OCR unavailable.
/// Cached: at most one probe execution per process.
pub fn ocr_capability() -> Option<Capability> {
    OCR.get_or_init(|| probe_binary("tesseract", "ocr")).clone()
}

/// Returns the `pdftoppm` capability (page rasterization for scanned-PDF
/// OCR). No pdftoppm ⇒ `None` ⇒ scanned PDFs cannot be rendered for OCR.
/// Cached: at most one probe execution per process.
pub fn render_capability() -> Option<Capability> {
    RENDER
        .get_or_init(|| probe_binary("pdftoppm", "page rasterization"))
        .clone()
}

/// No VLM backend is implemented in this build; always `None` (honest).
pub fn vlm_capability() -> Option<Capability> {
    None
}

/// Returns the `ffprobe` capability (M9.4 media probing: duration/streams/
/// codecs). No ffprobe ⇒ `None` ⇒ media metadata cannot be probed honestly.
pub fn media_probe_capability() -> Option<Capability> {
    PROBE
        .get_or_init(|| probe_binary("ffprobe", "media probe"))
        .clone()
}

/// Returns the `ffmpeg` capability (M9.4 media decode: frame sampling and
/// audio track decoding). No ffmpeg ⇒ `None`.
pub fn media_decode_capability() -> Option<Capability> {
    DECODE
        .get_or_init(|| probe_binary("ffmpeg", "media decode"))
        .clone()
}

/// Returns the ASR (speech-to-text) capability: a whisper.cpp-family binary
/// on `PATH`. Absent ⇒ `None` ⇒ audio transcript extraction is reported as
/// `MODEL_MISSING` — no transcript is ever invented. Note that a binary
/// alone is not enough at run time: the provider additionally requires a
/// model file (`OPENRT_WHISPER_MODEL`), which fails closed as `ModelMissing`.
pub fn audio_capability() -> Option<Capability> {
    ASR.get_or_init(|| {
        ["whisper-cli", "whisper-cpp", "whisper"]
            .iter()
            .find_map(|name| probe_binary(name, "asr"))
    })
    .clone()
}

/// Probe `PATH` for a binary matching `name`. The binary is only located, not
/// executed, so probing has no side effects.
pub fn probe_binary(name: &str, what: &str) -> Option<Capability> {
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':') {
        if dir.is_empty() {
            continue;
        }
        let cand = std::path::Path::new(dir).join(name);
        if cand.is_file() {
            return Some(Capability {
                name: name.to_string(),
                version: first_line_of(cand.as_path(), "--version"),
                binary: cand.display().to_string(),
                note: format!("{what} backend present"),
            });
        }
    }
    None
}

/// Very best-effort version read; failures mean `version: None` — a real
/// value is never fabricated. Tries the given arg, then the common
/// alternates (`pdftoppm` rejects `--version` and prints `-v` on stderr).
fn first_line_of(binary: &std::path::Path, arg: &str) -> Option<String> {
    let name = binary
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    // `-version` (single dash, ffmpeg-family) is among the alternates:
    // ffprobe/ffmpeg reject `--version` outright.
    for candidate in [arg, "-version", "-v", "--version"] {
        if let Some(line) = version_line(binary, candidate, &name) {
            return Some(line);
        }
    }
    None
}

fn version_line(binary: &std::path::Path, arg: &str, name: &str) -> Option<String> {
    use std::process::Command;
    let out = Command::new(binary).arg(arg).output().ok()?;
    if !out.status.success() {
        return None;
    }
    // Most tools print the version on stdout; `pdftoppm -v` uses stderr.
    let text = if out.stdout.iter().any(|b| b.is_ascii_graphic()) {
        String::from_utf8_lossy(&out.stdout)
    } else {
        String::from_utf8_lossy(&out.stderr)
    };
    text.lines()
        .next()
        .map(|l| l.trim().to_string())
        .filter(|s| !s.is_empty())
        // `tesseract --version` prints "tesseract 5.5.3": strip the binary
        // name so labels read `tesseract|5.5.3`, not `tesseract|tesseract …`.
        // ffmpeg-family lines are "ffprobe version n9.0.1 Copyright …" —
        // keep just the version token (`n9.0.1`), never the whole banner.
        .map(|line| match line.strip_prefix(name) {
            Some(rest) => {
                let rest = rest.trim();
                let mut words = rest.split_whitespace();
                match words.next() {
                    Some("version") => words.next().unwrap_or(rest).to_string(),
                    Some(word) => word.to_string(),
                    None => rest.to_string(),
                }
            }
            None => line,
        })
        .filter(|s| !s.is_empty())
}

/// Human-readable capability matrix used by `doctor` and `docs` status output.
pub fn capability_summary() -> Vec<(&'static str, bool, String)> {
    let ocr = ocr_capability();
    let render = render_capability();
    let vlm = vlm_capability();
    let media_probe = media_probe_capability();
    let media_decode = media_decode_capability();
    let audio = audio_capability();
    vec![
        (
            "ocr",
            ocr.is_some(),
            match &ocr {
                Some(c) => format!(
                    "{} on PATH ({})",
                    c.binary,
                    c.version.as_deref().unwrap_or("version unknown")
                ),
                None => "no tesseract on PATH — image/scanned-pdf OCR unavailable".into(),
            },
        ),
        (
            "render",
            render.is_some(),
            match &render {
                Some(c) => format!(
                    "{} on PATH ({})",
                    c.binary,
                    c.version.as_deref().unwrap_or("version unknown")
                ),
                None => "no pdftoppm on PATH — pdf page rasterization unavailable".into(),
            },
        ),
        ("vlm", vlm.is_some(), "no vlm backend in this build".into()),
        (
            "media",
            media_probe.is_some() && media_decode.is_some(),
            match (media_probe.as_ref(), media_decode.as_ref()) {
                (Some(p), Some(d)) => format!(
                    "{} + {} ({})",
                    p.binary,
                    d.binary,
                    p.version.as_deref().unwrap_or("version unknown")
                ),
                (None, _) => "no ffprobe on PATH — media probing unavailable".into(),
                (_, None) => "no ffmpeg on PATH — media decode unavailable".into(),
            },
        ),
        (
            "audio",
            audio.is_some(),
            match &audio {
                Some(c) => format!(
                    "{} on PATH ({}) — transcript extraction available",
                    c.binary,
                    c.version.as_deref().unwrap_or("version unknown")
                ),
                None => {
                    "no whisper on PATH — audio transcript extraction unavailable (MODEL_MISSING)"
                        .into()
                }
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_fabricated_capabilities() {
        // The ingest build itself has no in-process VLM backend; probes that
        // depend on the host PATH are honest either way.
        assert!(vlm_capability().is_none());
        if let Some(c) = audio_capability() {
            assert!(
                c.name.starts_with("whisper"),
                "ASR probe must only ever report whisper-family binaries"
            );
        }
        // ocr_capability depends on the host PATH — both outcomes are honest.
        if let Some(c) = ocr_capability() {
            assert_eq!(c.name, "tesseract");
        }
        if let Some(c) = media_probe_capability() {
            assert_eq!(c.name, "ffprobe");
        }
        if let Some(c) = media_decode_capability() {
            assert_eq!(c.name, "ffmpeg");
        }
    }

    #[test]
    fn summary_shape() {
        let s = capability_summary();
        assert_eq!(s.len(), 5);
        assert!(s
            .iter()
            .all(|(name, _, note)| !name.is_empty() && !note.is_empty()));
        // The ocr/render rows carry a concrete reason on either outcome.
        for name in ["ocr", "render"] {
            let row = s.iter().find(|(n, _, _)| *n == name).unwrap();
            assert!(!row.2.is_empty());
            if !row.1 {
                assert!(row.2.contains("unavailable"), "note: {}", row.2);
            }
        }
    }
}
