//! Local-only metrics with a versioned schema.
//!
//! Part of the Phase 5 telemetry slice (M5.7): a single metrics pipeline that
//! rides the Phase 1 [`EventBus`](crate::EventBus) — no parallel architecture
//! and no egress (ADR-005). Every emitted metric carries
//! [`METRICS_SCHEMA_VERSION`] so Phase 6 observability can evolve the schema
//! additively without breaking local consumers.

use std::collections::BTreeMap;

use serde_json::json;

use crate::{EventBus, EventKind};

/// Version of the local metric/event schema. Bump additively only.
pub const METRICS_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq)]
pub struct MetricRecord {
    pub name: String,
    pub value: f64,
    pub unit: String,
    pub schema_version: u32,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum MetricsError {
    #[error("metric name must not be empty")]
    EmptyName,
    #[error("metric unit must not be empty")]
    EmptyUnit,
}

/// In-process metric store: gauge-style (last value wins per name), sorted
/// snapshots, emission through the shared local event bus only.
#[derive(Debug, Clone, Default)]
pub struct MetricsRegistry {
    records: BTreeMap<String, MetricRecord>,
}

impl MetricsRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn schema_version(&self) -> u32 {
        METRICS_SCHEMA_VERSION
    }

    pub fn record(&mut self, name: &str, value: f64, unit: &str) -> Result<(), MetricsError> {
        if name.trim().is_empty() {
            return Err(MetricsError::EmptyName);
        }
        if unit.trim().is_empty() {
            return Err(MetricsError::EmptyUnit);
        }
        self.records.insert(
            name.to_string(),
            MetricRecord {
                name: name.to_string(),
                value,
                unit: unit.to_string(),
                schema_version: METRICS_SCHEMA_VERSION,
            },
        );
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&MetricRecord> {
        self.records.get(name)
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Deterministic snapshot ordered by metric name.
    pub fn snapshot(&self) -> Vec<MetricRecord> {
        self.records.values().cloned().collect()
    }

    /// Emit every recorded metric onto the local bus. Returns how many
    /// events were accepted. Local-only: the bus fans out to registered
    /// sinks and has no network sink in Phase 5.
    pub async fn emit_all(&self, bus: &EventBus) -> usize {
        let mut sent = 0usize;
        for record in self.records.values() {
            let ev = openrt_core::DomainEvent::new(EventKind::MetricRecorded).with_data(json!({
                "schema_version": record.schema_version,
                "name": record.name,
                "value": record.value,
                "unit": record.unit,
            }));
            if bus.emit(ev).await {
                sent += 1;
            }
        }
        sent
    }
}
