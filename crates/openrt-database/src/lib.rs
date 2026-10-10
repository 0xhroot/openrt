//! Phase 7B database tools (`database.*`) — see `sqlite.rs`, `postgres.rs`,
//! `backend.rs`, `tools.rs`.

pub mod backend;
pub mod postgres;
pub mod sqlite;
pub mod tools;

pub use backend::{DatabaseBackend, DatabaseManager, DbError, QueryResult, TableInfo};
pub use tools::{database_metas, database_tools};
