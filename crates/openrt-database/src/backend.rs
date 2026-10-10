//! Database backend contract + manager (M7B.4).
//!
//! Every backend is reached only through this trait; tools never see raw
//! clients. Connections come from operator config (by id) — tool callers can
//! never supply paths or DSNs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use openrt_config::{DatabaseConfig, DbKind};

/// Normalized database errors (mapped to the fabric error model by tools).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DbError {
    /// Bad caller input (empty SQL, wrong statement class, …).
    #[error("invalid input: {0}")]
    Invalid(String),
    /// Referenced entity does not exist (connection, table).
    #[error("not found: {0}")]
    NotFound(String),
    /// Backend/connection not available on this host.
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// Operation exceeded its time budget (busy timeout / command timeout).
    #[error("timeout: {0}")]
    Timeout(String),
    /// Backend executed but failed.
    #[error("backend failure: {0}")]
    Backend(String),
}

/// Result of a row-returning query (column-oriented, JSON-friendly).
#[derive(Debug, Clone, Serialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub row_count: usize,
}

/// Result of a write (DML/DDL): affected rows + re-read verification.
#[derive(Debug, Clone, Serialize)]
pub struct WriteResult {
    pub changes: u64,
    /// `last_insert_rowid()` when the backend supports it (SQLite).
    pub last_insert_rowid: Option<i64>,
    /// Post-condition verification (backend-native `changes()` / re-read).
    pub verification: serde_json::Value,
}

/// Result of an atomic multi-statement transaction.
#[derive(Debug, Clone, Serialize)]
pub struct TxResult {
    pub statements_executed: usize,
    pub total_changes: u64,
    pub verification: serde_json::Value,
}

/// One column's schema info.
#[derive(Debug, Clone, Serialize)]
pub struct ColumnInfo {
    pub name: String,
    pub type_name: String,
    pub pk: bool,
}

/// One table's schema info.
#[derive(Debug, Clone, Serialize)]
pub struct TableInfo {
    pub name: String,
    pub columns: Vec<ColumnInfo>,
}

/// The database backend contract. All methods are synchronous internally
/// (callers wrap with `spawn_blocking`); state (open transaction) lives inside
/// the backend instance.
pub trait DatabaseBackend: Send + Sync {
    /// Connection id (from operator config).
    fn id(&self) -> &str;
    /// `"sqlite"` or `"postgres"`.
    fn kind(&self) -> &'static str;
    /// Connectivity probe: version string on success, Unavailable on failure.
    fn probe(&self) -> Result<String, DbError>;
    fn list_tables(&self) -> Result<Vec<String>, DbError>;
    fn describe_table(&self, name: &str) -> Result<TableInfo, DbError>;
    /// Read-only query. Backends enforce read-only at the backend level
    /// (SQLite `query_only`; Postgres defaults to read path via wrapper).
    fn query(&self, sql: &str) -> Result<QueryResult, DbError>;
    /// Execute one DML/DDL statement.
    fn write(&self, sql: &str) -> Result<WriteResult, DbError>;
    /// Session transaction control (SQLite: real connection-level txn;
    /// PostgreSQL over one-shot `psql`: structured UNAVAILABLE).
    fn begin(&self) -> Result<(), DbError>;
    fn commit(&self) -> Result<(), DbError>;
    fn rollback(&self) -> Result<(), DbError>;
    /// Atomic multi-statement transaction (run-to-completion or rollback).
    fn transaction(&self, statements: &[String]) -> Result<TxResult, DbError>;
    /// Whether a session transaction is currently open.
    fn in_transaction(&self) -> bool;
}

/// Runtime database manager: pre-built backends (or structured
/// unavailability) per configured connection id.
pub struct DatabaseManager {
    backends: Mutex<HashMap<String, Result<Arc<dyn DatabaseBackend>, String>>>,
    default_id: Option<String>,
    configured_ids: Vec<String>,
}

impl DatabaseManager {
    /// Build from operator config. A connection that fails to open is stored
    /// as `Err(reason)` (structured UNAVAILABLE at tool level), never a
    /// startup failure — fail-soft for availability, fail-closed for access.
    pub fn from_config(cfg: &DatabaseConfig) -> Self {
        let mut backends = HashMap::new();
        for c in &cfg.connections {
            let built: Result<Arc<dyn DatabaseBackend>, String> = match c.kind {
                DbKind::Sqlite => match c.sqlite_path.clone() {
                    Some(path) => crate::sqlite::SqliteBackend::open(c.id.clone(), path)
                        .map(|b| Arc::new(b) as Arc<dyn DatabaseBackend>)
                        .map_err(|e| e.to_string()),
                    None => {
                        Err("sqlite_path missing (config validation should catch this)".to_string())
                    }
                },
                DbKind::Postgres => match c.pg_dsn_env.clone() {
                    Some(env) => Ok(Arc::new(crate::postgres::PostgresBackend::new(
                        c.id.clone(),
                        env,
                    )) as Arc<dyn DatabaseBackend>),
                    None => {
                        Err("pg_dsn_env missing (config validation should catch this)".to_string())
                    }
                },
            };
            backends.insert(c.id.clone(), built);
        }
        let configured_ids = cfg.connections.iter().map(|c| c.id.clone()).collect();
        Self {
            backends: Mutex::new(backends),
            default_id: cfg.default.clone(),
            configured_ids,
        }
    }

    /// Configured connection ids (for discovery/status).
    pub fn configured_ids(&self) -> Vec<String> {
        self.configured_ids.clone()
    }

    /// Resolve a connection id (explicit or default) to its backend.
    pub fn get(&self, requested: Option<&str>) -> Result<Arc<dyn DatabaseBackend>, DbError> {
        let id = match requested {
            Some(id) => id.to_string(),
            None => match &self.default_id {
                Some(d) => d.clone(),
                None => match self.configured_ids.len() {
                    0 => {
                        return Err(DbError::Unavailable(
                            "no database connections configured (fabric.database.connections)"
                                .into(),
                        ))
                    }
                    1 => self.configured_ids[0].clone(),
                    _ => {
                        return Err(DbError::Invalid(format!(
                            "multiple connections configured {:?}; pass `connection`",
                            self.configured_ids
                        )))
                    }
                },
            },
        };
        let map = self.backends.lock().expect("db manager poisoned");
        match map.get(&id) {
            Some(Ok(b)) => Ok(b.clone()),
            Some(Err(reason)) => Err(DbError::Unavailable(format!(
                "connection `{id}` unavailable: {reason}"
            ))),
            None => Err(DbError::NotFound(format!(
                "connection `{id}` not configured (known: {:?})",
                self.configured_ids
            ))),
        }
    }

    /// Status of every configured connection (probe on call; blocking).
    pub fn status_blocking(&self) -> Vec<serde_json::Value> {
        let map = self.backends.lock().expect("db manager poisoned");
        let mut out: Vec<serde_json::Value> = self
            .configured_ids
            .iter()
            .map(|id| {
                let entry = map.get(id);
                let (available, backend, reason) = match entry {
                    Some(Ok(b)) => match b.probe() {
                        Ok(v) => (true, b.kind().to_string(), Some(v)),
                        Err(e) => (false, b.kind().to_string(), Some(e.to_string())),
                    },
                    Some(Err(r)) => (false, "unknown".to_string(), Some(r.clone())),
                    None => (false, "unknown".to_string(), Some("not built".to_string())),
                };
                serde_json::json!({
                    "id": id,
                    "available": available,
                    "backend": backend,
                    "detail": reason,
                })
            })
            .collect();
        out.sort_by_key(|v| v["id"].as_str().unwrap_or_default().to_string());
        out
    }
}
