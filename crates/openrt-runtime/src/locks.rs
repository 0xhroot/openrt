//! Per-session serialization of task execution (SEC-08).
//!
//! The pipeline folds the exchange into a session snapshot and persists it
//! whole via an overwrite (`update_session`, ADR-006 overwrite document
//! semantics). Two tasks racing on the *same* session can therefore lose one
//! fold. [`SessionLocks`] serializes entrants per session id, so the same
//! session's runs never overlap while unrelated sessions proceed in parallel.

use std::collections::HashMap;
use std::sync::Arc;

use openrt_core::SessionId;
use tokio::sync::{Mutex, OwnedMutexGuard};

/// Guard released when the owning task run completes.
pub type SessionGuard = OwnedMutexGuard<()>;

/// Shared registry of per-session task locks.
///
/// Held by the [`Runtime`](crate::Runtime) and cloned into each
/// [`TaskHandle`](crate::TaskHandle). Cloning shares the same registry.
#[derive(Debug, Clone, Default)]
pub struct SessionLocks {
    /// Arc so a cloned [`SessionLocks`] observes the same registry.
    per_session: Arc<Mutex<HashMap<SessionId, Arc<Mutex<()>>>>>,
}

impl SessionLocks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire the run lock for `id`. Only tasks targeting the same session
    /// serialize; the registry lock is released before awaiting the session
    /// lock, so a busy session never blocks lookups for other sessions.
    pub async fn lock(&self, id: SessionId) -> SessionGuard {
        let session_lock = {
            let mut map = self.per_session.lock().await;
            map.entry(id)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        session_lock.lock_owned().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn acquirable_uncontended() {
        let locks = SessionLocks::new();
        let _guard = locks.lock(SessionId::new()).await;
    }

    #[tokio::test]
    async fn same_session_serializes() {
        let locks = SessionLocks::new();
        let sid = SessionId::new();
        let guard = locks.lock(sid).await;

        let waiter = tokio::spawn({
            let locks = locks.clone();
            async move { locks.lock(sid).await }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            !waiter.is_finished(),
            "a second run on the same session must wait for the first"
        );

        drop(guard);
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("the waiter must acquire after the first run releases")
            .unwrap();
    }

    #[tokio::test]
    async fn different_sessions_do_not_serialize() {
        let locks = SessionLocks::new();
        let a = SessionId::new();
        let b = SessionId::new();

        let _ga = locks.lock(a).await;
        // `b` must be acquirable while `a` is held — otherwise the lock is
        // accidentally global (the SEC-08 regression being guarded).
        let _gb = tokio::time::timeout(Duration::from_millis(50), locks.lock(b))
            .await
            .expect("different sessions must not block each other");
    }
}
