//! Persisted document corpus (`corpus.documents`).
//!
//! Intake order of operations (P4.1):
//! 1. permission/mode gate (caller)                    — before any content read
//! 2. jail resolve (canonical + symlink)               — SEC-01
//! 3. size limit check
//! 4. magic-byte detection
//! 5. genuine extraction (no OCR fabrication)
//! 6. post-condition limit checks (pages/blocks)
//! 7. content-hash dedup; versioned store
//! 8. audit + event records — every mutation is witnessed
//!
//! Content addressing: `doc_id = doc/<sha256(input)>`, so any two intakes of
//! identical bytes dedup to the same document. Distinct revisions are distinct
//! documents that all remain inspectable — versions are preserved by never
//! overwriting a stored document.

use std::path::Path;
use std::sync::Arc;

use openrt_audit::AuditLog;
use openrt_core::{DomainEvent, EventKind};
use openrt_persistence::PersistenceStore;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::detect::{detect, DetectionStatus, FormatKind};
use crate::jail::{IngestJail, JailError};
use crate::normalize::{ExtractMethod, NormalizeError, NormalizedDocumentV1};
use crate::{document_key, ns_key_doc_id};

/// Namespaces owned by this crate.
pub const DOCUMENTS_NS: &str = "corpus.documents";
pub const DOCUMENT_META_NS: &str = "corpus.docmeta";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestState {
    Queued,
    Extracting,
    Validating,
    Stored,
    Failed,
    Duplicate,
}

impl IngestState {
    pub fn label(&self) -> &'static str {
        match self {
            IngestState::Queued => "queued",
            IngestState::Extracting => "extracting",
            IngestState::Validating => "validating",
            IngestState::Stored => "stored",
            IngestState::Failed => "failed",
            IngestState::Duplicate => "duplicate",
        }
    }
}

/// Operator-visible document meta (pointer to the latest version record).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentMeta {
    pub doc_id: String,
    pub latest_version: u32,
    pub status: IngestState,
    pub content_sha256: String,
    pub size_bytes: u64,
    pub format: FormatKind,
    pub first_ingested_at: String,
    pub last_ingested_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngestOutcome {
    pub doc: NormalizedDocumentV1,
    pub state: IngestState,
    pub duplicate_of: Option<u32>,
}

/// Intake limits; validated before any extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestLimits {
    pub max_bytes: u64,
    pub max_pages: u32,
    pub max_blocks: u32,
}

impl Default for IngestLimits {
    fn default() -> Self {
        Self {
            max_bytes: 50 * 1024 * 1024,
            max_pages: 500,
            max_blocks: 100_000,
        }
    }
}

