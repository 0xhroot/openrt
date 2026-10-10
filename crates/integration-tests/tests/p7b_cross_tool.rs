//! M7B cross-tool orchestration: mixed fabric flows over the ONE runtime,
//! an agent-driven scripted round-trip, and the kill switch halting fabric
//! tools (ADR-008 scope consistency).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use openrt_config::{
    Config, DatabaseConfig, DatabaseConnectionConfig, DbKind, PermissionsConfig, RuleConfig,
    RuleEffect,
};
use openrt_core::TaskStatus;
use openrt_models::mock::MockProvider;

fn tmp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "openrt-p7b-cross-{prefix}-{}-{:x}",
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

fn config_with(ws: &Path, caps: &[&str], sqlite: Option<PathBuf>) -> Config {
    let mut config = Config {
        runtime: openrt_config::RuntimeConfig {
            workspace_dir: Some(ws.to_path_buf()),
            screen_backend: Some("sim".into()),
            computer_backend: Some("sim".into()),
            ..Default::default()
        },
        permissions: allow_rules(caps),
        ..Default::default()
    };
    if let Some(path) = sqlite {
        config.fabric.database = DatabaseConfig {
            connections: vec![DatabaseConnectionConfig {
                id: "main".into(),
                kind: DbKind::Sqlite,
                sqlite_path: Some(path),
                pg_dsn_env: None,
            }],
            default: Some("main".into()),
        };
    }
    config
}

async fn build_with(config: Config, provider: Arc<MockProvider>) -> Arc<openrt_runtime::Runtime> {
    openrt_runtime::RuntimeBuilder::new()
        .config(config)
        .store(Arc::new(openrt_persistence::MemoryStore::new()))
        .with_provider(provider)
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

/// TEST (cross-tool): database → query → excel.write → excel.read in one
/// runtime through the same gate (no per-subsystem bypasses).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn database_to_excel_pipeline() {
    let ws = tmp_dir("pipe");
    let db_path = ws.join("pipe.db");
    let config = config_with(
        &ws,
        &["database.*", "excel.*", "artifact.excel.*"],
        Some(db_path),
    );
    let rt = build_with(config, Arc::new(MockProvider::echo("mock"))).await;

    json_out(
        &rt,
        "database.transaction",
        serde_json::json!({
            "statements": [
                "CREATE TABLE metrics (k TEXT, v INTEGER)",
                "INSERT INTO metrics VALUES ('latency', 42)",
                "INSERT INTO metrics VALUES ('errors', 7)"
            ]
        }),
    )
    .await;

    let rows = json_out(
        &rt,
        "database.query",
        serde_json::json!({ "sql": "SELECT k, v FROM metrics ORDER BY k" }),
    )
    .await;
    assert_eq!(rows["ok"], serde_json::json!(true), "{rows}");
    // Query rows are arrays matching `columns` (k, v).
    let table: Vec<Vec<serde_json::Value>> = rows["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|r| r.as_array().expect("row array").clone())
        .collect();
    assert_eq!(table.len(), 2, "{rows}");

    let mut grid = vec![vec![serde_json::json!("k"), serde_json::json!("v")]];
    grid.extend(table);

    let xlsx = ws.join("metrics.xlsx").display().to_string();
    let written = json_out(
        &rt,
        "excel.write",
        serde_json::json!({
            "path": xlsx,
            "sheet": "Sheet1",
            "rows": grid
        }),
    )
    .await;
    // excel.write requires an existing workbook — create first, then write.
    if written["ok"] != serde_json::json!(true) {
        json_out(
            &rt,
            "excel.create",
            serde_json::json!({ "path": xlsx.clone(), "rows": [] }),
        )
        .await;
        json_out(
            &rt,
            "excel.write",
            serde_json::json!({
                "path": xlsx,
                "sheet": "Sheet1",
                "rows": grid
            }),
        )
        .await;
    }

    let read = json_out(&rt, "excel.read", serde_json::json!({ "path": xlsx })).await;
    assert_eq!(read["ok"], serde_json::json!(true), "{read}");
    let blob = read.to_string();
    assert!(blob.contains("latency") || blob.contains("42"), "{read}");

    let _ = std::fs::remove_dir_all(&ws);
}

/// TEST (agent path): a scripted model drives real 7B tools through the full
/// production pipeline (provider → agent → gate → tool → audit/events).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_driven_excel_roundtrip() {
    let ws = tmp_dir("agent");
    let xlsx = ws.join("agent.xlsx");
    let xlsx_s = xlsx.display().to_string();

    let config = config_with(&ws, &["excel.*", "artifact.excel.*"], None);
    let provider = Arc::new(MockProvider::tool_sequence("mock").with_tool_script(vec![
        (
            "excel.create",
            serde_json::json!({
                "path": xlsx_s,
                "sheet": "Report",
                "rows": [["name", "value"], ["requests", 128]]
            }),
        ),
        ("excel.read", serde_json::json!({ "path": xlsx_s })),
    ]));
    let rt = build_with(config, provider.clone()).await;

    let session = rt.create_session("cross").await.unwrap();
    let task = rt
        .run_task(session.id, "build the report workbook and read it back")
        .await
        .unwrap();
    assert_eq!(task.status, TaskStatus::Completed, "{task:?}");
    assert!(xlsx.exists(), "agent-driven workbook missing");
    assert!(
        provider.call_count() >= 3,
        "expected create → read → final model turns, saw {}",
        provider.call_count()
    );

    let _ = std::fs::remove_dir_all(&ws);
}

/// ADR-008: a computer-scope HALT stops side-effecting fabric tools at the
/// gate — the same kill switch 7A tools honor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_switch_halts_fabric_tools() {
    let ws = tmp_dir("halt");
    let db_path = ws.join("halt.db");
    let config = config_with(&ws, &["database.*"], Some(db_path));
    let rt = build_with(config, Arc::new(MockProvider::echo("mock"))).await;

    // Works before the halt.
    json_out(
        &rt,
        "database.transaction",
        serde_json::json!({ "statements": ["CREATE TABLE t (v INTEGER)"] }),
    )
    .await;

    let tripped = rt.halt("operator halt for 7B test").await.unwrap();
    assert!(tripped, "halt must trip");

    let err = rt
        .executor
        .execute(
            "database.query",
            serde_json::json!({ "sql": "SELECT 1" }),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, openrt_tools::ToolError::KillSwitchEngaged { .. }),
        "expected KillSwitchEngaged, got {err:?}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}
