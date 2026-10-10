//! M7B database acceptance through the real gate: SQLite round-trip with
//! verification, read-only enforcement, session transactions, and honest
//! UNAVAILABLE when no connection is configured.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use openrt_config::{
    Config, DatabaseConfig, DatabaseConnectionConfig, DbKind, PermissionsConfig, RuleConfig,
    RuleEffect,
};

fn tmp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "openrt-p7b-db-{prefix}-{}-{:x}",
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

fn base_config(ws: &Path) -> Config {
    Config {
        runtime: openrt_config::RuntimeConfig {
            workspace_dir: Some(ws.to_path_buf()),
            screen_backend: Some("sim".into()),
            computer_backend: Some("sim".into()),
            ..Default::default()
        },
        permissions: allow_rules(&["database.*"]),
        ..Default::default()
    }
}

async fn build(config: Config) -> Arc<openrt_runtime::Runtime> {
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

/// Connect → schema → insert(+verify) → query → list/describe, all through
/// the production gate, with read-only enforcement on `database.query`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_roundtrip_through_the_gate() {
    let ws = tmp_dir("rt");
    let db_path = ws.join("fabric.db");
    let mut config = base_config(&ws);
    config.fabric.database = DatabaseConfig {
        connections: vec![DatabaseConnectionConfig {
            id: "main".into(),
            kind: DbKind::Sqlite,
            sqlite_path: Some(db_path.clone()),
            pg_dsn_env: None,
        }],
        default: Some("main".into()),
    };
    let rt = build(config).await;

    // Probe: sqlite file is created and reports a version.
    let conn = json_out(&rt, "database.connect", serde_json::json!({})).await;
    assert_eq!(conn["ok"], serde_json::json!(true), "{conn}");

    // Schema + seed rows through the atomic transaction tool (any statements).
    let tx = json_out(
        &rt,
        "database.transaction",
        serde_json::json!({
            "statements": [
                "CREATE TABLE events (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
                "INSERT INTO events (name) VALUES ('alpha')",
                "INSERT INTO events (name) VALUES ('beta')",
            ]
        }),
    )
    .await;
    assert_eq!(tx["ok"], serde_json::json!(true), "{tx}");

    // Read-back through the read-only query tool.
    let rows = json_out(
        &rt,
        "database.query",
        serde_json::json!({ "sql": "SELECT id, name FROM events ORDER BY id" }),
    )
    .await;
    assert_eq!(rows["ok"], serde_json::json!(true), "{rows}");
    let cols = rows["columns"].as_array().expect("columns array");
    let name_i = cols.iter().position(|c| c == "name").expect("name column");
    let data = rows["rows"].as_array().expect("rows array");
    assert_eq!(data.len(), 2, "{rows}");
    assert_eq!(data[0][name_i], serde_json::json!("alpha"), "{rows}");

    // insert tool + verify_sql (observe → verify through the tool itself).
    let ins = json_out(
        &rt,
        "database.insert",
        serde_json::json!({
            "sql": "INSERT INTO events (name) VALUES ('gamma')",
            "verify_sql": "SELECT COUNT(*) AS n FROM events WHERE name = 'gamma'"
        }),
    )
    .await;
    assert_eq!(ins["ok"], serde_json::json!(true), "{ins}");

    // Read-only enforcement: a write through database.query must be refused
    // by the backend (structured failure — never a silent success).
    let ro = json_out(
        &rt,
        "database.query",
        serde_json::json!({ "sql": "DELETE FROM events" }),
    )
    .await;
    assert_eq!(ro["ok"], serde_json::json!(false), "write via query: {ro}");
    assert!(ro["error"]["code"].is_string(), "{ro}");

    // Discovery surfaces.
    let tables = json_out(&rt, "database.list_tables", serde_json::json!({})).await;
    assert_eq!(tables["ok"], serde_json::json!(true), "{tables}");
    assert!(
        tables.to_string().contains("events"),
        "events table missing: {tables}"
    );
    let desc = json_out(
        &rt,
        "database.describe_table",
        serde_json::json!({ "table": "events" }),
    )
    .await;
    assert_eq!(desc["ok"], serde_json::json!(true), "{desc}");

    let _ = std::fs::remove_dir_all(&ws);
}

/// Session transaction: begin → insert → rollback leaves state untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_transaction_rolls_back() {
    let ws = tmp_dir("tx");
    let db_path = ws.join("tx.db");
    let mut config = base_config(&ws);
    config.fabric.database = DatabaseConfig {
        connections: vec![DatabaseConnectionConfig {
            id: "main".into(),
            kind: DbKind::Sqlite,
            sqlite_path: Some(db_path),
            pg_dsn_env: None,
        }],
        default: Some("main".into()),
    };
    let rt = build(config).await;

    json_out(
        &rt,
        "database.transaction",
        serde_json::json!({ "statements": ["CREATE TABLE t (v INTEGER)"] }),
    )
    .await;

    let begin = json_out(&rt, "database.begin", serde_json::json!({})).await;
    assert_eq!(begin["ok"], serde_json::json!(true), "{begin}");
    let ins = json_out(
        &rt,
        "database.insert",
        serde_json::json!({ "sql": "INSERT INTO t (v) VALUES (1)" }),
    )
    .await;
    assert_eq!(ins["ok"], serde_json::json!(true), "{ins}");
    let rb = json_out(&rt, "database.rollback", serde_json::json!({})).await;
    assert_eq!(rb["ok"], serde_json::json!(true), "{rb}");

    let rows = json_out(
        &rt,
        "database.query",
        serde_json::json!({ "sql": "SELECT COUNT(*) AS n FROM t" }),
    )
    .await;
    assert_eq!(rows["ok"], serde_json::json!(true), "{rows}");
    let n = rows["rows"][0][0]
        .as_i64()
        .unwrap_or_else(|| panic!("unexpected row: {rows}"));
    assert_eq!(n, 0, "rollback did not discard the insert: {rows}");

    let _ = std::fs::remove_dir_all(&ws);
}

/// No configured connection ⇒ structured UNAVAILABLE (honest, not fake ok).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_connection_reports_unavailable() {
    let ws = tmp_dir("none");
    let rt = build(base_config(&ws)).await;
    for (name, args) in [
        ("database.connect", serde_json::json!({})),
        ("database.list_tables", serde_json::json!({})),
        ("database.query", serde_json::json!({ "sql": "SELECT 1" })),
    ] {
        let out = json_out(&rt, name, args).await;
        assert_eq!(out["ok"], serde_json::json!(false), "{name}: {out}");
        assert_eq!(
            out["error"]["code"],
            serde_json::json!("UNAVAILABLE"),
            "{name}: {out}"
        );
        assert!(
            out["error"]["why"]
                .as_str()
                .unwrap_or("")
                .contains("connection")
                || out["error"]["why"]
                    .as_str()
                    .unwrap_or("")
                    .contains("configured")
                || out["error"]["why"].as_str().unwrap_or("").contains("no"),
            "{name}: {out}"
        );
    }
    let _ = std::fs::remove_dir_all(&ws);
}
