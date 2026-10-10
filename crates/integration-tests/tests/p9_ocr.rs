//! Phase 9 (M9.2) — real OCR over images and PDFs, end-to-end through the
//! production corpus path:
//! * image intake stores recognized text with bbox + confidence provenance
//!   and honest media dimensions;
//! * image-only (scanned) PDFs take the full-OCR route with a `tesseract|`
//!   extractor label — never text-layer fiction;
//! * mixed born-digital + scanned PDFs route per page (text layer where it
//!   exists, OCR where it does not) and record both real producers;
//! * detection status for images tracks the actual OCR capability.
//!
//! Tests are gated on the host's real backends (via the production
//! `ocr_capability`/`render_capability` probes): without them they skip with
//! a message instead of faking OCR.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use openrt_config::{Config, RuntimeConfig};
use openrt_ingest::capabilities::{ocr_capability, render_capability};
use openrt_ingest::jail::IngestJail;
use openrt_models::mock::MockProvider;

/// Committed fixture: 1200x400 grayscale JPEG, single word "OPENRT" rendered
/// large and clean (verified readable by tesseract at creation time).
const FIXTURE_JPEG: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/ocr_text.jpg"
));

fn tmp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "openrt-p9-ocr-{prefix}-{}-{:x}",
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
    openrt_runtime::RuntimeBuilder::new()
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
    let have = ocr_capability();
    if have.is_none() {
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

/// Build a minimal PDF with a proper xref table. `pages` are the per-page
/// content streams; object 7 is the fixture JPEG as a DCTDecode XObject
/// referenced from the pages that draw it.
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

/// M9.2 — image intake: the stored document carries the recognized word with
/// bbox/confidence provenance, real dimensions, and image modality.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn image_ocr_stores_text_with_provenance() {
    if !ocr_or_skip() {
        return;
    }
    let root = tmp_dir("img");
    let rt = build(runtime_config(&root)).await;
    let jail = IngestJail::new(root.clone()).unwrap();
    std::fs::write(root.join("ocr_text.jpg"), FIXTURE_JPEG).unwrap();

    let out = rt
        .corpus
        .ingest_path(&jail, Path::new("ocr_text.jpg"), "p9-test")
        .await
        .unwrap();
    assert_eq!(out.state.label(), "stored");
    assert_eq!(out.doc.format.label(), "image/jpeg");
    assert_eq!(out.doc.modality, openrt_ingest::Modality::Image);
    assert_eq!(out.doc.pages, 1);

    let media = out.doc.media.as_ref().expect("media dims probed");
    assert_eq!(media.width, Some(1200));
    assert_eq!(media.height, Some(400));
    // Header-parsed color model (M9.3 image metadata), not guessed.
    assert_eq!(media.color_space.as_deref(), Some("gray"));

    assert!(
        out.doc.extractor.starts_with("tesseract|"),
        "extractor: {}",
        out.doc.extractor
    );
    assert!(!out.doc.blocks.is_empty(), "OCR produced no blocks");
    for b in &out.doc.blocks {
        assert_eq!(b.source.method, openrt_ingest::ExtractMethod::Ocr);
        assert_eq!(b.page, Some(1));
        assert!(b.source.bbox.is_some(), "OCR block lacks bbox");
        let conf = b.source.confidence.expect("OCR block lacks confidence");
        assert!(
            (0.0..=1.0).contains(&conf),
            "confidence out of range: {conf}"
        );
    }
    let text = out.doc.full_text().to_uppercase();
    assert!(text.contains("OPENRT"), "OCR text: {text:?}");

    // Provenance survives storage.
    let shown = rt.corpus.inspect(&out.doc.doc_id).await.unwrap();
    assert_eq!(shown.extractor, out.doc.extractor);
    assert_eq!(shown.modality, openrt_ingest::Modality::Image);
    assert_eq!(shown.blocks, out.doc.blocks);
}

