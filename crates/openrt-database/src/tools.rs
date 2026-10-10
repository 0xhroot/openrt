//! `database.*` tools — registered on the existing ToolRegistry through the
//! normal ToolSpec/GateExecutor path (caps `database.<res>.all`).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use openrt_fabric::{fail, FabricError, FabricMeta, RiskLevel, SupportState, ToolCategory};
use openrt_tools::{Tool, ToolError, ToolOutput, ToolSpec};

use crate::backend::{DatabaseManager, DbError};

fn map_db(e: DbError, where_: &str) -> FabricError {
    match e {
        DbError::Invalid(m) => {
            FabricError::invalid_input("database operation", m, "fix the arguments/SQL and retry")
        }
        DbError::NotFound(m) => FabricError::not_found(
            "database entity",
            m,
            "use database.list_tables / configured connection ids",
        ),
        DbError::Unavailable(m) => FabricError::unavailable(
            "database backend",
            m,
            where_,
            "configure fabric.database.connections and ensure the backend (sqlite file / psql + DSN env) exists",
        ),
        DbError::Timeout(m) => FabricError::timeout(
            "database operation",
            m,
            where_,
            "the database is busy or slow; retry or reduce concurrent writers",
        ),
        DbError::Backend(m) => FabricError::backend_failure(
            "database operation",
            m,
            where_,
            "check the SQL and backend health; the transaction (if any) was rolled back",
        ),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DbOp {
    Connect,
    ListTables,
    Describe,
    Query,
    Insert,
    Update,
    Delete,
    Begin,
    Commit,
    Rollback,
    Transaction,
}

impl DbOp {
    fn name(self) -> &'static str {
        match self {
            Self::Connect => "database.connect",
            Self::ListTables => "database.list_tables",
            Self::Describe => "database.describe_table",
            Self::Query => "database.query",
            Self::Insert => "database.insert",
            Self::Update => "database.update",
            Self::Delete => "database.delete",
            Self::Begin => "database.begin",
            Self::Commit => "database.commit",
            Self::Rollback => "database.rollback",
            Self::Transaction => "database.transaction",
        }
    }
}

pub struct DatabaseTool {
    pub spec: ToolSpec,
    pub manager: Arc<DatabaseManager>,
    pub op: DbOp,
}

fn conn_arg(args: &Value) -> Option<String> {
    args.get("connection")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn stmt_class_ok(sql: &str, verb: &str) -> bool {
    let t = sql.trim_start().to_ascii_uppercase();
    t.starts_with(verb) || t.starts_with("WITH")
}

async fn blocking<T, F>(manager: Arc<DatabaseManager>, f: F) -> Result<T, FabricError>
where
    T: Send + 'static,
    F: FnOnce(Arc<DatabaseManager>) -> Result<T, FabricError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || f(manager))
        .await
        .map_err(|e| {
            FabricError::backend_failure(
                "database worker",
                format!("blocking task join error: {e}"),
                "database runtime",
                "retry; if persistent, restart the runtime",
            )
        })?
}

#[async_trait]
impl Tool for DatabaseTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let tool = self.op.name();
        let manager = self.manager.clone();
        let op = self.op;
        let args = args.clone();
        let res = blocking(manager, move |m| op_exec(&m, op, &args)).await;
        match res {
            Ok(v) => Ok(ToolOutput::Json(v)),
            Err(e) => fail(tool, e),
        }
    }
}

