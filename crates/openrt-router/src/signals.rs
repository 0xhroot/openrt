//! Learned routing signals (M12.18): task-type → model outcome history in
//! `routing.signals`, used ONLY to re-order candidates that already passed
//! the router's policy/mode/eligibility checks. Subordinate by
//! construction: a signal preference loses whenever the preferred artifact
//! fails the same routing checks.

use openrt_persistence::PersistenceStore;
use serde::{Deserialize, Serialize};

use crate::RouterError;

pub const SIGNALS_NS: &str = "routing.signals";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingSignal {
    pub id: String,
    pub task_type: String,
    pub artifact_id: String,
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    pub source: String,
    pub at: String,
}

impl RoutingSignal {
    pub fn new(
        task_type: &str,
        artifact_id: &str,
        success: bool,
        latency_ms: Option<u64>,
        source: &str,
    ) -> Self {
        let at = openrt_core::now().to_rfc3339();
        let id = openrt_core::sha256_hex(
            format!("{task_type}\u{1f}{artifact_id}\u{1f}{at}\u{1f}{source}").as_bytes(),
        );
        Self {
            id,
            task_type: task_type.to_string(),
            artifact_id: artifact_id.to_string(),
            success,
            latency_ms,
            source: source.to_string(),
            at,
        }
    }
}

/// Persist one signal record (idempotent content-addressed key).
pub async fn record_signal(
    store: &dyn PersistenceStore,
    signal: &RoutingSignal,
) -> Result<(), RouterError> {
    let key = format!("signal.{}", signal.id);
    let value = serde_json::to_value(signal)?;
    store.put_record(SIGNALS_NS, &key, &value).await?;
    Ok(())
}

/// All recorded signals, oldest first (deterministic).
pub async fn list_signals(store: &dyn PersistenceStore) -> Result<Vec<RoutingSignal>, RouterError> {
    let mut out: Vec<RoutingSignal> = Vec::new();
    for (key, value) in store.list_records(SIGNALS_NS).await? {
        if !key.starts_with("signal.") {
            continue;
        }
        out.push(serde_json::from_value(value)?);
    }
    out.sort_by(|a, b| a.at.cmp(&b.at).then(a.id.cmp(&b.id)));
    Ok(out)
}

/// Preference derived from history for one task type. `ranking` is empty
/// until the task type has `min_samples` recorded signals — preferences are
/// never emitted from thin air.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalPreference {
    pub task_type: String,
    pub ranking: Vec<String>,
    pub success_rates: Vec<(String, f64)>,
    pub samples: usize,
    pub min_samples: usize,
    pub method: String,
}

/// Deterministic preference: success rate desc, sample count desc, id asc.
pub fn preference(
    signals: &[RoutingSignal],
    task_type: &str,
    min_samples: usize,
) -> SignalPreference {
    let mut totals: std::collections::BTreeMap<String, (usize, usize)> =
        std::collections::BTreeMap::new();
    let mut samples = 0usize;
    for s in signals.iter().filter(|s| s.task_type == task_type) {
        samples += 1;
        let e = totals.entry(s.artifact_id.clone()).or_insert((0, 0));
        e.0 += 1;
        if s.success {
            e.1 += 1;
        }
    }
    if samples < min_samples {
        return SignalPreference {
            task_type: task_type.to_string(),
            ranking: Vec::new(),
            success_rates: Vec::new(),
            samples,
            min_samples,
            method: "signal-preference/v1".into(),
        };
    }
    let mut scored: Vec<(String, f64, usize)> = totals
        .iter()
        .map(|(id, (n, ok))| (id.clone(), *ok as f64 / *n as f64, *n))
        .collect();
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.2.cmp(&a.2))
            .then(a.0.cmp(&b.0))
    });
    SignalPreference {
        task_type: task_type.to_string(),
        success_rates: scored.iter().map(|(id, r, _)| (id.clone(), *r)).collect(),
        ranking: scored.iter().map(|(id, _, _)| id.clone()).collect(),
        samples,
        min_samples,
        method: "signal-preference/v1".into(),
    }
}