/// M9.2 — a PDF whose only page content is a raster image takes the full-OCR
/// route: extractor is the real backend, blocks carry provenance, and no
/// text-layer fiction is invented.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn image_only_pdf_routes_to_full_ocr() {
    if !ocr_or_skip() || !render_or_skip() {
        return;
    }
    let root = tmp_dir("scan");
    let rt = build(runtime_config(&root)).await;
    let jail = IngestJail::new(root.clone()).unwrap();

    let pdf = build_pdf(&[PdfPage {
        width: 1200,
        height: 400,
        draws_image: true,
        text: "",
    }]);
    std::fs::write(root.join("scanned.pdf"), &pdf).unwrap();

    let out = rt
        .corpus
        .ingest_path(&jail, Path::new("scanned.pdf"), "p9-test")
        .await
        .unwrap();
    assert_eq!(out.doc.format.label(), "application/pdf");
    assert_eq!(out.doc.pages, 1);
    assert!(
        out.doc.extractor.starts_with("tesseract|"),
        "extractor: {}",
        out.doc.extractor
    );
    assert!(
        !out.doc.extractor.contains("openrt-pdf-text"),
        "image-only pdf must not claim a text-layer extractor: {}",
        out.doc.extractor
    );
    assert!(!out.doc.blocks.is_empty());
    for b in &out.doc.blocks {
        assert_eq!(b.source.method, openrt_ingest::ExtractMethod::Ocr);
        assert_eq!(b.page, Some(1));
        assert!(b.source.bbox.is_some());
        assert!(b.source.confidence.is_some());
    }
    let text = out.doc.full_text().to_uppercase();
    assert!(text.contains("OPENRT"), "OCR text: {text:?}");
    // Binary image data never leaks into stored text.
    assert!(!text.contains('\u{0}'), "NUL byte in extracted text");
}

/// M9.2 — mixed PDF: page 1 comes from the text layer (no invented
/// confidence/bbox), page 2 from OCR (real bbox/confidence), and the
/// document extractor records both producers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_pdf_routes_pages_per_text_layer() {
    if !ocr_or_skip() || !render_or_skip() {
        return;
    }
    let root = tmp_dir("mixed");
    let rt = build(runtime_config(&root)).await;
    let jail = IngestJail::new(root.clone()).unwrap();

    let pdf = build_pdf(&[
        PdfPage {
            width: 1200,
            height: 400,
            draws_image: false,
            text: "Born digital page one",
        },
        PdfPage {
            width: 1200,
            height: 400,
            draws_image: true,
            text: "",
        },
    ]);
    std::fs::write(root.join("mixed.pdf"), &pdf).unwrap();

    let out = rt
        .corpus
        .ingest_path(&jail, Path::new("mixed.pdf"), "p9-test")
        .await
        .unwrap();
    assert_eq!(out.doc.pages, 2);

    let p1: Vec<_> = out
        .doc
        .blocks
        .iter()
        .filter(|b| b.page == Some(1))
        .collect();
    let p2: Vec<_> = out
        .doc
        .blocks
        .iter()
        .filter(|b| b.page == Some(2))
        .collect();
    assert!(!p1.is_empty(), "page 1 lost its text layer");
    assert!(!p2.is_empty(), "page 2 was not OCR'd");

    let born = p1.iter().find(|b| b.text.contains("Born digital"));
    let born = born.expect("page 1 text missing");
    assert_eq!(born.source.method, openrt_ingest::ExtractMethod::PdfText);
    assert!(born.source.bbox.is_none(), "text layer has no bbox");
    assert!(born.source.confidence.is_none(), "text layer has no conf");

    let ocr_text: String = p2
        .iter()
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
        .to_uppercase();
    assert!(ocr_text.contains("OPENRT"), "page 2 OCR text: {ocr_text:?}");
    for b in &p2 {
        assert_eq!(b.source.method, openrt_ingest::ExtractMethod::Ocr);
        assert!(b.source.bbox.is_some());
        assert!(b.source.confidence.is_some());
    }

    assert!(
        out.doc.extractor.contains("openrt-pdf-text"),
        "extractor: {}",
        out.doc.extractor
    );
    assert!(
        out.doc.extractor.contains("tesseract|"),
        "extractor: {}",
        out.doc.extractor
    );

    // Block keys renumbered in reading order, still unique after the fill.
    let keys: Vec<&str> = out.doc.blocks.iter().map(|b| b.key.as_str()).collect();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(keys.len(), sorted.len(), "duplicate block keys: {keys:?}");
}

/// M9.2 — detection honesty: image extraction status equals the real OCR
/// capability on this host, not a hardcoded claim.
#[test]
fn image_detect_status_tracks_capability() {
    let r = openrt_ingest::detect(FIXTURE_JPEG, Some("x.jpg"));
    assert_eq!(
        r.status.is_supported(),
        ocr_capability().is_some(),
        "detect status diverged from capability: {:?}",
        r.status
    );
    if ocr_capability().is_some() {
        assert!(r.status.is_supported(), "{:?}", r.status);
    }
}