fn op_exec(m: &DatabaseManager, op: DbOp, args: &Value) -> Result<Value, FabricError> {
    let where_ = op.name();
    let conn = conn_arg(args);

    match op {
        DbOp::Connect => {
            let rows = m.status_blocking();
            if rows.is_empty() {
                return Err(FabricError::unavailable(
                    "database connections",
                    "no connections configured",
                    where_,
                    "add fabric.database.connections to the config",
                ));
            }
            let available = rows.iter().filter(|r| r["available"] == true).count();
            Ok(json!({
                "ok": true,
                "connections": rows,
                "available": available,
                "verification": {
                    "ok": available > 0,
                    "method": "live backend probe per configured connection",
                },
            }))
        }
        DbOp::ListTables => {
            let b = m.get(conn.as_deref()).map_err(|e| map_db(e, where_))?;
            let tables = b.list_tables().map_err(|e| map_db(e, where_))?;
            Ok(json!({
                "ok": true,
                "connection": b.id(),
                "backend": b.kind(),
                "tables": tables,
            }))
        }
        DbOp::Describe => {
            let table = args.get("table").and_then(Value::as_str).ok_or_else(|| {
                FabricError::invalid_input(
                    "database.describe_table",
                    "missing `table`",
                    "pass the table name",
                )
            })?;
            let b = m.get(conn.as_deref()).map_err(|e| map_db(e, where_))?;
            let info = b.describe_table(table).map_err(|e| map_db(e, where_))?;
            Ok(json!({
                "ok": true,
                "connection": b.id(),
                "backend": b.kind(),
                "table": info,
            }))
        }
        DbOp::Query => {
            let sql = args.get("sql").and_then(Value::as_str).ok_or_else(|| {
                FabricError::invalid_input("database.query", "missing `sql`", "pass the SQL string")
            })?;
            let max_rows = args
                .get("max_rows")
                .and_then(Value::as_u64)
                .unwrap_or(500)
                .clamp(1, 5000) as usize;
            let b = m.get(conn.as_deref()).map_err(|e| map_db(e, where_))?;
            let mut qr = b.query(sql).map_err(|e| map_db(e, where_))?;
            let mut truncated = false;
            if qr.rows.len() > max_rows {
                qr.rows.truncate(max_rows);
                qr.row_count = qr.rows.len();
                truncated = true;
            }
            Ok(json!({
                "ok": true,
                "connection": b.id(),
                "backend": b.kind(),
                "columns": qr.columns,
                "rows": qr.rows,
                "row_count": qr.row_count,
                "truncated": truncated,
                "verification": {
                    "ok": true,
                    "method": "read-only enforcement at the backend (sqlite query_only / SELECT wrapper)",
                },
            }))
        }
        DbOp::Insert | DbOp::Update | DbOp::Delete => {
            let verb = match op {
                DbOp::Insert => "INSERT",
                DbOp::Update => "UPDATE",
                _ => "DELETE",
            };
            let sql = args.get("sql").and_then(Value::as_str).ok_or_else(|| {
                FabricError::invalid_input(where_, "missing `sql`", "pass the statement SQL string")
            })?;
            if !stmt_class_ok(sql, verb) {
                return Err(FabricError::invalid_input(
                    where_,
                    format!("`{where_}` only accepts {verb} statements (or a WITH cte)"),
                    "use the matching tool (insert/update/delete) or database.transaction for other statements",
                ));
            }
            let verify_sql = args
                .get("verify_sql")
                .and_then(Value::as_str)
                .map(str::to_string);
            let sql = sql.to_string();
            let b = m.get(conn.as_deref()).map_err(|e| map_db(e, where_))?;
            let wr = b.write(&sql).map_err(|e| map_db(e, where_))?;
            let mut verification = wr.verification.clone();
            let mut ok = true;
            let mut error = Value::Null;
            if let Some(vsql) = &verify_sql {
                match b.query(vsql) {
                    Ok(re) => {
                        verification["re_read"] = json!({
                            "ok": true,
                            "columns": re.columns,
                            "rows": re.rows,
                            "row_count": re.row_count,
                        });
                    }
                    Err(e) => {
                        ok = false;
                        let fe = FabricError::verification_failed(
                            format!("{where_} re-read"),
                            map_db(e, where_).why,
                            "fix `verify_sql` or inspect the written state",
                        );
                        error = fe.to_json();
                    }
                }
            }
            Ok(json!({
                "ok": ok,
                "connection": b.id(),
                "backend": b.kind(),
                "changes": wr.changes,
                "last_insert_rowid": wr.last_insert_rowid,
                "verification": verification,
                "error": error,
            }))
        }
        DbOp::Begin => {
            let b = m.get(conn.as_deref()).map_err(|e| map_db(e, where_))?;
            b.begin().map_err(|e| map_db(e, where_))?;
            Ok(json!({
                "ok": true,
                "connection": b.id(),
                "backend": b.kind(),
                "in_transaction": b.in_transaction(),
            }))
        }
        DbOp::Commit => {
            let b = m.get(conn.as_deref()).map_err(|e| map_db(e, where_))?;
            b.commit().map_err(|e| map_db(e, where_))?;
            Ok(json!({
                "ok": true,
                "connection": b.id(),
                "backend": b.kind(),
                "in_transaction": b.in_transaction(),
                "verification": { "ok": true, "method": "COMMIT accepted by backend" },
            }))
        }
        DbOp::Rollback => {
            let b = m.get(conn.as_deref()).map_err(|e| map_db(e, where_))?;
            b.rollback().map_err(|e| map_db(e, where_))?;
            Ok(json!({
                "ok": true,
                "connection": b.id(),
                "backend": b.kind(),
                "in_transaction": b.in_transaction(),
                "verification": { "ok": true, "method": "ROLLBACK accepted by backend" },
            }))
        }
        DbOp::Transaction => {
            let stmts = args
                .get("statements")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    FabricError::invalid_input(
                        "database.transaction",
                        "missing `statements` array",
                        "pass {\"statements\": [\"SQL\", ...]}",
                    )
                })?;
            let stmts: Vec<String> = stmts
                .iter()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .collect();
            let b = m.get(conn.as_deref()).map_err(|e| map_db(e, where_))?;
            let tx = b.transaction(&stmts).map_err(|e| map_db(e, where_))?;
            Ok(json!({
                "ok": true,
                "connection": b.id(),
                "backend": b.kind(),
                "statements_executed": tx.statements_executed,
                "total_changes": tx.total_changes,
                "in_transaction": b.in_transaction(),
                "verification": tx.verification,
            }))
        }
    }
}

