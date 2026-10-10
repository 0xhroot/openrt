//! Phase 10 M10.5 — the time-machine model over the ONE history ledger.
//!
//! Four capabilities, all read-only over `history.ledger` / explicit
//! continuations in `history.forks` (the parent ledger is NEVER written by
//! any view):
//!
//! * **timeline** ([`Runtime::history_timeline`]) — verified, filterable,
//!   ordered ledger view (session / from-seq / recent-limit).
//! * **state-as-of** ([`Runtime::history_state_as_of`]) — replay + a
//!   per-session last-known-state summary at that point.
//! * **fork / branch compare** ([`Runtime::history_branch_compare`]) —
//!   verified divergence view between a fork branch and the parent chain
//!   (creation stays on the existing `history_fork` authority).
//! * **return-to-current** ([`Runtime::history_return_to_current`]) —
//!   re-anchors the view at the parent head with the fork catalog.
//!
//! Everything fails closed on chain verification: a tampered ledger or fork
//! chain yields an error, never a fabricated view.

use serde::{Deserialize, Serialize};

use openrt_timeseries::{
    fork_commits, fork_ref, fork_verify_chain, ledger_records, list_forks, ForkCommit, ForkRef,
    HistoryRecord,
};

use crate::{Error, Runtime};

/// Filter/window for [`Runtime::history_timeline`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TimelineOptions {
    /// Only rows correlated to this session id (string form; as stored).
    #[serde(default)]
    pub session: Option<String>,
    /// First sequence to include (0 or 1 = from the genesis row).
    #[serde(default)]
    pub from_seq: u64,
    /// Cap to the most recent `limit` rows of the filtered window
    /// (chronological order is preserved). `None` = no cap.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// A verified slice of the parent ledger, oldest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Timeline {
    /// Current parent-ledger tail at read time.
    pub tail_seq: u64,
    pub records: Vec<HistoryRecord>,
}

/// Last-known state of one session within an as-of view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionAsOf {
    pub session_id: String,
    pub events: usize,
    pub last_seq: u64,
    pub last_status: String,
    pub last_event_kind: String,
    pub last_note: String,
}

/// Ledger state as of a historical point: the rows plus the per-session
/// summary a surface renders (no UI-local state).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateAsOf {
    pub as_of: u64,
    pub tail_seq: u64,
    pub records: Vec<HistoryRecord>,
    pub sessions: Vec<SessionAsOf>,
}

/// One side of a branch comparison at a shared sequence number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchEventSummary {
    pub node_id: String,
    pub status: String,
    pub event_kind: String,
    pub note: String,
}

/// Per-sequence divergence row between the parent chain and a fork branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchDiffEntry {
    pub seq: u64,
    pub parent: Option<BranchEventSummary>,
    pub branch: Option<BranchEventSummary>,
    /// True when the two chains do not carry the same event at this seq
    /// (including one-sided rows).
    pub diverged: bool,
}

/// Verified comparison of one fork branch against the parent ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BranchComparison {
    pub branch: String,
    pub base_seq: u64,
    pub base_digest: String,
    /// Shared history length (rows both chains agree on).
    pub shared_prefix_len: u64,
    pub parent_tail_seq: u64,
    /// Fork commits that passed chain verification.
    pub fork_records_verified: u64,
    pub entries: Vec<BranchDiffEntry>,
}

/// Re-anchored view at the parent head: current state + fork catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CurrentView {
    pub tail_seq: u64,
    pub state: StateAsOf,
    pub forks: Vec<ForkRef>,
}

fn summary_of(rec: &ForkCommit) -> BranchEventSummary {
    BranchEventSummary {
        node_id: rec.node_id.clone(),
        status: rec.status.clone(),
        event_kind: rec.event_kind.clone(),
        note: rec.note.clone(),
    }
}

impl Runtime {
    // ---- timeline ----

    /// Verified, ordered timeline over the parent ledger. Chain verification
    /// runs first (fail-closed): tampered history never yields a view.
    pub async fn history_timeline(&self, opts: &TimelineOptions) -> Result<Timeline, Error> {
        let tail = openrt_timeseries::tail_seq(&*self.store).await?;
        // `ledger_records` verifies the whole chain before returning rows.
        let mut records = ledger_records(&*self.store).await?;
        if let Some(session) = &opts.session {
            records.retain(|r| &r.session_id == session);
        }
        if opts.from_seq > 1 {
            let from = opts.from_seq;
            records.retain(|r| r.seq >= from);
        }
        if let Some(limit) = opts.limit {
            if records.len() > limit {
                records = records.split_off(records.len() - limit);
            }
        }
        Ok(Timeline {
            tail_seq: tail,
            records,
        })
    }

