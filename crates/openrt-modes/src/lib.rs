//! ADR-003 operating-mode state machine: Provisioning / Sovereign.
//!
//! The mode describes the operator's *machine policy*. It is a persisted,
//! explicitly-transitioned state machine (namespace `modes.state`), not a
//! network probe and not a config flag. Every networked operation in the model
//! fabric funnels through [`ModeManager::ensure_network_allowed`] *before*
//! touching a network client, so a refused operation is proven to never reach
//! the wire.
//!
//! # Semantics (ADR-003)
//!
//! * **Provisioning** — network conditionally allowed for explicit,
//!   permission-gated materialization (discovery, install, bundle refresh).
//!   Content entering must still be verified before it can run in Sovereign.
//! * **Sovereign** — network interfaces blocked (loopback only);
//!   network-dependent features disabled. Unknown/inconclusive scan results
//!   refuse activation (fail-closed); no silent pass.
//!
//! # Audited transitions
//!
//! Every [`ModeManager::transition`] appends a `mode.transition` record to the
//! ADR-002 chained audit log (over the same store). An audit-recording failure
//! aborts the transition; a persistence failure after a successful append is
//! compensated with an explicit `mode.transition.aborted` entry so the log
//! never silently diverges from the effective state.
//!
//! # Fail-closed default
//!
//! A store holding no `current` record reads as **Sovereign** — the safest
//! posture for a machine that has never been provisioned.

use std::sync::Arc;

use openrt_audit::AuditLog;
use openrt_core::Timestamp;
use serde::{Deserialize, Serialize};

/// The enforcement engine over an [`openrt_persistence::PersistenceStore`].
pub struct ModeManager {
    store: Arc<dyn openrt_persistence::PersistenceStore>,
    audit: AuditLog,
    inner: tokio::sync::Mutex<()>,
}

/// Operating modes the fabric reasons about. Matches `openrt_config`'s
/// `OperatingMode` wire values (`serde` lowercase) so config seeding and
/// persisted state can be exchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionMode {
    Provisioning,
    Sovereign,
}

impl std::fmt::Display for ExecutionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ExecutionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Provisioning => "provisioning",
            Self::Sovereign => "sovereign",
        }
    }

    /// Whether network operations are permitted in this mode (ADR-003).
    pub fn network_allowed(&self) -> bool {
        matches!(self, Self::Provisioning)
    }
}

impl From<openrt_config::OperatingMode> for ExecutionMode {
    fn from(m: openrt_config::OperatingMode) -> Self {
        match m {
            openrt_config::OperatingMode::Provisioning => Self::Provisioning,
            openrt_config::OperatingMode::Sovereign => Self::Sovereign,
        }
    }
}

/// The persisted current-mode record (`modes.state/current`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeRecord {
    pub mode: ExecutionMode,
    /// RFC 3339 timestamp of the last effective transition.
    pub since: Timestamp,
    /// Audit sequence of the transition that produced this state (0 if taken
    /// from config seeding).
    pub seq: u64,
}

/// A human/CLI view of the current mode plus the last few transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeStatus {
    pub current: ExecutionMode,
    pub since: Timestamp,
    pub seq: u64,
    pub transitions: Vec<TransitionView>,
}