fn base_spec(name: &str, description: &str, cap: &str) -> ToolSpec {
    ToolSpec::new(name, description)
        .with_caps(&[cap])
        .with_schema(json!({ "type": "object", "properties": {} }))
}

/// Register all `database.*` tools over one manager.
pub fn database_tools(manager: Arc<DatabaseManager>) -> Vec<Arc<dyn Tool>> {
    let conn_schema = json!({ "type": "string", "description": "connection id (optional when a default/single connection exists)" });
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();

    tools.push(Arc::new(DatabaseTool {
        spec: base_spec(
            "database.connect",
            "Probe every configured database connection and report availability, backend kind, and version (no SQL executed).",
            "database.connect.all",
        ),
        manager: manager.clone(),
        op: DbOp::Connect,
    }));
    tools.push(Arc::new(DatabaseTool {
        spec: ToolSpec::new(
            "database.list_tables",
            "List tables in the selected database connection.",
        )
        .with_caps(&["database.read.all"])
        .with_schema(json!({
            "type": "object",
            "properties": { "connection": conn_schema.clone() }
        })),
        manager: manager.clone(),
        op: DbOp::ListTables,
    }));
    tools.push(Arc::new(DatabaseTool {
        spec: ToolSpec::new(
            "database.describe_table",
            "Describe one table's columns, types, and primary keys.",
        )
        .with_caps(&["database.read.all"])
        .with_schema(json!({
            "type": "object",
            "required": ["table"],
            "properties": {
                "table": { "type": "string" },
                "connection": conn_schema.clone()
            }
        })),
        manager: manager.clone(),
        op: DbOp::Describe,
    }));
    tools.push(Arc::new(DatabaseTool {
        spec: ToolSpec::new(
            "database.query",
            "Run a read-only SQL query (high risk: raw SQL). Backend enforces read-only; row count capped; structured errors.",
        )
        .with_caps(&["database.query.all"])
        .with_schema(json!({
            "type": "object",
            "required": ["sql"],
            "properties": {
                "sql": { "type": "string" },
                "connection": conn_schema.clone(),
                "max_rows": { "type": "integer", "minimum": 1, "maximum": 5000, "default": 500 }
            }
        })),
        manager: manager.clone(),
        op: DbOp::Query,
    }));
    for (op, name) in [
        (DbOp::Insert, "database.insert"),
        (DbOp::Update, "database.update"),
        (DbOp::Delete, "database.delete"),
    ] {
        let verb = match op {
            DbOp::Insert => "INSERT",
            DbOp::Update => "UPDATE",
            _ => "DELETE",
        };
        tools.push(Arc::new(DatabaseTool {
            spec: ToolSpec::new(
                name,
                &format!(
                    "Execute one {verb} statement (high risk: raw SQL). Optional `verify_sql` re-reads state after the write for verification."
                ),
            )
            .with_caps(&["database.write.all"])
            .with_schema(json!({
                "type": "object",
                "required": ["sql"],
                "properties": {
                    "sql": { "type": "string" },
                    "connection": conn_schema.clone(),
                    "verify_sql": { "type": "string", "description": "read-only SQL run after the write to verify the post-condition" }
                }
            })),
            manager: manager.clone(),
            op,
        }));
    }
    for (op, name) in [
        (DbOp::Begin, "database.begin"),
        (DbOp::Commit, "database.commit"),
        (DbOp::Rollback, "database.rollback"),
    ] {
        let description = match op {
            DbOp::Begin => "Begin a session transaction on the connection (SQLite; one-shot psql reports structured UNAVAILABLE).",
            DbOp::Commit => "Commit the open session transaction.",
            _ => "Roll back the open session transaction.",
        };
        tools.push(Arc::new(DatabaseTool {
            spec: ToolSpec::new(name, description)
                .with_caps(&["database.transaction.all"])
                .with_schema(json!({
                    "type": "object",
                    "properties": { "connection": conn_schema.clone() }
                })),
            manager: manager.clone(),
            op,
        }));
    }
    tools.push(Arc::new(DatabaseTool {
        spec: ToolSpec::new(
            "database.transaction",
            "Run statements atomically (BEGIN IMMEDIATE … COMMIT; any failure rolls the whole batch back).",
        )
        .with_caps(&["database.transaction.all"])
        .with_schema(json!({
            "type": "object",
            "required": ["statements"],
            "properties": {
                "statements": { "type": "array", "items": { "type": "string" }, "minItems": 1 },
                "connection": conn_schema.clone()
            }
        })),
        manager,
        op: DbOp::Transaction,
    }));
    tools
}