    // ---- state-as-of ----

    /// Replay the ledger as of `as_of_seq` and derive the per-session
    /// last-known state at that point (read-only; never rewrites history).
    pub async fn history_state_as_of(&self, as_of_seq: u64) -> Result<StateAsOf, Error> {
        let records = openrt_timeseries::materialize_as_of(&*self.store, as_of_seq).await?;
        let tail = openrt_timeseries::tail_seq(&*self.store).await?;
        // Derive session summaries in chronological order, then order by
        // session id for a deterministic render.
        let mut order: Vec<String> = Vec::new();
        let mut sessions: std::collections::HashMap<String, SessionAsOf> =
            std::collections::HashMap::new();
        for rec in &records {
            if rec.session_id.is_empty() {
                continue;
            }
            let entry = sessions
                .entry(rec.session_id.clone())
                .or_insert_with(|| SessionAsOf {
                    session_id: rec.session_id.clone(),
                    events: 0,
                    last_seq: rec.seq,
                    last_status: rec.status.clone(),
                    last_event_kind: rec.event_kind.clone(),
                    last_note: rec.note.clone(),
                });
            if !order.contains(&rec.session_id) {
                order.push(rec.session_id.clone());
            }
            entry.events += 1;
            entry.last_seq = rec.seq;
            entry.last_status = rec.status.clone();
            entry.last_event_kind = rec.event_kind.clone();
            entry.last_note = rec.note.clone();
        }
        let mut summaries: Vec<SessionAsOf> = order
            .into_iter()
            .filter_map(|sid| sessions.remove(&sid))
            .collect();
        summaries.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        Ok(StateAsOf {
            as_of: as_of_seq,
            tail_seq: tail,
            records,
            sessions: summaries,
        })
    }

    // ---- fork catalog + branch compare ----

    /// Every fork branch (typed refs), sorted by name.
    pub async fn history_forks(&self) -> Result<Vec<ForkRef>, Error> {
        Ok(list_forks(&*self.store).await?)
    }

    /// Verified divergence of `branch` from the parent ledger: shared prefix
    /// (`base_seq`) plus per-sequence rows where the chains differ. Both
    /// chains are verified first (fail-closed).
    pub async fn history_branch_compare(&self, branch: &str) -> Result<BranchComparison, Error> {
        let ref_rec = fork_ref(&*self.store, branch).await?;
        // Parent chain (ledger_records verifies it) + fork chain.
        let parent = ledger_records(&*self.store).await?;
        let fork_records_verified = fork_verify_chain(&*self.store, branch).await?;
        let commits = fork_commits(&*self.store, branch).await?;
        let parent_tail = parent.last().map(|r| r.seq).unwrap_or(0);
        let fork_tail = commits.last().map(|c| c.seq).unwrap_or(0);
        let max_seq = parent_tail.max(fork_tail);

        let mut entries = Vec::new();
        for seq in (ref_rec.base_seq + 1)..=max_seq {
            let parent_side = parent
                .iter()
                .find(|r| r.seq == seq)
                .map(|r| BranchEventSummary {
                    node_id: r.node_id.clone(),
                    status: r.status.clone(),
                    event_kind: r.event_kind.clone(),
                    note: r.note.clone(),
                });
            let branch_side = commits.iter().find(|c| c.seq == seq).map(summary_of);
            let diverged = parent_side != branch_side;
            entries.push(BranchDiffEntry {
                seq,
                parent: parent_side,
                branch: branch_side,
                diverged,
            });
        }
        Ok(BranchComparison {
            branch: ref_rec.branch,
            base_seq: ref_rec.base_seq,
            base_digest: ref_rec.base_digest,
            shared_prefix_len: ref_rec.base_seq,
            parent_tail_seq: parent_tail,
            fork_records_verified,
            entries,
        })
    }

    // ---- return-to-current ----

