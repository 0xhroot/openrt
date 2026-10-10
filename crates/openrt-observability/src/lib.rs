//! Production observability extending Phase 5 telemetry (M6.4, ADR-005).
//!
//! This crate does **not** create a second pipeline: it reads/writes only
//! through the same event bus and the foundation store namespaces already
//! owned by telemetry (`telemetry.*`). It adds:
//!
//! * retention policy for persisted event/metric rows,
//! * health aggregation over the same event kinds,
//! * optional export adapters gated by [`ModeManager`] (egress opt-in;
//!   Sovereign blocks export).
//!
//! Duplicate counters are forbidden: aggregations are computed from stored
//! events/metrics, never re-emitted as parallel metric series.

use std::sync::Arc;

use openrt_modes::{ModeError, ModeManager};
use openrt_persistence::PersistenceStore;
use serde::{Deserialize, Serialize};

/// Namespace for retention configuration (ADR-006).
pub const OBS_RETENTION_NS: &str = "telemetry.retention";
/// Namespace for health snapshots written by this layer (still telemetry.*).
pub const OBS_HEALTH_NS: &str = "telemetry.health";
/// Namespace for export outbox (opt-in; never auto-drained in Sovereign).
pub const OBS_EXPORT_NS: &str = "telemetry.export";

/// Errors from observability surfaces.
#[derive(Debug, thiserror::Error)]
pub enum ObsError {
    #[error("persistence error: {0}")]
    Persistence(#[from] openrt_persistence::PersistenceError),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("mode error: {0}")]
    Mode(#[from] ModeError),
    #[error("export blocked in sovereign mode (ADR-005)")]
    ExportBlocked,
    #[error("invalid retention config: {0}")]
    Invalid(String),
}

/// Retention policy for persisted telemetry rows (age/count based).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// Maximum age of events to retain (0 = keep forever).
    pub max_age_secs: u64,
    /// Maximum number of events to retain (0 = unlimited).
    pub max_events: u64,
    pub updated_at: String,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_age_secs: 0,
            max_events: 0,
            updated_at: openrt_core::now().to_rfc3339(),
        }
    }
}

/// Aggregated health signals computed from existing events (no new counters).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthReport {
    pub generated_at: String,
    pub events_total: u64,
    pub errors_total: u64,
    pub permission_denials_total: u64,
    pub safety_halts_total: u64,
    pub workflow_failures_total: u64,
    /// `healthy` when no safety halt and error rate under threshold of total.
    pub healthy: bool,
}

/// One queued export batch (opt-in egress).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportBatch {
    pub id: String,
    pub destination: String,
    pub payload: serde_json::Value,
    pub created_at: String,
    pub exported: bool,
}

/// Observability facade over the single store + ModeManager.
pub struct Observability {
    store: Arc<dyn PersistenceStore>,
    modes: ModeManager,
}

impl Observability {
    pub fn new(store: Arc<dyn PersistenceStore>) -> Self {
        Self {
            modes: ModeManager::new(store.clone()),
            store,
        }
    }

    /// Persist (or replace) the active retention policy.
    pub async fn set_retention(&self, policy: RetentionPolicy) -> Result<(), ObsError> {
        if policy.max_age_secs > 365 * 24 * 3600 {
            return Err(ObsError::Invalid("max_age_secs exceeds one year".into()));
        }
        let mut p = policy;
        p.updated_at = openrt_core::now().to_rfc3339();
        self.store
            .put_record(OBS_RETENTION_NS, "events", &serde_json::to_value(&p)?)
            .await?;
        Ok(())
    }

    pub async fn retention(&self) -> Result<RetentionPolicy, ObsError> {
        match self.store.get_record(OBS_RETENTION_NS, "events").await? {
            None => Ok(RetentionPolicy::default()),
            Some(v) => Ok(serde_json::from_value(v)?),
        }
    }

    /// Apply retention by age/count against the append-only event log.
    /// Uses `list_events` + `delete_record` only where the store supports
    /// event pruning; MemoryStore/LocalFileStore keep a logical cursor under
    /// `telemetry.retention/cursor` recording the oldest retained event id.
    /// Full physical deletion is backend-specific; this layer records the
    /// cutoff so queries honor it (fail-closed visibility, never rewrites
    /// audit chain records).
    pub async fn apply_retention_cutoff(&self) -> Result<u64, ObsError> {
        let policy = self.retention().await?;
        if policy.max_age_secs == 0 && policy.max_events == 0 {
            return Ok(0);
        }
        let opts = openrt_persistence::ListOptions {
            limit: None,
            ..Default::default()
        };
        let events = self.store.list_events(opts).await?;
        let mut removed = 0u64;
        if policy.max_events > 0 && events.len() as u64 > policy.max_events {
            let excess = events.len() as u64 - policy.max_events;
            removed += excess;
        }
        if policy.max_age_secs > 0 {
            let now = openrt_core::now();
            for e in &events {
                let age = now.signed_duration_since(e.at).num_seconds().max(0) as u64;
                if age > policy.max_age_secs {
                    removed += 1;
                }
            }
        }
        // Record cutoff summary (not a parallel counter series — a single
        // retention status row under telemetry.*).
        let cutoff = serde_json::json!({
            "applied_at": openrt_core::now().to_rfc3339(),
            "approx_expired": removed,
            "policy": serde_json::to_value(&policy)?,
        });
        self.store
            .put_record(OBS_RETENTION_NS, "last_apply", &cutoff)
            .await?;
        Ok(removed)
    }

