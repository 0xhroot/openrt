//! PostgreSQL backend over the official `psql` client (structured argv only —
//! never a shell, never logging the DSN).
//!
//! Honest platform note: this host has a `psql` client but no reachable
//! server, so the backend reports structured UNAVAILABLE until an operator
//! provides a DSN environment variable pointing at a live server. Session
//! transactions (`begin`/`commit`/`rollback`) are not supported over one-shot
//! `psql` invocations and say so; use `database.transaction` (atomic script).

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::backend::{
    ColumnInfo, DatabaseBackend, DbError, QueryResult, TableInfo, TxResult, WriteResult,
};

const CMD_TIMEOUT: Duration = Duration::from_secs(15);

pub struct PostgresBackend {
    id: String,
    dsn_env: String,
}

struct Out {
    status: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

fn run_psql(dsn: &str, extra: &[&str], sql: Option<&str>) -> Out {
    let mut cmd = Command::new("psql");
    cmd.arg("-X")
        .arg("-q")
        .arg("-v")
        .arg("ON_ERROR_STOP=1")
        .arg("-d")
        .arg(dsn)
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Out {
                status: None,
                stdout: String::new(),
                stderr: format!("spawn psql: {e}"),
                timed_out: false,
            }
        }
    };
    if let Some(sql) = sql {
        use std::io::Write;
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(sql.as_bytes());
            let _ = stdin.write_all(b"\n");
        }
    }
    drop(child.stdin.take());
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > CMD_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Out {
                        status: None,
                        stdout: String::new(),
                        stderr: format!("psql exceeded {CMD_TIMEOUT:?}"),
                        timed_out: true,
                    };
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Out {
                    status: None,
                    stdout: String::new(),
                    stderr: format!("wait psql: {e}"),
                    timed_out: false,
                };
            }
        }
    }
    let output = child
        .wait_with_output()
        .unwrap_or_else(|e| std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: Vec::new(),
            stderr: format!("collect psql: {e}").into_bytes(),
        });
    Out {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        timed_out: false,
    }
}

impl PostgresBackend {
    pub fn new(id: String, dsn_env: String) -> Self {
        Self { id, dsn_env }
    }

    fn dsn(&self) -> Result<String, DbError> {
        std::env::var(&self.dsn_env).map_err(|_| {
            DbError::Unavailable(format!(
                "environment variable `{}` (DSN) is not set — DSNs are never stored in config",
                self.dsn_env
            ))
        })
    }

    fn sql(&self, extra: &[&str], sql: Option<&str>) -> Result<String, DbError> {
        let dsn = self.dsn()?;
        let out = run_psql(&dsn, extra, sql);
        if out.timed_out {
            return Err(DbError::Timeout(out.stderr));
        }
        match out.status {
            Some(0) => Ok(out.stdout),
            _ => Err(DbError::Backend(format!(
                "psql failed: {}",
                out.stderr.trim()
            ))),
        }
    }

    /// Wrap a user SELECT so results come back as one JSON array (columns
    /// preserved as object keys), with a hard row cap.
    fn wrap_select(sql: &str, max_rows: usize) -> Result<String, DbError> {
        let t = sql.trim().trim_end_matches(';');
        if t.is_empty() {
            return Err(DbError::Invalid("empty SQL".into()));
        }
        if !t.to_ascii_lowercase().starts_with("select")
            && !t.to_ascii_lowercase().starts_with("with")
            && !t.to_ascii_lowercase().starts_with("table")
            && !t.to_ascii_lowercase().starts_with("values")
        {
            return Err(DbError::Invalid(
                "`database.query` is read-only: expected a SELECT/WITH statement".into(),
            ));
        }
        Ok(format!(
            "SELECT COALESCE(json_agg(j), '[]') FROM (SELECT * FROM ({t}) q LIMIT {max_rows}) j"
        ))
    }
}

