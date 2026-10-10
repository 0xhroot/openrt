//! End-to-end MCP tests against a real subprocess (fake_mcp.py): import,
//! invocation, schema refusal, deny-by-default caps, secret hygiene, drift
//! discovery, fail-closed startup, and isError mapping.

use std::collections::BTreeMap;
use std::sync::Arc;

use openrt_config::{McpConfig, McpServerConfig, McpTrust};
use openrt_mcp::{management_tools, McpManager};
use openrt_tools::{Tool, ToolOutput};

fn fixture() -> String {
    format!("{}/tests/fixtures/fake_mcp.py", env!("CARGO_MANIFEST_DIR"))
}

fn server_cfg(name: &str, extra_args: &[&str]) -> McpServerConfig {
    let mut args = vec![fixture()];
    args.extend(extra_args.iter().map(|s| s.to_string()));
    McpServerConfig {
        name: name.into(),
        command: "python3".into(),
        args,
        env: BTreeMap::new(),
        trust: McpTrust::Untrusted,
        declared_capabilities: vec![],
    }
}

fn cfg(servers: Vec<McpServerConfig>) -> McpConfig {
    McpConfig { servers }
}

async fn json_out(t: &Arc<dyn Tool>, args: serde_json::Value) -> serde_json::Value {
    match t.run(args).await.expect("tool must succeed") {
        ToolOutput::Json(v) => v,
        ToolOutput::Text(s) => serde_json::json!({ "text": s }),
    }
}

fn by_name(tools: &[Arc<dyn Tool>], name: &str) -> Arc<dyn Tool> {
    tools
        .iter()
        .find(|t| t.spec().name == name)
        .unwrap_or_else(|| panic!("{name} missing"))
        .clone()
}

#[tokio::test]
async fn imports_and_invokes_a_real_server() {
    let mut srv = server_cfg("fake", &[]);
    // Env values must reach the child but NEVER appear in any output.
    srv.env
        .insert("FAKE_MCP_TOKEN".into(), "super-secret-env-value".into());
    let manager = McpManager::connect_all(&cfg(vec![srv]));
    let tools = manager.imported_tools();
    let names: Vec<&str> = tools.iter().map(|t| t.spec().name.as_str()).collect();
    // bad_schema is refused at import; echo/add/always_fails are registered.
    assert!(names.contains(&"mcp.fake.echo"), "{names:?}");
    assert!(names.contains(&"mcp.fake.add"), "{names:?}");
    assert!(names.contains(&"mcp.fake.always_fails"), "{names:?}");
    assert!(!names.iter().any(|n| n.contains("bad_schema")), "{names:?}");

    // Two caps per imported tool (global + per server), both 3-segment.
    let echo = by_name(&tools, "mcp.fake.echo");
    let caps: Vec<&str> = echo
        .spec()
        .required_caps
        .iter()
        .map(|c| c.as_str())
        .collect();
    assert_eq!(caps, vec!["mcp.invoke.all", "mcp.fake.invoke"]);

    // Management tools.
    let mut all: Vec<Arc<dyn Tool>> = tools.clone();
    all.extend(management_tools(manager.clone()));

    // Status: connected, tools listed, env VALUES absent, key present.
    let status = json_out(&by_name(&all, "mcp.servers"), serde_json::json!({})).await;
    let blob = serde_json::to_string(&status).unwrap();
    assert_eq!(status["connected"], serde_json::json!(1));
    assert_eq!(status["servers"][0]["connected"], serde_json::json!(true));
    assert_eq!(
        status["servers"][0]["server_info"],
        serde_json::json!("fake-mcp 1.0.0 (protocol 2025-06-18)")
    );
    assert!(blob.contains("FAKE_MCP_TOKEN"), "env key names are shown");
    assert!(
        !blob.contains("super-secret-env-value"),
        "env VALUES must never appear: {blob}"
    );
    // Rejected tool visible in inventory with its reason.
    let tools_arr = status["servers"][0]["tools"].as_array().unwrap();
    let rejected = tools_arr
        .iter()
        .find(|t| t["name"] == "bad_schema")
        .expect("bad_schema listed with rejection");
    assert!(rejected["rejected"].as_str().unwrap().contains("banana"));

    manager.shutdown();
}

