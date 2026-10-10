//! M7B office acceptance: real OOXML round-trips through the gate, overwrite
//! refusals, artifact provenance, and honest soffice-backed render/export
//! (real PDF when LibreOffice is present, structured UNAVAILABLE when not).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use openrt_config::{Config, PermissionsConfig, RuleConfig, RuleEffect};

fn tmp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "openrt-p7b-office-{prefix}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Allow rules are glob patterns over BOTH the tool id (checked first by the
/// gate) and each required capability — pass e.g. `["excel.*",
/// "artifact.excel.*"]` or exact ids for tighter tests.
fn allow_rules(patterns: &[&str]) -> PermissionsConfig {
    PermissionsConfig {
        rules: patterns
            .iter()
            .map(|p| RuleConfig {
                effect: RuleEffect::Allow,
                capability: (*p).to_string(),
                resource: None,
            })
            .collect(),
    }
}

async fn build(ws: &Path) -> Arc<openrt_runtime::Runtime> {
    let config = Config {
        runtime: openrt_config::RuntimeConfig {
            workspace_dir: Some(ws.to_path_buf()),
            screen_backend: Some("sim".into()),
            computer_backend: Some("sim".into()),
            ..Default::default()
        },
        permissions: allow_rules(&[
            "excel.*",
            "ppt.*",
            "docx.*",
            "artifact.list",
            "artifact.excel.*",
            "artifact.ppt.*",
            "artifact.docx.*",
            "fabric.artifacts.read",
        ]),
        ..Default::default()
    };
    openrt_runtime::RuntimeBuilder::new()
        .config(config)
        .store(Arc::new(openrt_persistence::MemoryStore::new()))
        .with_provider(Arc::new(openrt_models::mock::MockProvider::echo("mock")))
        .build()
        .await
        .unwrap()
}

fn ctx() -> openrt_tools::ToolExecutionContext {
    openrt_tools::ToolExecutionContext {
        session_id: Some(openrt_core::SessionId::new().to_string()),
        task_id: Some(openrt_core::TaskId::new().to_string()),
        channel: None,
    }
}

async fn json_out(
    rt: &openrt_runtime::Runtime,
    name: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    let out = rt
        .executor
        .execute(name, args, &ctx())
        .await
        .unwrap_or_else(|e| panic!("{name} failed: {e:?}"));
    match out {
        openrt_tools::ToolOutput::Json(v) => v,
        openrt_tools::ToolOutput::Text(s) => serde_json::json!({ "text": s }),
    }
}

/// excel.create → excel.read round-trip; silent overwrite refused; excel.write
/// appends/replaces and re-reads through the same gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn excel_roundtrip_and_overwrite_refusal() {
    let ws = tmp_dir("xlsx");
    let rt = build(&ws).await;
    let path = ws.join("report.xlsx");
    let path_s = path.display().to_string();

    let created = json_out(
        &rt,
        "excel.create",
        serde_json::json!({
            "path": path_s,
            "sheet": "Data",
            "rows": [["h1", "h2"], ["v1", "v2"]]
        }),
    )
    .await;
    assert_eq!(created["ok"], serde_json::json!(true), "{created}");
    assert!(path.exists(), "xlsx not written");

    let read = json_out(&rt, "excel.read", serde_json::json!({ "path": path_s })).await;
    assert_eq!(read["ok"], serde_json::json!(true), "{read}");
    let rows = read["sheets"][0]["rows"].clone();
    let rows_s = rows.to_string();
    assert!(rows_s.contains("h1") && rows_s.contains("v2"), "{read}");

    // Silent overwrite refused (structured invalid-arguments refusal).
    let refusal = rt
        .executor
        .execute(
            "excel.create",
            serde_json::json!({ "path": path_s, "rows": [["x"]] }),
            &ctx(),
        )
        .await;
    match refusal {
        Err(openrt_tools::ToolError::InvalidArguments(..)) => {}
        Ok(openrt_tools::ToolOutput::Json(v)) => {
            assert_eq!(
                v["ok"],
                serde_json::json!(false),
                "overwrite must fail: {v}"
            );
        }
        other => panic!("expected overwrite refusal, got {other:?}"),
    }

    // excel.write with overwrite semantics via the dedicated write tool.
    let wrote = json_out(
        &rt,
        "excel.write",
        serde_json::json!({
            "path": path_s,
            "sheet": "Data",
            "rows": [["v3", "v4"]]
        }),
    )
    .await;
    assert_eq!(wrote["ok"], serde_json::json!(true), "{wrote}");
    let read2 = json_out(&rt, "excel.read", serde_json::json!({ "path": path_s })).await;
    assert!(read2.to_string().contains("v3"), "{read2}");

    let _ = std::fs::remove_dir_all(&ws);
}

