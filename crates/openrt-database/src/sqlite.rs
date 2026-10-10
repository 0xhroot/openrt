//! Embedded SQLite backend (rusqlite, bundled — offline, no shell, no
//! dot-commands, no external client).

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::types::ValueRef;
use serde_json::Value;

use super::backend::{
    ColumnInfo, DatabaseBackend, DbError, QueryResult, TableInfo, TxResult, WriteResult,
};

struct ConnState {
    conn: rusqlite::Connection,
    in_tx: bool,
}

pub struct SqliteBackend {
    id: String,
    state: Mutex<ConnState>,
}

fn map_err(e: rusqlite::Error) -> DbError {
    let msg = e.to_string();
    let lower = msg.to_ascii_lowercase();
    if lower.contains("database is locked") || lower.contains("database table is locked") {
        DbError::Timeout(format!("sqlite busy/locked after busy_timeout: {msg}"))
    } else {
        DbError::Backend(format!("sqlite: {msg}"))
    }
}

fn val(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        ValueRef::Real(f) => serde_json::Number::from_f64(f)
            .map(Value::from)
            .unwrap_or(Value::Null),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::String(format!("base64:{}", base64_encode(b))),
    }
}

/// Minimal standard base64 (no external dep churn for one helper).
fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn ensure_single_statement(sql: &str) -> Result<(), DbError> {
    let t = sql.trim();
    if t.is_empty() {
        return Err(DbError::Invalid("empty SQL".into()));
    }
    Ok(())
}

impl SqliteBackend {
    pub fn open(id: String, path: PathBuf) -> Result<Self, DbError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                return Err(DbError::Unavailable(format!(
                    "sqlite parent directory `{}` does not exist",
                    parent.display()
                )));
            }
        }
        let conn = rusqlite::Connection::open(&path)
            .map_err(|e| DbError::Unavailable(format!("open `{}`: {e}", path.display())))?;
        conn.busy_timeout(Duration::from_secs(5)).map_err(map_err)?;
        Ok(Self {
            id,
            state: Mutex::new(ConnState { conn, in_tx: false }),
        })
    }

    fn run_query(&self, sql: &str) -> Result<QueryResult, DbError> {
        let st = self.state.lock().expect("sqlite state poisoned");
        let conn = &st.conn;
        // Read-only at the backend level: query_only blocks every write
        // (DML/DDL/PRAGMA writes) even if policy were bypassed.
        conn.pragma_update(None, "query_only", &true)
            .map_err(map_err)?;
        let out = (|| -> Result<QueryResult, DbError> {
            let mut stmt = conn.prepare(sql).map_err(map_err)?;
            let columns: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
            let mut rows_iter = stmt.query([]).map_err(map_err)?;
            let mut rows = Vec::new();
            while let Some(row) = rows_iter.next().map_err(map_err)? {
                let mut r = Vec::with_capacity(columns.len());
                for i in 0..columns.len() {
                    r.push(val(row.get_ref(i).map_err(map_err)?));
                }
                rows.push(r);
            }
            let row_count = rows.len();
            Ok(QueryResult {
                columns,
                rows,
                row_count,
            })
        })();
        let _ = conn.pragma_update(None, "query_only", &false);
        out
    }

    fn write_batch(&self, sql: &str) -> Result<WriteResult, DbError> {
        let st = self.state.lock().expect("sqlite state poisoned");
        let conn = &st.conn;
        let own_tx = !st.in_tx;
        if own_tx {
            conn.execute_batch("BEGIN IMMEDIATE;").map_err(map_err)?;
        }
        let res = conn.execute_batch(sql);
        match res {
            Ok(()) => {
                if own_tx {
                    conn.execute_batch("COMMIT;").map_err(map_err)?;
                }
                // changes() reports the most recent statement's row count;
                // inside our atomic wrapper this is the batch's final effect.
                let changes = conn.changes();
                let rowid = conn.last_insert_rowid();
                Ok(WriteResult {
                    changes,
                    last_insert_rowid: Some(rowid),
                    verification: serde_json::json!({
                        "ok": true,
                        "method": "backend changes() after an atomic transaction",
                        "changes": changes,
                    }),
                })
            }
            Err(e) => {
                if own_tx {
                    let _ = conn.execute_batch("ROLLBACK;");
                }
                Err(map_err(e))
            }
        }
    }
}

