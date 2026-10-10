//! Content-based format detection (magic bytes first; declared extension is a
//! hint, never a claim) with an honest intake status per format family:
//! `Supported` / `Unsupported` / `Corrupt` / `Ambiguous`.

use serde::{Deserialize, Serialize};

use crate::normalize::ExtractMethod;

/// Detected container/format family for ingestion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormatKind {
    Text,
    Markdown,
    Csv,
    Json,
    Pdf,
    Png,
    Jpeg,
    /// TIFF image (multi-page containers are OCR'd page by page).
    Tiff,
    Html,
    /// OOXML word-processing document (zip container, `word/document.xml`).
    Docx,
    /// OOXML spreadsheet (zip container, `xl/workbook.xml`).
    Xlsx,
    /// OOXML presentation (zip container, `ppt/presentation.xml`).
    Pptx,
    /// Recognised audio container (RIFF/WAVE, MP3, Ogg, FLAC, M4A, …).
    Audio,
    /// Recognised video container (MP4/AVI/Matroska/WebM, …).
    Video,
    /// Recognised as unparseable by this build (unknown zip, opaque binary).
    Other,
}

impl FormatKind {
    pub fn label(&self) -> &'static str {
        match self {
            FormatKind::Text => "text/plain",
            FormatKind::Markdown => "text/markdown",
            FormatKind::Csv => "text/csv",
            FormatKind::Json => "application/json",
            FormatKind::Pdf => "application/pdf",
            FormatKind::Png => "image/png",
            FormatKind::Jpeg => "image/jpeg",
            FormatKind::Tiff => "image/tiff",
            FormatKind::Html => "text/html",
            FormatKind::Docx => {
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            }
            FormatKind::Xlsx => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            FormatKind::Pptx => {
                "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            }
            // Family recognised, subtype unresolved from magic alone (ffprobe
            // refines it at extraction time).
            FormatKind::Audio => "audio/*",
            FormatKind::Video => "video/*",
            FormatKind::Other => "application/octet-stream",
        }
    }

    /// The extraction method that produces a `NormalizedDocumentV1`, if this
    /// build can genuinely parse the format. Image families are OCR-gated:
    /// they only report a path when a real OCR backend is present in this
    /// environment (capability is environment-dependent, never assumed).
    pub fn supported_extraction(self) -> Option<ExtractMethod> {
        match self {
            FormatKind::Text => Some(ExtractMethod::TextParser),
            FormatKind::Markdown => Some(ExtractMethod::TextParser),
            FormatKind::Csv => Some(ExtractMethod::CsvParser),
            FormatKind::Json => Some(ExtractMethod::JsonParser),
            FormatKind::Pdf => Some(ExtractMethod::PdfText),
            FormatKind::Html => Some(ExtractMethod::HtmlParser),
            FormatKind::Docx | FormatKind::Xlsx | FormatKind::Pptx => Some(ExtractMethod::Ooxml),
            FormatKind::Png | FormatKind::Jpeg | FormatKind::Tiff => {
                crate::capabilities::ocr_capability().map(|_| ExtractMethod::Ocr)
            }
            // Media intake (M9.4) is gated on the real backends that must
            // genuinely run: ffprobe (metadata) + ffmpeg (decode) for both,
            // a whisper engine for audio transcripts, tesseract for video
            // frame text. Capability is environment-dependent, never assumed.
            FormatKind::Audio => {
                let ok = crate::capabilities::media_probe_capability().is_some()
                    && crate::capabilities::media_decode_capability().is_some()
                    && crate::capabilities::audio_capability().is_some();
                ok.then_some(ExtractMethod::Audio)
            }
            FormatKind::Video => {
                let ok = crate::capabilities::media_probe_capability().is_some()
                    && crate::capabilities::media_decode_capability().is_some()
                    && crate::capabilities::ocr_capability().is_some();
                ok.then_some(ExtractMethod::Video)
            }
            FormatKind::Other => None,
        }
    }

    /// Families whose identity MUST be established by magic bytes: when the
    /// extension claims one of these but the content shows no magic, the two
    /// disagree and the report is `Ambiguous` (never silently reinterpreted).
    fn magic_required(self) -> bool {
        matches!(
            self,
            FormatKind::Pdf
                | FormatKind::Png
                | FormatKind::Jpeg
                | FormatKind::Tiff
                | FormatKind::Docx
                | FormatKind::Xlsx
                | FormatKind::Pptx
        )
    }
}

