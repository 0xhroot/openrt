//! Context-window scheduler: per-session and global token budgets with LRU
//! recycling.
//!
//! [`ContextScheduler`] hands out token grants against three nested ceilings:
//! the model's own context ceiling, the per-session budget and the global
//! budget. Grants stamp each session with a monotonic order counter (not a
//! wall clock) so eviction ordering is deterministic; [`ContextScheduler::evict_lru`]
//! reclaims the least-recently-stamped session whole.
//!
//! State can be persisted through any [`PersistenceStore`] under [`SCHED_NS`]
//! with [`save`] / [`load`]; loading restores budgets, usage and the order
//! counter so subsequent stamps continue monotonically.

use std::collections::BTreeMap;

use openrt_persistence::{PersistenceError, PersistenceStore};
use serde::{Deserialize, Serialize};

/// Store namespace used for context-scheduler state.
pub const SCHED_NS: &str = "sched.context";

/// Key under which [`CtxSchedulerState`] is persisted in [`SCHED_NS`].
const STATE_KEY: &str = "state";

/// Errors from the context-window scheduler.
#[derive(Debug, thiserror::Error)]
pub enum CtxSchedError {
    #[error(
        "context request exceeds model capacity: model max {model_max}, requested {requested}"
    )]
    ModelContextExceeded { model_max: u64, requested: u64 },
    #[error(
        "context request exceeds session budget: session max {session_max}, used {used}, requested {requested}"
    )]
    SessionBudgetExceeded {
        session_max: u64,
        used: u64,
        requested: u64,
    },
    #[error(
        "context request exceeds global budget: global max {global_max}, used {global_used}, requested {requested}"
    )]
    GlobalBudgetExceeded {
        global_max: u64,
        global_used: u64,
        requested: u64,
    },
    #[error("unknown session `{0}`")]
    UnknownSession(String),
    #[error("invalid amount: {0}")]
    InvalidAmount(String),
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
}

/// A successful token grant against a session's context budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub session_id: String,
    pub tokens: u64,
    pub session_used_total: u64,
    pub model_max: u64,
}

/// One session fully reclaimed by [`ContextScheduler::evict_lru`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecycleEvent {
    pub session_id: String,
    pub reclaimed_tokens: u64,
    pub reason: String,
}

/// Per-session usage and last-activity stamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCtx {
    /// Tokens currently charged to this session.
    pub used: u64,
    /// Monotonic order stamp of the session's most recent grant. This is an
    /// order counter, not a wall clock, so behavior is fully deterministic.
    pub last: u64,
}

/// Context-window token scheduler.
#[derive(Debug, Clone)]
pub struct ContextScheduler {
    session_max: u64,
    global_max: u64,
    global_used: u64,
    sessions: BTreeMap<String, SessionCtx>,
    /// Monotonic stamp source; incremented on every grant. Deterministic by
    /// construction (deliberately not wall-clock based).
    order_counter: u64,
}

impl ContextScheduler {
    /// Build a scheduler with per-session ceiling `session_max` and total
    /// ceiling `global_max` (both in tokens).
    pub fn new(session_max: u64, global_max: u64) -> Self {
        Self {
            session_max,
            global_max,
            global_used: 0,
            sessions: BTreeMap::new(),
            order_counter: 0,
        }
    }

    /// Never-used shorthand for constructing new SessionCtx.
    fn zero_ctx() -> SessionCtx {
        SessionCtx { used: 0, last: 0 }
    }

