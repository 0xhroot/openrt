//! Model-load and residency-tier observations for the Memory Fabric (M1).
//!
//! These are **observation fields only**. OpenRT is an orchestrator: actual
//! weight loading and execution happen in providers/backends, so most of these
//! counters cannot be observed by OpenRT today and must remain `None`.
//!
//! The contract:
//! * `None` means "not observed" — never fabricated, never a synthetic zero.
//! * A value is only ever supplied by the component that actually observed it
//!   (e.g. the registry records the bytes it streamed for integrity hashing;
//!   a future provider adapter records cold/warm load counts it received).
//! * These structs change nothing about admission or execution; they feed
//!   telemetry surfaces only.

use serde::{Deserialize, Serialize};

/// Version of the observation schema. Bump additively only.
pub const OBSERVATION_SCHEMA_VERSION: u32 = 1;

/// Model/provider-oriented I/O observation set.
///
/// Mirrors the counters a Colibrì-style memory fabric emits (cold/warm load
/// counts, bytes read, disk service vs compute stall, tier hit/miss) as an
/// *OpenRT-observable* record: every field is `None` until a real observer
/// fills it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelLoadTelemetry {
    pub schema_version: u32,
    /// How many tensor loads hit the cold path (first read).
    pub cold_load_count: Option<u64>,
    /// How many tensor loads hit a warm/fast path.
    pub warm_load_count: Option<u64>,
    /// Bytes read from the artifact by the observing component.
    pub bytes_read: Option<u64>,
    /// I/O service time spent reading, in nanoseconds (thread/service time).
    pub io_service_time_ns: Option<u64>,
    /// Wall time the compute path blocked waiting for I/O, in nanoseconds —
    /// the stall actually felt, distinct from service time.
    pub observed_stall_ns: Option<u64>,
    /// Residency-tier hits observed (a resident read found the data).
    pub tier_hit_count: Option<u64>,
    /// Residency-tier misses observed (a resident read had to go to disk).
    pub tier_miss_count: Option<u64>,
    /// Who observed these values (e.g. `registry:register_local`), so a later
    /// reader can tell observed data from a placeholder. Empty = last writer
    /// did not declare one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub observed_by: String,
}

impl Default for ModelLoadTelemetry {
    fn default() -> Self {
        Self {
            schema_version: OBSERVATION_SCHEMA_VERSION,
            cold_load_count: None,
            warm_load_count: None,
            bytes_read: None,
            io_service_time_ns: None,
            observed_stall_ns: None,
            tier_hit_count: None,
            tier_miss_count: None,
            observed_by: String::new(),
        }
    }
}

impl ModelLoadTelemetry {
    /// An all-Unknown observation set: the honest state until an observer
    /// actually measures something.
    pub fn unobserved() -> Self {
        Self::default()
    }

    /// True when every field is unobserved.
    pub fn is_unobserved(&self) -> bool {
        self.cold_load_count.is_none()
            && self.warm_load_count.is_none()
            && self.bytes_read.is_none()
            && self.io_service_time_ns.is_none()
            && self.observed_stall_ns.is_none()
            && self.tier_hit_count.is_none()
            && self.tier_miss_count.is_none()
    }

    /// Known values as gauge samples for an existing telemetry surface.
    ///
    /// Like [`crate::residency::ResidencySnapshot::gauge_samples`], only
    /// observed values appear: an unobserved counter is never a zero.
    pub fn gauge_samples(&self) -> Vec<(&'static str, f64, &'static str)> {
        let mut out = Vec::new();
        let ns = [
            ("model_io.cold_loads", self.cold_load_count, "count"),
            ("model_io.warm_loads", self.warm_load_count, "count"),
            ("model_io.bytes_read", self.bytes_read, "bytes"),
            ("model_io.io_service_time_ns", self.io_service_time_ns, "ns"),
            ("model_io.stall_time_ns", self.observed_stall_ns, "ns"),
            ("model_io.tier_hits", self.tier_hit_count, "count"),
            ("model_io.tier_misses", self.tier_miss_count, "count"),
        ];
        for (name, value, unit) in ns {
            if let Some(v) = value {
                out.push((name, v as f64, unit));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_unobserved() {
        let t = ModelLoadTelemetry::unobserved();
        assert!(t.is_unobserved());
        assert_eq!(t.schema_version, OBSERVATION_SCHEMA_VERSION);
        assert!(t.gauge_samples().is_empty());
    }

    #[test]
    fn observed_values_produce_matching_gauges() {
        let t = ModelLoadTelemetry {
            cold_load_count: Some(12),
            warm_load_count: Some(2048),
            bytes_read: Some(1_000_000_000),
            io_service_time_ns: Some(400_000_000),
            observed_stall_ns: Some(10_000_000),
            tier_hit_count: Some(5000),
            tier_miss_count: Some(12),
            observed_by: "test".into(),
            ..Default::default()
        };
        assert!(!t.is_unobserved());
        let g = t.gauge_samples();
        assert_eq!(g.len(), 7);
        for (name, value, unit) in &g {
            assert!(*value > 0.0, "{name}: only observed values are emitted");
            assert!(!unit.is_empty(), "{name}: unit must not be empty");
        }
    }

    #[test]
    fn partial_observation_is_partial_gauges() {
        let t = ModelLoadTelemetry {
            bytes_read: Some(4096),
            observed_by: "registry:register_local".into(),
            ..Default::default()
        };
        let g = t.gauge_samples();
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].0, "model_io.bytes_read");
        assert_eq!(g[0].1, 4096.0);
        assert_eq!(g[0].2, "bytes");
    }

    #[test]
    fn serialization_roundtrip() {
        let t = ModelLoadTelemetry {
            cold_load_count: Some(3),
            bytes_read: Some(99),
            io_service_time_ns: Some(5),
            observed_by: "test".into(),
            ..Default::default()
        };
        let json = serde_json::to_value(&t).unwrap();
        assert_eq!(json["schema_version"], 1);
        let back: ModelLoadTelemetry = serde_json::from_value(json).unwrap();
        assert_eq!(back, t);
    }
}