/// One audited transition, as recorded (mirrors the audit entry fields).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionView {
    pub seq: u64,
    pub at: Timestamp,
    pub actor: String,
    pub from: String,
    pub to: String,
    pub ext: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum ModeError {
    #[error("persistence error: {0}")]
    Persistence(#[from] openrt_persistence::PersistenceError),
    #[error("audit error: {0}")]
    Audit(#[from] openrt_audit::AuditError),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("network operations are blocked in sovereign mode (ADR-003)")]
    NetworkBlocked,
    #[error("operation requires permission-gated materialization in provisioning mode only: {0}")]
    ProvisioningRequired(String),
    #[error("invalid mode `{0}`")]
    InvalidMode(String),
}

/// Store namespace for mode state (ADR-006 one-store rule).
pub const MODES_NS: &str = "modes.state";
const CURRENT_KEY: &str = "current";
const HISTORY_KEY: &str = "history";
/// Bounded number of transition views kept in `modes.state/history`.
const HISTORY_LIMIT: usize = 32;

/// Seconds-precision epoch RFC 3339 marker used when no transition is recorded.
const EPOCH_RFC3339: &str = "1970-01-01T00:00:00Z";

impl ModeManager {
    pub fn new(store: Arc<dyn openrt_persistence::PersistenceStore>) -> Self {
        Self {
            audit: AuditLog::new(store.clone()),
            store,
            inner: tokio::sync::Mutex::new(()),
        }
    }

    /// Current mode. Fail-closed default: **Sovereign** whenever no record
    /// exists. Never blocks, never touches the network.
    pub async fn current(&self) -> Result<ModeRecord, ModeError> {
        let stored = self.store.get_record(MODES_NS, CURRENT_KEY).await?;
        match stored {
            None => Ok(ModeRecord {
                mode: ExecutionMode::Sovereign,
                since: epoch(),
                seq: 0,
            }),
            Some(v) => Ok(serde_json::from_value(v)?),
        }
    }

    /// Diagnostic status: current mode + audited transition list.
    pub async fn status(&self) -> Result<ModeStatus, ModeError> {
        let cur = self.current().await?;
        let hist = self.read_history().await?;
        Ok(ModeStatus {
            current: cur.mode,
            since: cur.since,
            seq: cur.seq,
            transitions: hist,
        })
    }

    /// Enforce the mode before any networked operation. In Sovereign mode this
    /// fails closed with [`ModeError::NetworkBlocked`] *before* any client call.
    /// Callers must invoke this at the actual network access point.
    pub async fn ensure_network_allowed(&self) -> Result<(), ModeError> {
        if self.current().await?.mode.network_allowed() {
            Ok(())
        } else {
            Err(ModeError::NetworkBlocked)
        }
    }

    /// Enforce the mode for a permission-gated materialization operation
    /// (`what`, e.g. `hf.discovery`, `model.download`). Provisioning allows it;
    /// Sovereign refuses with a reason.
    pub async fn ensure_materialization_allowed(&self, what: &str) -> Result<(), ModeError> {
        if self.current().await?.mode.network_allowed() {
            Ok(())
        } else {
            Err(ModeError::ProvisioningRequired(what.to_string()))
        }
    }

    /// Apply `mode` as the initial persisted mode **only** when the store holds
    /// no mode record yet (fail-closed bootstrap). A previously persisted
    /// operator transition always wins over configuration, so re-running
    /// against an already-provisioned store never silently overrides a human
    /// decision. Sovereign needs no record: it is already the fail-closed
    /// default.
    pub async fn seed_from_config(&self, mode: ExecutionMode) -> Result<(), ModeError> {
        if mode == ExecutionMode::Sovereign {
            return Ok(());
        }
        if self.current().await?.seq == 0 {
            self.transition("config", mode, "initial mode from configuration")
                .await?;
        }
        Ok(())
    }

    /// Explicit, audited mode transition (ADR-003: "an audited state change").
    ///
    /// Ordering: the transition is recorded in the ADR-002 chained audit log
    /// first (fail-closed — an append failure aborts, the mode never changes),
    /// then the persisted record is updated. If the persistence update fails
    /// after a successful append, a compensating `mode.transition.aborted`
    /// entry is appended so the log cannot silently diverge from the effective
    /// state; the effective state is unchanged and an error is returned.
    pub async fn transition(
        &self,
        actor: &str,
        new_mode: ExecutionMode,
        reason: &str,
    ) -> Result<u64, ModeError> {
        let _guard = self.inner.lock().await;
        let cur = self.current().await?;
        if cur.mode == new_mode {
            // Idempotent: already in the requested mode; nothing to record.
            return Ok(cur.seq);
        }
        let since = openrt_core::now();
        let seq = self
            .audit
            .append(
                actor,
                "mode.transition",
                "execution_mode",
                &format!("to:{}", new_mode.as_str()),
                serde_json::json!({
                    "from": cur.mode.as_str(),
                    "to": new_mode.as_str(),
                    "reason": reason,
                }),
            )
            .await?;
        let record = ModeRecord {
            mode: new_mode,
            since,
            seq,
        };
        let value = serde_json::to_value(&record)?;
        if let Err(e) = self.store.put_record(MODES_NS, CURRENT_KEY, &value).await {
            // Compensating audit entry: the transition was recorded but did not
            // take effect — never leave a silent divergence (fail-closed).
            let _ = self
                .audit
                .append(
                    actor,
                    "mode.transition.aborted",
                    "execution_mode",
                    "denied",
                    serde_json::json!({
                        "from": cur.mode.as_str(),
                        "to": new_mode.as_str(),
                        "store_error": e.to_string(),
                    }),
                )
                .await;
            return Err(ModeError::Persistence(e));
        }
        self.push_history(TransitionView {
            seq,
            at: since,
            actor: actor.to_string(),
            from: cur.mode.as_str().to_string(),
            to: new_mode.as_str().to_string(),
            ext: serde_json::json!({ "reason": reason }),
        })
        .await?;
        Ok(seq)
    }

    async fn read_history(&self) -> Result<Vec<TransitionView>, ModeError> {
        let stored = self.store.get_record(MODES_NS, HISTORY_KEY).await?;
        match stored {
            None => Ok(Vec::new()),
            Some(v) => Ok(serde_json::from_value(v)?),
        }
    }

    async fn push_history(&self, view: TransitionView) -> Result<(), ModeError> {
        let mut all = self.read_history().await?;
        all.push(view);
        if all.len() > HISTORY_LIMIT {
            all.drain(0..(all.len() - HISTORY_LIMIT));
        }
        let value = serde_json::to_value(&all)?;
        self.store.put_record(MODES_NS, HISTORY_KEY, &value).await?;
        Ok(())
    }
}

/// Helper used by `ModeRecord::since` default (deterministic epoch marker).
pub(crate) fn epoch() -> openrt_core::Timestamp {
    chrono::DateTime::parse_from_rfc3339(EPOCH_RFC3339)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(|_| openrt_core::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_persistence::MemoryStore;

    fn manager() -> ModeManager {
        ModeManager::new(Arc::new(MemoryStore::new()))
    }

    #[tokio::test]
    async fn default_mode_is_sovereign() {
        let m = manager();
        let cur = m.current().await.unwrap();
        assert_eq!(cur.mode, ExecutionMode::Sovereign);
        assert_eq!(cur.seq, 0);
    }

    #[tokio::test]
    async fn sovereign_blocks_network_operations() {
        let m = manager();
        assert!(matches!(
            m.ensure_network_allowed().await,
            Err(ModeError::NetworkBlocked)
        ));
        assert!(matches!(
            m.ensure_materialization_allowed("hf.discovery").await,
            Err(ModeError::ProvisioningRequired(_))
        ));
    }

    #[tokio::test]
    async fn provisioning_allows_materialization_but_not_implicitly() {
        let m = manager();
        m.transition("operator", ExecutionMode::Provisioning, "provision host")
            .await
            .unwrap();
        assert!(m.ensure_network_allowed().await.is_ok());
        assert!(m
            .ensure_materialization_allowed("hf.discovery")
            .await
            .is_ok());
        // The mode itself must still be explicitly requested — this method is
        // the only gate; callers decide what to materialize per-action.
        assert_eq!(m.current().await.unwrap().mode, ExecutionMode::Provisioning);
    }

    #[tokio::test]
    async fn transition_is_audited_and_persisted() {
        let m = manager();
        let seq = m
            .transition("operator", ExecutionMode::Provisioning, "bootstrap")
            .await
            .unwrap();
        assert!(seq >= 1);
        let cur = m.current().await.unwrap();
        assert_eq!(cur.mode, ExecutionMode::Provisioning);
        assert_eq!(cur.seq, seq);
        // The audit chain records the transition and is intact.
        assert!(m.audit.verify().await.unwrap() >= 1);
    }

    #[tokio::test]
    async fn transition_is_idempotent() {
        let m = manager();
        m.transition("operator", ExecutionMode::Provisioning, "a")
            .await
            .unwrap();
        let seq_before = m.current().await.unwrap().seq;
        let seq_again = m
            .transition("operator", ExecutionMode::Provisioning, "b")
            .await
            .unwrap();
        assert_eq!(seq_before, seq_again, "no-op transition must not append");
    }

    #[tokio::test]
    async fn history_tracks_last_transitions() {
        let m = manager();
        m.transition("operator", ExecutionMode::Provisioning, "p1")
            .await
            .unwrap();
        m.transition("operator", ExecutionMode::Sovereign, "s1")
            .await
            .unwrap();
        m.transition("operator", ExecutionMode::Provisioning, "p2")
            .await
            .unwrap();
        let status = m.status().await.unwrap();
        assert_eq!(status.transitions.len(), 3);
        assert_eq!(status.transitions[2].to, "provisioning");
        assert_eq!(status.current, ExecutionMode::Provisioning);
    }

    #[tokio::test]
    async fn store_failure_aborts_transition_fail_closed() {
        let store = FailingStore;
        let m = ModeManager::new(Arc::new(store));
        assert!(m
            .transition("operator", ExecutionMode::Provisioning, "x")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn serde_wire_values_match_config() {
        // config OperatingMode serializes to the same lowercase strings.
        let as_json = serde_json::to_string(&ExecutionMode::Sovereign).unwrap();
        assert_eq!(as_json, "\"sovereign\"");
        let from_cfg = ExecutionMode::from(openrt_config::OperatingMode::Provisioning);
        assert_eq!(from_cfg, ExecutionMode::Provisioning);
    }

    #[tokio::test]
    async fn seed_from_config_only_when_unset_and_never_overrides_operator() {
        // No record yet: config wins.
        let m = manager();
        m.seed_from_config(ExecutionMode::Provisioning)
            .await
            .unwrap();
        assert_eq!(m.current().await.unwrap().mode, ExecutionMode::Provisioning);

        // Sovereign seeding is a no-op (already the fail-closed default).
        let m2 = manager();
        m2.seed_from_config(ExecutionMode::Sovereign).await.unwrap();
        assert_eq!(m2.current().await.unwrap().seq, 0);

        // An operator's persisted choice is never overridden by a later seed.
        let m3 = manager();
        m3.transition("operator", ExecutionMode::Provisioning, "manual")
            .await
            .unwrap();
        m3.transition("operator", ExecutionMode::Sovereign, "lock down")
            .await
            .unwrap();
        m3.seed_from_config(ExecutionMode::Provisioning)
            .await
            .unwrap();
        assert_eq!(m3.current().await.unwrap().mode, ExecutionMode::Sovereign);
    }

    struct FailingStore;
    #[async_trait::async_trait]
    impl openrt_persistence::PersistenceStore for FailingStore {
        async fn create_session(
            &self,
            _s: &openrt_core::Session,
        ) -> Result<(), openrt_persistence::PersistenceError> {
            unimplemented!()
        }
        async fn update_session(
            &self,
            _s: &openrt_core::Session,
        ) -> Result<(), openrt_persistence::PersistenceError> {
            unimplemented!()
        }
        async fn load_session(
            &self,
            _id: openrt_core::SessionId,
        ) -> Result<openrt_core::Session, openrt_persistence::PersistenceError> {
            unimplemented!()
        }
        async fn list_sessions(
            &self,
        ) -> Result<Vec<openrt_core::Session>, openrt_persistence::PersistenceError> {
            unimplemented!()
        }
        async fn save_task(
            &self,
            _t: &openrt_core::Task,
        ) -> Result<(), openrt_persistence::PersistenceError> {
            unimplemented!()
        }
        async fn load_task(
            &self,
            _id: openrt_core::TaskId,
        ) -> Result<openrt_core::Task, openrt_persistence::PersistenceError> {
            unimplemented!()
        }
        async fn list_tasks(
            &self,
            _s: Option<openrt_core::SessionId>,
        ) -> Result<Vec<openrt_core::Task>, openrt_persistence::PersistenceError> {
            unimplemented!()
        }
        async fn save_execution(
            &self,
            _r: &openrt_core::ExecutionRecord,
        ) -> Result<(), openrt_persistence::PersistenceError> {
            unimplemented!()
        }
        async fn load_execution(
            &self,
            _id: openrt_core::ExecutionId,
        ) -> Result<openrt_core::ExecutionRecord, openrt_persistence::PersistenceError> {
            unimplemented!()
        }
        async fn append_event(
            &self,
            _e: &openrt_core::DomainEvent,
        ) -> Result<(), openrt_persistence::PersistenceError> {
            unimplemented!()
        }
        async fn list_events(
            &self,
            _o: openrt_persistence::ListOptions,
        ) -> Result<Vec<openrt_core::DomainEvent>, openrt_persistence::PersistenceError> {
            unimplemented!()
        }
        async fn put_record(
            &self,
            _n: &str,
            _k: &str,
            _v: &serde_json::Value,
        ) -> Result<(), openrt_persistence::PersistenceError> {
            Err(openrt_persistence::PersistenceError::Unsupported(
                "failing test store".into(),
            ))
        }
        async fn get_record(
            &self,
            _n: &str,
            _k: &str,
        ) -> Result<Option<serde_json::Value>, openrt_persistence::PersistenceError> {
            Ok(None)
        }
        async fn list_records(
            &self,
            _n: &str,
        ) -> Result<Vec<(String, serde_json::Value)>, openrt_persistence::PersistenceError>
        {
            unimplemented!()
        }
        async fn delete_record(
            &self,
            _n: &str,
            _k: &str,
        ) -> Result<(), openrt_persistence::PersistenceError> {
            unimplemented!()
        }
        async fn clear(&self) -> Result<(), openrt_persistence::PersistenceError> {
            unimplemented!()
        }
    }
}