/// Catalog metadata for every `database.*` tool.
pub fn database_metas(host: &str) -> Vec<FabricMeta> {
    let mk = |tool: &str, risk: RiskLevel, effects: &[&str], rollback: &str| {
        FabricMeta::new(tool, ToolCategory::Database, risk)
            .with_platform(host, SupportState::Green)
            .side_effects(effects)
            .rollback(rollback)
    };
    vec![
        mk(
            "database.connect",
            RiskLevel::Low,
            &["connects to configured database (no SQL)"],
            "none (probe only)",
        ),
        mk(
            "database.list_tables",
            RiskLevel::Medium,
            &["reads schema catalog"],
            "none (read-only)",
        ),
        mk(
            "database.describe_table",
            RiskLevel::Medium,
            &["reads schema catalog"],
            "none (read-only)",
        ),
        FabricMeta::new("database.query", ToolCategory::Database, RiskLevel::High)
            .with_platform(host, SupportState::Green)
            .side_effects(&["runs raw SQL (backend-enforced read-only)"])
            .rollback("none (read-only at backend)"),
        mk(
            "database.insert",
            RiskLevel::High,
            &["writes rows"],
            "compensating DELETE via database.delete",
        ),
        mk(
            "database.update",
            RiskLevel::High,
            &["updates rows"],
            "restore prior values (no automatic undo)",
        ),
        mk(
            "database.delete",
            RiskLevel::High,
            &["deletes rows"],
            "none (data loss is irreversible)",
        ),
        mk(
            "database.begin",
            RiskLevel::High,
            &["opens a write transaction"],
            "database.rollback",
        ),
        mk(
            "database.commit",
            RiskLevel::High,
            &["commits transaction changes"],
            "none after commit (rollback before)",
        ),
        mk(
            "database.rollback",
            RiskLevel::Medium,
            &["discards open transaction changes"],
            "n/a (this IS the rollback)",
        ),
        FabricMeta::new(
            "database.transaction",
            ToolCategory::Database,
            RiskLevel::High,
        )
        .with_platform(host, SupportState::Green)
        .side_effects(&["atomic multi-statement write"])
        .rollback("automatic rollback on failure; restore manually on success"),
    ]
}
