//! OpenRT foundation store (ADR-006): the single local store that all phases
//! persist through.
//!
//! Phase 1 provides the store contract plus two backends:
//!
//! * [`store::MemoryStore`] — in-memory, for tests and ephemeral runs;
//! * [`store::LocalFileStore`] — a durable JSON-file backend with typed
//!   namespaces (`sessions/`, `tasks/`, `executions/`, `events/`) and an
//!   append-only event log, atomic writes, restrictive file permissions and
//!   versioned schema marker.
//!
//! The [`PersistenceStore`] trait is the only surface consumers may use; later
//! phases add backends (SQLite is permitted by the roadmap but not required in
//! Phase 1) without touching consumers.

pub mod error;
pub mod store;

pub use error::PersistenceError;
pub use store::{default_store_dir, ListOptions, LocalFileStore, MemoryStore, PersistenceStore};
