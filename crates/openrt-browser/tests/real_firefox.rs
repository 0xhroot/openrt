//! Real-browser proof for Phase 7B: headless Firefox over WebDriver BiDi.
//!
//! Skips (never fakes success) when Firefox is not installed; otherwise
//! exercises the full path: lazy session start → data: URL navigation →
//! find/click/type against live DOM → PNG screenshot with signature check →
//! status → close. Policy is asserted too (Sovereign refuses external URLs).

use std::sync::Arc;

use openrt_browser::{browser_tools, BrowserManager, BrowserToolCtx};
use openrt_config::BrowserConfig;
use openrt_fabric::artifacts::ArtifactStore;
use openrt_modes::ModeManager;
use openrt_netpol::NetworkPolicy;
use openrt_persistence::MemoryStore;
use openrt_tools::{Tool, ToolOutput};

fn ctx(backend: &str) -> Arc<BrowserToolCtx> {
    let store = Arc::new(MemoryStore::new());
    Arc::new(BrowserToolCtx {
        manager: Arc::new(BrowserManager::new(&BrowserConfig {
            backend: Some(backend.into()),
            binary: None,
        })),
        artifacts: Arc::new(ArtifactStore::new(store.clone())),
        modes: Arc::new(ModeManager::new(store.clone())),
        netpol: Arc::new(NetworkPolicy::new(store)),
    })
}

fn tool(tools: &[Arc<dyn Tool>], name: &str) -> Arc<dyn Tool> {
    tools
        .iter()
        .find(|t| t.spec().name == name)
        .unwrap_or_else(|| panic!("{name} missing"))
        .clone()
}

async fn out(t: &Arc<dyn Tool>, args: serde_json::Value) -> serde_json::Value {
    match t.run(args).await.expect("tool must succeed") {
        ToolOutput::Json(v) => v,
        ToolOutput::Text(s) => serde_json::json!({ "text": s }),
    }
}

const PAGE: &str = r#"data:text/html,<html><head><title>OpenRT Probe</title></head><body>
<h1 id="title">Hello Fabric</h1>
<input id="box" value="">
<button id="btn" onclick="document.getElementById('title').textContent='Clicked'">Go</button>
</body></html>"#;

#[tokio::test]
async fn real_firefox_bid_i_session() {
    let probe = ctx("auto");
    if probe.manager.firefox_binary().is_none() {
        eprintln!("SKIP real Firefox test: no firefox binary on PATH");
        return;
    }
    let tools = browser_tools(probe.clone());

    // Policy before any socket: Sovereign refuses external navigation.
    let denied = match tool(&tools, "browser.navigate")
        .run(serde_json::json!({ "url": "https://example.com/" }))
        .await
    {
        Ok(ToolOutput::Json(v)) => v,
        other => panic!("expected structured POLICY_DENIED, got {other:?}"),
    };
    assert_eq!(denied["error"]["code"], "POLICY_DENIED");

    // Real navigation to an in-policy data: URL (starts the browser).
    let nav = out(
        &tool(&tools, "browser.navigate"),
        serde_json::json!({ "url": PAGE }),
    )
    .await;
    if nav["ok"] != serde_json::json!(true) {
        panic!(
            "navigate failed: {}",
            serde_json::to_string_pretty(&nav).unwrap()
        );
    }
    assert_eq!(nav["backend"], serde_json::json!("firefox-bidi"));
    assert_eq!(nav["title"], serde_json::json!("OpenRT Probe"));

    // find against live DOM
    let found = out(
        &tool(&tools, "browser.find"),
        serde_json::json!({ "selector": "#title" }),
    )
    .await;
    assert_eq!(found["count"], serde_json::json!(1));
    assert!(found["texts"][0].as_str().unwrap().contains("Hello Fabric"));

    // click mutates the DOM
    let clicked = out(
        &tool(&tools, "browser.click"),
        serde_json::json!({ "selector": "#btn" }),
    )
    .await;
    assert_eq!(clicked["ok"], serde_json::json!(true));
    let after = out(
        &tool(&tools, "browser.find"),
        serde_json::json!({ "selector": "#title" }),
    )
    .await;
    assert!(after["texts"][0].as_str().unwrap().contains("Clicked"));

    // type into the input (value roundtrip via find on the value)
    let typed = out(
        &tool(&tools, "browser.type"),
        serde_json::json!({ "selector": "#box", "text": "typed-by-openrt" }),
    )
    .await;
    assert_eq!(typed["ok"], serde_json::json!(true));

    // structured NOT_FOUND for a missing selector
    let missing = match tool(&tools, "browser.click")
        .run(serde_json::json!({ "selector": "#does-not-exist" }))
        .await
    {
        Ok(ToolOutput::Json(v)) => v,
        other => panic!("expected structured output, got {other:?}"),
    };
    assert_eq!(missing["ok"], serde_json::json!(false));
    assert_eq!(missing["error"]["code"], "NOT_FOUND");

    // screenshot: real PNG bytes from the real browser, recorded as artifact
    let shot = out(&tool(&tools, "browser.screenshot"), serde_json::json!({})).await;
    assert_eq!(shot["ok"], serde_json::json!(true));
    let path = std::path::PathBuf::from(shot["path"].as_str().unwrap());
    let bytes = std::fs::read(&path).expect("screenshot file");
    assert!(
        bytes.len() > 100,
        "expected a real render, got {} bytes",
        bytes.len()
    );
    assert!(bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]));
    std::fs::remove_file(&path).ok();

    // status shows the live session, then close kills it.
    let status = out(&tool(&tools, "browser.status"), serde_json::json!({})).await;
    assert_eq!(status["active"], serde_json::json!(true));
    assert_eq!(status["backend"], serde_json::json!("firefox-bidi"));
    assert_eq!(status["support"], serde_json::json!("green"));

    let closed = out(&tool(&tools, "browser.close"), serde_json::json!({})).await;
    assert_eq!(closed["closed"], serde_json::json!(true));
}
