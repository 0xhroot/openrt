//! Persistence errors.

use std::path::PathBuf;

use openrt_core::{ExecutionId, SessionId, TaskId};

/// Errors from the foundation store.
#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    #[error("namespace `{0}` not found in store root {1}")]
    NamespaceMissing(String, PathBuf),
    #[error("session {0} not found")]
    SessionNotFound(SessionId),
    #[error("task {0} not found")]
    TaskNotFound(TaskId),
    #[error("execution {0} not found")]
    ExecutionNotFound(ExecutionId),
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("corrupt record `{0}`: {1}")]
    Corrupt(String, String),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("unsupported store backend `{0}`")]
    Unsupported(String),
    #[error("store not initialized (call open/init first): {0}")]
    NotInitialized(String),
}

impl From<std::io::Error> for PersistenceError {
    fn from(e: std::io::Error) -> Self {
        PersistenceError::Io {
            path: PathBuf::new(),
            source: e,
        }
    }
}