#[tokio::test]
async fn invocation_schema_validation_and_iserror() {
    let manager = McpManager::connect_all(&cfg(vec![server_cfg("fake", &[])]));
    let tools = manager.imported_tools();
    let echo = by_name(&tools, "mcp.fake.echo");

    // Happy path.
    let out = json_out(&echo, serde_json::json!({ "message": "hi" })).await;
    assert_eq!(out["ok"], serde_json::json!(true));
    assert_eq!(out["text"], serde_json::json!("echo: hi"));

    // Client-side schema rejection BEFORE any request (missing required).
    let err = echo
        .run(serde_json::json!({}))
        .await
        .expect_err("schema must reject");
    assert!(matches!(
        err,
        openrt_tools::ToolError::InvalidArguments(_, _)
    ));

    // Server-side isError becomes structured BACKEND_FAILURE (ok:false).
    let fails = by_name(&tools, "mcp.fake.always_fails");
    let out = match fails.run(serde_json::json!({})).await {
        Ok(ToolOutput::Json(v)) => v,
        other => panic!("expected structured output, got {other:?}"),
    };
    assert_eq!(out["ok"], serde_json::json!(false));
    assert_eq!(out["error"]["code"], serde_json::json!("BACKEND_FAILURE"));
    assert!(
        out["error"]["why"]
            .as_str()
            .unwrap()
            .contains("boom from server"),
        "{out}"
    );

    manager.shutdown();
}

#[tokio::test]
async fn discover_reports_drift_and_status_lists_rejections() {
    let marker = std::env::temp_dir().join(format!("fake-mcp-drift-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let mut srv = server_cfg("fake", &[]);
    srv.env
        .insert("FAKE_MCP_DRIFT_MARKER".into(), marker.display().to_string());
    let manager = McpManager::connect_all(&cfg(vec![srv]));
    let mut all: Vec<Arc<dyn Tool>> = manager.imported_tools();
    all.extend(management_tools(manager.clone()));

    // Before the marker exists: no drift.
    let d = json_out(&by_name(&all, "mcp.discover"), serde_json::json!({})).await;
    let drift = &d["servers"][0]["drift"];
    assert_eq!(drift["added"], serde_json::json!([]));
    assert_eq!(drift["removed"], serde_json::json!([]));

    // Create the marker → server now advertises an extra tool.
    std::fs::write(&marker, b"x").unwrap();
    let d = json_out(&by_name(&all, "mcp.discover"), serde_json::json!({})).await;
    let drift = &d["servers"][0]["drift"];
    assert_eq!(drift["added"], serde_json::json!(["drifted"]));
    assert_eq!(drift["removed"], serde_json::json!([]));
    let rejected = drift["rejected"].as_array().unwrap();
    assert!(rejected
        .iter()
        .any(|r| r.as_str().unwrap().contains("bad_schema")));

    let _ = std::fs::remove_file(&marker);
    manager.shutdown();
}

#[tokio::test]
async fn missing_binary_fails_closed_per_server() {
    let good = server_cfg("good", &[]);
    let bad = McpServerConfig {
        name: "bad".into(),
        command: "/nonexistent/definitely-not-a-server".into(),
        args: vec![],
        env: BTreeMap::new(),
        trust: McpTrust::Untrusted,
        declared_capabilities: vec![],
    };
    let manager = McpManager::connect_all(&cfg(vec![bad, good]));
    let tools = manager.imported_tools();
    let names: Vec<&str> = tools.iter().map(|t| t.spec().name.as_str()).collect();
    assert!(
        names.contains(&"mcp.good.echo"),
        "good server still imports: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains(".bad.")),
        "bad server imports nothing: {names:?}"
    );

    let status = manager.status_value();
    assert_eq!(status["failed"], serde_json::json!(1));
    let bad_entry = status["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "bad")
        .unwrap();
    assert_eq!(bad_entry["connected"], serde_json::json!(false));
    assert!(bad_entry["failure"].as_str().unwrap().contains("spawn"));
    manager.shutdown();
}

#[tokio::test]
async fn hanging_initialize_fails_closed_with_timeout() {
    let manager = McpManager::connect_all(&cfg(vec![server_cfg("hang", &["--hang"])]));
    assert!(manager.imported_tools().is_empty());
    let status = manager.status_value();
    let entry = status["servers"].as_array().unwrap().first().unwrap();
    assert_eq!(entry["connected"], serde_json::json!(false));
    let failure = entry["failure"].as_str().unwrap();
    assert!(
        failure.contains("no response within") || failure.contains("initialize"),
        "{failure}"
    );
    manager.shutdown();
}

#[tokio::test]
async fn echo_works_with_rejected_tools_present() {
    let manager = McpManager::connect_all(&cfg(vec![server_cfg("fake", &[])]));
    let tools = manager.imported_tools();
    // A healthy client keeps working even when a sibling tool was refused
    // at import (rejected tools never enter the registry).
    let echo = by_name(&tools, "mcp.fake.echo");
    let out = echo.run(serde_json::json!({ "message": "x" })).await;
    assert!(out.is_ok());
    manager.shutdown();
}