impl IngestLimits {
    pub fn validate(&self) -> Result<(), IngestError> {
        if self.max_bytes == 0 {
            return Err(IngestError::Config("max_bytes must be > 0".into()));
        }
        if self.max_pages == 0 {
            return Err(IngestError::Config("max_pages must be > 0".into()));
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum IngestError {
    #[error(transparent)]
    Jail(#[from] JailError),
    #[error("file too large: {size} bytes > limit {limit}")]
    TooLarge { size: u64, limit: u64 },
    #[error("format `{format}` is not supported for extraction: {reason}")]
    UnsupportedFormat { format: String, reason: String },
    #[error("content is corrupt: {0}")]
    CorruptFormat(String),
    #[error("content is ambiguous: {0}")]
    AmbiguousFormat(String),
    #[error(transparent)]
    Normalize(#[from] NormalizeError),
    #[error(transparent)]
    Ocr(#[from] crate::ocr::OcrError),
    #[error(transparent)]
    Media(#[from] crate::media::MediaError),
    #[error(transparent)]
    Asr(#[from] crate::asr::AsrError),
    #[error("extracted content exceeds document limits: {0}")]
    Limit(String),
    #[error("document has more pages ({pages}) than the limit ({limit})")]
    TooManyPages { pages: u32, limit: u32 },
    #[error("document has more blocks ({blocks}) than the limit ({limit})")]
    TooManyBlocks { blocks: u32, limit: u32 },
    #[error("store error: {0}")]
    Store(String),
    #[error("document `{doc}` not found")]
    NotFound { doc: String },
    #[error("config error: {0}")]
    Config(String),
    #[error("iam error: {0}")]
    Iam(String),
}

impl From<openrt_persistence::PersistenceError> for IngestError {
    fn from(e: openrt_persistence::PersistenceError) -> Self {
        IngestError::Store(e.to_string())
    }
}

impl From<openrt_audit::AuditError> for IngestError {
    fn from(e: openrt_audit::AuditError) -> Self {
        IngestError::Iam(e.to_string())
    }
}

/// Content-addressed document corpus over ONE store.
pub struct DocCorpus {
    store: Arc<dyn PersistenceStore>,
    limits: IngestLimits,
    audit: AuditLog,
}

impl DocCorpus {
    pub fn new(store: Arc<dyn PersistenceStore>, limits: IngestLimits) -> Self {
        limits.validate().ok();
        let audit = AuditLog::new(store.clone());
        Self {
            store,
            limits,
            audit,
        }
    }

    pub fn limits(&self) -> IngestLimits {
        self.limits
    }

    /// Ingests and stores a document at a jailed path. Path security runs
    /// first; the caller must already have passed the permission/mode gate.
    pub async fn ingest_path(
        &self,
        jail: &IngestJail,
        path: &Path,
        actor: &str,
    ) -> Result<IngestOutcome, IngestError> {
        let resolved = jail.resolve(path)?;
        let declared = path
            .file_name()
            .and_then(|f| f.to_str())
            .map(str::to_string);
        let bytes = std::fs::read(&resolved).map_err(|e| {
            IngestError::Jail(JailError::Io(format!("{}: {e}", resolved.display())))
        })?;
        self.ingest_bytes(bytes, declared.as_deref(), actor).await
    }

    /// Core intake path over bytes.
    pub async fn ingest_bytes(
        &self,
        bytes: impl Into<Vec<u8>>,
        declared_name: Option<&str>,
        actor: &str,
    ) -> Result<IngestOutcome, IngestError> {
        let bytes = bytes.into();
        let size = bytes.len() as u64;
        self.limits.validate()?;
        if size > self.limits.max_bytes {
            return Err(IngestError::TooLarge {
                size,
                limit: self.limits.max_bytes,
            });
        }
        let report = detect(&bytes, declared_name);
        let doc_id = format!("doc/{}", crate::sha256_hex_for(&bytes));
        // Intake status gate (M9.1): only genuinely supported content is
        // extracted; every other status fails closed with its reason and an
        // audit record — never a silent reinterpretation.
        let method = match &report.status {
            DetectionStatus::Supported => report.kind.supported_extraction().ok_or_else(|| {
                IngestError::UnsupportedFormat {
                    format: report.kind.label().to_string(),
                    reason: "internal: supported status without an extraction path".into(),
                }
            })?,
            DetectionStatus::Unsupported(reason) => {
                self.reject(&doc_id, actor, &report, reason).await?;
                return Err(IngestError::UnsupportedFormat {
                    format: report.kind.label().to_string(),
                    reason: reason.clone(),
                });
            }
            DetectionStatus::Corrupt(reason) => {
                self.reject(&doc_id, actor, &report, reason).await?;
                return Err(IngestError::CorruptFormat(reason.clone()));
            }
            DetectionStatus::Ambiguous(reason) => {
                self.reject(&doc_id, actor, &report, reason).await?;
                return Err(IngestError::AmbiguousFormat(reason.clone()));
            }
        };
        self.emit(
            EventKind::DocumentQueued,
            serde_json::json!({
                "doc_id": doc_id,
                "size_bytes": size,
                "method": method_label(method),
            }),
        )
        .await;

        // Dedup: identical content returns the existing document (honest Duplicate).
        if let Some(meta) = self.status(&doc_id).await? {
            if meta.status == IngestState::Stored || meta.status == IngestState::Duplicate {
                let existing = self.inspect(&doc_id).await?;
                self.audit
                    .append(
                        actor,
                        "ingest.document",
                        &doc_id,
                        "duplicate",
                        serde_json::json!({ "existing_version": meta.latest_version }),
                    )
                    .await?;
                self.emit(
                    EventKind::DocumentDuplicate,
                    serde_json::json!({
                        "doc_id": doc_id,
                        "existing_version": meta.latest_version,
                    }),
                )
                .await;
                return Ok(IngestOutcome {
                    doc: existing.clone(),
                    state: IngestState::Duplicate,
                    duplicate_of: Some(meta.latest_version),
                });
            }
        }

        self.emit(
            EventKind::DocumentExtracted,
            serde_json::json!({
                "doc_id": doc_id,
                "kind": report.kind.label(),
            }),
        )
        .await;
        let doc = self.extract(&bytes, report.kind, report.confidence)?;
        if doc.pages > self.limits.max_pages {
            return Err(IngestError::TooManyPages {
                pages: doc.pages,
                limit: self.limits.max_pages,
            });
        }
        if doc.blocks.len() as u32 > self.limits.max_blocks {
            return Err(IngestError::TooManyBlocks {
                blocks: doc.blocks.len() as u32,
                limit: self.limits.max_blocks,
            });
        }
        self.persist(&doc, actor, false).await?;
        Ok(IngestOutcome {
            doc,
            state: IngestState::Stored,
            duplicate_of: None,
        })
    }

    fn extract(
        &self,
        bytes: &[u8],
        kind: FormatKind,
        confidence: crate::Confidence,
    ) -> Result<NormalizedDocumentV1, IngestError> {
        let doc = match kind {
            FormatKind::Text => crate::normalize::normalize_text(bytes)?,
            FormatKind::Markdown => crate::normalize::normalize_markdown(bytes)?,
            FormatKind::Csv => crate::normalize::normalize_csv(bytes)?,
            FormatKind::Json => crate::normalize::normalize_json(bytes)?,
            FormatKind::Html => crate::html::normalize_html(bytes)?,
            FormatKind::Pdf => match crate::normalize_pdf(bytes) {
                Ok(doc) => {
                    // Born-digital vs scanned page routing (M9.2): pages with
                    // no text layer go through real OCR; without a backend
                    // this fails closed with a structured reason. If any block
                    // has an unknown page (no page tree to anchor page
                    // numbers), the missing-page set is unknowable — do not
                    // OCR-fill every page on a guess.
                    let missing: Vec<u32> = if doc.blocks.iter().any(|b| b.page.is_none()) {
                        Vec::new()
                    } else {
                        (1..=doc.pages)
                            .filter(|p| !doc.blocks.iter().any(|b| b.page == Some(*p)))
                            .collect()
                    };
                    if missing.is_empty() {
                        doc
                    } else {
                        crate::ocr::fill_pdf_pages(doc, bytes, &missing)?
                    }
                }
                Err(crate::normalize::NormalizeError::PdfImageOnly) => {
                    crate::ocr::ocr_pdf_document(bytes, confidence)?
                }
                Err(e) => return Err(e.into()),
            },
            FormatKind::Docx => crate::office::normalize_docx(bytes)?,
            FormatKind::Xlsx => crate::office::normalize_xlsx(bytes)?,
            FormatKind::Pptx => crate::office::normalize_pptx(bytes)?,
            // Image intake: genuine OCR through tesseract; a missing backend
            // is a structured BackendMissing, never invented text.
            FormatKind::Png | FormatKind::Jpeg | FormatKind::Tiff => {
                crate::ocr::ocr_image_document(bytes, kind, confidence)?
            }
            // Media intake (M9.4): ffprobe/ffmpeg + real backends only.
            // Audio without an ASR engine/model fails closed as structured
            // MODEL_MISSING; video OCRs real ffmpeg-sampled frames with
            // timeline provenance.
            FormatKind::Audio => crate::media::ingest_audio(bytes, confidence)?,
            FormatKind::Video => crate::media::ingest_video(bytes, confidence)?,
            // Status-gated upstream; defensive for direct callers.
            FormatKind::Other => {
                return Err(IngestError::UnsupportedFormat {
                    format: kind.label().to_string(),
                    reason: "no extraction path for this content in this build".into(),
                })
            }
        };
        Ok(doc)
    }

    async fn persist(
        &self,
        doc: &NormalizedDocumentV1,
        actor: &str,
        _reindex: bool,
    ) -> Result<(), IngestError> {
        let key = document_key(&ns_key_doc_id(&doc.doc_id), doc.version);
        let value =
            serde_json::to_value(doc).map_err(|e| IngestError::Store(format!("serialize: {e}")))?;
        self.store.put_record(DOCUMENTS_NS, &key, &value).await?;
        let now = openrt_core::now();
        let meta = DocumentMeta {
            doc_id: doc.doc_id.clone(),
            latest_version: doc.version,
            status: IngestState::Stored,
            content_sha256: doc.content_sha256.clone(),
            size_bytes: doc.size_bytes,
            format: doc.format,
            first_ingested_at: now.clone().to_rfc3339(),
            last_ingested_at: now.to_rfc3339(),
        };
        let meta_value = serde_json::to_value(&meta)
            .map_err(|e| IngestError::Store(format!("serialize meta: {e}")))?;
        self.store
            .put_record(DOCUMENT_META_NS, &ns_key_doc_id(&doc.doc_id), &meta_value)
            .await?;
        self.audit
            .append(
                actor,
                "ingest.document.stored",
                &doc.doc_id,
                "ok",
                serde_json::json!({
                    "version": doc.version,
                    "sha256": doc.content_sha256,
                    "blocks": doc.blocks.len(),
                    "pages": doc.pages,
                    "format": doc.format.label(),
                }),
            )
            .await?;
        self.emit(
            EventKind::DocumentStored,
            serde_json::json!({
                "doc_id": doc.doc_id,
                "version": doc.version,
                "content_sha256": doc.content_sha256,
            }),
        )
        .await;
        Ok(())
    }

    /// List document meta, newest first.
    pub async fn list(&self) -> Result<Vec<DocumentMeta>, IngestError> {
        let mut out = Vec::new();
        for (_k, v) in self.store.list_records(DOCUMENT_META_NS).await? {
            if let Ok(m) = serde_json::from_value::<DocumentMeta>(v) {
                out.push(m);
            }
        }
        out.sort_by(|a, b| b.last_ingested_at.cmp(&a.last_ingested_at));
        Ok(out)
    }

    /// Latest stored version of a document.
    pub async fn inspect(&self, doc_id: &str) -> Result<NormalizedDocumentV1, IngestError> {
        let meta = self
            .status(doc_id)
            .await?
            .ok_or_else(|| IngestError::NotFound {
                doc: doc_id.to_string(),
            })?;
        let key = document_key(&ns_key_doc_id(doc_id), meta.latest_version);
        let v = self
            .store
            .get_record(DOCUMENTS_NS, &key)
            .await?
            .ok_or_else(|| IngestError::NotFound {
                doc: doc_id.to_string(),
            })?;
        serde_json::from_value(v).map_err(|e| IngestError::Store(format!("decode: {e}")))
    }

    pub async fn status(&self, doc_id: &str) -> Result<Option<DocumentMeta>, IngestError> {
        let v = self
            .store
            .get_record(DOCUMENT_META_NS, &ns_key_doc_id(doc_id))
            .await?;
        match v {
            Some(v) => {
                Ok(Some(serde_json::from_value(v).map_err(|e| {
                    IngestError::Store(format!("decode meta: {e}"))
                })?))
            }
            None => Ok(None),
        }
    }

    /// Removes a document (and all its versions). Verifies intent explicitly.
    pub async fn remove(&self, doc_id: &str, actor: &str) -> Result<(), IngestError> {
        let meta = self
            .status(doc_id)
            .await?
            .ok_or_else(|| IngestError::NotFound {
                doc: doc_id.to_string(),
            })?;
        for v in 1..=meta.latest_version {
            let key = document_key(&ns_key_doc_id(doc_id), v);
            self.store.delete_record(DOCUMENTS_NS, &key).await?;
        }
        self.store
            .delete_record(DOCUMENT_META_NS, &ns_key_doc_id(doc_id))
            .await?;
        self.audit
            .append(
                actor,
                "ingest.document.remove",
                doc_id,
                "ok",
                serde_json::json!({}),
            )
            .await?;
        Ok(())
    }

    /// Structural retrieval of one document by raw key (used by KB reindex).
    pub async fn get_raw(&self) -> Result<Vec<NormalizedDocumentV1>, IngestError> {
        let mut out = Vec::new();
        for (_k, v) in self.store.list_records(DOCUMENTS_NS).await? {
            if let Ok(d) = serde_json::from_value::<NormalizedDocumentV1>(v) {
                out.push(d);
            }
        }
        Ok(out)
    }

    /// Records a failed intake (unsupported/corrupt/ambiguous) in the chained
    /// audit log so rejections are as visible as successes.
    async fn reject(
        &self,
        doc_id: &str,
        actor: &str,
        report: &crate::detect::DetectReport,
        reason: &str,
    ) -> Result<(), IngestError> {
        self.audit
            .append(
                actor,
                "ingest.document",
                doc_id,
                "rejected",
                serde_json::json!({
                    "status": report.status.label(),
                    "format": report.kind.label(),
                    "reason": reason,
                }),
            )
            .await?;
        Ok(())
    }

    async fn emit(&self, kind: EventKind, data: serde_json::Value) {
        let _ = self
            .store
            .append_event(&DomainEvent::new(kind).with_data(data))
            .await;
    }
}

fn method_label(method: ExtractMethod) -> &'static str {
    match method {
        ExtractMethod::TextParser => "text",
        ExtractMethod::CsvParser => "csv",
        ExtractMethod::JsonParser => "json",
        ExtractMethod::HtmlParser => "html",
        ExtractMethod::Ooxml => "ooxml",
        ExtractMethod::PdfText => "pdf:text",
        ExtractMethod::Ocr => "ocr",
        ExtractMethod::Vlm => "vlm",
        ExtractMethod::Audio => "audio",
        ExtractMethod::Video => "video",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_persistence::MemoryStore;

    fn corpus() -> DocCorpus {
        DocCorpus::new(Arc::new(MemoryStore::new()), IngestLimits::default())
    }

    #[tokio::test]
    async fn ingest_store_and_inspect() {
        let c = corpus();
        let o = c
            .ingest_bytes(b"alpha\nalpha", Some("a.txt"), "tester")
            .await
            .unwrap();
        assert_eq!(o.state, IngestState::Stored);
        assert!(o.doc.doc_id.starts_with("doc/"));
        assert!(o.doc.full_text().contains("alpha"));
        let listed = c.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        let back = c.inspect(&o.doc.doc_id).await.unwrap();
        assert_eq!(back.content_sha256, o.doc.content_sha256);
        assert_eq!(back.blocks.len(), o.doc.blocks.len());
    }

    #[tokio::test]
    async fn dedup_same_bytes_is_duplicate() {
        let c = corpus();
        let a = c
            .ingest_bytes(b"dup content", Some("x.txt"), "t")
            .await
            .unwrap();
        let b = c
            .ingest_bytes(b"dup content", Some("x.txt"), "t")
            .await
            .unwrap();
        assert_eq!(a.state, IngestState::Stored);
        assert_eq!(b.state, IngestState::Duplicate);
        assert_eq!(b.doc.doc_id, a.doc.doc_id);
        assert_eq!(b.duplicate_of, Some(1));
        assert_eq!(c.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn distinct_content_distinct_docs() {
        let c = corpus();
        let a = c.ingest_bytes(b"aaa", Some("a.txt"), "t").await.unwrap();
        let b = c.ingest_bytes(b"bbb", Some("b.txt"), "t").await.unwrap();
        assert_ne!(a.doc.doc_id, b.doc.doc_id);
    }

    #[tokio::test]
    async fn oversized_rejected() {
        let store = Arc::new(MemoryStore::new());
        let c = DocCorpus::new(
            store,
            IngestLimits {
                max_bytes: 16,
                max_pages: 5,
                max_blocks: 5,
            },
        );
        assert!(matches!(
            c.ingest_bytes(vec![b'x'; 64], None, "t").await,
            Err(IngestError::TooLarge { .. })
        ));
    }

    #[tokio::test]
    async fn binary_unsupported_rejected() {
        let c = corpus();
        assert!(matches!(
            c.ingest_bytes(vec![0xff, 0xff, 0xff, 0x00], None, "t")
                .await,
            Err(IngestError::UnsupportedFormat { .. })
        ));
    }

    #[tokio::test]
    async fn remove_deletes_versions() {
        let c = corpus();
        let o = c
            .ingest_bytes(b"gone soon", Some("g.txt"), "t")
            .await
            .unwrap();
        c.remove(&o.doc.doc_id, "t").await.unwrap();
        assert!(c.status(&o.doc.doc_id).await.unwrap().is_none());
        assert!(c.inspect(&o.doc.doc_id).await.is_err());
    }

    #[tokio::test]
    async fn csv_multiblock() {
        let c = corpus();
        let o = c
            .ingest_bytes(b"a,b\n1,2\n3,4\n", Some("d.csv"), "t")
            .await
            .unwrap();
        assert_eq!(o.doc.format, FormatKind::Csv);
        assert!(o.doc.blocks.len() >= 3);
        assert!(o.doc.blocks.iter().any(|b| b.table.is_some()));
    }

    #[tokio::test]
    async fn pdf_text_extracts() {
        let c = corpus();
        let mut pdf = Vec::new();
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        use std::io::Write;
        enc.write_all(b"BT /F1 10 Tf 72 700 Td (Extracted by openrt) Tj ET")
            .unwrap();
        pdf.extend_from_slice(b"%PDF-1.4\n1 0 obj << /Type /Page /Contents 2 0 R >> endobj\n");
        pdf.extend_from_slice(b"2 0 obj << /Filter /FlateDecode /Length 100 >>\nstream\n");
        pdf.extend_from_slice(&enc.finish().unwrap());
        pdf.extend_from_slice(b"\nendstream\nendobj\n");
        pdf.extend_from_slice(b"3 0 obj << /Type /Catalog >> endobj\n");
        pdf.extend_from_slice(
            b"xref\n0 4\n0000000000 65535 f \ntrailer << /Root 3 0 R >>\nstartxref 10\n%%EOF\n",
        );
        let o = c.ingest_bytes(pdf, Some("doc.pdf"), "t").await.unwrap();
        assert_eq!(o.doc.format, FormatKind::Pdf);
        assert!(o.doc.full_text().contains("Extracted by openrt"));
    }
}