/// Honest intake status for a detected format (mission Section A: every
/// detection carries `SUPPORTED` / `UNSUPPORTED` / `CORRUPT` / `AMBIGUOUS`
/// with a concrete reason — never a silent guess).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectionStatus {
    /// This build has a genuine extraction path for the detected content.
    Supported,
    /// Format family recognised, but no extraction path in this build or
    /// environment (images with no OCR backend, audio/video before their
    /// backends, opaque containers).
    Unsupported(String),
    /// The content contradicts its own container (truncated PDF without
    /// `%%EOF`, text declared but binary bytes, broken PNG/zip headers).
    Corrupt(String),
    /// The evidence does not decide one format (conflicting OOXML members,
    /// declared JSON that is not structural JSON, declared magic format
    /// over plain-text content).
    Ambiguous(String),
}

impl DetectionStatus {
    pub fn is_supported(&self) -> bool {
        matches!(self, DetectionStatus::Supported)
    }

    pub fn label(&self) -> &'static str {
        match self {
            DetectionStatus::Supported => "supported",
            DetectionStatus::Unsupported(_) => "unsupported",
            DetectionStatus::Corrupt(_) => "corrupt",
            DetectionStatus::Ambiguous(_) => "ambiguous",
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            DetectionStatus::Supported => None,
            DetectionStatus::Unsupported(r)
            | DetectionStatus::Corrupt(r)
            | DetectionStatus::Ambiguous(r) => Some(r),
        }
    }
}

/// How sure we are. `None` means "no magic and no agreeing extension".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    None,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectReport {
    pub kind: FormatKind,
    pub confidence: Confidence,
    pub declared_ext: Option<String>,
    pub magic_ok: bool,
    pub size_bytes: u64,
    /// Intake status with a concrete reason for every non-supported outcome.
    pub status: DetectionStatus,
}

/// True when the leading magic bytes match a recognisable signature.
fn magic_kind(bytes: &[u8]) -> Option<FormatKind> {
    if bytes.starts_with(b"%PDF-") {
        return Some(FormatKind::Pdf);
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some(FormatKind::Png);
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some(FormatKind::Jpeg);
    }
    // TIFF: little-endian `II\x2a\x00` / big-endian `MM\x00\x2a`.
    if bytes.starts_with(b"II\x2a\x00") || bytes.starts_with(b"MM\x00\x2a") {
        return Some(FormatKind::Tiff);
    }
    if bytes.starts_with(b"PK\x03\x04") {
        // zip container — refined by member inspection below.
        return Some(FormatKind::Other);
    }
    // RIFF containers: WAVE audio / AVI video.
    if bytes.starts_with(b"RIFF") && bytes.len() >= 12 {
        return Some(match &bytes[8..12] {
            b"WAVE" => FormatKind::Audio,
            b"AVI " => FormatKind::Video,
            _ => FormatKind::Other,
        });
    }
    // ISO-BMFF (MP4 family): [size]ftyp[brand] — M4A/M4B are audio, else video.
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        return Some(match &bytes[8..12] {
            b"M4A " | b"M4B " | b"F4A " | b"F4B " => FormatKind::Audio,
            _ => FormatKind::Video,
        });
    }
    // Matroska/WebM (EBML header).
    if bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return Some(FormatKind::Video);
    }
    // Ogg / FLAC / ID3-tagged or raw MPEG audio frame sync.
    if bytes.starts_with(b"OggS") || bytes.starts_with(b"fLaC") || bytes.starts_with(b"ID3") {
        return Some(FormatKind::Audio);
    }
    // Raw MPEG frame sync — never at a UTF-16 BOM (FF FE would otherwise
    // look like a frame header).
    if bytes.starts_with(&[0xff, 0xfe]) {
        return None;
    }
    if bytes.len() >= 4 && bytes[0] == 0xff && (bytes[1] & 0xe0) == 0xe0 && (bytes[1] & 0x06) != 0 {
        return Some(FormatKind::Audio);
    }
    None
}