    /// Grant `requested` tokens to `session`, honoring the model ceiling, the
    /// per-session ceiling and the global ceiling in that order.
    pub fn acquire(
        &mut self,
        session: &str,
        requested: u64,
        model_max: u64,
    ) -> Result<Grant, CtxSchedError> {
        if requested == 0 {
            return Err(CtxSchedError::InvalidAmount(
                "requested tokens must be > 0".to_string(),
            ));
        }
        if requested > model_max {
            return Err(CtxSchedError::ModelContextExceeded {
                model_max,
                requested,
            });
        }
        if requested > self.session_max {
            return Err(CtxSchedError::SessionBudgetExceeded {
                session_max: self.session_max,
                used: 0,
                requested,
            });
        }
        let entry = self
            .sessions
            .entry(session.to_string())
            .or_insert_with(Self::zero_ctx);
        if entry.used + requested > self.session_max {
            return Err(CtxSchedError::SessionBudgetExceeded {
                session_max: self.session_max,
                used: entry.used,
                requested,
            });
        }
        if self.global_used + requested > self.global_max {
            return Err(CtxSchedError::GlobalBudgetExceeded {
                global_max: self.global_max,
                global_used: self.global_used,
                requested,
            });
        }
        self.order_counter += 1;
        entry.last = self.order_counter;
        entry.used += requested;
        self.global_used += requested;
        Ok(Grant {
            session_id: session.to_string(),
            tokens: requested,
            session_used_total: entry.used,
            model_max,
        })
    }

    /// Return `tokens` to the pool. Requests above the session's current usage
    /// are clamped (never underflow). Unknown sessions error.
    pub fn release(&mut self, session: &str, tokens: u64) -> Result<(), CtxSchedError> {
        let Some(entry) = self.sessions.get_mut(session) else {
            return Err(CtxSchedError::UnknownSession(session.to_string()));
        };
        let actual = tokens.min(entry.used);
        entry.used -= actual;
        self.global_used = self.global_used.saturating_sub(actual);
        Ok(())
    }

    /// Tokens currently charged to `session` (0 when unknown).
    pub fn used(&self, session: &str) -> u64 {
        self.sessions.get(session).map_or(0, |s| s.used)
    }

    /// Tokens remaining in the global budget (clamped at 0).
    pub fn free_global(&self) -> u64 {
        self.global_max.saturating_sub(self.global_used)
    }

    /// Known sessions in deterministic (lexicographic) order.
    pub fn session_set(&self) -> Vec<String> {
        self.sessions.keys().cloned().collect()
    }

    /// Reclaim tokens until at least `reclaim_target_tokens` are free, evicting
    /// whole sessions in least-recently-stamped order. Returns the events in
    /// eviction order.
    pub fn evict_lru(&mut self, reclaim_target_tokens: u64) -> Vec<RecycleEvent> {
        let mut events = Vec::new();
        while self.free_global() < reclaim_target_tokens && !self.sessions.is_empty() {
            let victim = self
                .sessions
                .iter()
                .min_by_key(|(_, ctx)| ctx.last)
                .map(|(id, ctx)| (id.clone(), ctx.used));
            let Some((id, used)) = victim else {
                break;
            };
            self.sessions.remove(&id);
            self.global_used = self.global_used.saturating_sub(used);
            events.push(RecycleEvent {
                session_id: id,
                reclaimed_tokens: used,
                reason: "lru".to_string(),
            });
        }
        events
    }
}

/// Serialized context-scheduler state (namespace `sched.context`, key `state`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CtxSchedulerState {
    pub session_max: u64,
    pub global_max: u64,
    pub global_used: u64,
    /// `(session_id, used, last)` triples.
    pub sessions: Vec<(String, u64, u64)>,
}

/// Persist scheduler state under [`SCHED_NS`]/`state`.
pub async fn save(
    store: &dyn PersistenceStore,
    sched: &ContextScheduler,
) -> Result<(), CtxSchedError> {
    let state = CtxSchedulerState {
        session_max: sched.session_max,
        global_max: sched.global_max,
        global_used: sched.global_used,
        sessions: sched
            .sessions
            .iter()
            .map(|(id, ctx)| (id.clone(), ctx.used, ctx.last))
            .collect(),
    };
    let value = serde_json::to_value(state).map_err(PersistenceError::from)?;
    store.put_record(SCHED_NS, STATE_KEY, &value).await?;
    Ok(())
}