/// PPTX + DOCX create/read round-trips (real OOXML, self-verified on write).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pptx_and_docx_roundtrip() {
    let ws = tmp_dir("docs");
    let rt = build(&ws).await;
    let ppt_path = ws.join("deck.pptx").display().to_string();
    let doc_path = ws.join("doc.docx").display().to_string();

    let deck = json_out(
        &rt,
        "ppt.create",
        serde_json::json!({
            "path": ppt_path,
            "title": "Phase 7B",
            "slides": [
                { "title": "One", "bullets": ["alpha", "beta"] },
                { "title": "Two" }
            ]
        }),
    )
    .await;
    assert_eq!(deck["ok"], serde_json::json!(true), "{deck}");
    assert!(
        deck["verification"]["ok"] == serde_json::json!(true),
        "{deck}"
    );

    let doc = json_out(
        &rt,
        "docx.create",
        serde_json::json!({
            "path": doc_path,
            "title": "Acceptance",
            "blocks": [
                { "type": "heading", "level": 1, "text": "Section" },
                { "type": "paragraph", "text": "Body paragraph." },
                { "type": "bullet", "text": "Item one" }
            ]
        }),
    )
    .await;
    assert_eq!(doc["ok"], serde_json::json!(true), "{doc}");

    let doc_read = json_out(&rt, "docx.read", serde_json::json!({ "path": doc_path })).await;
    assert_eq!(doc_read["ok"], serde_json::json!(true), "{doc_read}");
    let doc_s = doc_read.to_string();
    assert!(
        doc_s.contains("Section") && doc_s.contains("Body paragraph"),
        "{doc_read}"
    );
    assert!(doc_read["source"]["sha256"].is_string(), "{doc_read}");

    let _ = std::fs::remove_dir_all(&ws);
}

/// Render/export through the real LibreOffice backend when present (real PDF
/// bytes verified), otherwise honest structured UNAVAILABLE — never fake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn render_honesty_with_or_without_soffice() {
    let ws = tmp_dir("render");
    let rt = build(&ws).await;
    let ppt_path = ws.join("deck.pptx").display().to_string();
    let doc_path = ws.join("memo.docx").display().to_string();

    json_out(
        &rt,
        "ppt.create",
        serde_json::json!({
            "path": ppt_path,
            "title": "T",
            "slides": [{ "title": "S" }]
        }),
    )
    .await;
    json_out(
        &rt,
        "docx.create",
        serde_json::json!({
            "path": doc_path,
            "blocks": [{ "type": "paragraph", "text": "text" }]
        }),
    )
    .await;

    let (soffice, _) = openrt_office::detect_soffice(None);
    let have_backend = soffice.is_some();

    for (tool, path) in [
        ("ppt.export_pdf", ppt_path.clone()),
        ("docx.export_pdf", doc_path.clone()),
        ("ppt.render", ppt_path.clone()),
    ] {
        let out = json_out(&rt, tool, serde_json::json!({ "path": path })).await;
        if have_backend {
            assert_eq!(out["ok"], serde_json::json!(true), "{tool}: {out}");
            let produced = out["output"].as_str().expect("output path");
            let bytes = std::fs::read(produced).expect("rendered file");
            assert!(!bytes.is_empty(), "{tool}: empty output");
            if produced.ends_with(".pdf") {
                assert!(
                    bytes.starts_with(b"%PDF"),
                    "{tool}: not a real PDF: {produced}"
                );
            }
            assert!(
                bytes.len() > 1000,
                "{tool}: suspiciously small: {}",
                bytes.len()
            );
        } else {
            assert_eq!(out["ok"], serde_json::json!(false), "{tool}: {out}");
            assert_eq!(
                out["error"]["code"],
                serde_json::json!("UNAVAILABLE"),
                "{tool}: {out}"
            );
        }
    }

    // Source files are never modified by render/export.
    let src = std::fs::read(ws.join("deck.pptx")).unwrap();
    assert!(!src.starts_with(b"%PDF"), "source was overwritten");

    let _ = std::fs::remove_dir_all(&ws);
}

/// Artifact provenance: created files are recorded with matching SHA-256.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn artifacts_record_matching_hashes() {
    let ws = tmp_dir("art");
    let rt = build(&ws).await;
    let path = ws.join("a.xlsx").display().to_string();

    json_out(
        &rt,
        "excel.create",
        serde_json::json!({ "path": path, "rows": [["1"]] }),
    )
    .await;

    let list = json_out(&rt, "artifact.list", serde_json::json!({})).await;
    assert_eq!(list["ok"], serde_json::json!(true), "{list}");
    let items = list["artifacts"].as_array().expect("artifacts array");
    assert!(!items.is_empty(), "{list}");
    let rec = items
        .iter()
        .find(|a| {
            a["location"]
                .as_str()
                .unwrap_or_default()
                .ends_with("a.xlsx")
        })
        .expect("record for a.xlsx");
    let recorded = rec["sha256"].as_str().expect("sha256");
    let bytes = std::fs::read(ws.join("a.xlsx")).unwrap();
    assert_eq!(recorded, openrt_verify::sha256_hex(&bytes), "{rec}");

    let _ = std::fs::remove_dir_all(&ws);
}
