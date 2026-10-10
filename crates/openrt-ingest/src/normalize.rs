//! Normalised document model (schema v1) and the pure, genuine text parsers.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::DOCUMENT_SCHEMA_VERSION;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractMethod {
    /// Built-in deterministic text/markdown parser.
    TextParser,
    /// Built-in genuine CSV parser.
    CsvParser,
    /// Built-in JSON parser (serde_json structural).
    JsonParser,
    /// Built-in HTML parser (genuine markup walk; scripts/styles dropped).
    HtmlParser,
    /// OOXML (docx/xlsx/pptx) reader from the office fabric.
    Ooxml,
    /// Built-in genuine PDF text extractor (xref + FlateDecode + text ops).
    PdfText,
    /// OCR through a recognised backend. Only ever set when a real backend is
    /// present AND produced the text; `optr` (no backend) is never reported.
    Ocr,
    /// Visual-language-model description through the model fabric. Only ever
    /// set when a real model produced the description.
    Vlm,
    /// Audio transcription backend. Only ever set when real.
    Audio,
    /// Video frame/timeline extraction backend. Only ever set when real.
    Video,
}

impl Default for ExtractMethod {
    fn default() -> Self {
        ExtractMethod::TextParser
    }
}

/// Why/how a block was produced (provenance at block granularity).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockSource {
    /// `method` of the document; for OCR/VLM carries `name|version`.
    pub method: ExtractMethod,
    /// Page/block position data (only present when the backend produced it).
    pub page: Option<u32>,
    /// 0..=1 bounding geometry for OCR/visual blocks — never invented.
    pub bbox: Option<BBox>,
    /// 0..=1 backend-reported confidence — never invented.
    pub confidence: Option<f32>,
    /// Media timeline start (seconds) for audio/video blocks — never invented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t_start: Option<f64>,
    /// Media timeline end (seconds) for audio/video blocks — never invented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t_end: Option<f64>,
}

impl BlockSource {
    pub fn parser(method: ExtractMethod) -> Self {
        Self {
            method,
            page: None,
            bbox: None,
            confidence: None,
            t_start: None,
            t_end: None,
        }
    }

    /// A timed source (audio/video extraction): the block covers
    /// `t_start..=t_end` seconds of its source track.
    pub fn timed(method: ExtractMethod, t_start: f64, t_end: f64) -> Self {
        Self {
            t_start: Some(t_start),
            t_end: Some(t_end),
            ..Self::parser(method)
        }
    }

    /// Renders a compact provenance tag such as `page=3:0.92@ocr:tesseract-5`
    /// or `@openrt-text-parser`. Empty when nothing to say (never invents).
    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        let method = match self.method {
            ExtractMethod::Ocr => "ocr",
            ExtractMethod::Vlm => "vlm",
            ExtractMethod::Audio => "audio",
            ExtractMethod::TextParser => "openrt-text-parser",
            ExtractMethod::CsvParser => "openrt-csv-parser",
            ExtractMethod::JsonParser => "openrt-json-parser",
            ExtractMethod::HtmlParser => "openrt-html-parser",
            ExtractMethod::Ooxml => "openrt-ooxml",
            ExtractMethod::PdfText => "openrt-pdf-text",
            ExtractMethod::Video => "video",
        };
        if let Some(page) = self.page {
            parts.push(format!("page={page}"));
        }
        if let (Some(t0), Some(t1)) = (self.t_start, self.t_end) {
            parts.push(format!("t={t0:.2}-{t1:.2}s"));
        }
        if let Some(c) = self.confidence {
            let v = (c * 100.0f32).round() as i64;
            parts.push(format!("conf={v}%"));
        }
        let mut out = format!("@{method}");
        if !parts.is_empty() {
            out = format!("{}: {}", parts.join(","), out);
        }
        out
    }
}