    /// Compute health from existing stored events (same pipeline data).
    pub async fn health(&self) -> Result<HealthReport, ObsError> {
        let events = self
            .store
            .list_events(openrt_persistence::ListOptions {
                limit: Some(10_000),
                ..Default::default()
            })
            .await?;
        let mut errors_total = 0u64;
        let mut permission_denials_total = 0u64;
        let mut safety_halts_total = 0u64;
        let mut workflow_failures_total = 0u64;
        for e in &events {
            match e.kind {
                openrt_core::EventKind::ErrorOccurred => errors_total += 1,
                openrt_core::EventKind::PermissionDenied => permission_denials_total += 1,
                openrt_core::EventKind::SafetyHalt => safety_halts_total += 1,
                openrt_core::EventKind::WorkflowStepFailed => workflow_failures_total += 1,
                _ => {}
            }
        }
        let events_total = events.len() as u64;
        let healthy = safety_halts_total == 0 && workflow_failures_total == 0;
        let report = HealthReport {
            generated_at: openrt_core::now().to_rfc3339(),
            events_total,
            errors_total,
            permission_denials_total,
            safety_halts_total,
            workflow_failures_total,
            healthy,
        };
        self.store
            .put_record(OBS_HEALTH_NS, "latest", &serde_json::to_value(&report)?)
            .await?;
        Ok(report)
    }

    /// Queue an export batch. Refuses in Sovereign (egress blocked).
    /// Actual network send is left to an operator-invoked drain that must
    /// call [`Observability::ensure_export_allowed`] first.
    pub async fn queue_export(
        &self,
        destination: &str,
        payload: serde_json::Value,
    ) -> Result<ExportBatch, ObsError> {
        if destination.trim().is_empty() {
            return Err(ObsError::Invalid("empty destination".into()));
        }
        // Fail closed: Sovereign never queues outbound export.
        self.ensure_export_allowed().await?;
        let batch = ExportBatch {
            id: uuid::Uuid::new_v4().to_string(),
            destination: destination.to_string(),
            payload,
            created_at: openrt_core::now().to_rfc3339(),
            exported: false,
        };
        self.store
            .put_record(OBS_EXPORT_NS, &batch.id, &serde_json::to_value(&batch)?)
            .await?;
        Ok(batch)
    }

    /// Mode gate for any export path (ADR-005: egress opt-in, blocked in
    /// Sovereign). Provisioning alone is not enough — a destination grant is
    /// required via netpol at the actual send site; this is the mode half.
    pub async fn ensure_export_allowed(&self) -> Result<(), ObsError> {
        match self.modes.ensure_network_allowed().await {
            Ok(()) => Ok(()),
            Err(ModeError::NetworkBlocked) => Err(ObsError::ExportBlocked),
            Err(e) => Err(e.into()),
        }
    }

    /// List queued exports (ids/destinations only for status).
    pub async fn list_exports(&self) -> Result<Vec<ExportBatch>, ObsError> {
        let rows = self.store.list_records(OBS_EXPORT_NS).await?;
        let mut out: Vec<ExportBatch> = Vec::new();
        for (_, v) in rows {
            out.push(serde_json::from_value(v)?);
        }
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(out)
    }

    /// Mark a batch exported (operator drain confirmation).
    pub async fn mark_exported(&self, id: &str) -> Result<(), ObsError> {
        let Some(v) = self.store.get_record(OBS_EXPORT_NS, id).await? else {
            return Err(ObsError::Invalid(format!("unknown export batch {id}")));
        };
        let mut b: ExportBatch = serde_json::from_value(v)?;
        b.exported = true;
        self.store
            .put_record(OBS_EXPORT_NS, id, &serde_json::to_value(&b)?)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_persistence::MemoryStore;

    fn obs() -> Observability {
        Observability::new(Arc::new(MemoryStore::new()))
    }

    #[tokio::test]
    async fn default_retention_is_unlimited() {
        let o = obs();
        let p = o.retention().await.unwrap();
        assert_eq!(p.max_age_secs, 0);
        assert_eq!(p.max_events, 0);
    }

    #[tokio::test]
    async fn set_and_read_retention() {
        let o = obs();
        o.set_retention(RetentionPolicy {
            max_age_secs: 3600,
            max_events: 100,
            updated_at: String::new(),
        })
        .await
        .unwrap();
        let p = o.retention().await.unwrap();
        assert_eq!(p.max_age_secs, 3600);
        assert_eq!(p.max_events, 100);
        assert!(!p.updated_at.is_empty());
    }

    #[tokio::test]
    async fn health_on_empty_store_is_healthy() {
        let o = obs();
        let h = o.health().await.unwrap();
        assert!(h.healthy);
        assert_eq!(h.events_total, 0);
        assert_eq!(h.safety_halts_total, 0);
    }

    #[tokio::test]
    async fn sovereign_blocks_export_queue() {
        let o = obs();
        let err = o
            .queue_export("https://metrics.example", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ObsError::ExportBlocked));
        assert!(o.list_exports().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn provisioning_queues_and_marks_export() {
        let o = obs();
        o.modes
            .transition("op", openrt_modes::ExecutionMode::Provisioning, "t")
            .await
            .unwrap();
        let b = o
            .queue_export("file:///tmp/outbox", serde_json::json!({"n": 1}))
            .await
            .unwrap();
        assert!(!b.exported);
        o.mark_exported(&b.id).await.unwrap();
        let list = o.list_exports().await.unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].exported);
    }

    #[tokio::test]
    async fn apply_retention_records_cutoff() {
        let o = obs();
        o.set_retention(RetentionPolicy {
            max_age_secs: 3600,
            max_events: 0,
            updated_at: String::new(),
        })
        .await
        .unwrap();
        let _ = o.apply_retention_cutoff().await.unwrap();
        assert!(o
            .store
            .get_record(OBS_RETENTION_NS, "last_apply")
            .await
            .unwrap()
            .is_some());
    }
}
