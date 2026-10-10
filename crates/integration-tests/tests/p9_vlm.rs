//! Phase 9 (M9.3) — the vision/VLM pipeline end-to-end:
//! * unreachable endpoints and absent models surface as the structured
//!   `UNAVAILABLE` / `MODEL_MISSING` statuses — never a fabricated answer;
//! * when the real vision model is present it answers real questions over a
//!   real image (images transport), with provenance;
//! * the engineering-drawing route produces real OCR regions and reports the
//!   VLM part honestly whatever its status;
//! * doctor carries a `vision.vlm` row.

use std::path::PathBuf;
use std::sync::Arc;

use openrt_config::{Config, RuntimeConfig};
use openrt_ingest::capabilities::ocr_capability;
use openrt_models::mock::MockProvider;

/// Default endpoint/model probed by the workspace (matches config defaults).
const ENDPOINT: &str = "http://127.0.0.1:11434";
const MODEL: &str = "moondream";

/// Same committed fixture as p9_ocr (1200x400 grayscale JPEG, "OPENRT").
const FIXTURE_JPEG: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/ocr_text.jpg"
));

fn tmp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "openrt-p9-vlm-{prefix}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// M9.3 — a dead endpoint reports UNAVAILABLE with a reason (always runs;
/// localhost port 1 refuses instantly, no external network involved).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dead_endpoint_reports_unavailable() {
    let status = openrt_vlm::probe_vision("http://127.0.0.1:1", MODEL).await;
    assert_eq!(status.label(), "UNAVAILABLE", "{status}");
    assert!(!status.reason().is_empty());
}

/// M9.3 — probing the real endpoint reports one of the three honest states,
/// each with its invariants (no invented availability, no empty reasons).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_reports_honest_status() {
    let status = openrt_vlm::probe_vision(ENDPOINT, MODEL).await;
    match &status {
        openrt_vlm::VisionStatus::Available { endpoint, model } => {
            assert_eq!(endpoint, ENDPOINT);
            assert!(model.contains("moondream"), "model: {model}");
        }
        openrt_vlm::VisionStatus::ModelMissing {
            model,
            reason,
            available,
        } => {
            assert_eq!(model, MODEL);
            assert!(reason.contains("pull"), "{reason}");
            assert!(!available.is_empty(), "listing must be shown");
        }
        openrt_vlm::VisionStatus::Unavailable { reason } => {
            // Honest when no Ollama runs on the host.
            assert!(!reason.is_empty());
        }
    }
}

/// M9.3 — an endpoint that is alive but lacks the requested model reports
/// MODEL_MISSING with the installed list (structured, actionable). Skips
/// when no endpoint is reachable at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_model_is_structured() {
    let status = openrt_vlm::probe_vision(ENDPOINT, "openrt-absent-model-xyz").await;
    match &status {
        openrt_vlm::VisionStatus::Unavailable { reason } => {
            eprintln!("SKIP: no Ollama endpoint reachable: {reason}");
        }
        openrt_vlm::VisionStatus::ModelMissing {
            model,
            reason,
            available,
        } => {
            assert_eq!(model, "openrt-absent-model-xyz");
            assert!(reason.contains("openrt-absent-model-xyz"), "{reason}");
            assert!(!available.is_empty());
        }
        openrt_vlm::VisionStatus::Available { model, .. } => {
            panic!("absent model must not be available: {model}")
        }
    }
}

