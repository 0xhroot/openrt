//! Phase 9 (M9.1) — content-based document intake, end-to-end through the
//! real runtime corpus:
//! * honest `SUPPORTED` / `UNSUPPORTED` / `CORRUPT` / `AMBIGUOUS` statuses
//!   with reasons, carried into production intake (not only unit reports);
//! * extraction through the SEC-01 jail — path escapes rejected before any
//!   read;
//! * storage, inspection, listing, and removal over the shared store;
//! * page numbers and extractor provenance surviving extraction + storage.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use openrt_config::{Config, RuntimeConfig};
use openrt_ingest::jail::IngestJail;
use openrt_models::mock::MockProvider;

fn tmp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "openrt-p9-ingest-{prefix}-{}-{:x}",
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

/// Fixture: a minimal flate-compressed, two-page PDF. Page numbers must be
/// derived from the page objects — not the page-tree node — and must survive
/// storage.
fn two_page_pdf() -> Vec<u8> {
    use std::io::Write;
    fn flate(text: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(text).unwrap();
        enc.finish().unwrap()
    }
    let s1 = flate(b"BT (Page one text) Tj ET");
    let s2 = flate(b"BT (Page two text) Tj ET");
    let mut pdf = Vec::new();
    pdf.extend_from_slice(b"%PDF-1.4\n");
    pdf.extend_from_slice(b"1 0 obj << /Type /Page /Contents 2 0 R >> endobj\n");
    pdf.extend_from_slice(
        format!(
            "2 0 obj << /Filter /FlateDecode /Length {} >>\nstream\n",
            s1.len()
        )
        .as_bytes(),
    );
    pdf.extend_from_slice(&s1);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");
    pdf.extend_from_slice(b"3 0 obj << /Type /Page /Contents 4 0 R >> endobj\n");
    pdf.extend_from_slice(
        format!(
            "4 0 obj << /Filter /FlateDecode /Length {} >>\nstream\n",
            s2.len()
        )
        .as_bytes(),
    );
    pdf.extend_from_slice(&s2);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");
    pdf.extend_from_slice(b"5 0 obj << /Type /Catalog >> endobj\n");
    pdf.extend_from_slice(
        b"xref\n0 6\n0000000000 65535 f \ntrailer << /Root 5 0 R >>\nstartxref 1\n%%EOF\n",
    );
    pdf
}

/// M9.1 — an HTML file ingested through the production corpus path: stored,
/// inspectable, listed, removable; extractor provenance recorded honestly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn html_extract_stores_lists_inspects_removes() {
    let root = tmp_dir("html");
    let rt = build(runtime_config(&root)).await;

    let html = root.join("page.html");
    std::fs::write(
        &html,
        "<!DOCTYPE html><html><body><h1>Section One</h1>\
         <p>The turbine rated 40 MW at peak.</p><ul><li>item a</li></ul></body></html>",
    )
    .unwrap();
    let jail = IngestJail::new(root.clone()).unwrap();

    let out = rt
        .corpus
        .ingest_path(&jail, Path::new("page.html"), "p9-test")
        .await
        .unwrap();
    assert_eq!(out.state.label(), "stored");
    assert!(out.doc.doc_id.starts_with("doc/"));
    assert_eq!(out.doc.format.label(), "text/html");
    assert_eq!(out.doc.extractor, "openrt-html-parser/pure");
    assert!(out.doc.blocks.iter().any(|b| b.text.contains("40 MW")));
    // Heading block kind, honest extractor label on provenance.
    assert!(out.doc.blocks.iter().any(|b| {
        matches!(b.kind, openrt_ingest::BlockKind::Heading)
            && b.source.label().contains("@openrt-html-parser")
    }));

    // Re-ingest of identical content: duplicate, not a second document.
    let dup = rt
        .corpus
        .ingest_path(&jail, Path::new("page.html"), "p9-test")
        .await
        .unwrap();
    assert_eq!(dup.doc.doc_id, out.doc.doc_id);
    assert_eq!(dup.state.label(), "duplicate");

    // List → inspect → remove round trip.
    let list = rt.corpus.list().await.unwrap();
    assert!(list.iter().any(|m| m.doc_id == out.doc.doc_id));
    let shown = rt.corpus.inspect(&out.doc.doc_id).await.unwrap();
    assert_eq!(shown.doc_id, out.doc.doc_id);
    assert!(!shown.blocks.is_empty());
    rt.corpus.remove(&out.doc.doc_id, "p9-test").await.unwrap();
    let list = rt.corpus.list().await.unwrap();
    assert!(list.iter().all(|m| m.doc_id != out.doc.doc_id));
    assert!(matches!(
        rt.corpus.inspect(&out.doc.doc_id).await,
        Err(openrt_ingest::corpus::IngestError::NotFound { .. })
    ));
}

