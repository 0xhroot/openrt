//! Phase 4 — multimodal intake, normalised document model, and chunking.
//!
//! # Posture
//!
//! * Genuine extraction only. A parser that cannot extract text (encrypted PDF,
//!   unsupported structure, no text layer) fails with an explicit error — no
//!   fabricated pages, text, timestamps, or confidence values.
//! * Format detection is magic-byte first; the declared extension is a hint
//!   that must agree to raise confidence, never a reason to claim a format.
//! * Content is treats as data: nothing in an ingested document can escalate
//!   permissions, change modes, touch the registry, or dispatch tools. The
//!   container block never executes anything it parses.
//! * Path access is jailed (canonical + symlink rejection, SEC-01). Content is
//!   hashed at intake for dedup, versioning, and evidence integrity.

pub mod asr;
pub mod capabilities;
pub mod chunk;
pub mod corpus;
pub mod detect;
pub mod html;
pub mod jail;
pub mod media;
pub mod normalize;
pub mod ocr;
pub mod office;
pub mod pdf;
pub mod render;

/// Version of the persisted normalised document schema.
pub const DOCUMENT_SCHEMA_VERSION: u32 = 1;

pub use detect::{detect, Confidence, DetectReport, DetectionStatus, FormatKind};
pub use html::normalize_html;
pub use normalize::{
    normalize_csv, normalize_json, normalize_markdown, normalize_pdf, normalize_text, Block,
    BlockKind, BlockSource, ExtractMethod, MediaMeta, Modality, NormalizedDocumentV1,
};
pub use ocr::{
    image_color, image_dims, ocr_image_document, ocr_pdf_document, ocr_pdf_page_data,
    ocr_pdf_pages, pdf_page_count, OcrError, OcrPage, OcrPdfResult, OcrRegion, OcrWord, Psm,
    Tesseract,
};
pub use office::{normalize_docx, normalize_pptx, normalize_xlsx};
pub use render::Pdftoppm;

/// Key for a document version inside `corpus.documents`:
/// `doc-<content-sha256-prefix>-v<version>`. The prefix keeps listing localised
/// and the version makes every revision addressable and immutable.
pub fn document_key(doc_id: &str, version: u32) -> String {
    format!("{doc_id}-v{version}")
}

/// Safe namespace key for a stable document id (ids are already `doc/<hash>`).
pub fn ns_key_doc_id(doc_id: &str) -> String {
    openrt_core::encode_safe_key(doc_id)
}

/// SHA-256 hex of a byte slice (content addressing + dedup + evidence).
pub fn sha256_hex_for(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// Deterministic block key: `b{n}` in reading order (shared by parsers).
pub(crate) fn block_key(index: usize) -> String {
    format!("b{index}")
}