    /// Re-anchor the view at the parent head: current as-of state plus the
    /// fork catalog (surfaces keep no UI-local state).
    pub async fn history_return_to_current(&self) -> Result<CurrentView, Error> {
        let tail = openrt_timeseries::tail_seq(&*self.store).await?;
        let state = self.history_state_as_of(tail).await?;
        let forks = self.history_forks().await?;
        Ok(CurrentView {
            tail_seq: tail,
            state,
            forks,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use openrt_persistence::{MemoryStore, PersistenceStore};
    use openrt_timeseries::{append_event, tail_seq, verify_chain};

    async fn build(store: Arc<MemoryStore>) -> Arc<Runtime> {
        crate::RuntimeBuilder::new()
            .config(openrt_config::Config::default())
            .store(store)
            .with_provider(Arc::new(openrt_models::mock::MockProvider::echo("mock")))
            .bus_capacity(16)
            .build()
            .await
            .unwrap()
    }

    /// Seed a two-session ledger with 6 rows: s1×3, s2×3 (interleaved).
    async fn seed(store: &dyn openrt_persistence::PersistenceStore) {
        for i in 0..3 {
            append_event(store, "sess-a", &format!("n{i}"), "ok", "task.step", "a")
                .await
                .unwrap();
            append_event(store, "sess-b", &format!("n{i}"), "wait", "task.step", "b")
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn timeline_filters_session_range_and_recent_limit() {
        let store = Arc::new(MemoryStore::new());
        seed(&*store).await;
        let rt = build(store).await;

        let full = rt
            .history_timeline(&TimelineOptions::default())
            .await
            .unwrap();
        assert_eq!(full.tail_seq, 6);
        assert_eq!(full.records.len(), 6);

        let a = rt
            .history_timeline(&TimelineOptions {
                session: Some("sess-a".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(a.records.len(), 3);
        assert!(a.records.iter().all(|r| r.session_id == "sess-a"));

        let window = rt
            .history_timeline(&TimelineOptions {
                from_seq: 4,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(window.records.len(), 3);
        assert!(window.records.iter().all(|r| r.seq >= 4));

        let recent = rt
            .history_timeline(&TimelineOptions {
                limit: Some(2),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(recent.records.len(), 2);
        assert_eq!(recent.records[0].seq, 5);
        assert_eq!(recent.records[1].seq, 6);
    }

    #[tokio::test]
    async fn timeline_fails_closed_on_tampered_ledger() {
        let store = Arc::new(MemoryStore::new());
        seed(&*store).await;
        let rt = build(store.clone()).await;
        // Tamper row 3.
        let mut rec = store
            .get_record(openrt_timeseries::HISTORY_LEDGER_NS, "seq.000000000003")
            .await
            .unwrap()
            .unwrap();
        rec["note"] = serde_json::json!("rewritten");
        store
            .put_record(
                openrt_timeseries::HISTORY_LEDGER_NS,
                "seq.000000000003",
                &rec,
            )
            .await
            .unwrap();

        let err = rt
            .history_timeline(&TimelineOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::History(_)), "{err}");
        // State-as-of fails closed too (verify_chain inside materialize).
        let err = rt.history_state_as_of(3).await.unwrap_err();
        assert!(matches!(err, Error::History(_)), "{err}");
    }

    #[tokio::test]
    async fn state_as_of_summarizes_sessions_at_the_point() {
        let store = Arc::new(MemoryStore::new());
        seed(&*store).await;
        let rt = build(store).await;

        // As of seq 4: sess-a has 2 rows (1,3), sess-b has 2 rows (2,4).
        let state = rt.history_state_as_of(4).await.unwrap();
        assert_eq!(state.as_of, 4);
        assert_eq!(state.tail_seq, 6);
        assert_eq!(state.records.len(), 4);
        assert_eq!(state.sessions.len(), 2);
        let a = state
            .sessions
            .iter()
            .find(|s| s.session_id == "sess-a")
            .unwrap();
        assert_eq!(a.events, 2);
        assert_eq!(a.last_seq, 3);
        assert_eq!(a.last_status, "ok");
        let b = state
            .sessions
            .iter()
            .find(|s| s.session_id == "sess-b")
            .unwrap();
        assert_eq!(b.events, 2);
        assert_eq!(b.last_seq, 4);

        // Genesis point: one session, one event.
        let first = rt.history_state_as_of(1).await.unwrap();
        assert_eq!(first.records.len(), 1);
        assert_eq!(first.sessions.len(), 1);
        assert_eq!(first.sessions[0].session_id, "sess-a");
        // Beyond the head clamps to everything recorded (still honest).
        let all = rt.history_state_as_of(99).await.unwrap();
        assert_eq!(all.records.len(), 6);
    }

    #[tokio::test]
    async fn fork_from_historical_point_keeps_parent_intact() {
        let store = Arc::new(MemoryStore::new());
        seed(&*store).await;
        let rt = build(store.clone()).await;

        let before = serde_json::to_vec(&ledger_records(&*store).await.unwrap()).unwrap();
        let tail_before = tail_seq(&*store).await.unwrap();

        // Fork at seq 3 (a historical point), then diverge.
        rt.history_fork("whatif", "seq.000000000003").await.unwrap();
        rt.history_fork_commit("whatif", "n-alt", "ok", "task.step", "try")
            .await
            .unwrap();
        assert_eq!(rt.history_fork_verify("whatif").await.unwrap(), 1);

        // Parent ledger byte-identical; chain verifies; tail unchanged.
        let after = serde_json::to_vec(&ledger_records(&*store).await.unwrap()).unwrap();
        assert_eq!(before, after, "parent ledger must never be rewritten");
        assert_eq!(tail_seq(&*store).await.unwrap(), tail_before);
        assert_eq!(verify_chain(&*store).await.unwrap(), tail_before);

        let forks = rt.history_forks().await.unwrap();
        assert_eq!(forks.len(), 1);
        assert_eq!(forks[0].branch, "whatif");
        assert_eq!(forks[0].base_seq, 3);
        assert_eq!(forks[0].commits, 1);
    }

    #[tokio::test]
    async fn fork_base_beyond_tail_fails_closed() {
        let store = Arc::new(MemoryStore::new());
        seed(&*store).await;
        let rt = build(store).await;
        let err = rt
            .history_fork("future", "seq.000000000099")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::History(_)), "{err}");
        // No phantom branch was created.
        assert!(rt.history_forks().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn branch_compare_reports_shared_prefix_and_divergence() {
        let store = Arc::new(MemoryStore::new());
        seed(&*store).await; // tail 6
        let rt = build(store.clone()).await;

        rt.history_fork("trial", "seq.000000000003").await.unwrap();
        rt.history_fork_commit("trial", "n-alt", "ok", "task.step", "divergent")
            .await
            .unwrap();

        // Parent continues past the fork point (simulated current history).
        append_event(&*store, "sess-a", "n-late", "ok", "task.step", "post")
            .await
            .unwrap();

        let cmp = rt.history_branch_compare("trial").await.unwrap();
        assert_eq!(cmp.branch, "trial");
        assert_eq!(cmp.base_seq, 3);
        assert_eq!(cmp.shared_prefix_len, 3);
        assert_eq!(cmp.parent_tail_seq, 7);
        assert_eq!(cmp.fork_records_verified, 1);
        assert!(!cmp.base_digest.is_empty());
        // Sequences 4..=7: parent has rows 4,5,6,7; branch has row 4 only.
        assert_eq!(cmp.entries.len(), 4);
        for entry in &cmp.entries {
            assert!(entry.diverged, "seq {} must diverge", entry.seq);
        }
        assert_eq!(cmp.entries[0].seq, 4);
        assert!(cmp.entries[0].parent.is_some());
        assert!(cmp.entries[0].branch.is_some());
        assert_eq!(cmp.entries[0].branch.as_ref().unwrap().note, "divergent");
        assert!(cmp.entries[1].branch.is_none());
        // No rows at or below the shared prefix.
        assert!(cmp.entries.iter().all(|e| e.seq > cmp.base_seq));
    }

    #[tokio::test]
    async fn branch_compare_unknown_branch_fails_closed() {
        let store = Arc::new(MemoryStore::new());
        seed(&*store).await;
        let rt = build(store).await;
        let err = rt.history_branch_compare("nope").await.unwrap_err();
        assert!(matches!(err, Error::History(_)), "{err}");
    }

    #[tokio::test]
    async fn return_to_current_reanchors_with_fork_catalog() {
        let store = Arc::new(MemoryStore::new());
        seed(&*store).await;
        let rt = build(store.clone()).await;

        rt.history_fork("branch.x", "seq.000000000002")
            .await
            .unwrap();

        let view = rt.history_return_to_current().await.unwrap();
        assert_eq!(view.tail_seq, 6);
        assert_eq!(view.state.as_of, 6);
        assert_eq!(view.state.records.len(), 6);
        assert_eq!(view.state.sessions.len(), 2);
        assert_eq!(view.forks.len(), 1);
        assert_eq!(view.forks[0].branch, "branch.x");
        assert_eq!(view.forks[0].base_seq, 2);

        // Parent integrity holds after every time-machine read.
        assert_eq!(verify_chain(&*store).await.unwrap(), 6);
    }

    #[tokio::test]
    async fn create_fork_at_empty_base_is_genesis() {
        // base_seq 0 = fork from genesis (no shared rows).
        let store = Arc::new(MemoryStore::new());
        seed(&*store).await;
        let rt = build(store).await;
        rt.history_fork("from-zero", "seq.000000000000")
            .await
            .unwrap();
        let cmp = rt.history_branch_compare("from-zero").await.unwrap();
        assert_eq!(cmp.base_seq, 0);
        assert_eq!(cmp.shared_prefix_len, 0);
        assert_eq!(
            cmp.entries.len(),
            6,
            "whole parent chain diverges vs empty fork"
        );
        assert!(cmp.entries.iter().all(|e| e.branch.is_none()));
    }
}