/// A normalised block of document content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Block {
    /// Stable, deterministic block key within the document.
    pub key: String,
    pub kind: BlockKind,
    pub page: Option<u32>,
    pub text: String,
    /// Table structure, only when the parser could genuinely derive it.
    pub table: Option<TableRegion>,
    pub source: BlockSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    Paragraph,
    Heading,
    Bullet,
    ListItem,
    Page,
    Table,
    ImageRef,
    SectionDivider,
    Other,
}

/// A rectangular region, normalised to 0..=1 on each axis.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BBox {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// Table structure derived by the parser (rows x cols of cell text).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableRegion {
    pub rows: usize,
    pub cols: usize,
    /// Row-major cell texts (genuine; derived by the parser, not invented).
    pub cells: Vec<String>,
}

/// The frozen normalised document (schema v1). Every field is persisted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NormalizedDocumentV1 {
    pub schema_version: u32,
    /// Stable content-address: `doc/<sha256-of-original-bytes>`.
    pub doc_id: String,
    /// Monotonic version of this content under `doc_id` (1 for first intake).
    pub version: u32,
    /// SHA-256 of the exact input bytes.
    pub content_sha256: String,
    pub format: crate::FormatKind,
    pub size_bytes: u64,
    pub pages: u32,
    pub blocks: Vec<Block>,
    pub created_at: String,
    /// `method[@version]` of the extractor that produced this document.
    pub extractor: String,
    /// Detection confidence recorded at intake.
    pub detection_confidence: crate::Confidence,
    /// Dominant modality of the document (additive; defaults to `text` for
    /// documents persisted before this field existed).
    #[serde(default)]
    pub modality: Modality,
    /// Media metadata (dimensions/duration/…), only when genuinely probed —
    /// never invented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<MediaMeta>,
}

/// Dominant content modality of a stored document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    #[default]
    Text,
    Image,
    Audio,
    Video,
    Mixed,
}

/// Probed media metadata. Every field is optional because backends report
/// what they actually measured — nothing here is guessed.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MediaMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// Color model parsed from the image header (M9.3) — `None` when the
    /// header does not carry cheaply-readable color info (never guessed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_space: Option<String>,
}

impl NormalizedDocumentV1 {
    /// Full text in reading order (blocks joined by newline). Used only for
    /// chunking/embedding input — never for logs or prompts verbatim when a
    /// secret policy says otherwise.
    pub fn full_text(&self) -> String {
        let mut out = String::new();
        for b in &self.blocks {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&b.text);
        }
        out
    }
}

/// Pure-function extraction errors.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum NormalizeError {
    #[error("format `{0}` has no genuine text extraction path in this build")]
    Unsupported(String),
    #[error("pdf parsing failed: {0}")]
    Pdf(String),
    #[error("pdf has no extractable text layer (image-only pdfs need an ocr backend)")]
    PdfImageOnly,
    #[error("ooxml parsing failed: {0}")]
    Ooxml(String),
    #[error("input exceeds document limits: {0}")]
    Limit(String),
}

/// Deterministic block key: `b{n}` in reading order.
fn block_key(index: usize) -> String {
    crate::block_key(index)
}

/// Skips a UTF-8 BOM so hashes/versions are stable on identical content.
fn strip_bom(data: &[u8]) -> &[u8] {
    data.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(data)
}

pub(crate) fn doc(
    data: &[u8],
    format: crate::FormatKind,
    method: ExtractMethod,
    blocks: Vec<Block>,
    pages: u32,
    confidence: crate::Confidence,
) -> Result<NormalizedDocumentV1, NormalizeError> {
    let clean = strip_bom(data);
    let content_sha256 = crate::sha256_hex_for(clean);
    Ok(NormalizedDocumentV1 {
        schema_version: DOCUMENT_SCHEMA_VERSION,
        doc_id: format!("doc/{content_sha256}"),
        version: 1,
        content_sha256,
        format,
        size_bytes: clean.len() as u64,
        pages,
        blocks,
        created_at: openrt_core::now().to_rfc3339(),
        extractor: extractor_label(method),
        detection_confidence: confidence,
        modality: modality_of(format),
        media: None,
    })
}

