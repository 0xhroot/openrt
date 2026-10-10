//! Phase 9 (TEST 13) — the full end-to-end chain on real backends:
//! multimodal intake (mixed PDF: text layer + raster page through real
//! tesseract OCR) → page-scoped chunking → hybrid retrieval over the
//! knowledge store → hallucination verification (deterministic gate) →
//! injection defense over the answer context.
//!
//! Every leg goes through the production entry points:
//! `Runtime::corpus.ingest_path` → `chunk_normalized` → `persist_chunk` →
//! `hybrid_search` → `Runtime::verify_claim` → `Runtime::evidence_context`.
//! Provenance (page, block keys, text sha) must survive chunking → citation
//! → verdict record; the scan/label legs stay honest (UNTRUSTED is not
//! FLAGGED; a clean corpus produces no audit entries).
//!
//! Gated on the host's real backends (`ocr_capability`/`render_capability`):
//! without them the test skips with a message instead of faking OCR.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use openrt_audit::AuditQuery;
use openrt_config::{Config, RuntimeConfig};
use openrt_evidence::list_trust;
use openrt_ingest::capabilities::{ocr_capability, render_capability};
use openrt_ingest::chunk::{chunk_normalized, ChunkConfig, ChunkStrategy};
use openrt_ingest::jail::IngestJail;
use openrt_knowledge::{
    hybrid_search, load_chunk, persist_chunk, Citation, HybridOptions, PersistedChunk,
    RetrievalSource, RetrieveRequest,
};
use openrt_models::mock::MockProvider;
use openrt_retrieval::context::BLOCK_BEGIN_PREFIX;
use openrt_runtime::{EvidenceContextRequest, RuntimeBuilder};
use openrt_verifier::{
    list_verdicts, Basis, FirewallAction, Grounding, Independence, Verdict, VerifyInput,
    VerifyOptions, MIN_GROUNDING,
};

/// Committed fixture: 1200x400 grayscale JPEG, single word "OPENRT" rendered
/// large and clean (verified readable by tesseract at creation time).
const FIXTURE_JPEG: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/ocr_text.jpg"
));

const KB: &str = "kb-e2e";
const SOURCE: &str = "p9-e2e";
const CLAIM: &str = "openrt appears on a born digital page one";