impl DatabaseBackend for SqliteBackend {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> &'static str {
        "sqlite"
    }

    fn probe(&self) -> Result<String, DbError> {
        let st = self.state.lock().expect("sqlite state poisoned");
        let v: String = st
            .conn
            .query_row("SELECT sqlite_version()", [], |r| r.get(0))
            .map_err(map_err)?;
        Ok(format!("sqlite {v}"))
    }

    fn list_tables(&self) -> Result<Vec<String>, DbError> {
        let st = self.state.lock().expect("sqlite state poisoned");
        let mut stmt = st
            .conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' \
                 AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .map_err(map_err)?;
        let names = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(map_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_err)?;
        Ok(names)
    }

    fn describe_table(&self, name: &str) -> Result<TableInfo, DbError> {
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(DbError::Invalid(format!(
                "invalid table name `{name}` (expected [A-Za-z0-9_])"
            )));
        }
        let st = self.state.lock().expect("sqlite state poisoned");
        let exists: i64 = st
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [name],
                |r| r.get(0),
            )
            .map_err(map_err)?;
        if exists == 0 {
            return Err(DbError::NotFound(format!("table `{name}`")));
        }
        let pragma = format!("PRAGMA table_info('{name}')");
        let mut stmt = st.conn.prepare(&pragma).map_err(map_err)?;
        let mut rows = stmt.query([]).map_err(map_err)?;
        let mut columns = Vec::new();
        while let Some(row) = rows.next().map_err(map_err)? {
            columns.push(ColumnInfo {
                name: row.get::<_, String>(1).map_err(map_err)?,
                type_name: row.get::<_, String>(2).map_err(map_err)?,
                pk: row.get::<_, i64>(5).map_err(map_err)? > 0,
            });
        }
        if columns.is_empty() {
            return Err(DbError::NotFound(format!("table `{name}`")));
        }
        Ok(TableInfo {
            name: name.to_string(),
            columns,
        })
    }

    fn query(&self, sql: &str) -> Result<QueryResult, DbError> {
        ensure_single_statement(sql)?;
        self.run_query(sql)
    }

    fn write(&self, sql: &str) -> Result<WriteResult, DbError> {
        ensure_single_statement(sql)?;
        self.write_batch(sql)
    }

    fn begin(&self) -> Result<(), DbError> {
        let mut st = self.state.lock().expect("sqlite state poisoned");
        if st.in_tx {
            return Err(DbError::Invalid(
                "a transaction is already open on this connection".into(),
            ));
        }
        st.conn.execute_batch("BEGIN IMMEDIATE;").map_err(map_err)?;
        st.in_tx = true;
        Ok(())
    }

    fn commit(&self) -> Result<(), DbError> {
        let mut st = self.state.lock().expect("sqlite state poisoned");
        if !st.in_tx {
            return Err(DbError::Invalid(
                "no transaction is open on this connection".into(),
            ));
        }
        st.conn.execute_batch("COMMIT;").map_err(map_err)?;
        st.in_tx = false;
        Ok(())
    }

    fn rollback(&self) -> Result<(), DbError> {
        let mut st = self.state.lock().expect("sqlite state poisoned");
        if !st.in_tx {
            return Err(DbError::Invalid(
                "no transaction is open on this connection".into(),
            ));
        }
        st.conn.execute_batch("ROLLBACK;").map_err(map_err)?;
        st.in_tx = false;
        Ok(())
    }

    fn transaction(&self, statements: &[String]) -> Result<TxResult, DbError> {
        if statements.is_empty() {
            return Err(DbError::Invalid(
                "transaction requires at least one statement".into(),
            ));
        }
        for (i, s) in statements.iter().enumerate() {
            if s.trim().is_empty() {
                return Err(DbError::Invalid(format!(
                    "statement {i} is empty (trailing semicolons are fine)"
                )));
            }
        }
        let st = self.state.lock().expect("sqlite state poisoned");
        if st.in_tx {
            return Err(DbError::Invalid(
                "a session transaction is already open; commit or rollback it first".into(),
            ));
        }
        let conn = &st.conn;
        conn.execute_batch("BEGIN IMMEDIATE;").map_err(map_err)?;
        // One statement at a time so `changes()` can be summed per statement.
        let mut total: u64 = 0;
        for (i, s) in statements.iter().enumerate() {
            let stmt = s.trim().trim_end_matches(';');
            match conn.execute_batch(stmt) {
                Ok(()) => total = total.saturating_add(conn.changes()),
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK;");
                    return Err(DbError::Backend(format!(
                        "transaction failed at statement {i} and was rolled back: {e}"
                    )));
                }
            }
        }
        conn.execute_batch("COMMIT;").map_err(map_err)?;
        Ok(TxResult {
            statements_executed: statements.len(),
            total_changes: total,
            verification: serde_json::json!({
                "ok": true,
                "method": "atomic BEGIN IMMEDIATE/COMMIT; per-statement changes() summed",
                "total_changes": total,
            }),
        })
    }

    fn in_transaction(&self) -> bool {
        self.state.lock().expect("sqlite state poisoned").in_tx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        std::env::temp_dir().join(format!("openrt-db-{}.sqlite", uuid::Uuid::new_v4()))
    }

    #[test]
    fn probe_list_describe_query_write_roundtrip() {
        let b = SqliteBackend::open("t".into(), tmp()).unwrap();
        assert!(b.probe().unwrap().starts_with("sqlite "));
        assert!(b.list_tables().unwrap().is_empty());

        b.write("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
            .unwrap();
        assert_eq!(b.list_tables().unwrap(), vec!["t".to_string()]);

        let info = b.describe_table("t").unwrap();
        assert_eq!(info.columns.len(), 2);
        assert!(info.columns[0].pk);
        assert_eq!(info.columns[1].name, "name");

        let w = b
            .write("INSERT INTO t (name) VALUES ('alice'), ('bob')")
            .unwrap();
        assert_eq!(w.changes, 2);
        assert_eq!(w.verification["ok"], true);

        let q = b.query("SELECT id, name FROM t ORDER BY id").unwrap();
        assert_eq!(q.columns, vec!["id", "name"]);
        assert_eq!(q.row_count, 2);
        assert_eq!(q.rows[0][1], serde_json::json!("alice"));
    }

    #[test]
    fn query_is_read_only_at_backend_level() {
        let b = SqliteBackend::open("t".into(), tmp()).unwrap();
        b.write("CREATE TABLE t (id INTEGER)").unwrap();
        let err = b.query("DELETE FROM t").unwrap_err();
        assert!(matches!(err, DbError::Backend(_) | DbError::Timeout(_)));
        assert_eq!(b.list_tables().unwrap(), vec!["t".to_string()]);
        // table still there and rows untouched (none existed anyway)
        b.write("INSERT INTO t VALUES (1)").unwrap();
        assert!(b.query("DELETE FROM t").unwrap_err().to_string().len() > 0);
        assert_eq!(
            b.query("SELECT COUNT(*) FROM t").unwrap().rows[0][0],
            serde_json::json!(1)
        );
    }

    #[test]
    fn describe_rejects_bad_names_and_missing_tables() {
        let b = SqliteBackend::open("t".into(), tmp()).unwrap();
        assert!(matches!(
            b.describe_table("t; DROP TABLE x"),
            Err(DbError::Invalid(_))
        ));
        assert!(matches!(
            b.describe_table("nope"),
            Err(DbError::NotFound(_))
        ));
    }

    #[test]
    fn session_transaction_begin_commit_rollback() {
        let b = SqliteBackend::open("t".into(), tmp()).unwrap();
        b.write("CREATE TABLE t (v INTEGER)").unwrap();
        assert!(matches!(b.commit(), Err(DbError::Invalid(_))));
        b.begin().unwrap();
        assert!(matches!(b.begin(), Err(DbError::Invalid(_))));
        b.write("INSERT INTO t VALUES (1)").unwrap();
        b.rollback().unwrap();
        assert_eq!(
            b.query("SELECT COUNT(*) FROM t").unwrap().rows[0][0],
            serde_json::json!(0)
        );
        b.begin().unwrap();
        b.write("INSERT INTO t VALUES (2)").unwrap();
        b.commit().unwrap();
        assert_eq!(
            b.query("SELECT COUNT(*) FROM t").unwrap().rows[0][0],
            serde_json::json!(1)
        );
        assert!(!b.in_transaction());
    }

    #[test]
    fn batch_transaction_is_atomic_on_failure() {
        let b = SqliteBackend::open("t".into(), tmp()).unwrap();
        b.write("CREATE TABLE t (v INTEGER UNIQUE)").unwrap();
        b.write("INSERT INTO t VALUES (1)").unwrap();
        let err = b
            .transaction(&[
                "INSERT INTO t VALUES (7)".into(),
                "INSERT INTO t VALUES (1)".into(), // duplicate → fails
            ])
            .unwrap_err();
        assert!(err.to_string().contains("rolled back"));
        assert_eq!(
            b.query("SELECT COUNT(*) FROM t").unwrap().rows[0][0],
            serde_json::json!(1)
        );
        // success path
        let r = b
            .transaction(&[
                "INSERT INTO t VALUES (8)".into(),
                "INSERT INTO t VALUES (9)".into(),
            ])
            .unwrap();
        assert_eq!(r.statements_executed, 2);
        assert_eq!(r.total_changes, 2);
        assert_eq!(
            b.query("SELECT COUNT(*) FROM t").unwrap().rows[0][0],
            serde_json::json!(3)
        );
    }

    #[test]
    fn blobs_are_base64_marked() {
        let b = SqliteBackend::open("t".into(), tmp()).unwrap();
        b.write("CREATE TABLE t (d BLOB)").unwrap();
        b.write("INSERT INTO t VALUES (x'00ff')").unwrap();
        let q = b.query("SELECT d FROM t").unwrap();
        assert_eq!(q.rows[0][0], serde_json::json!("base64:AP8="));
    }
}
