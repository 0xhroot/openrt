//! M7B acceptance: every Phase 7B tool is registered in the ONE production
//! registry, is deny-by-default through the real gate, appears in `tools.list`
//! with fabric metadata, and is kill-scoped consistently with 7A rules.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use openrt_config::{Config, PermissionsConfig, RuleConfig, RuleEffect};

fn tmp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "openrt-p7b-deny-{prefix}-{}-{:x}",
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

async fn build(ws: &Path, caps: &[&str]) -> Arc<openrt_runtime::Runtime> {
    let config = Config {
        runtime: openrt_config::RuntimeConfig {
            workspace_dir: Some(ws.to_path_buf()),
            terminal_timeout_secs: Some(30),
            screen_backend: Some("sim".into()),
            computer_backend: Some("sim".into()),
            ..Default::default()
        },
        permissions: allow_rules(caps),
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

/// Registration completeness: all 7B families are in the ONE registry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fabric_tools_are_registered() {
    let ws = tmp_dir("reg");
    let rt = build(&ws, &[]).await;
    let names = rt.registry.names();
    for name in [
        "database.connect",
        "database.list_tables",
        "database.describe_table",
        "database.query",
        "database.insert",
        "database.update",
        "database.delete",
        "database.begin",
        "database.commit",
        "database.rollback",
        "database.transaction",
        "excel.create",
        "excel.write",
        "excel.read",
        "ppt.create",
        "ppt.render",
        "ppt.export_pdf",
        "docx.create",
        "docx.read",
        "docx.export_pdf",
        "browser.navigate",
        "browser.find",
        "browser.click",
        "browser.type",
        "browser.select",
        "browser.submit",
        "browser.screenshot",
        "browser.download",
        "browser.upload",
        "browser.status",
        "browser.close",
        "mcp.servers",
        "mcp.discover",
        "application.list",
        "application.verify",
        "artifact.list",
        "tools.list",
    ] {
        assert!(
            names.contains(&name.to_string()),
            "{name} missing: {names:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&ws);
}

/// Deny-by-default: no allow rules ⇒ every 7B tool is refused by the gate
/// and its body never runs (one execution authority, same as 7A).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fabric_tools_deny_by_default() {
    let ws = tmp_dir("deny");
    let rt = build(&ws, &["filesystem.read"]).await;
    for name in [
        "database.query",
        "database.insert",
        "excel.create",
        "excel.read",
        "ppt.render",
        "docx.create",
        "browser.navigate",
        "browser.screenshot",
        "mcp.servers",
        "mcp.discover",
        "application.verify",
        "artifact.list",
        "tools.list",
    ] {
        let err = rt
            .executor
            .execute(name, serde_json::json!({}), &ctx())
            .await
            .unwrap_err();
        assert!(
            matches!(err, openrt_tools::ToolError::PermissionDenied { .. }),
            "{name}: expected PermissionDenied, got {err:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&ws);
}

/// `tools.list` is the single discovery surface: every 7B tool row carries
/// fabric metadata (category, risk, platform support) and every required
/// capability is a valid 3-segment capability.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_list_covers_every_fabric_tool() {
    let ws = tmp_dir("list");
    let rt = build(&ws, &["tools.list", "tools.discovery.all"]).await;

    let rows = json_out(&rt, "tools.list", serde_json::json!({})).await["tools"]
        .as_array()
        .cloned()
        .expect("tools array");
    assert!(!rows.is_empty(), "tools.list returned no tools");

    let covered = [
        "database.",
        "excel.",
        "ppt.",
        "docx.",
        "browser.",
        "mcp.",
        "application.",
        "artifact.",
        "computer.",
    ];
    let mut seen = 0usize;
    for row in &rows {
        let name = row["name"].as_str().unwrap_or_default();
        if !covered.iter().any(|p| name.starts_with(p)) {
            continue;
        }
        seen += 1;
        // Metadata present (category/risk are null only for uncovered tools).
        assert!(
            row["category"].is_string(),
            "{name} has no fabric category: {row}"
        );
        assert!(row["risk"].is_string(), "{name} has no risk: {row}");
        assert!(
            row["platforms"].is_object(),
            "{name} has no platform support map: {row}"
        );
        // Capabilities are 3-segment (Capability::from_str contract).
        for cap in row["required_caps"].as_array().unwrap_or(&vec![]) {
            let c = cap.as_str().unwrap_or_default();
            assert_eq!(
                c.split('.').filter(|s| !s.is_empty()).count(),
                3,
                "{name}: capability `{c}` is not 3 segments"
            );
        }
    }
    assert!(seen >= 44, "expected the full 7B surface, saw {seen} rows");
    // tools.list describes itself (snapshot registered after it).
    assert!(
        rows.iter().any(|r| r["name"] == "tools.list"),
        "tools.list missing from its own snapshot"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// Kill scopes: side-effecting 7B tools halt with the computer scope; the
/// read-only discovery surfaces stay available to the operator.
#[test]
fn fabric_tools_are_kill_scoped() {
    for tool in [
        "browser.navigate",
        "browser.screenshot",
        "database.query",
        "database.transaction",
        "excel.create",
        "ppt.render",
        "docx.export_pdf",
        "mcp.servers",
        "mcp.discover",
        "application.verify",
    ] {
        assert_eq!(
            openrt_safety::tool_scope(tool),
            Some(openrt_safety::KillScope::Computer),
            "{tool}"
        );
    }
    for tool in ["tools.list", "artifact.list", "echo"] {
        assert_eq!(
            openrt_safety::tool_scope(tool),
            None,
            "{tool} must not be computer-scoped"
        );
    }
}