impl DatabaseBackend for PostgresBackend {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> &'static str {
        "postgres"
    }

    fn probe(&self) -> Result<String, DbError> {
        // Client presence first (no DSN needed).
        let ver = Command::new("psql")
            .arg("--version")
            .output()
            .map_err(|e| DbError::Unavailable(format!("psql client not found: {e}")))?;
        if !ver.status.success() {
            return Err(DbError::Unavailable("psql --version failed".into()));
        }
        let client = String::from_utf8_lossy(&ver.stdout).trim().to_string();
        let dsn = self.dsn()?;
        let out = run_psql(&dsn, &["-t", "-A", "-c", "SELECT version()"], None);
        if out.timed_out {
            return Err(DbError::Timeout(out.stderr));
        }
        match out.status {
            Some(0) => Ok(format!("{client}; {}", out.stdout.trim())),
            _ => Err(DbError::Unavailable(format!(
                "cannot connect with DSN from `{}`: {}",
                self.dsn_env,
                out.stderr.trim()
            ))),
        }
    }

    fn list_tables(&self) -> Result<Vec<String>, DbError> {
        let out = self.sql(
            &["-t", "-A", "-c",
              "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public' ORDER BY table_name"],
            None,
        )?;
        Ok(out
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect())
    }

    fn describe_table(&self, name: &str) -> Result<TableInfo, DbError> {
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(DbError::Invalid(format!(
                "invalid table name `{name}` (expected [A-Za-z0-9_])"
            )));
        }
        let sql = format!(
            "SELECT column_name, data_type, (column_name = ANY (string_to_array((SELECT string_agg(column_name, ',') FROM information_schema.key_column_usage k WHERE k.table_name = t.table_name), ','))) AS pk \
             FROM information_schema.columns t WHERE table_name = '{name}' AND table_schema = 'public' ORDER BY ordinal_position"
        );
        let json = self.sql(
            &[
                "-t",
                "-A",
                "-c",
                &format!("SELECT COALESCE(json_agg(j), '[]') FROM ({sql}) j"),
            ],
            None,
        )?;
        let rows: Vec<Value> = serde_json::from_str(json.trim())
            .map_err(|e| DbError::Backend(format!("parse describe result: {e}")))?;
        if rows.is_empty() {
            return Err(DbError::NotFound(format!("table `{name}`")));
        }
        let columns = rows
            .into_iter()
            .map(|r| ColumnInfo {
                name: r["column_name"].as_str().unwrap_or_default().to_string(),
                type_name: r["data_type"].as_str().unwrap_or_default().to_string(),
                pk: r["pk"].as_bool().unwrap_or(false),
            })
            .collect();
        Ok(TableInfo {
            name: name.to_string(),
            columns,
        })
    }

    fn query(&self, sql: &str) -> Result<QueryResult, DbError> {
        let wrapped = Self::wrap_select(sql, 500)?;
        let json = self.sql(&["-t", "-A", "-c", &wrapped], None)?;
        let arr: Value = serde_json::from_str(json.trim())
            .map_err(|e| DbError::Backend(format!("parse query result: {e}")))?;
        let rows_v = arr.as_array().cloned().unwrap_or_default();
        let mut columns: Vec<String> = Vec::new();
        for r in &rows_v {
            if let Some(obj) = r.as_object() {
                for k in obj.keys() {
                    if !columns.contains(k) {
                        columns.push(k.clone());
                    }
                }
            }
        }
        let rows = rows_v
            .iter()
            .map(|r| {
                columns
                    .iter()
                    .map(|c| r.get(c).cloned().unwrap_or(Value::Null))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let row_count = rows.len();
        Ok(QueryResult {
            columns,
            rows,
            row_count,
        })
    }

    fn write(&self, sql: &str) -> Result<WriteResult, DbError> {
        let t = sql.trim();
        if t.is_empty() {
            return Err(DbError::Invalid("empty SQL".into()));
        }
        let out = self.sql(&["-c", t], None)?;
        // With `-c`, psql prints the command tag (e.g. `INSERT 0 3`) to stdout.
        let changes = out
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().last())
            .and_then(|t| t.parse::<u64>().ok())
            .unwrap_or(0);
        Ok(WriteResult {
            changes,
            last_insert_rowid: None,
            verification: serde_json::json!({
                "ok": true,
                "method": "psql command tag parsed from successful -c execution",
                "tag": out.trim(),
            }),
        })
    }

    fn begin(&self) -> Result<(), DbError> {
        Err(DbError::Unavailable(
            "one-shot psql backend holds no cross-invocation transaction; use `database.transaction` (atomic script)".into(),
        ))
    }

    fn commit(&self) -> Result<(), DbError> {
        Err(DbError::Unavailable(
            "one-shot psql backend holds no cross-invocation transaction; use `database.transaction` (atomic script)".into(),
        ))
    }

    fn rollback(&self) -> Result<(), DbError> {
        Err(DbError::Unavailable(
            "one-shot psql backend holds no cross-invocation transaction; use `database.transaction` (atomic script)".into(),
        ))
    }

    fn transaction(&self, statements: &[String]) -> Result<TxResult, DbError> {
        if statements.is_empty() {
            return Err(DbError::Invalid(
                "transaction requires at least one statement".into(),
            ));
        }
        let mut script = String::from("BEGIN;\n");
        for (i, s) in statements.iter().enumerate() {
            if s.trim().is_empty() {
                return Err(DbError::Invalid(format!("statement {i} is empty")));
            }
            script.push_str(s.trim().trim_end_matches(';'));
            script.push_str(";\n");
        }
        script.push_str("COMMIT;\n");
        let dsn = self.dsn()?;
        let out = run_psql(&dsn, &["-f", "-"], Some(&script));
        if out.timed_out {
            // Connection drop implicitly rolls the open transaction back.
            return Err(DbError::Timeout(format!(
                "transaction timed out (connection dropped; server rolled back): {}",
                out.stderr
            )));
        }
        match out.status {
            Some(0) => Ok(TxResult {
                statements_executed: statements.len(),
                total_changes: 0,
                verification: serde_json::json!({
                    "ok": true,
                    "method": "psql ON_ERROR_STOP atomic BEGIN/COMMIT script (disconnect rolls back)",
                }),
            }),
            _ => Err(DbError::Backend(format!(
                "transaction failed and was rolled back (connection dropped or ON_ERROR_STOP): {}",
                out.stderr.trim()
            ))),
        }
    }

    fn in_transaction(&self) -> bool {
        false
    }
}