/// M9.1 — the status gate is production, not decorative: ambiguous and
/// corrupt content never reaches storage, and the error carries the honest
/// reason the report would have shown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_gate_rejects_ambiguous_and_corrupt_with_reasons() {
    let root = tmp_dir("gate");
    let rt = build(runtime_config(&root)).await;
    let jail = IngestJail::new(root.clone()).unwrap();

    std::fs::write(root.join("data.json"), "{bad json").unwrap();
    let err = rt
        .corpus
        .ingest_path(&jail, Path::new("data.json"), "p9-test")
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("ambiguous"), "msg: {msg}");
    assert!(msg.contains("structural json"), "msg: {msg}");

    std::fs::write(root.join("broken.pdf"), b"%PDF-1.7 truncated no eof").unwrap();
    let err = rt
        .corpus
        .ingest_path(&jail, Path::new("broken.pdf"), "p9-test")
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("corrupt"), "msg: {msg}");
    assert!(msg.contains("%%EOF"), "msg: {msg}");

    // Nothing was stored.
    assert!(rt.corpus.list().await.unwrap().is_empty());
}

/// M9.1 — detection statuses are reachable from the shared `detect` entry
/// point (the CLI's `ingest inspect` path), including supported fixtures.
#[test]
fn detect_statuses_are_honest() {
    let html = b"<!DOCTYPE html><html><body><p>hi</p></body></html>";
    let r = openrt_ingest::detect(html, Some("x.html"));
    assert!(r.status.is_supported(), "{:?}", r.status);

    let r = openrt_ingest::detect(b"{bad json", Some("x.json"));
    assert!(!r.status.is_supported());
    assert!(r.status.reason().unwrap().contains("json"));

    let r = openrt_ingest::detect(b"%PDF-1.7 no eof here", Some("x.pdf"));
    assert!(!r.status.is_supported());
    assert!(r.status.reason().unwrap().contains("%%EOF"));
}

/// M9.1 — SEC-01: an absolute path outside the jail root and a relative
/// traversal are both rejected before any content is read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jail_rejects_path_escapes() {
    let root = tmp_dir("jail");
    let outside = tmp_dir("jail-outside");
    let secret = outside.join("secret.txt");
    std::fs::write(&secret, "top secret").unwrap();
    let rt = build(runtime_config(&root)).await;
    let jail = IngestJail::new(root.clone()).unwrap();

    let err = rt
        .corpus
        .ingest_path(&jail, &secret, "p9-test")
        .await
        .unwrap_err();
    assert!(
        matches!(err, openrt_ingest::corpus::IngestError::Jail(_)),
        "expected jail rejection, got: {err}"
    );

    let err = rt
        .corpus
        .ingest_path(&jail, Path::new("../jail-outside/secret.txt"), "p9-test")
        .await
        .unwrap_err();
    assert!(
        matches!(err, openrt_ingest::corpus::IngestError::Jail(_)),
        "expected jail rejection, got: {err}"
    );
    assert!(rt.corpus.list().await.unwrap().is_empty());
}

/// M9.1 — page numbers come from real page objects and survive storage +
/// inspection (the provenance basis for later citation milestones).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pdf_page_numbers_survive_storage() {
    let root = tmp_dir("pdf");
    let rt = build(runtime_config(&root)).await;
    let jail = IngestJail::new(root.clone()).unwrap();

    std::fs::write(root.join("two.pdf"), two_page_pdf()).unwrap();
    let out = rt
        .corpus
        .ingest_path(&jail, Path::new("two.pdf"), "p9-test")
        .await
        .unwrap();
    assert_eq!(out.doc.pages, 2);
    let shown = rt.corpus.inspect(&out.doc.doc_id).await.unwrap();
    assert_eq!(shown.pages, 2);
    let pages: Vec<Option<u32>> = shown.blocks.iter().map(|b| b.page).collect();
    assert_eq!(pages, vec![Some(1), Some(2)]);
    assert!(shown.extractor.starts_with("openrt-pdf"));
}