/// OOXML member markers inside a zip (central-directory names are stored
/// uncompressed, so a byte scan is genuine magic-level identification).
fn zip_ooxml_families(bytes: &[u8]) -> Vec<(&'static str, FormatKind)> {
    let mut found = Vec::new();
    if bytes
        .windows(b"word/document.xml".len())
        .any(|w| w == b"word/document.xml")
    {
        found.push(("word/document.xml", FormatKind::Docx));
    }
    if bytes
        .windows(b"xl/workbook.xml".len())
        .any(|w| w == b"xl/workbook.xml")
    {
        found.push(("xl/workbook.xml", FormatKind::Xlsx));
    }
    if bytes
        .windows(b"ppt/presentation.xml".len())
        .any(|w| w == b"ppt/presentation.xml")
    {
        found.push(("ppt/presentation.xml", FormatKind::Pptx));
    }
    found
}

/// End-of-central-directory marker present (a structurally complete zip).
fn zip_has_eocd(bytes: &[u8]) -> bool {
    bytes.len() >= 22 && bytes.windows(4).any(|w| w == b"PK\x05\x06")
}

/// Textual heuristics run only after a UTF-8 validity check: markdown signals,
/// then JSON structure, then CSV shape, else plain text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextGuess {
    Plain,
    Markdown,
    Json,
    Csv,
}

fn guess_text(text: &str) -> TextGuess {
    let trimmed = text.trim_start();
    let first = trimmed.chars().next();
    if (first == Some('{') || first == Some('['))
        && serde_json::from_str::<serde_json::Value>(trimmed).is_ok()
    {
        return TextGuess::Json;
    }
    let mut markdown_hits = 0i32;
    for line in text.lines().take(40) {
        let l = line.trim_start();
        let is_marker = l.starts_with("# ")
            || l.starts_with("## ")
            || l.starts_with("### ")
            || l.starts_with("```")
            || l.starts_with("~~~")
            || l.starts_with("- ")
            || l.starts_with("* ")
            || l.starts_with("1. ")
            || (l.contains("](") && l.contains('['));
        if is_marker {
            markdown_hits += 1;
        }
    }
    if markdown_hits >= 2 {
        return TextGuess::Markdown;
    }
    if looks_like_csv(text) {
        return TextGuess::Csv;
    }
    TextGuess::Plain
}

/// A cheap structural CSV probe: several lines each containing a consistent
/// count of comma-separated fields (ignoring quoted commas) and at least two.
fn looks_like_csv(text: &str) -> bool {
    let mut counts = Vec::new();
    for line in text.lines().take(20) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut n = 1u32;
        let mut in_quotes = false;
        for ch in trimmed.chars() {
            match ch {
                '"' => in_quotes = !in_quotes,
                ',' if !in_quotes => n += 1,
                _ => {}
            }
        }
        counts.push(n);
    }
    if counts.len() < 2 {
        return false;
    }
    let first = counts[0];
    first >= 2 && counts.iter().all(|c| *c == first)
}

/// HTML is recognised structurally (a real `<html>` document), not by any
/// extension: a page fragment with `<html…</html>` or an HTML5 doctype is HTML.
fn looks_like_html(text: &str) -> bool {
    let lower_head: String = text
        .chars()
        .take(1024)
        .flat_map(|c| c.to_lowercase())
        .collect();
    if lower_head.trim_start().starts_with("<!doctype html") {
        return true;
    }
    let lower: String = text.to_lowercase();
    lower.contains("<html") && lower.contains("</html>")
}

fn extension_hint(path: &str) -> Option<FormatKind> {
    let lower = path.to_ascii_lowercase();
    let ext = lower.rsplit('.').next()?;
    match ext {
        "txt" | "text" => Some(FormatKind::Text),
        "md" | "markdown" => Some(FormatKind::Markdown),
        "csv" => Some(FormatKind::Csv),
        "json" | "jsonl" => Some(FormatKind::Json),
        "pdf" => Some(FormatKind::Pdf),
        "png" => Some(FormatKind::Png),
        "jpg" | "jpeg" => Some(FormatKind::Jpeg),
        "tif" | "tiff" => Some(FormatKind::Tiff),
        "html" | "htm" => Some(FormatKind::Html),
        "docx" => Some(FormatKind::Docx),
        "xlsx" | "xlsm" => Some(FormatKind::Xlsx),
        "pptx" => Some(FormatKind::Pptx),
        "mp3" | "wav" | "flac" | "ogg" | "oga" | "m4a" | "aac" | "opus" => Some(FormatKind::Audio),
        "mp4" | "m4v" | "mkv" | "webm" | "avi" | "mov" => Some(FormatKind::Video),
        _ => None,
    }
}