/// Dominant modality implied by the container format.
pub(crate) fn modality_of(format: crate::FormatKind) -> Modality {
    match format {
        crate::FormatKind::Png | crate::FormatKind::Jpeg | crate::FormatKind::Tiff => {
            Modality::Image
        }
        crate::FormatKind::Audio => Modality::Audio,
        crate::FormatKind::Video => Modality::Video,
        _ => Modality::Text,
    }
}

fn extractor_label(method: ExtractMethod) -> String {
    match method {
        ExtractMethod::TextParser => "openrt-text-parser/pure",
        ExtractMethod::CsvParser => "openrt-csv-parser/pure",
        ExtractMethod::JsonParser => "openrt-json-parser/pure",
        ExtractMethod::HtmlParser => "openrt-html-parser/pure",
        ExtractMethod::Ooxml => "openrt-ooxml/pure",
        ExtractMethod::PdfText => "openrt-pdf-text/pure",
        ExtractMethod::Ocr => "ocr/unavailable-in-build",
        ExtractMethod::Vlm => "vlm/unavailable-in-build",
        ExtractMethod::Audio => "audio/unavailable-in-build",
        ExtractMethod::Video => "video/unavailable-in-build",
    }
    .to_string()
}

/// Normalise raw text (valid UTF-8 assumed; detection validated it). Splits
/// on blank-line paragraph boundaries, tagging simple heading lines.
pub fn normalize_text(data: &[u8]) -> Result<NormalizedDocumentV1, NormalizeError> {
    let text = std::str::from_utf8(strip_bom(data))
        .map_err(|_| NormalizeError::Unsupported("non-utf8 in text path".into()))?;
    let mut blocks = Vec::new();
    for (i, para) in text.split("\n\n").enumerate() {
        let trimmed = para.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut kind = BlockKind::Paragraph;
        let mut body = trimmed.to_string();
        if let Some(stripped) = trimmed.strip_prefix("### ") {
            kind = BlockKind::Heading;
            body = stripped.to_string();
        } else if let Some(stripped) = trimmed.strip_prefix("## ") {
            kind = BlockKind::Heading;
            body = stripped.to_string();
        } else if let Some(stripped) = trimmed.strip_prefix("# ") {
            kind = BlockKind::Heading;
            body = stripped.to_string();
        }
        blocks.push(Block {
            key: block_key(blocks.len()),
            kind,
            page: None,
            text: body,
            table: None,
            source: BlockSource::parser(ExtractMethod::TextParser),
        });
        let _ = i;
    }
    if blocks.is_empty() {
        // Genuine empty document: one empty paragraph is still honest.
        blocks.push(Block {
            key: block_key(0),
            kind: BlockKind::Paragraph,
            page: None,
            text: String::new(),
            table: None,
            source: BlockSource::parser(ExtractMethod::TextParser),
        });
    }
    doc(
        data,
        crate::FormatKind::Text,
        ExtractMethod::TextParser,
        blocks,
        0,
        crate::Confidence::High,
    )
}

