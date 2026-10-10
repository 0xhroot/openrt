//! Resource manager foundation: system monitoring, budgets, GPU abstraction.
//!
//! Pure-Rust `/proc` parsers for the host metrics; the single `libc` FFI call
//! (`statvfs`) lives in [`crate::system::statvfs_avail`]. Rustc rejects the
//! bare `unsafe` blocks, and they are isolated behind a crate-level
//! `unsafe_code = allow` (see the crate `Cargo.toml`).

pub mod gpu;
pub mod limits;
pub mod observations;
pub mod plan;
pub mod preflight;
pub mod residency;
pub mod storage;
pub mod system;

pub use gpu::{
    parse_nvidia_smi_csv, GpuDetector, GpuDevice, GpuKind, GpuObservation, GpuObservationSource,
    GpuObserver, MockGpuObserver, NoGpuDetector, NoGpuObserver, NvidiaSmiObserver,
};
pub use limits::{Admission, ResourceDecision, ResourceGate, ResourceLimits, ResourceOverride};
pub use observations::{ModelLoadTelemetry, OBSERVATION_SCHEMA_VERSION};
pub use plan::{
    BackendTierCapabilities, CapabilityNote, GateProposal, MemoryBudget, MemoryPlan,
    PlanConfidence, PlanInput, PlanStatus, PlanTierStatus, ResidentBasis, Tier, TierBudget,
    MEMORY_PLAN_SCHEMA_VERSION,
};
pub use preflight::{
    PreflightVerdict, Remediation, ResourceDenial, ResourceKind, ResourcePreflight,
    ResourceReading, ResourceStatus,
};
pub use residency::{
    parse_proc_status, parse_smaps_rollup, ProcStatusMemory, ResidencySnapshot, ResidencySource,
    SmapsRollupMemory, RESIDENCY_SCHEMA_VERSION,
};
pub use storage::{
    observe_local_artifact, stat_size_bytes, StorageObservation, StorageObservationSource,
};
pub use system::{DiskUsage, MemInfo, SystemInfo};

/// Aggregate errors for the resources crate.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ResourceError {
    #[error("io error: {0}")]
    Io(String),
    #[error("resource `{0}` not available: {1}")]
    Unavailable(String, String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_parses_fields() {
        let text = "MemTotal:       16289860 kB\nMemFree:         1000000 kB\nMemAvailable:   8000000 kB\nBuffers:          100000 kB\nCached:          3000000 kB\n";
        let mi = MemInfo::parse(text).unwrap();
        assert_eq!(mi.total_kb, 16_289_860);
        assert_eq!(mi.available_kb, 8_000_000);
    }

    #[test]
    fn meminfo_handles_missing_available() {
        let text = "MemTotal:       1024 kB\n";
        let mi = MemInfo::parse(text).unwrap();
        assert_eq!(mi.total_kb, 1024);
        assert_eq!(mi.available_kb, 1024);
    }

    #[test]
    fn cpuinfo_parses() {
        let text = "processor\t: 0\nmodel name\t: Intel(R) Xeon(R)\n\nprocessor\t: 1\nmodel name\t: Intel(R) Xeon(R)\n";
        let ci = crate::system::CpuInfo::parse(text);
        assert_eq!(ci.count, 2);
        assert_eq!(ci.model.as_deref(), Some("Intel(R) Xeon(R)"));
    }
}