/// Restore a scheduler from [`SCHED_NS`]/`state`, or `None` when nothing has
/// been saved yet. The order counter resumes at the maximum restored stamp
/// plus one, so future grants stamp monotonically after the restored ones.
pub async fn load(store: &dyn PersistenceStore) -> Result<Option<ContextScheduler>, CtxSchedError> {
    let Some(value) = store.get_record(SCHED_NS, STATE_KEY).await? else {
        return Ok(None);
    };
    let state: CtxSchedulerState = serde_json::from_value(value).map_err(PersistenceError::from)?;
    let mut sched = ContextScheduler::new(state.session_max, state.global_max);
    sched.global_used = state.global_used;
    for (id, used, last) in state.sessions {
        sched.sessions.insert(id, SessionCtx { used, last });
    }
    sched.order_counter = sched
        .sessions
        .values()
        .map(|ctx| ctx.last)
        .max()
        .unwrap_or(0)
        + 1;
    Ok(Some(sched))
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_persistence::MemoryStore;

    #[test]
    fn basic_grant_and_counters() {
        let mut sched = ContextScheduler::new(1_000, 5_000);
        let grant = sched.acquire("s1", 200, 4_096).unwrap();
        assert_eq!(grant.session_id, "s1");
        assert_eq!(grant.tokens, 200);
        assert_eq!(grant.session_used_total, 200);
        assert_eq!(grant.model_max, 4_096);
        assert_eq!(sched.used("s1"), 200);
        assert_eq!(sched.free_global(), 4_800);
        assert_eq!(sched.used("unknown"), 0);
        assert_eq!(sched.session_set(), vec!["s1".to_string()]);
    }

    #[test]
    fn model_context_ceiling() {
        let mut sched = ContextScheduler::new(10_000, 10_000);
        match sched.acquire("s1", 5_000, 4_096) {
            Err(CtxSchedError::ModelContextExceeded {
                model_max,
                requested,
            }) => {
                assert_eq!(model_max, 4_096);
                assert_eq!(requested, 5_000);
            }
            other => panic!("expected ModelContextExceeded, got {other:?}"),
        }
        assert_eq!(sched.free_global(), 10_000);
    }

    #[test]
    fn per_session_ceiling_across_acquires() {
        let mut sched = ContextScheduler::new(1_000, 10_000);
        sched.acquire("s1", 600, 4_096).unwrap();
        match sched.acquire("s1", 500, 4_096) {
            Err(CtxSchedError::SessionBudgetExceeded {
                session_max,
                used,
                requested,
            }) => {
                assert_eq!(session_max, 1_000);
                assert_eq!(used, 600);
                assert_eq!(requested, 500);
            }
            other => panic!("expected SessionBudgetExceeded, got {other:?}"),
        }

        // requested alone above the session ceiling reports used = 0.
        match sched.acquire("s2", 2_000, 4_096) {
            Err(CtxSchedError::SessionBudgetExceeded {
                session_max: _,
                used,
                ..
            }) => assert_eq!(used, 0),
            other => panic!("expected SessionBudgetExceeded, got {other:?}"),
        }
    }

    #[test]
    fn global_ceiling_across_sessions() {
        let mut sched = ContextScheduler::new(1_000, 1_000);
        sched.acquire("s1", 500, 4_096).unwrap();
        sched.acquire("s2", 500, 4_096).unwrap();
        assert_eq!(sched.free_global(), 0);

        match sched.acquire("s1", 100, 4_096) {
            Err(CtxSchedError::GlobalBudgetExceeded {
                global_max,
                global_used,
                requested,
            }) => {
                assert_eq!(global_max, 1_000);
                assert_eq!(global_used, 1_000);
                assert_eq!(requested, 100);
            }
            other => panic!("expected GlobalBudgetExceeded, got {other:?}"),
        }

        sched.release("s1", 500).unwrap();
        assert_eq!(sched.free_global(), 500);
        let retry = sched.acquire("s1", 100, 4_096).unwrap();
        assert_eq!(retry.session_used_total, 100);
    }

    #[test]
    fn lru_eviction_reclaims_least_recent() {
        let mut sched = ContextScheduler::new(500, 1_000);
        sched.acquire("a", 100, 4_096).unwrap();
        sched.acquire("b", 100, 4_096).unwrap();
        sched.acquire("c", 100, 4_096).unwrap();
        assert_eq!(sched.free_global(), 700);

        let events = sched.evict_lru(900);
        let ids: Vec<&str> = events.iter().map(|e| e.session_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
        assert!(events
            .iter()
            .all(|e| e.reason == "lru" && e.reclaimed_tokens == 100));
        assert_eq!(sched.free_global(), 900);
        assert_eq!(sched.used("a"), 0);
        assert_eq!(sched.used("b"), 0);
        assert_eq!(sched.used("c"), 100);
        assert_eq!(sched.session_set(), vec!["c".to_string()]);
    }

    #[test]
    fn evict_lru_noop_when_already_free() {
        let mut sched = ContextScheduler::new(500, 1_000);
        sched.acquire("a", 100, 4_096).unwrap();
        assert!(sched.evict_lru(50).is_empty());
        assert_eq!(sched.used("a"), 100);
    }

    #[test]
    fn clamped_release_does_not_underflow() {
        let mut sched = ContextScheduler::new(1_000, 1_000);
        sched.acquire("s1", 100, 4_096).unwrap();
        assert_eq!(sched.free_global(), 900);

        sched.release("s1", 10_000).unwrap();
        assert_eq!(sched.used("s1"), 0);
        assert_eq!(sched.free_global(), 1_000);

        // A second release of anything clamps to 0 as well.
        sched.release("s1", 1).unwrap();
        assert_eq!(sched.used("s1"), 0);
        assert_eq!(sched.free_global(), 1_000);

        match sched.release("nope", 5) {
            Err(CtxSchedError::UnknownSession(id)) => assert_eq!(id, "nope"),
            other => panic!("expected UnknownSession, got {other:?}"),
        }
    }

    #[test]
    fn zero_request_rejected() {
        let mut sched = ContextScheduler::new(1_000, 1_000);
        match sched.acquire("s1", 0, 4_096) {
            Err(CtxSchedError::InvalidAmount(_)) => {}
            other => panic!("expected InvalidAmount, got {other:?}"),
        }
        assert_eq!(sched.session_set(), Vec::<String>::new());
    }

    #[tokio::test]
    async fn save_load_restores_and_resumes_counter() {
        let store = MemoryStore::new();
        let mut sched = ContextScheduler::new(1_000, 1_000);
        sched.acquire("s1", 100, 4_096).unwrap();
        sched.acquire("s2", 250, 4_096).unwrap();
        let before_set = sched.session_set();
        save(&store, &sched).await.unwrap();
        drop(sched);

        let mut restored = load(&store).await.unwrap().expect("state was saved");
        assert_eq!(restored.used("s1"), 100);
        assert_eq!(restored.used("s2"), 250);
        assert_eq!(restored.free_global(), 650);
        assert_eq!(restored.session_set(), before_set);

        // Stamps resume strictly after the restored maximum, so an acquire on
        // a fresh session is evicted last.
        restored.acquire("s3", 100, 4_096).unwrap();
        let ids: Vec<String> = restored
            .evict_lru(1_000)
            .iter()
            .map(|e| e.session_id.clone())
            .collect();
        assert_eq!(ids, vec!["s1", "s2", "s3"]);
    }

    #[tokio::test]
    async fn load_absent_is_none() {
        let store = MemoryStore::new();
        assert!(load(&store).await.unwrap().is_none());
    }
}