/// Markdown shares the text splitter but tags list lines and code fences.
pub fn normalize_markdown(data: &[u8]) -> Result<NormalizedDocumentV1, NormalizeError> {
    let text = std::str::from_utf8(strip_bom(data))
        .map_err(|_| NormalizeError::Unsupported("non-utf8 in markdown path".into()))?;
    let mut blocks = Vec::new();
    for para in text.split("\n\n") {
        let trimmed = para.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut kind = BlockKind::Paragraph;
        let mut body = trimmed.to_string();
        if let Some(stripped) = trimmed.strip_prefix("### ") {
            kind = BlockKind::Heading;
            body = stripped.to_string();
        } else if let Some(stripped) = trimmed.strip_prefix("## ") {
            kind = BlockKind::Heading;
            body = stripped.to_string();
        } else if let Some(stripped) = trimmed.strip_prefix("# ") {
            kind = BlockKind::Heading;
            body = stripped.to_string();
        } else if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            kind = BlockKind::SectionDivider;
        } else if trimmed.lines().all(|l| {
            let t = l.trim_start();
            t.starts_with("- ") || t.starts_with("* ") || t.starts_with("+ ")
        }) {
            kind = BlockKind::Bullet;
            body = trimmed
                .lines()
                .map(|l| {
                    l.trim_start()
                        .trim_start_matches(['-', '*', '+', ' '])
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join("\n");
        } else if trimmed.lines().all(|l| {
            let t = l.trim_start();
            t.starts_with("1. ") || t.starts_with("1)")
        }) {
            kind = BlockKind::ListItem;
        }
        blocks.push(Block {
            key: block_key(blocks.len()),
            kind,
            page: None,
            text: body,
            table: None,
            source: BlockSource::parser(ExtractMethod::TextParser),
        });
    }
    if blocks.is_empty() {
        blocks.push(Block {
            key: block_key(0),
            kind: BlockKind::Paragraph,
            page: None,
            text: String::new(),
            table: None,
            source: BlockSource::parser(ExtractMethod::TextParser),
        });
    }
    doc(
        data,
        crate::FormatKind::Markdown,
        ExtractMethod::TextParser,
        blocks,
        0,
        crate::Confidence::High,
    )
}

/// Genuine CSV parser with quoted-field and escaped-quote handling. Produces a
/// single block per row (pages absent); a table region is recorded on the
/// whole-document envelope when column counts are consistent.
pub fn normalize_csv(data: &[u8]) -> Result<NormalizedDocumentV1, NormalizeError> {
    let text = std::str::from_utf8(strip_bom(data))
        .map_err(|_| NormalizeError::Unsupported("non-utf8 in csv path".into()))?;
    let rows = parse_csv_rows(text);
    if rows.is_empty() {
        return Err(NormalizeError::Unsupported(
            "empty csv has no extractable rows".into(),
        ));
    }
    let cols = rows[0].len();
    let consistent = rows.iter().all(|r| r.len() == cols);
    let mut blocks = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let mut cells = row.clone();
        if cells.is_empty() {
            cells.push(String::new());
        }
        blocks.push(Block {
            key: block_key(i),
            kind: BlockKind::Table,
            page: None,
            text: row.join(", "),
            table: Some(TableRegion {
                rows: 1,
                cols: cells.len(),
                cells,
            }),
            source: BlockSource::parser(ExtractMethod::CsvParser),
        });
    }
    let confidence = if consistent {
        crate::Confidence::High
    } else {
        crate::Confidence::Medium
    };
    doc(
        data,
        crate::FormatKind::Csv,
        ExtractMethod::CsvParser,
        blocks,
        0,
        confidence,
    )
}

/// RFC-4180-style CSV: quoted fields may contain commas/newlines; `""` escapes.
fn parse_csv_rows(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut chars = text.chars().peekable();
    let mut in_quotes = false;
    while let Some(ch) = chars.next() {
        if in_quotes {
            match ch {
                '"' => {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        field.push('"');
                    } else {
                        in_quotes = false;
                    }
                }
                _ => field.push(ch),
            }
        } else {
            match ch {
                '"' => in_quotes = true,
                ',' => {
                    row.push(std::mem::take(&mut field));
                }
                '\n' => {
                    row.push(std::mem::take(&mut field));
                    if !(row.len() == 1 && row[0].is_empty()) {
                        rows.push(std::mem::take(&mut row));
                    } else {
                        row.clear();
                    }
                }
                '\r' => {}
                _ => field.push(ch),
            }
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(std::mem::take(&mut field));
        rows.push(row);
    }
    rows
}