/// Extensions whose declared format must be backed by real UTF-8 text.
fn textual_kind(k: FormatKind) -> bool {
    matches!(
        k,
        FormatKind::Text
            | FormatKind::Markdown
            | FormatKind::Csv
            | FormatKind::Json
            | FormatKind::Html
    )
}

/// Content-sanity checks applied to a magic-identified container. Every
/// failure is a concrete `Corrupt`/`Ambiguous` reason, never a silent pass.
fn content_status(kind: FormatKind, bytes: &[u8], was_zip: bool) -> DetectionStatus {
    // --- Corrupt: container contradicts itself -----------------------------
    if kind == FormatKind::Pdf && !has_pdf_eof(bytes) {
        return DetectionStatus::Corrupt(
            "pdf header present but no %%EOF marker — file is truncated or damaged".into(),
        );
    }
    if kind == FormatKind::Png {
        if bytes.len() < 33 || &bytes[12..16] != b"IHDR" {
            return DetectionStatus::Corrupt("png header is short or missing IHDR".into());
        }
    }
    if kind == FormatKind::Jpeg && bytes.len() < 10 {
        return DetectionStatus::Corrupt("jpeg header truncated".into());
    }
    if kind == FormatKind::Tiff && bytes.len() < 8 {
        return DetectionStatus::Corrupt("tiff header truncated".into());
    }
    if was_zip && !zip_has_eocd(bytes) {
        return DetectionStatus::Corrupt(
            "zip container has no end-of-central-directory — file is truncated".into(),
        );
    }

    // --- Ambiguous: conflicting evidence inside the container --------------
    if was_zip {
        let families = zip_ooxml_families(bytes);
        if families.len() > 1 {
            let members: Vec<&str> = families.iter().map(|(m, _)| *m).collect();
            return DetectionStatus::Ambiguous(format!(
                "zip carries conflicting office members ({}); refusing to guess the document type",
                members.join(", ")
            ));
        }
    }

    // --- Extraction path availability (content kind decides; a disagreeing
    //     extension is a hint and never changes the outcome) ----------------
    if kind.supported_extraction().is_some() {
        return DetectionStatus::Supported;
    }
    match kind {
        FormatKind::Png | FormatKind::Jpeg | FormatKind::Tiff => DetectionStatus::Unsupported(
            "no ocr backend on PATH (tesseract) — image text extraction unavailable".into(),
        ),
        FormatKind::Audio => DetectionStatus::Unsupported(audio_status_reason()),
        FormatKind::Video => DetectionStatus::Unsupported(video_status_reason()),
        _ => DetectionStatus::Unsupported(format!(
            "no extraction path for `{}` in this build",
            kind.label()
        )),
    }
}

/// Why audio intake is unavailable, naming the first missing gate (checked
/// in pipeline order). Never reached when `supported_extraction()` passed.
fn audio_status_reason() -> String {
    if crate::capabilities::audio_capability().is_none() {
        "no ASR backend on PATH (whisper) — audio transcript extraction unavailable (MODEL_MISSING)"
            .into()
    } else if crate::capabilities::media_decode_capability().is_none() {
        "no ffmpeg on PATH — audio decode unavailable".into()
    } else {
        "no ffprobe on PATH — media probing unavailable".into()
    }
}

/// Why video intake is unavailable, naming the first missing gate.
fn video_status_reason() -> String {
    if crate::capabilities::media_probe_capability().is_none() {
        "no ffprobe on PATH — media probing unavailable".into()
    } else if crate::capabilities::media_decode_capability().is_none() {
        "no ffmpeg on PATH — video frame sampling unavailable".into()
    } else {
        "no tesseract on PATH — video frame text extraction unavailable (MODEL_MISSING)".into()
    }
}

