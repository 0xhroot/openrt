//! M7B browser acceptance through the real gate: deterministic mock backend
//! for interaction contracts, Sovereign-mode egress refusal, and an honest
//! check of the auto backend (real Firefox when present on the host).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use openrt_config::{BrowserConfig, Config, PermissionsConfig, RuleConfig, RuleEffect};

fn tmp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "openrt-p7b-browser-{prefix}-{}-{:x}",
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

fn browser_patterns() -> &'static [&'static str] {
    &["browser.*"]
}

async fn build(ws: &Path, backend: Option<&str>) -> Arc<openrt_runtime::Runtime> {
    let mut config = Config {
        runtime: openrt_config::RuntimeConfig {
            workspace_dir: Some(ws.to_path_buf()),
            screen_backend: Some("sim".into()),
            computer_backend: Some("sim".into()),
            ..Default::default()
        },
        permissions: allow_rules(browser_patterns()),
        ..Default::default()
    };
    config.fabric.browser = BrowserConfig {
        backend: backend.map(|s| s.to_string()),
        binary: None,
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

/// Mock backend: the full in-page interaction contract through the gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mock_backend_interaction_contract() {
    let ws = tmp_dir("mock");
    let rt = build(&ws, Some("mock")).await;

    // Local scheme (data:) is always allowed — even in Sovereign mode.
    let nav = json_out(
        &rt,
        "browser.navigate",
        serde_json::json!({ "url": "data:text/html,<title>Probe</title><p id=t>hi</p>" }),
    )
    .await;
    assert_eq!(nav["ok"], serde_json::json!(true), "{nav}");
    assert_eq!(nav["backend"], serde_json::json!("mock"), "{nav}");

    let find = json_out(&rt, "browser.find", serde_json::json!({ "selector": "#t" })).await;
    assert_eq!(find["ok"], serde_json::json!(true), "{find}");

    let click = json_out(
        &rt,
        "browser.click",
        serde_json::json!({ "selector": "#t" }),
    )
    .await;
    assert_eq!(click["ok"], serde_json::json!(true), "{click}");

    let typed = json_out(
        &rt,
        "browser.type",
        serde_json::json!({ "selector": "input", "text": "hello" }),
    )
    .await;
    assert_eq!(typed["ok"], serde_json::json!(true), "{typed}");

    let shot = json_out(&rt, "browser.screenshot", serde_json::json!({})).await;
    assert_eq!(shot["ok"], serde_json::json!(true), "{shot}");

    let status = json_out(&rt, "browser.status", serde_json::json!({})).await;
    assert_eq!(status["ok"], serde_json::json!(true), "{status}");
    assert_eq!(status["active"], serde_json::json!(true), "{status}");
    assert_eq!(status["backend"], serde_json::json!("mock"), "{status}");

    let closed = json_out(&rt, "browser.close", serde_json::json!({})).await;
    assert_eq!(closed["ok"], serde_json::json!(true), "{closed}");

    let _ = std::fs::remove_dir_all(&ws);
}

/// Sovereign mode (the fail-closed default) refuses external egress before
/// the request leaves the host. M10.9: AgentShield now refuses it at the
/// GATE (fail-closed deny, before the browser engine sees the call); the
/// browser layer's own structured POLICY_DENIED refusal is covered
/// directly by `openrt-browser`'s unit tests.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sovereign_denies_external_navigation() {
    let ws = tmp_dir("sov");
    let rt = build(&ws, Some("mock")).await;
    let err = rt
        .executor
        .execute(
            "browser.navigate",
            serde_json::json!({ "url": "https://example.com/" }),
            &ctx(),
        )
        .await
        .unwrap_err();
    match err {
        openrt_tools::ToolError::PermissionDenied { tool, reason } => {
            assert_eq!(tool, "browser.navigate");
            assert!(
                reason.contains("AgentShield"),
                "the gate refuses sovereign egress first: {reason}"
            );
            assert!(
                !reason.contains("denied by policy"),
                "shield reasons itself, never impersonates policy: {reason}"
            );
        }
        other => panic!("expected gate-level PermissionDenied, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&ws);
}

/// Auto backend: reports the real backend when one exists on this host, or an
/// honest inactive status with a reason — mock is NEVER selected by `auto`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_backend_is_never_the_mock() {
    let ws = tmp_dir("auto");
    let rt = build(&ws, None).await;
    let status = json_out(&rt, "browser.status", serde_json::json!({})).await;
    assert_eq!(status["ok"], serde_json::json!(true), "{status}");
    let support = status["support"].as_str().unwrap_or_default();
    assert_ne!(support, "partial", "auto must not pick the mock: {status}");

    match openrt_browser::BrowserManager::new(&BrowserConfig::default()).support() {
        openrt_fabric::SupportState::Green => {
            // Real backend on this host: navigate a data: URL and verify.
            let nav = json_out(
                &rt,
                "browser.navigate",
                serde_json::json!({ "url": "data:text/html,<title>OpenRT Probe</title>" }),
            )
            .await;
            assert_eq!(nav["ok"], serde_json::json!(true), "{nav}");
            assert_ne!(nav["backend"], serde_json::json!("mock"), "{nav}");
            json_out(&rt, "browser.close", serde_json::json!({})).await;
        }
        _ => {
            // Honest unavailable/inactive: reason present, no fake session.
            assert_eq!(status["active"], serde_json::json!(false), "{status}");
            assert!(
                status["support"].as_str().is_some(),
                "support state must be reported: {status}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&ws);
}