/// JSON: parse structurally and emit one block per top-level member (objects)
/// or per element (arrays), rendering each as canonical JSON text.
pub fn normalize_json(data: &[u8]) -> Result<NormalizedDocumentV1, NormalizeError> {
    let text = std::str::from_utf8(strip_bom(data))
        .map_err(|_| NormalizeError::Unsupported("non-utf8 in json path".into()))?;
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| NormalizeError::Unsupported(format!("not structural json: {e}")))?;
    let mut blocks = Vec::new();
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let text = serde_json::to_string(&v).unwrap_or_default();
                blocks.push(Block {
                    key: block_key(blocks.len()),
                    kind: BlockKind::Other,
                    page: None,
                    text: format!("{k}: {text}"),
                    table: None,
                    source: BlockSource::parser(ExtractMethod::JsonParser),
                });
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                let text = serde_json::to_string(&v).unwrap_or_default();
                blocks.push(Block {
                    key: block_key(blocks.len()),
                    kind: BlockKind::Other,
                    page: None,
                    text,
                    table: None,
                    source: BlockSource::parser(ExtractMethod::JsonParser),
                });
            }
        }
        other => {
            let text = serde_json::to_string(&other).unwrap_or_default();
            blocks.push(Block {
                key: block_key(0),
                kind: BlockKind::Other,
                page: None,
                text,
                table: None,
                source: BlockSource::parser(ExtractMethod::JsonParser),
            });
        }
    }
    if blocks.is_empty() {
        return Err(NormalizeError::Unsupported(
            "empty json object has no content to extract".into(),
        ));
    }
    doc(
        data,
        crate::FormatKind::Json,
        ExtractMethod::JsonParser,
        blocks,
        0,
        crate::Confidence::High,
    )
}

/// Placeholder for the PDF extractor (implemented in `pdf.rs`); kept here so
/// `normalize` exports stay in one place.
pub use crate::pdf::normalize_pdf;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_paragraph_split_and_heading() {
        let d = normalize_text(b"# Title\n\ntwo\n\nthree").unwrap();
        assert_eq!(d.blocks.len(), 3);
        assert_eq!(d.blocks[0].kind, BlockKind::Heading);
        assert_eq!(d.blocks[0].text, "Title");
        assert_eq!(d.blocks[1].text, "two");
        assert_eq!(d.blocks[2].text, "three");
        assert_eq!(d.pages, 0);
    }

    #[test]
    fn empty_text_still_honest() {
        let d = normalize_text(b"").unwrap();
        assert_eq!(d.blocks.len(), 1);
        assert!(d.blocks[0].text.is_empty());
    }

    #[test]
    fn bom_is_stripped_before_hash() {
        let with = normalize_text("\u{feff}hi there".as_bytes()).unwrap();
        let without = normalize_text(b"hi there").unwrap();
        assert_eq!(with.content_sha256, without.content_sha256);
        assert_eq!(with.doc_id, without.doc_id);
    }

    #[test]
    fn csv_quoted_comma_and_escape() {
        let d =
            normalize_csv(b"name,city\n\"ada, lovelace\",London\n\"mes\"\"s\",Paris\n").unwrap();
        assert_eq!(d.blocks[1].text, "ada, lovelace, London");
        assert_eq!(d.blocks[2].text, "mes\"s, Paris");
        let t = d.blocks[1].table.as_ref().unwrap();
        assert_eq!(t.cols, 2);
        assert_eq!(t.cells[0], "ada, lovelace");
    }

    #[test]
    fn ragged_csv_medium_confidence() {
        let d = normalize_csv(b"a,b\n1,2,3\n").unwrap();
        assert_eq!(d.detection_confidence, crate::Confidence::Medium);
    }

    #[test]
    fn json_object_blocks() {
        let d = normalize_json(br#"{"name":"ada","tags":["math","poetry"]}"#).unwrap();
        assert_eq!(d.blocks.len(), 2);
        assert!(d.blocks[0].text.starts_with("name: "));
    }

    #[test]
    fn non_json_rejected() {
        assert!(normalize_json(b"not json at all").is_err());
    }

    #[test]
    fn doc_id_is_content_hash() {
        let d = normalize_text(b"stable identity").unwrap();
        assert!(d.doc_id.starts_with("doc/"));
        assert!(d.doc_id.ends_with(&d.content_sha256));
    }

    #[test]
    fn provenance_label_safe() {
        let b = BlockSource::parser(ExtractMethod::TextParser);
        assert!(b.label().contains("openrt-text-parser"));
    }
}