fn has_pdf_eof(bytes: &[u8]) -> bool {
    let tail_start = bytes.len().saturating_sub(4096);
    bytes[tail_start..]
        .windows(b"%%EOF".len())
        .any(|w| w == b"%%EOF")
}

/// Magic-first detection with extension cross-check. `size_bytes` is taken
/// from the caller (never trusted from the file name).
pub fn detect(bytes: &[u8], declared_path: Option<&str>) -> DetectReport {
    let size_bytes = bytes.len() as u64;
    let ext_kind = declared_path.and_then(extension_hint);

    if let Some(magic) = magic_kind(bytes) {
        let was_zip = bytes.starts_with(b"PK\x03\x04");
        let kind = if was_zip {
            let families = zip_ooxml_families(bytes);
            match families.len() {
                0 => FormatKind::Other,
                1 => families[0].1,
                // Conflicting members: keep `Other`; status below reports the
                // ambiguity with both member names.
                _ => FormatKind::Other,
            }
        } else {
            magic
        };
        let confidence = match ext_kind {
            Some(e) if e == kind => Confidence::High,
            _ => Confidence::Medium,
        };
        let status = content_status(kind, bytes, was_zip);
        return DetectReport {
            kind,
            confidence,
            declared_ext: declared_path.map(|p| p.to_string()),
            magic_ok: true,
            size_bytes,
            status,
        };
    }

    // No magic: the content must be valid, NUL-free text to be parseable.
    let utf8 = std::str::from_utf8(bytes).map(|s| s.to_string());
    let textual_ext = ext_kind.map(textual_kind).unwrap_or(false);
    let has_nul = bytes.contains(&0);
    if utf8.is_err() || has_nul {
        let status = if textual_ext {
            if utf8.is_err() {
                DetectionStatus::Corrupt(
                    "declared text format but the bytes are not valid UTF-8".into(),
                )
            } else {
                DetectionStatus::Corrupt("declared text format but the bytes contain NUL".into())
            }
        } else if let Some(e) = ext_kind {
            DetectionStatus::Ambiguous(format!(
                "declared `{}` but the content is neither that format's binary nor valid text",
                e.label()
            ))
        } else {
            DetectionStatus::Unsupported(
                "unrecognised binary: no magic signature and no declared format".into(),
            )
        };
        return DetectReport {
            kind: FormatKind::Other,
            confidence: Confidence::None,
            declared_ext: declared_path.map(|p| p.to_string()),
            magic_ok: false,
            size_bytes,
            status,
        };
    }

    let text = utf8.unwrap_or_default();
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text).to_string();

    // Declared magic-required format over plain-text content: the extension
    // and the content genuinely disagree — operator decision required.
    if let Some(e) = ext_kind {
        if e.magic_required() {
            return DetectReport {
                kind: FormatKind::Text,
                confidence: Confidence::None,
                declared_ext: declared_path.map(|p| p.to_string()),
                magic_ok: false,
                size_bytes,
                status: DetectionStatus::Ambiguous(format!(
                    "declared `{}` but the content has no matching magic and reads as plain text",
                    e.label()
                )),
            };
        }
    }

    // HTML: declared or structurally identified.
    if ext_kind == Some(FormatKind::Html) || looks_like_html(&text) {
        let confidence = if ext_kind == Some(FormatKind::Html) {
            Confidence::High
        } else {
            Confidence::Medium
        };
        return DetectReport {
            kind: FormatKind::Html,
            confidence,
            declared_ext: declared_path.map(|p| p.to_string()),
            magic_ok: false,
            size_bytes,
            status: DetectionStatus::Supported,
        };
    }

    match guess_text(&text) {
        TextGuess::Json => {
            let confidence = if matches!(ext_kind, Some(FormatKind::Json)) {
                Confidence::High
            } else {
                Confidence::Medium
            };
            DetectReport {
                kind: FormatKind::Json,
                confidence,
                declared_ext: declared_path.map(|p| p.to_string()),
                magic_ok: false,
                size_bytes,
                status: DetectionStatus::Supported,
            }
        }
        TextGuess::Markdown => DetectReport {
            kind: FormatKind::Markdown,
            confidence: Confidence::Medium,
            declared_ext: declared_path.map(|p| p.to_string()),
            magic_ok: false,
            size_bytes,
            status: DetectionStatus::Supported,
        },
        TextGuess::Csv => DetectReport {
            kind: FormatKind::Csv,
            confidence: Confidence::Medium,
            declared_ext: declared_path.map(|p| p.to_string()),
            magic_ok: false,
            size_bytes,
            status: DetectionStatus::Supported,
        },
        TextGuess::Plain => {
            // Declared JSON over non-JSON text: the JSON parser would fail and
            // the text parser is a reinterpretation — flag, never guess.
            if ext_kind == Some(FormatKind::Json) {
                return DetectReport {
                    kind: FormatKind::Text,
                    confidence: Confidence::None,
                    declared_ext: declared_path.map(|p| p.to_string()),
                    magic_ok: false,
                    size_bytes,
                    status: DetectionStatus::Ambiguous(
                        "declared json but the content is not structural json".into(),
                    ),
                };
            }
            // Audio/video with an agreeing textual container but no magic:
            // extension alone never claims a format.
            if let Some(e @ (FormatKind::Audio | FormatKind::Video)) = ext_kind {
                return DetectReport {
                    kind: e,
                    confidence: Confidence::None,
                    declared_ext: declared_path.map(|p| p.to_string()),
                    magic_ok: false,
                    size_bytes,
                    status: DetectionStatus::Unsupported(format!(
                        "declared `{}` but the content shows no {} container signature",
                        e.label(),
                        e.label()
                    )),
                };
            }
            let kind = match ext_kind {
                Some(k @ (FormatKind::Text | FormatKind::Markdown)) => k,
                _ => FormatKind::Text,
            };
            DetectReport {
                kind,
                confidence: if matches!(
                    ext_kind,
                    Some(FormatKind::Text) | Some(FormatKind::Markdown)
                ) {
                    Confidence::High
                } else {
                    Confidence::None
                },
                declared_ext: declared_path.map(|p| p.to_string()),
                magic_ok: false,
                size_bytes,
                status: DetectionStatus::Supported,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_of(bytes: &[u8], path: Option<&str>) -> DetectionStatus {
        detect(bytes, path).status
    }

    #[test]
    fn pdf_magic_high() {
        let r = detect(b"%PDF-1.7\n...\n%%EOF", Some("x.pdf"));
        assert_eq!(r.kind, FormatKind::Pdf);
        assert_eq!(r.confidence, Confidence::High);
        assert!(r.magic_ok);
        assert_eq!(r.status, DetectionStatus::Supported);
    }

    #[test]
    fn pdf_magic_without_ext_medium() {
        let r = detect(b"%PDF-1.4 fake body %%EOF", None);
        assert_eq!(r.kind, FormatKind::Pdf);
        assert_eq!(r.confidence, Confidence::Medium);
    }

    #[test]
    fn truncated_pdf_is_corrupt() {
        let r = detect(b"%PDF-1.7\n1 0 obj << /Type /Page >>", Some("x.pdf"));
        assert_eq!(r.kind, FormatKind::Pdf);
        assert!(matches!(r.status, DetectionStatus::Corrupt(_)));
    }

    #[test]
    fn png_magic() {
        let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00];
        let r = detect(&png, Some("img.png"));
        assert_eq!(r.kind, FormatKind::Png);
        assert_eq!(r.confidence, Confidence::High);
        assert!(matches!(r.status, DetectionStatus::Corrupt(_)));
    }

    #[test]
    fn jpeg_magic_wins_over_txt_ext() {
        let jpeg = [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, b'J', b'F', b'I', b'F'];
        let r = detect(&jpeg, Some("photo.txt"));
        assert_eq!(r.kind, FormatKind::Jpeg);
        assert_eq!(r.confidence, Confidence::Medium); // ext disagrees
                                                      // Content decides the kind (magic first). Intake support is
                                                      // capability-honest: supported only with a real OCR backend.
        assert_eq!(
            r.status.is_supported(),
            crate::capabilities::ocr_capability().is_some()
        );
        if !r.status.is_supported() {
            assert!(r.status.reason().unwrap().contains("ocr backend"));
        }
    }

    #[test]
    fn tiff_magic_detected() {
        let mut tiff = b"II\x2a\x00".to_vec();
        tiff.extend_from_slice(&[0x08, 0x00, 0x00, 0x00, 0x00, 0x00]);
        let r = detect(&tiff, Some("scan.tiff"));
        assert_eq!(r.kind, FormatKind::Tiff);
        assert_eq!(r.confidence, Confidence::High);
        assert_eq!(
            r.status.is_supported(),
            crate::capabilities::ocr_capability().is_some()
        );
    }

    #[test]
    fn short_tiff_is_corrupt() {
        let r = detect(b"II\x2a\x00", Some("scan.tif"));
        assert!(matches!(r.status, DetectionStatus::Corrupt(_)));
    }

    #[test]
    fn json_shaped_text() {
        let r = detect(br#"{"a": 1, "b": [2, 3]}"#, Some("data.json"));
        assert_eq!(r.kind, FormatKind::Json);
        assert_eq!(r.confidence, Confidence::High);
        assert_eq!(r.status, DetectionStatus::Supported);
    }

    #[test]
    fn csv_shape() {
        let csv = b"name,age,city\nada,36,London\nbob,42,Paris\n";
        let r = detect(csv, Some("t.csv"));
        assert_eq!(r.kind, FormatKind::Csv);
        assert!(r.status.is_supported());
    }

    #[test]
    fn markdown_heuristic() {
        let md =
            b"# Title\n\nSome text with a [link](https://example.org).\n\n- item one\n- item two\n";
        let r = detect(md, None);
        assert_eq!(r.kind, FormatKind::Markdown);
        assert!(r.status.is_supported());
    }

    #[test]
    fn plain_text_falls_through() {
        let r = detect(b"just some ordinary words here", None);
        assert_eq!(r.kind, FormatKind::Text);
        assert_eq!(r.status, DetectionStatus::Supported);
    }

    #[test]
    fn invalid_utf8_is_other_unsupported() {
        let bad = [0xff, 0xfe, 0x00, 0x80, 0x01];
        let r = detect(&bad, None);
        assert_eq!(r.kind, FormatKind::Other);
        assert_eq!(r.confidence, Confidence::None);
        assert!(matches!(r.status, DetectionStatus::Unsupported(_)));
    }

    #[test]
    fn text_declared_but_binary_is_corrupt() {
        let bad = [0xff, 0xfe, 0x00, 0x80];
        let r = detect(&bad, Some("notes.txt"));
        assert!(matches!(r.status, DetectionStatus::Corrupt(_)));
    }

    #[test]
    fn zip_container_other_unsupported() {
        // A structurally complete but member-less zip is unsupported, not an
        // extraction path.
        let mut zip = b"PK\x03\x04".to_vec();
        zip.extend_from_slice(&[0x14, 0x00]);
        zip.extend_from_slice(&[0u8; 18]);
        // Minimal EOCD so it is not classified as truncated.
        zip.extend_from_slice(b"PK\x05\x06");
        zip.extend_from_slice(&[0u8; 18]);
        let r = detect(&zip, Some("archive.zip"));
        assert_eq!(r.kind, FormatKind::Other);
        assert!(matches!(r.status, DetectionStatus::Unsupported(_)));
    }

    #[test]
    fn truncated_zip_is_corrupt() {
        let zip = [0x50, 0x4b, 0x03, 0x04, 0x14];
        let r = detect(&zip, Some("doc.docx"));
        assert!(matches!(r.status, DetectionStatus::Corrupt(_)));
    }

    #[test]
    fn docx_member_detection_supported() {
        let mut zip = b"PK\x03\x04".to_vec();
        zip.extend_from_slice(&[0x14, 0x00]);
        zip.extend_from_slice(&[0u8; 18]);
        zip.extend_from_slice(b"word/document.xml");
        zip.extend_from_slice(&[0u8; 8]);
        zip.extend_from_slice(b"PK\x05\x06");
        zip.extend_from_slice(&[0u8; 18]);
        let r = detect(&zip, Some("report.docx"));
        assert_eq!(r.kind, FormatKind::Docx);
        assert_eq!(r.confidence, Confidence::High);
        assert!(r.status.is_supported());
    }

    #[test]
    fn conflicting_office_members_ambiguous() {
        let mut zip = b"PK\x03\x04".to_vec();
        zip.extend_from_slice(&[0x14, 0x00]);
        zip.extend_from_slice(&[0u8; 18]);
        zip.extend_from_slice(b"word/document.xml");
        zip.extend_from_slice(&[0u8; 4]);
        zip.extend_from_slice(b"xl/workbook.xml");
        zip.extend_from_slice(&[0u8; 4]);
        zip.extend_from_slice(b"PK\x05\x06");
        zip.extend_from_slice(&[0u8; 18]);
        let r = detect(&zip, None);
        assert!(matches!(r.status, DetectionStatus::Ambiguous(_)));
    }

    #[test]
    fn declared_json_non_json_is_ambiguous() {
        let r = detect(b"hello, this is not json at all", Some("data.json"));
        assert!(matches!(r.status, DetectionStatus::Ambiguous(_)));
    }

    #[test]
    fn declared_pdf_over_text_is_ambiguous() {
        let r = detect(b"just a text file someone renamed", Some("fake.pdf"));
        assert!(matches!(r.status, DetectionStatus::Ambiguous(_)));
        assert_eq!(r.kind, FormatKind::Text);
    }

    #[test]
    fn html_doctype_detected_without_extension() {
        let r = detect(b"<!DOCTYPE html><html><body>hi</body></html>", None);
        assert_eq!(r.kind, FormatKind::Html);
        assert!(r.status.is_supported());
    }

    #[test]
    fn html_extension_supported() {
        let r = detect(b"<p>fragment without html tag</p>", Some("page.html"));
        assert_eq!(r.kind, FormatKind::Html);
        assert_eq!(r.confidence, Confidence::High);
        assert!(r.status.is_supported());
    }

    #[test]
    fn wave_audio_detected_capability_honest() {
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&[0x24, 0x00, 0x00, 0x00]);
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&[0u8; 16]);
        let r = detect(&wav, Some("clip.wav"));
        assert_eq!(r.kind, FormatKind::Audio);
        // Status is exactly the capability gate — supported iff every real
        // backend (ffprobe + ffmpeg + whisper) is present on this host.
        assert_eq!(
            r.status.is_supported(),
            FormatKind::Audio.supported_extraction().is_some()
        );
        if !r.status.is_supported() {
            let reason = r.status.reason().expect("unsupported carries a reason");
            if crate::capabilities::audio_capability().is_none() {
                assert!(reason.contains("MODEL_MISSING"), "{reason}");
                assert!(reason.contains("whisper"), "{reason}");
            }
        }
    }

    #[test]
    fn mp4_brand_video_detected_capability_honest() {
        let mut mp4 = 32u32.to_be_bytes().to_vec();
        mp4.extend_from_slice(b"ftypisom");
        mp4.extend_from_slice(&[0u8; 24]);
        let r = detect(&mp4, Some("clip.mp4"));
        assert_eq!(r.kind, FormatKind::Video);
        assert_eq!(
            r.status.is_supported(),
            FormatKind::Video.supported_extraction().is_some()
        );
        if !r.status.is_supported() {
            assert!(
                r.status
                    .reason()
                    .expect("unsupported carries a reason")
                    .len()
                    > 10
            );
        }
    }

    #[test]
    fn m4a_brand_audio_detected() {
        let mut m4a = 32u32.to_be_bytes().to_vec();
        m4a.extend_from_slice(b"ftypM4A ");
        m4a.extend_from_slice(&[0u8; 24]);
        let r = detect(&m4a, None);
        assert_eq!(r.kind, FormatKind::Audio);
    }

    #[test]
    fn status_always_has_reason_when_not_supported() {
        for (bytes, path) in [
            (&b"%PDF-1.7 no eof"[..], Some("a.pdf")),
            (&b"\xff\xfe\x00"[..], Some("a.txt")),
            (&b"hello world"[..], Some("a.json")),
        ] {
            let s = status_of(bytes, path);
            if !s.is_supported() {
                assert!(s.reason().is_some());
                assert!(!s.reason().unwrap().is_empty());
            }
        }
    }
}
