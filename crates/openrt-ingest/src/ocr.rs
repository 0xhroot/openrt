//! Tesseract OCR backend — genuine word-level recognition with real bounding
//! boxes and confidences, plus scanned-PDF page OCR over `pdftoppm`
//! rasterizations.
//!
//! Honesty contract (mission Section A):
//! * confidences are the backend's own (TSV `conf` /100), never invented;
//! * bounding boxes are the backend's geometry, normalized to 0..=1 of the
//!   image the backend actually saw;
//! * a missing backend is a structured `OcrError::BackendMissing` — there is
//!   no fallback that pretends to recognize text;
//! * `extractor` carries the real backend name and its reported version.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

use crate::capabilities::{ocr_capability, Capability};
use crate::normalize::{
    BBox, Block, BlockKind, BlockSource, ExtractMethod, MediaMeta, Modality, NormalizedDocumentV1,
};
use crate::{block_key, sha256_hex_for};

/// Structured OCR/render failures. Every variant carries a concrete reason.
#[derive(Debug, Error)]
pub enum OcrError {
    #[error("backend `{backend}` unavailable for {capability}: {reason}")]
    BackendMissing {
        backend: String,
        capability: String,
        reason: String,
    },
    #[error("failed to spawn `{bin}`: {err}")]
    Spawn { bin: String, err: String },
    #[error("`{bin}` failed (status {status}): {stderr}")]
    Failed {
        bin: String,
        status: i32,
        stderr: String,
    },
    #[error("layout parse failed: {0}")]
    Parse(String),
    #[error("temp artifact error: {0}")]
    Temp(String),
    #[error("image geometry unreadable: {0}")]
    Dims(String),
    #[error("pdf rendering failed: {0}")]
    Render(String),
}

/// Page-segmentation mode (tesseract `--psm`). Explicit enum — no magic
/// numbers leak into call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Psm {
    /// Fully automatic page segmentation, no OSD.
    Auto,
    /// Automatically segment the page, assuming a single column of text.
    SingleColumn,
    /// Treat the page as a single text block.
    SingleBlock,
    /// Sparse text — find as much text as possible in no particular order.
    SparseText,
}

impl Psm {
    pub fn as_str(self) -> &'static str {
        match self {
            Psm::Auto => "3",
            Psm::SingleColumn => "4",
            Psm::SingleBlock => "6",
            Psm::SparseText => "11",
        }
    }
}

/// One recognized word with real geometry and backend confidence.
#[derive(Debug, Clone, PartialEq)]
pub struct OcrWord {
    pub text: String,
    /// 0..=1 normalized box of the word on the rasterized page/image.
    pub bbox: BBox,
    /// 0..=1 backend confidence (TSV `conf` / 100).
    pub confidence: f32,
    pub block: u32,
    pub par: u32,
    pub line: u32,
}

/// A line of words inside a region (reading order preserved).
#[derive(Debug, Clone, PartialEq)]
pub struct OcrLine {
    pub text: String,
    pub bbox: BBox,
    pub confidence: f32,
    /// Indices into [`OcrPage::words`].
    pub words: Vec<usize>,
}

/// A layout region (tesseract block/paragraph): the unit stored as a
/// document block. Bbox/confidence are unions/means of its real words.
#[derive(Debug, Clone, PartialEq)]
pub struct OcrRegion {
    pub block: u32,
    pub par: u32,
    pub text: String,
    pub bbox: BBox,
    pub confidence: f32,
    pub lines: Vec<OcrLine>,
}

/// OCR result for one page/image.
#[derive(Debug, Clone, PartialEq)]
pub struct OcrPage {
    pub words: Vec<OcrWord>,
    pub width_px: u32,
    pub height_px: u32,
}