fn tmp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "openrt-p9-e2e-{prefix}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn build(config: Config) -> Arc<openrt_runtime::Runtime> {
    RuntimeBuilder::new()
        .config(config)
        .store(Arc::new(openrt_persistence::MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
        .build()
        .await
        .unwrap()
}

fn runtime_config(ws: &Path) -> Config {
    Config {
        runtime: RuntimeConfig {
            workspace_dir: Some(ws.to_path_buf()),
            ..RuntimeConfig::default()
        },
        ..Config::default()
    }
}

/// True when this host can genuinely OCR; prints a skip message otherwise.
fn ocr_or_skip() -> bool {
    if ocr_capability().is_none() {
        eprintln!("SKIP: no tesseract backend on PATH — OCR claims not faked");
        return false;
    }
    true
}

/// True when this host can genuinely rasterize PDF pages.
fn render_or_skip() -> bool {
    if render_capability().is_none() {
        eprintln!("SKIP: no pdftoppm backend on PATH — PDF rasterization not faked");
        return false;
    }
    true
}

/// Build a minimal PDF with a proper xref table (same fixture approach as
/// `p9_ocr`): page 1 carries a text layer, page 2 draws the fixture JPEG so
/// it can only be read by OCR.
fn build_pdf(page_specs: &[PdfPage]) -> Vec<u8> {
    let mut objs: Vec<(u32, Vec<u8>)> = Vec::new();
    let kids: Vec<String> = (0..page_specs.len())
        .map(|i| format!("{} 0 R", 3 + i * 2))
        .collect();
    objs.push((1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()));
    objs.push((
        2,
        format!(
            "<< /Type /Pages /Kids [{}] /Count {} >>",
            kids.join(" "),
            page_specs.len()
        )
        .into_bytes(),
    ));
    for (i, spec) in page_specs.iter().enumerate() {
        let page_num = 3 + i as u32 * 2;
        let content_num = page_num + 1;
        let resources = if spec.draws_image {
            " /Resources << /XObject << /Im0 7 0 R >> >>"
        } else {
            ""
        };
        objs.push((
            page_num,
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {} {}] /Contents {} 0 R{resources} >>",
                spec.width, spec.height, content_num
            )
            .into_bytes(),
        ));
        let stream = if spec.draws_image {
            format!("q {} 0 0 {} 0 0 cm /Im0 Do Q", spec.width, spec.height).into_bytes()
        } else {
            format!(
                "BT /F1 40 Tf 60 {} Td ({}) Tj ET",
                spec.height - 60,
                spec.text
            )
            .into_bytes()
        };
        let mut content = format!("<< /Length {} >>\nstream\n", stream.len()).into_bytes();
        content.extend_from_slice(&stream);
        content.extend_from_slice(b"\nendstream");
        objs.push((content_num, content));
    }
    let w = page_specs[0].width;
    let h = page_specs[0].height;
    let mut xobj = format!(
        "<< /Type /XObject /Subtype /Image /Width {w} /Height {h} \
         /ColorSpace /DeviceGray /BitsPerComponent 8 \
         /Filter /DCTDecode /Length {} >>\nstream\n",
        FIXTURE_JPEG.len()
    )
    .into_bytes();
    xobj.extend_from_slice(FIXTURE_JPEG);
    xobj.extend_from_slice(b"\nendstream");
    objs.push((7, xobj));

    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets: Vec<(u32, usize)> = Vec::new();
    for (num, body) in &objs {
        offsets.push((*num, pdf.len()));
        pdf.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref_at = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for (_, off) in &offsets {
        pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer << /Size {} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
            objs.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

#[derive(Clone, Copy)]
struct PdfPage {
    width: u32,
    height: u32,
    draws_image: bool,
    text: &'static str,
}

/// TEST 13 — one document flows through the whole fabric: real multimodal
/// extraction, provenance-preserving chunking, hybrid retrieval, a
/// deterministic verification pass with an honest degradation, and an
/// injection-scanned (clean) answer context — all through production
/// entry points.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_chain_from_multimodal_ingest_to_verified_answer_context() {
    if !ocr_or_skip() || !render_or_skip() {
        return;
    }
    let root = tmp_dir("chain");
    let rt = build(runtime_config(&root)).await;
    let jail = IngestJail::new(root.clone()).unwrap();

    let pdf = build_pdf(&[
        PdfPage {
            width: 1200,
            height: 400,
            draws_image: false,
            text: "Born digital page one telemetry",
        },
        PdfPage {
            width: 1200,
            height: 400,
            draws_image: true,
            text: "",
        },
    ]);
    std::fs::write(root.join("e2e.pdf"), &pdf).unwrap();

    // ------------------------------------------------------------------
    // 1. Multimodal intake: real text layer on page 1, real OCR on page 2.
    // ------------------------------------------------------------------
    let out = rt
        .corpus
        .ingest_path(&jail, Path::new("e2e.pdf"), SOURCE)
        .await
        .unwrap();
    assert_eq!(out.doc.pages, 2);
    assert!(
        out.doc.extractor.contains("openrt-pdf-text") && out.doc.extractor.contains("tesseract|"),
        "both real producers recorded: {}",
        out.doc.extractor
    );
    let ocr_on_p2 = out.doc.blocks.iter().any(|b| {
        b.page == Some(2)
            && b.source.method == openrt_ingest::ExtractMethod::Ocr
            && b.text.to_uppercase().contains("OPENRT")
            && b.source.bbox.is_some()
            && b.source.confidence.is_some()
    });
    assert!(ocr_on_p2, "page 2 OCR block with provenance missing");

    // ------------------------------------------------------------------
    // 2. Page-scoped chunking keeps provenance (page + block keys + sha).
    // ------------------------------------------------------------------
    let chunks = chunk_normalized(
        &out.doc,
        &ChunkConfig {
            strategy: ChunkStrategy::Page,
            ..ChunkConfig::default()
        },
    )
    .expect("chunking the real document");
    assert_eq!(chunks.len(), 2, "{:?}", chunks);
    let page1 = chunks
        .iter()
        .find(|c| c.page == Some(1))
        .expect("page 1 chunk");
    let page2 = chunks
        .iter()
        .find(|c| c.page == Some(2))
        .expect("page 2 chunk");
    assert!(page1.text.contains("Born digital"), "{}", page1.text);
    assert!(
        page2.text.to_uppercase().contains("OPENRT"),
        "OCR text lost in chunking: {}",
        page2.text
    );
    assert!(!page1.block_keys.is_empty() && !page2.block_keys.is_empty());

    // ------------------------------------------------------------------
    // 3. Persist through the knowledge authority (the citation source).
    // ------------------------------------------------------------------
    for c in &chunks {
        persist_chunk(
            &*rt.store,
            KB,
            &PersistedChunk {
                chunk_id: c.chunk_id.clone(),
                document_id: c.doc_id.clone(),
                document_version: c.document_version,
                block_keys: c.block_keys.clone(),
                page: c.page,
                text_sha256: c.sha256.clone(),
                text: Some(c.text.clone()),
            },
        )
        .await
        .unwrap();
        // The stored sha is a real digest of the stored text.
        assert_eq!(
            c.sha256,
            openrt_core::sha256_hex(c.text.as_bytes()),
            "chunk sha must be the sha256 of its text"
        );
    }

    // ------------------------------------------------------------------
    // 4. Hybrid retrieval over the persisted knowledge (lexical-only here —
    //    honest: no dense backend configured on this runtime).
    // ------------------------------------------------------------------
    let query = out.doc.full_text();
    let report = hybrid_search(
        Arc::clone(&rt.store),
        KB,
        &RetrieveRequest {
            query_text: query.clone(),
            top_k: 4,
            threshold: 0.0,
        },
        &HybridOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(report.hits.len(), 2, "{:?}", report.hits);
    assert!(
        report.dense_backend.starts_with("UNAVAILABLE"),
        "no fabricated dense scores: {}",
        report.dense_backend
    );
    assert_eq!(report.rerank, openrt_knowledge::RerankOutcome::NotRequested);
    for hit in &report.hits {
        assert_eq!(hit.source, RetrievalSource::Lexical, "{hit:?}");
        assert!(hit.bm25_score.unwrap_or(0.0) > 0.0, "{hit:?}");
        assert!(hit.fused_score > 0.0, "{hit:?}");
    }

    // The OCR-derived citation is among them, with page provenance intact.
    let ocr_hit = report
        .hits
        .iter()
        .find(|h| h.citation.page == Some(2))
        .expect("the OCR chunk must be retrievable by its own text");
    let ocr_chunk = load_chunk(
        &*rt.store,
        KB,
        &ocr_hit.citation.document_id,
        &ocr_hit.citation.chunk_id,
    )
    .await
    .unwrap()
    .expect("retrieved citation re-loads from the store");
    assert!(
        ocr_chunk
            .text
            .as_deref()
            .unwrap_or("")
            .to_uppercase()
            .contains("OPENRT"),
        "{ocr_chunk:?}"
    );

    // ------------------------------------------------------------------
    // 5. Hallucination verification over the retrieved citations.
    // ------------------------------------------------------------------
    let mut citations: Vec<Citation> = Vec::new();
    let mut evidence_texts: Vec<String> = Vec::new();
    for hit in &report.hits {
        let pc = load_chunk(
            &*rt.store,
            KB,
            &hit.citation.document_id,
            &hit.citation.chunk_id,
        )
        .await
        .unwrap()
        .expect("retrieved citation re-loads from the store");
        citations.push(hit.citation.clone());
        evidence_texts.push(pc.text.expect("chunk text persisted"));
    }
    let vreport = rt
        .verify_claim(
            VerifyInput {
                kb_id: KB.into(),
                claim: CLAIM.into(),
                citations,
                evidence_texts,
            },
            &VerifyOptions::default(),
        )
        .await
        .unwrap();

    // Claim content tokens are grounded across BOTH legs (OCR word from the
    // scanned page + text-layer tokens) — real evidence, real coverage.
    assert_eq!(vreport.grounding, Grounding::Grounded, "{vreport:?}");
    assert!(vreport.deterministic.lexical_coverage >= MIN_GROUNDING);
    assert_eq!(vreport.decision, FirewallAction::Pass, "{vreport:?}");
    assert_eq!(vreport.verdict, Verdict::Supported);
    assert_eq!(vreport.basis, Basis::Deterministic);
    assert!(
        matches!(
            &vreport.independence,
            Independence::Degraded { reason } if reason.contains("no model verifier")
        ),
        "deterministic-only run must degrade honestly: {:?}",
        vreport.independence
    );
    assert_eq!(vreport.deterministic.citations_total, 2);
    assert_eq!(vreport.deterministic.citations_resolved, 2);

    // Verdict persisted first — with both resolved evidence refs.
    let records = list_verdicts(&*rt.store, KB).await.unwrap();
    assert_eq!(records.len(), 1, "verdict must live in verify.verdicts");
    assert_eq!(records[0].verdict_id, vreport.verdict_id);
    assert_eq!(records[0].decision, FirewallAction::Pass);
    assert_eq!(records[0].evidence_refs.len(), 2);
    assert!(records[0].evidence_refs.iter().all(|r| r.resolved));

    // ------------------------------------------------------------------
    // 6. Injection defense over the answer context (clean corpus → scanned,
    //    labeled, but zero flags and zero audit entries).
    // ------------------------------------------------------------------
    let ctx = rt
        .evidence_context(EvidenceContextRequest::new(KB, query.as_str()))
        .await
        .unwrap();
    assert_eq!(ctx.context.included, 2, "{:?}", ctx.context.blocks);
    assert_eq!(ctx.flagged, 0, "clean corpus: {:?}", ctx.categories);
    assert!(ctx.categories.is_empty());
    assert_eq!(ctx.audit_seq, None, "nothing flagged ⇒ no audit entry");
    assert!(ctx
        .context
        .text
        .starts_with("<evidence_context policy=\"untrusted\">"));
    assert!(ctx.context.text.ends_with("</evidence_context>"));
    assert!(
        ctx.context.text.contains(BLOCK_BEGIN_PREFIX),
        "every passage is wrapped as untrusted evidence"
    );
    assert_eq!(ctx.context.text.matches("[SECURITY WARNING").count(), 0);

    let trust = list_trust(&*rt.store, KB).await.unwrap();
    assert_eq!(
        trust.len(),
        2,
        "every retrieved passage is scanned: {trust:?}"
    );
    assert!(trust
        .iter()
        .all(|r| r.trust == openrt_core::TrustLabel::Untrusted && r.scanned && r.flags.is_empty()));

    let entries = rt
        .audit
        .query(&AuditQuery::new().with_action_prefix("evidence.injection_flagged"))
        .await
        .unwrap();
    assert!(
        entries.is_empty(),
        "clean corpus must not produce entries: {entries:?}"
    );
}