/// M9.3 — when the real vision model is present it answers a real question
/// over the real fixture image through the images transport, with
/// provenance. Skips honestly when the model is absent on this host.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn moondream_answers_real_question() {
    let status = openrt_vlm::probe_vision(ENDPOINT, MODEL).await;
    if !status.is_available() {
        eprintln!("SKIP: vision model not available: {status}");
        return;
    }
    // Small local models occasionally emit an empty completion (structured
    // InvalidResponse); retry a few times before calling it a failure.
    let mut answer = None;
    for _ in 0..3 {
        match openrt_vlm::ask_vision(
            ENDPOINT,
            MODEL,
            FIXTURE_JPEG,
            "What word is written in this image? Answer with the word only.",
        )
        .await
        {
            Ok(a) => {
                answer = Some(a);
                break;
            }
            Err(openrt_vlm::VlmError::InvalidResponse(reason)) => {
                eprintln!("retrying vision answer: {reason}");
            }
            Err(e) => panic!("vision call failed: {e}"),
        }
    }
    let answer = answer.expect("model kept returning empty answers");
    let text = answer.text.trim();
    assert!(!text.is_empty(), "empty answer");
    assert!(text.len() <= 500, "implausibly long answer: {text:?}");
    assert!(
        text.chars().any(|c| c.is_alphabetic()),
        "answer has no letters: {text:?}"
    );
    assert_eq!(answer.endpoint, ENDPOINT);
    assert!(answer.model.contains("moondream"), "{}", answer.model);
    assert!(answer.prompt.contains("word"));
}

/// M9.3 — engineering-drawing route: real OCR regions with bbox/confidence,
/// VLM part requested (answered or structurally failed, never silent).
/// Skips without tesseract.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drawing_route_regions_and_honest_vlm() {
    if ocr_capability().is_none() {
        eprintln!("SKIP: no tesseract backend on PATH — OCR claims not faked");
        return;
    }

    // Without a vision model request: OCR-only report.
    let report = openrt_vlm::drawing::analyze(FIXTURE_JPEG, Some("drawing.jpg"), None)
        .await
        .unwrap();
    assert!(report.ocr_backend.starts_with("tesseract|"));
    assert!(!report.regions.is_empty(), "no OCR regions found");
    assert_eq!(report.vlm, openrt_vlm::drawing::VlmPart::NotRequested);
    for r in &report.regions {
        assert!((0.0..=1.0).contains(&r.confidence));
        assert!(r.bbox.left <= r.bbox.right);
    }

    // With a vision model requested: answered or structured failure.
    let report =
        openrt_vlm::drawing::analyze(FIXTURE_JPEG, Some("drawing.jpg"), Some((ENDPOINT, MODEL)))
            .await
            .unwrap();
    match &report.vlm {
        openrt_vlm::drawing::VlmPart::Answered(answer) => {
            assert!(answer.model.contains("moondream"), "{}", answer.model);
            assert!(!answer.text.trim().is_empty());
        }
        openrt_vlm::drawing::VlmPart::Failed { error } => {
            let msg = error.to_string();
            assert!(
                msg.contains("unavailable")
                    || msg.contains("missing")
                    || msg.contains("empty answer"),
                "structured failure expected, got: {msg}"
            );
        }
        openrt_vlm::drawing::VlmPart::NotRequested => {
            panic!("vision was requested")
        }
    }
}

/// M9.3 — doctor carries the honest `vision.vlm` row (PASS/WARN, never a
/// fabricated PASS).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_includes_vision_row() {
    let ws = tmp_dir("doctor");
    let config = Config {
        runtime: RuntimeConfig {
            workspace_dir: Some(ws.clone()),
            ..RuntimeConfig::default()
        },
        ..Config::default()
    };
    let rt = openrt_runtime::RuntimeBuilder::new()
        .config(config)
        .store(Arc::new(openrt_persistence::MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
        .build()
        .await
        .unwrap();
    let report = rt.doctor().await;
    let row = report
        .checks
        .iter()
        .find(|c| c.name == "vision.vlm")
        .unwrap_or_else(|| panic!("doctor must include vision.vlm; got {:?}", report.checks));
    assert!(
        matches!(
            row.status,
            openrt_runtime::DoctorStatus::Pass | openrt_runtime::DoctorStatus::Warn
        ),
        "{row:?}"
    );
    assert!(row.detail.contains(MODEL), "{row:?}");
    // A PASS may only come from a genuinely available model.
    if row.status == openrt_runtime::DoctorStatus::Pass {
        let status = openrt_vlm::probe_vision(ENDPOINT, MODEL).await;
        assert!(
            status.is_available(),
            "doctor PASS but probe says: {status}"
        );
    }
}