impl OcrPage {
    /// Layout analysis from the real boxes: words are grouped into the
    /// backend's block/paragraph regions, lines in reading order.
    pub fn regions(&self) -> Vec<OcrRegion> {
        let mut by_par: BTreeMap<(u32, u32), Vec<usize>> = BTreeMap::new();
        for (i, w) in self.words.iter().enumerate() {
            by_par.entry((w.block, w.par)).or_default().push(i);
        }
        let mut regions = Vec::new();
        for ((block, par), word_ids) in by_par {
            let mut by_line: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
            for &i in &word_ids {
                by_line.entry(self.words[i].line).or_default().push(i);
            }
            let mut lines = Vec::new();
            for (_, ids) in by_line {
                let text = join_words(self, &ids);
                lines.push(OcrLine {
                    text,
                    bbox: union_bbox(self, &ids),
                    confidence: mean_conf(self, &ids),
                    words: ids,
                });
            }
            let all: Vec<usize> = lines.iter().flat_map(|l| l.words.iter().copied()).collect();
            let region = OcrRegion {
                block,
                par,
                text: lines
                    .iter()
                    .map(|l| l.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                bbox: union_bbox(self, &all),
                confidence: mean_conf(self, &all),
                lines,
            };
            regions.push(region);
        }
        regions
    }

    /// One document block per layout region, with page + bbox + confidence
    /// provenance attached.
    pub fn to_blocks(&self, page: u32) -> Vec<Block> {
        self.regions()
            .into_iter()
            .enumerate()
            .map(|(i, r)| Block {
                key: block_key(i),
                kind: BlockKind::Paragraph,
                page: Some(page),
                text: r.text,
                table: None,
                source: BlockSource {
                    method: ExtractMethod::Ocr,
                    page: Some(page),
                    bbox: Some(r.bbox),
                    confidence: Some(r.confidence),
                    t_start: None,
                    t_end: None,
                },
            })
            .collect()
    }
}

fn join_words(page: &OcrPage, ids: &[usize]) -> String {
    ids.iter()
        .map(|&i| page.words[i].text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

fn union_bbox(page: &OcrPage, ids: &[usize]) -> BBox {
    let mut left = f32::MAX;
    let mut top = f32::MAX;
    let mut right = f32::MIN;
    let mut bottom = f32::MIN;
    for &i in ids {
        let b = &page.words[i].bbox;
        left = left.min(b.left);
        top = top.min(b.top);
        right = right.max(b.right);
        bottom = bottom.max(b.bottom);
    }
    if ids.is_empty() {
        return BBox {
            left: 0.0,
            top: 0.0,
            right: 0.0,
            bottom: 0.0,
        };
    }
    BBox {
        left,
        top,
        right: right.max(left),
        bottom: bottom.max(top),
    }
}

fn mean_conf(page: &OcrPage, ids: &[usize]) -> f32 {
    if ids.is_empty() {
        return 0.0;
    }
    let sum: f32 = ids.iter().map(|&i| page.words[i].confidence).sum();
    (sum / ids.len() as f32).clamp(0.0, 1.0)
}

/// Parse tesseract TSV output into an [`OcrPage`]. Only level-5 (word) rows
/// carry text; `conf < 0` rows are backend placeholders and are dropped.
pub fn parse_tsv(tsv: &str, width_px: u32, height_px: u32) -> Result<OcrPage, OcrError> {
    if width_px == 0 || height_px == 0 {
        return Err(OcrError::Dims(
            "rasterization produced a 0-sized image".into(),
        ));
    }
    let wx = width_px as f32;
    let hy = height_px as f32;
    let mut words = Vec::new();
    for (idx, line) in tsv.lines().enumerate() {
        if line.is_empty() || (idx == 0 && line.starts_with("level\t")) {
            continue;
        }
        let cols: Vec<&str> = line.splitn(12, '\t').collect();
        if cols.len() < 11 {
            continue;
        }
        let level: u32 = match cols[0].trim().parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        if level != 5 {
            continue;
        }
        let conf: f32 = match cols[10].trim().parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        if conf < 0.0 {
            continue;
        }
        let text = cols.get(11).map(|t| t.trim()).unwrap_or("");
        if text.is_empty() {
            continue;
        }
        let left: f32 = match cols[6].trim().parse() {
            Ok(v) => v,
            Err(_) => return Err(OcrError::Parse(format!("bad left on line {idx}"))),
        };
        let top: f32 = match cols[7].trim().parse() {
            Ok(v) => v,
            Err(_) => return Err(OcrError::Parse(format!("bad top on line {idx}"))),
        };
        let width: f32 = match cols[8].trim().parse() {
            Ok(v) => v,
            Err(_) => return Err(OcrError::Parse(format!("bad width on line {idx}"))),
        };
        let height: f32 = match cols[9].trim().parse() {
            Ok(v) => v,
            Err(_) => return Err(OcrError::Parse(format!("bad height on line {idx}"))),
        };
        let block: u32 = cols[2].trim().parse().unwrap_or(0);
        let par: u32 = cols[3].trim().parse().unwrap_or(0);
        let ln: u32 = cols[4].trim().parse().unwrap_or(0);
        words.push(OcrWord {
            text: text.to_string(),
            bbox: BBox {
                left: (left / wx).clamp(0.0, 1.0),
                top: (top / hy).clamp(0.0, 1.0),
                right: ((left + width) / wx).clamp(0.0, 1.0),
                bottom: ((top + height) / hy).clamp(0.0, 1.0),
            },
            confidence: (conf / 100.0).clamp(0.0, 1.0),
            block,
            par,
            line: ln,
        });
    }
    Ok(OcrPage {
        words,
        width_px,
        height_px,
    })
}

/// Real image pixel dimensions for the containers this build accepts
/// (PNG / JPEG / TIFF). `None` when the header cannot be read — never
/// guessed.
pub fn image_dims(bytes: &[u8]) -> Option<(u32, u32)> {
    png_dims(bytes)
        .or_else(|| jpeg_dims(bytes))
        .or_else(|| tiff_dims(bytes))
}

/// Color model parsed from the image header (M9.3 image metadata):
/// `gray` / `rgb` / `rgba` / `palette` / `gray_alpha` / `cmyk`.
/// `None` when the container does not carry cheaply-readable color info
/// (TIFF) — never inferred from content.
pub fn image_color(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 26
        && bytes.starts_with(&[0x89, b'P', b'N', b'G'])
        && &bytes[12..16] == b"IHDR"
    {
        return match bytes[25] {
            0 => Some("gray"),
            2 => Some("rgb"),
            3 => Some("palette"),
            4 => Some("gray_alpha"),
            6 => Some("rgba"),
            _ => None,
        };
    }
    if bytes.len() > 11 && bytes.starts_with(&[0xff, 0xd8]) {
        let mut i = 2usize;
        while i + 9 < bytes.len() {
            if bytes[i] != 0xff {
                i += 1;
                continue;
            }
            let marker = bytes[i + 1];
            if marker == 0xff {
                i += 1;
                continue;
            }
            if marker == 0xd8 || marker == 0xd9 || marker == 0x01 || (0xd0..=0xd7).contains(&marker)
            {
                i += 2;
                continue;
            }
            if i + 3 >= bytes.len() {
                break;
            }
            let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
            if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
                return match bytes[i + 9] {
                    1 => Some("gray"),
                    3 => Some("rgb"),
                    4 => Some("cmyk"),
                    _ => None,
                };
            }
            i += 2 + len;
        }
        return None;
    }
    None
}

fn png_dims(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 24 || !b.starts_with(&[0x89, b'P', b'N', b'G']) || &b[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes([b[16], b[17], b[18], b[19]]);
    let h = u32::from_be_bytes([b[20], b[21], b[22], b[23]]);
    Some((w, h))
}

fn jpeg_dims(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 10 || !b.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    let mut i = 2usize;
    while i + 9 < b.len() {
        if b[i] != 0xff {
            i += 1;
            continue;
        }
        let marker = b[i + 1];
        if marker == 0xff {
            i += 1;
            continue;
        }
        if marker == 0xd8 || marker == 0xd9 || marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            i += 2;
            continue;
        }
        if i + 4 > b.len() {
            return None;
        }
        let seg_len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        let is_sof = matches!(
            marker,
            0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf
        );
        if is_sof {
            if i + 9 > b.len() {
                return None;
            }
            let h = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
            let w = u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32;
            return Some((w, h));
        }
        if seg_len < 2 {
            return None;
        }
        i += 2 + seg_len;
    }
    None
}

fn tiff_dims(b: &[u8]) -> Option<(u32, u32)> {
    let le = b.starts_with(b"II\x2a\x00");
    let be = b.starts_with(b"MM\x00\x2a");
    if !le && !be {
        return None;
    }
    let u16r = |o: usize| -> Option<u16> {
        if le {
            Some(u16::from_le_bytes([*b.get(o)?, *b.get(o + 1)?]))
        } else {
            Some(u16::from_be_bytes([*b.get(o)?, *b.get(o + 1)?]))
        }
    };
    let u32r = |o: usize| -> Option<u32> {
        if le {
            Some(u32::from_le_bytes([
                *b.get(o)?,
                *b.get(o + 1)?,
                *b.get(o + 2)?,
                *b.get(o + 3)?,
            ]))
        } else {
            Some(u32::from_be_bytes([
                *b.get(o)?,
                *b.get(o + 1)?,
                *b.get(o + 2)?,
                *b.get(o + 3)?,
            ]))
        }
    };
    if u16r(2)? != 42 {
        return None;
    }
    let ifd = u32r(4)? as usize;
    let count = u16r(ifd)? as usize;
    let mut w = 0u32;
    let mut h = 0u32;
    for k in 0..count {
        let e = ifd + 2 + k * 12;
        let tag = u16r(e)?;
        let typ = u16r(e + 2)?;
        let val = if typ == 3 {
            u16r(e + 8)? as u32
        } else if typ == 4 {
            u32r(e + 8)?
        } else {
            0
        };
        if tag == 256 {
            w = val;
        }
        if tag == 257 {
            h = val;
        }
    }
    if w > 0 && h > 0 {
        Some((w, h))
    } else {
        None
    }
}

/// A temporary file removed on drop.
pub struct TempArtifact {
    path: PathBuf,
}

impl TempArtifact {
    pub fn persist(tag: &str, ext: &str, bytes: &[u8]) -> Result<Self, OcrError> {
        let path = temp_path(tag, ext);
        std::fs::write(&path, bytes)
            .map_err(|e| OcrError::Temp(format!("write {}: {e}", path.display())))?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Adopt an already-written temp path (removal still happens on drop).
    pub(crate) fn taken(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for TempArtifact {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub(crate) fn temp_path(tag: &str, ext: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "openrt-ocr-{}-{nanos}-{tag}.{ext}",
        std::process::id()
    ))
}

static LANGS: OnceLock<Vec<String>> = OnceLock::new();

fn available_langs(bin: &Path) -> Vec<String> {
    let Ok(out) = Command::new(bin).arg("--list-langs").output() else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .skip(1) // header: "List of available languages in …"
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Real tesseract backend. Construct only through [`Tesseract::detect`] —
/// a missing backend is `None`, never a stub.
#[derive(Debug, Clone)]
pub struct Tesseract {
    pub capability: Capability,
    pub lang: String,
}

impl Tesseract {
    pub fn detect() -> Option<Self> {
        let capability = ocr_capability()?;
        let lang = {
            let langs = LANGS.get_or_init(|| available_langs(Path::new(&capability.binary)));
            if langs.iter().any(|l| l == "eng") || langs.is_empty() {
                "eng".to_string()
            } else {
                langs[0].clone()
            }
        };
        Some(Self { capability, lang })
    }

    /// `name|version` label for provenance (version only when the backend
    /// reported one).
    pub fn label(&self) -> String {
        match &self.capability.version {
            Some(v) => format!("{}|{}", self.capability.name, v),
            None => self.capability.name.clone(),
        }
    }

    /// Recognize text in an image file. Pixel geometry comes from the file
    /// header; boxes are normalized against the image the backend saw.
    pub fn recognize(&self, image: &Path, psm: Psm) -> Result<OcrPage, OcrError> {
        let data = std::fs::read(image)
            .map_err(|e| OcrError::Temp(format!("read {}: {e}", image.display())))?;
        let (w, h) =
            image_dims(&data).ok_or_else(|| OcrError::Dims(image.display().to_string()))?;
        let out = Command::new(&self.capability.binary)
            .arg(image)
            .arg("stdout")
            .arg("-l")
            .arg(&self.lang)
            .arg("--psm")
            .arg(psm.as_str())
            .arg("tsv")
            .output()
            .map_err(|e| OcrError::Spawn {
                bin: self.capability.binary.clone(),
                err: e.to_string(),
            })?;
        if !out.status.success() {
            return Err(OcrError::Failed {
                bin: self.capability.binary.clone(),
                status: out.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        let tsv = String::from_utf8_lossy(&out.stdout);
        parse_tsv(&tsv, w, h)
    }

    /// Recognize over raw image bytes (temp file, removed on drop).
    pub fn recognize_bytes(&self, bytes: &[u8], ext: &str, psm: Psm) -> Result<OcrPage, OcrError> {
        let tmp = TempArtifact::persist("img", ext, bytes)?;
        self.recognize(tmp.path(), psm)
    }
}

/// OCR results for whole pages: the backend label plus per-page data.
pub struct OcrPdfResult {
    /// Honest `name|version` of the backend that produced these pages.
    pub label: String,
    /// `(1-based page number, recognized page)` in the requested order.
    pub pages: Vec<(u32, OcrPage)>,
}

/// OCR specific pages of a PDF with an explicit page-segmentation mode.
/// Temp raster pages are removed on return.
pub fn ocr_pdf_page_data(
    pdf_bytes: &[u8],
    pages: &[u32],
    psm: Psm,
) -> Result<OcrPdfResult, OcrError> {
    let t = Tesseract::detect().ok_or_else(|| OcrError::BackendMissing {
        backend: "tesseract".into(),
        capability: "ocr".into(),
        reason: "no `tesseract` binary on PATH — this environment cannot read image-only pdf pages"
            .into(),
    })?;
    let r = crate::render::Pdftoppm::detect().ok_or_else(|| OcrError::BackendMissing {
        backend: "pdftoppm".into(),
        capability: "pdf page rasterization".into(),
        reason: "no `pdftoppm` binary on PATH — scanned pdf pages cannot be rasterized for ocr"
            .into(),
    })?;
    let pdf = TempArtifact::persist("doc", "pdf", pdf_bytes)?;
    let label = t.label();
    let mut out = Vec::new();
    for &p in pages {
        let png = r.render_page(pdf.path(), p)?;
        let page = t.recognize(png.path(), psm)?;
        out.push((p, page));
    }
    Ok(OcrPdfResult { label, pages: out })
}

/// OCR specific pages of a PDF; blocks plus the honest extractor label used
/// for provenance (production corpus path — `Psm::Auto`).
pub(crate) fn ocr_pdf_pages_inner(
    pdf_bytes: &[u8],
    pages: &[u32],
) -> Result<(Vec<Block>, String), OcrError> {
    let res = ocr_pdf_page_data(pdf_bytes, pages, Psm::Auto)?;
    let mut blocks = Vec::new();
    for (p, page) in &res.pages {
        blocks.extend(page.to_blocks(*p));
    }
    Ok((blocks, res.label))
}

/// OCR specific pages of a PDF; public wrapper used by callers that only
/// need the blocks (e.g. CLI `ingest ocr`).
pub fn ocr_pdf_pages(pdf_bytes: &[u8], pages: &[u32]) -> Result<Vec<Block>, OcrError> {
    ocr_pdf_pages_inner(pdf_bytes, pages).map(|(b, _)| b)
}

/// Full OCR pass over an image-only PDF: every page rasterized + recognized.
pub fn ocr_pdf_document(
    bytes: &[u8],
    confidence: crate::Confidence,
) -> Result<NormalizedDocumentV1, OcrError> {
    let pages = crate::pdf::count_pages(bytes);
    let page_nums: Vec<u32> = (1..=pages).collect();
    let (blocks, label) = ocr_pdf_pages_inner(bytes, &page_nums)?;
    let content_sha256 = sha256_hex_for(bytes);
    Ok(NormalizedDocumentV1 {
        schema_version: 1,
        doc_id: format!("doc/{content_sha256}"),
        version: 1,
        content_sha256,
        format: crate::FormatKind::Pdf,
        size_bytes: bytes.len() as u64,
        pages,
        blocks,
        created_at: openrt_core::now().to_rfc3339(),
        extractor: label,
        detection_confidence: confidence,
        modality: Modality::Text,
        media: None,
    })
}

/// Per-page routing fill: pages with no text layer are OCR'd and merged into
/// an existing born-digital document (block keys renumbered in reading
/// order; the extractor label records both real producers).
pub fn fill_pdf_pages(
    mut doc: NormalizedDocumentV1,
    bytes: &[u8],
    missing: &[u32],
) -> Result<NormalizedDocumentV1, OcrError> {
    let (ocr_blocks, label) = ocr_pdf_pages_inner(bytes, missing)?;
    doc.blocks.extend(ocr_blocks);
    doc.blocks.sort_by_key(|b| b.page.unwrap_or(0));
    for (i, b) in doc.blocks.iter_mut().enumerate() {
        b.key = block_key(i);
    }
    doc.extractor = format!("{}; {}", doc.extractor, label);
    Ok(doc)
}

/// OCR an image document (PNG/JPEG/TIFF) through tesseract. The document
/// stores the real recognized text with bbox/confidence provenance; modality
/// records the source family (image).
pub fn ocr_image_document(
    bytes: &[u8],
    kind: crate::FormatKind,
    confidence: crate::Confidence,
) -> Result<NormalizedDocumentV1, OcrError> {
    let t = Tesseract::detect().ok_or_else(|| OcrError::BackendMissing {
        backend: "tesseract".into(),
        capability: "ocr".into(),
        reason: "no `tesseract` binary on PATH — image text extraction unavailable".into(),
    })?;
    let ext = match kind {
        crate::FormatKind::Png => "png",
        crate::FormatKind::Jpeg => "jpg",
        crate::FormatKind::Tiff => "tif",
        _ => "img",
    };
    let page = t.recognize_bytes(bytes, ext, Psm::Auto)?;
    let blocks = page.to_blocks(1);
    let content_sha256 = sha256_hex_for(bytes);
    Ok(NormalizedDocumentV1 {
        schema_version: 1,
        doc_id: format!("doc/{content_sha256}"),
        version: 1,
        content_sha256,
        format: kind,
        size_bytes: bytes.len() as u64,
        pages: 1,
        blocks,
        created_at: openrt_core::now().to_rfc3339(),
        extractor: t.label(),
        detection_confidence: confidence,
        modality: Modality::Image,
        media: {
            let dims = image_dims(bytes);
            let color_space = image_color(bytes).map(str::to_string);
            if dims.is_none() && color_space.is_none() {
                None
            } else {
                Some(MediaMeta {
                    width: dims.map(|(w, _)| w),
                    height: dims.map(|(_, h)| h),
                    color_space,
                    ..MediaMeta::default()
                })
            }
        },
    })
}

/// Public page count over PDF bytes (used by CLI `ingest ocr`).
pub fn pdf_page_count(bytes: &[u8]) -> u32 {
    crate::pdf::count_pages(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Canonical tesseract-5 TSV shape (header + one word row + non-word
    /// placeholder rows). Fixture output only — no OCR is claimed here; the
    /// parser's honesty is what is under test.
    const TSV: &str = "\
level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext
1\t1\t0\t0\t0\t0\t0\t0\t800\t600\t-1\t
5\t1\t1\t1\t1\t1\t40\t60\t120\t40\t96.31\tHELLO
5\t1\t1\t1\t1\t2\t180\t62\t90\t38\t91.00\tWORLD
5\t1\t2\t2\t2\t1\t40\t200\t300\t45\t-1\t
5\t1\t2\t2\t2\t2\t40\t200\t140\t45\t75.50\tsecond
5\t1\t2\t2\t2\t3\t190\t200\t150\t45\t-1\t";

    #[test]
    fn tsv_parse_words_boxes_confidence() {
        let page = parse_tsv(TSV, 800, 600).unwrap();
        assert_eq!(page.words.len(), 3); // three conf=-1 placeholder rows dropped
        assert_eq!(page.words[0].text, "HELLO");
        assert!((page.words[0].confidence - 0.9631).abs() < 1e-4);
        assert!((page.words[0].bbox.left - 40.0 / 800.0).abs() < 1e-6);
        assert!((page.words[0].bbox.bottom - 100.0 / 600.0).abs() < 1e-6);
        assert_eq!(page.words[2].text, "second");
        assert_eq!(page.words[2].block, 2);
    }

    #[test]
    fn regions_group_real_boxes() {
        let page = parse_tsv(TSV, 800, 600).unwrap();
        let regions = page.regions();
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].block, 1);
        assert_eq!(regions[0].text, "HELLO WORLD");
        assert!(regions[0].confidence > 0.9);
        assert_eq!(regions[1].block, 2);
        assert_eq!(regions[1].text, "second");
        // Boxes are unions of the word boxes.
        assert!(regions[0].bbox.left <= page.words[0].bbox.left);
        assert!(regions[0].bbox.right >= page.words[1].bbox.right);
    }

    #[test]
    fn blocks_carry_page_bbox_confidence() {
        let page = parse_tsv(TSV, 800, 600).unwrap();
        let blocks = page.to_blocks(3);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].page, Some(3));
        assert_eq!(blocks[0].source.page, Some(3));
        assert!(blocks[0].source.bbox.is_some());
        let conf = blocks[0].source.confidence.unwrap();
        assert!((0.0..=1.0).contains(&conf));
        assert!(conf > 0.9);
        assert_eq!(blocks[0].source.method, ExtractMethod::Ocr);
        let label = blocks[0].source.label();
        assert!(label.starts_with("page=3,conf="), "label: {label}");
        assert!(label.ends_with("%: @ocr"), "label: {label}");
    }

    #[test]
    fn empty_tsv_is_empty_page() {
        let page = parse_tsv("level\tpage_num\t...\ttext\n", 100, 100).unwrap();
        assert!(page.words.is_empty());
        assert!(page.to_blocks(1).is_empty());
    }

    #[test]
    fn zero_dimensions_rejected() {
        assert!(matches!(parse_tsv("x", 0, 0), Err(OcrError::Dims(_))));
    }

    #[test]
    fn png_dims_parsed() {
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        assert_eq!(image_dims(&png), Some((640, 480)));
    }

    #[test]
    fn png_color_parsed() {
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        png.push(8); // bit depth
        png.push(0); // color type: grayscale
        assert_eq!(image_color(&png), Some("gray"));
        png[25] = 6; // color type: RGBA
        assert_eq!(image_color(&png), Some("rgba"));
        png[25] = 255; // invalid color type
        assert_eq!(image_color(&png), None);
    }

    #[test]
    fn jpeg_color_parsed() {
        // SOF0 with 3 components -> rgb (same shape as jpeg_dims fixture).
        let mut j = vec![0xff, 0xd8, 0xff, 0xc0, 0x00, 0x11, 0x08];
        j.extend_from_slice(&480u16.to_be_bytes());
        j.extend_from_slice(&640u16.to_be_bytes());
        j.push(3); // component count
        j.extend_from_slice(&[0x01, 0x11, 0x00]);
        assert_eq!(image_color(&j), Some("rgb"));
        let mut gray = j.clone();
        gray[11] = 1;
        assert_eq!(image_color(&gray), Some("gray"));
    }

    #[test]
    fn unknown_container_has_no_color() {
        assert_eq!(image_color(b"not an image"), None);
        // TIFF headers do not carry cheaply-readable color info here.
        assert_eq!(image_color(b"II\x2a\x00more bytes here"), None);
    }

    #[test]
    fn jpeg_dims_parsed() {
        // SOI + SOF0 segment with 480x640.
        let mut j = vec![0xff, 0xd8, 0xff, 0xc0, 0x00, 0x11, 0x08];
        j.extend_from_slice(&480u16.to_be_bytes());
        j.extend_from_slice(&640u16.to_be_bytes());
        j.extend_from_slice(&[0x03, 0x01, 0x11, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01]);
        assert_eq!(image_dims(&j), Some((640, 480)));
    }

    #[test]
    fn tiff_dims_parsed_le() {
        let mut t = vec![b'I', b'I', 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00];
        t.extend_from_slice(&2u16.to_le_bytes()); // 2 entries
                                                  // ImageWidth (256), SHORT (3), count 1, value 300
        t.extend_from_slice(&256u16.to_le_bytes());
        t.extend_from_slice(&3u16.to_le_bytes());
        t.extend_from_slice(&1u32.to_le_bytes());
        t.extend_from_slice(&300u16.to_le_bytes());
        t.extend_from_slice(&0u16.to_le_bytes());
        // ImageLength (257), SHORT (3), count 1, value 200
        t.extend_from_slice(&257u16.to_le_bytes());
        t.extend_from_slice(&3u16.to_le_bytes());
        t.extend_from_slice(&1u32.to_le_bytes());
        t.extend_from_slice(&200u16.to_le_bytes());
        t.extend_from_slice(&0u16.to_le_bytes());
        assert_eq!(image_dims(&t), Some((300, 200)));
    }

    #[test]
    fn unknown_bytes_have_no_dims() {
        assert_eq!(image_dims(b"not an image"), None);
    }

    #[test]
    fn backend_missing_is_structured() {
        // The error type itself carries backend/capability/reason — the shape
        // the mission requires for honest MODEL/BACKEND absence.
        let e = OcrError::BackendMissing {
            backend: "tesseract".into(),
            capability: "ocr".into(),
            reason: "no `tesseract` binary on PATH".into(),
        };
        let s = e.to_string();
        assert!(s.contains("tesseract"));
        assert!(s.contains("ocr"));
        assert!(s.contains("PATH"));
    }

    #[test]
    fn temp_artifact_removed_on_drop() {
        let path = {
            let a = TempArtifact::persist("t", "bin", b"data").unwrap();
            let p = a.path().to_path_buf();
            assert!(p.exists());
            p
        };
        assert!(!path.exists(), "drop must remove the temp file");
    }
}
