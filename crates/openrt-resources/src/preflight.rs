//! Resource **preflight**: the structured answer to "may this expensive
//! operation start, and if not, what exactly is short and what can the
//! operator do about it?".
//!
//! Why this module exists
//! ----------------------
//! [`crate::limits::ResourceGate::check`] answers a single yes/no with a
//! sentence. That is enough to *refuse* work but not enough to *explain* a
//! refusal. Operators hit exactly this: a tool they expected to work returned
//! `host memory usage 50.4% exceeds limit 50.0%` — which reads as "the tool is
//! broken" rather than "the host is 0.4% over an admission threshold".
//!
//! The preflight turns that sentence into a first-class value:
//!
//! * one [`ResourceReading`] per dimension (memory / cpu / vram / disk) with
//!   **current**, **limit**, **headroom** and an explicit status,
//! * a [`ResourceDenial`] carrying the numbers that caused the refusal and the
//!   set of remediations the operator actually has,
//! * a [`PreflightVerdict`] that separates "constrained" (admit, but tell the
//!   operator) from "denied" (do not start).
//!
//! Honesty rules
//! -------------
//! * A dimension that cannot be measured is [`ResourceStatus::Unknown`] and
//!   carries *why* it is unknown. It is never silently reported as OK.
//! * Headroom, not "used", is the primary admission signal: another process
//!   (a model server, a browser) legitimately holding memory must not refuse a
//!   20 KB tool call. See [`crate::limits::ResourceLimits::min_headroom_ratio`].
//! * This module computes. It never admits or executes anything on its own —
//!   [`crate::limits::ResourceGate`] remains the only authority.

use serde::{Deserialize, Serialize};

use crate::limits::ResourceLimits;
use crate::system::SystemInfo;

/// Which dimension a reading or denial refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceKind {
    Memory,
    Cpu,
    Vram,
    Disk,
}

impl ResourceKind {
    /// Stable lowercase label used by the CLI, doctor rows and audit records.
    pub fn as_str(&self) -> &'static str {
        match self {
            ResourceKind::Memory => "memory",
            ResourceKind::Cpu => "cpu",
            ResourceKind::Vram => "vram",
            ResourceKind::Disk => "disk",
        }
    }

    /// Human label for TUI surfaces ("Host memory", "GPU memory", …).
    pub fn label(&self) -> &'static str {
        match self {
            ResourceKind::Memory => "Host memory",
            ResourceKind::Cpu => "CPU",
            ResourceKind::Vram => "GPU memory",
            ResourceKind::Disk => "Disk",
        }
    }

    /// The byte-ish unit used by this dimension, if any.
    pub fn unit(&self) -> &'static str {
        match self {
            ResourceKind::Memory | ResourceKind::Vram | ResourceKind::Disk => "bytes",
            ResourceKind::Cpu => "percent",
        }
    }
}

/// Health of one dimension at preflight time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceStatus {
    /// Comfortably above the required headroom.
    Ok,
    /// Above the floor but close to it — admit, and say so.
    Tight,
    /// Below the floor — do not start.
    Blocked,
    /// Not measurable on this host. Always paired with a reason.
    Unknown,
}

impl ResourceStatus {
    /// Doctor/TUI status word.
    pub fn as_str(&self) -> &'static str {
        match self {
            ResourceStatus::Ok => "PASS",
            ResourceStatus::Tight => "WARN",
            ResourceStatus::Blocked => "BLOCKED",
            ResourceStatus::Unknown => "UNAVAILABLE",
        }
    }
}

/// One measured dimension with its current value, limit and headroom.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceReading {
    pub kind: ResourceKind,
    /// Measured value in the dimension's unit. `None` when unmeasurable.
    pub current: Option<u64>,
    /// Configured ceiling/floor in the same unit. `None` = unbounded.
    pub limit: Option<u64>,
    /// `limit - current` where both exist. `None` when either is missing.
    pub headroom: Option<u64>,
    /// Fraction of the limit currently consumed, 0.0–`f64::MAX`. `None` when
    /// the dimension is unmeasurable or unbounded.
    pub used_pct: Option<f64>,
    pub status: ResourceStatus,
    /// Why the reading is [`ResourceStatus::Unknown`]. Empty otherwise.
    pub unknown_reason: String,
}

impl ResourceReading {
    fn unknown(kind: ResourceKind, why: impl Into<String>) -> Self {
        Self {
            kind,
            current: None,
            limit: None,
            headroom: None,
            used_pct: None,
            status: ResourceStatus::Unknown,
            unknown_reason: why.into(),
        }
    }

    /// `current / limit` as a percentage, or `None` when unknown.
    pub fn used_pct(&self) -> Option<f64> {
        self.used_pct
    }

    /// One-line `current / limit (headroom)` summary for compact surfaces.
    pub fn summary(&self) -> String {
        match (self.current, self.limit, self.headroom) {
            (Some(c), Some(l), Some(h)) => {
                format!("{} / {} (headroom {})", c, l, h)
            }
            (Some(c), None, _) => format!("{} / unlimited", c),
            (Some(c), Some(l), None) => format!("{} / {}", c, l),
            _ => self.unknown_reason.clone(),
        }
    }
}

/// A single action the operator can take for a blocked dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Remediation {
    /// Re-check immediately (the host may have freed memory already).
    Retry,
    /// Pause and let the host settle, then re-check.
    Wait,
    /// Pick a model whose footprint fits the current headroom.
    SwitchModel,
    /// Open the full resource detail panel.
    OpenDetails,
    /// Abandon the operation.
    Cancel,
    /// Operator-authorised one-shot bypass, through the existing policy
    /// authority. Only ever set by the operator, never by the model.
    OperatorOverride,
}

impl Remediation {
    /// Stable verb for TUI buttons and audit records.
    pub fn verb(&self) -> &'static str {
        match self {
            Remediation::Retry => "Retry",
            Remediation::Wait => "Wait",
            Remediation::SwitchModel => "Switch Model",
            Remediation::OpenDetails => "Open Resource Details",
            Remediation::Cancel => "Cancel",
            Remediation::OperatorOverride => "Override",
        }
    }

    /// Short key label used by the compact permission-style dialog.
    pub fn key(&self) -> &'static str {
        match self {
            Remediation::Retry => "r",
            Remediation::Wait => "w",
            Remediation::SwitchModel => "m",
            Remediation::OpenDetails => "d",
            Remediation::Cancel => "Esc",
            Remediation::OperatorOverride => "o",
        }
    }
}

/// A structured refusal. Replaces the old free-text-only denial with numbers
/// the operator (and the TUI) can act on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceDenial {
    pub kind: ResourceKind,
    /// Plain-language cause ("host memory is above the admission threshold").
    pub reason: String,
    pub current: Option<u64>,
    pub limit: Option<u64>,
    pub headroom: Option<u64>,
    pub used_pct: Option<f64>,
    /// What the operator can do, in priority order.
    pub remediations: Vec<Remediation>,
}

impl ResourceDenial {
    /// Stable one-line form used when a structured value must become text
    /// (error messages, audit rows, CLI `--json` payloads).
    pub fn to_line(&self) -> String {
        match (self.current, self.limit) {
            (Some(c), Some(l)) => format!(
                "{} — {} (current {:.1}%, limit {:.1}%)",
                self.kind.label(),
                self.reason,
                self.used_pct.unwrap_or(0.0),
                ratio_pct(c, l)
            ),
            _ => format!("{} — {}", self.kind.label(), self.reason),
        }
    }

    /// Whether the given remediation is offered for this denial.
    pub fn offers(&self, r: Remediation) -> bool {
        self.remediations.contains(&r)
    }
}

impl std::fmt::Display for ResourceDenial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_line())
    }
}

impl std::error::Error for ResourceDenial {}

fn ratio_pct(current: u64, limit: u64) -> f64 {
    if limit == 0 {
        0.0
    } else {
        (current as f64 / limit as f64) * 100.0
    }
}

/// Everything measured, plus the decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourcePreflight {
    pub memory: ResourceReading,
    pub cpu: ResourceReading,
    pub vram: ResourceReading,
    pub disk: ResourceReading,
    /// Set when a dimension is [`ResourceStatus::Blocked`].
    pub denial: Option<ResourceDenial>,
}

impl ResourcePreflight {
    /// Compute the full posture from a host snapshot and the limits.
    pub fn evaluate(snapshot: &SystemInfo, limits: &ResourceLimits) -> Self {
        Self::evaluate_with_gpu(snapshot, limits, &crate::gpu::GpuObservation::empty())
    }

    /// Compute posture with an optional live VRAM observation (M9).
    ///
    /// The observation is **reported** — it never gains admission authority on
    /// its own, because no VRAM floor exists yet (see `ResourceLimits`). The
    /// gate semantics stay byte-identical to the pre-observer phase: an
    /// unmeasured VRAM dimension is `Unknown`, never silently OK.
    pub fn evaluate_with_gpu(
        snapshot: &SystemInfo,
        limits: &ResourceLimits,
        gpu: &crate::gpu::GpuObservation,
    ) -> Self {
        let memory = memory_reading(snapshot, limits);
        let disk = disk_reading(snapshot, limits);
        let cpu = cpu_reading(snapshot, limits);
        let vram = vram_reading(gpu);
        let denial = blocked(&memory, limits)
            .or_else(|| blocked(&disk, limits))
            .or_else(|| blocked(&cpu, limits));
        Self {
            memory,
            cpu,
            vram,
            disk,
            denial,
        }
    }

    /// All four readings, in display order.
    pub fn readings(&self) -> [&ResourceReading; 4] {
        [&self.memory, &self.cpu, &self.vram, &self.disk]
    }

    /// Dimensions that are measured but close to their floor.
    pub fn tight(&self) -> Vec<&ResourceReading> {
        self.readings()
            .into_iter()
            .filter(|r| r.status == ResourceStatus::Tight)
            .collect()
    }

    /// `false` when any dimension is blocked or unmeasurable-and-required.
    pub fn admissible(&self) -> bool {
        self.denial.is_none()
    }

    /// The admission decision: admit, and separately say whether the operator
    /// should be warned.
    pub fn verdict(&self) -> PreflightVerdict {
        match &self.denial {
            Some(d) => PreflightVerdict::Denied(d.clone()),
            None if !self.tight().is_empty() => {
                PreflightVerdict::Constrained(self.tight().iter().map(|r| r.kind).collect())
            }
            None => PreflightVerdict::Admitted,
        }
    }
}

/// The decision a preflight produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum PreflightVerdict {
    /// Start the operation.
    Admitted,
    /// Start the operation, but the operator should know a dimension is close
    /// to its floor. Selection is never blocked for this alone.
    Constrained(Vec<ResourceKind>),
    /// Do not start. Carries the numbers and the operator's options.
    Denied(ResourceDenial),
}

impl PreflightVerdict {
    pub fn is_admitted(&self) -> bool {
        !matches!(self, PreflightVerdict::Denied(_))
    }

    /// Admitted, but at least one dimension is close to its floor.
    pub fn is_constrained(&self) -> bool {
        matches!(self, PreflightVerdict::Constrained(k) if !k.is_empty())
    }

    /// The denial, if this verdict is a denial.
    pub fn denial(&self) -> Option<&ResourceDenial> {
        match self {
            PreflightVerdict::Denied(d) => Some(d),
            _ => None,
        }
    }
}

fn blocked(r: &ResourceReading, limits: &ResourceLimits) -> Option<ResourceDenial> {
    if r.status != ResourceStatus::Blocked {
        return None;
    }
    Some(ResourceDenial {
        kind: r.kind,
        reason: match r.kind {
            ResourceKind::Memory => {
                "host memory is above the configured admission threshold".to_string()
            }
            ResourceKind::Disk => "free disk is below the configured floor".to_string(),
            ResourceKind::Cpu => "CPU headroom is below the configured floor".to_string(),
            ResourceKind::Vram => "GPU memory is below the configured floor".to_string(),
        },
        current: r.current,
        limit: r.limit,
        headroom: r.headroom,
        used_pct: r.used_pct,
        remediations: default_remediations(limits),
    })
}

fn default_remediations(limits: &ResourceLimits) -> Vec<Remediation> {
    let mut r = vec![
        Remediation::Retry,
        Remediation::Wait,
        Remediation::SwitchModel,
        Remediation::OpenDetails,
    ];
    // Only offer what this deployment actually permits: a UI must not render an
    // "Override" affordance the gate would refuse.
    if limits.allow_operator_override {
        r.push(Remediation::OperatorOverride);
    }
    r.push(Remediation::Cancel);
    r
}

/// Memory posture. Headroom (`available`) is the primary signal; the historical
/// `max_mem_used_ratio` is the fallback used only when the total is unknown.
fn memory_reading(snapshot: &SystemInfo, limits: &ResourceLimits) -> ResourceReading {
    let total = snapshot.mem.total_kb;
    let avail = snapshot.mem.available_kb;
    if total == 0 {
        return ResourceReading::unknown(
            ResourceKind::Memory,
            "/proc/meminfo unavailable: MemTotal not reported",
        );
    }
    let total_bytes = total.saturating_mul(1024);
    let avail_bytes = avail.saturating_mul(1024);

    // Headroom floor in bytes. An explicit absolute floor wins over the ratio.
    let floor = limits
        .min_free_memory_bytes
        .unwrap_or_else(|| (total_bytes as f64 * limits.min_headroom_ratio) as u64);

    let headroom = avail_bytes.saturating_sub(floor);
    let used_pct = Some(ratio_pct(
        total_bytes.saturating_sub(avail_bytes),
        total_bytes,
    ));

    // Fallback path: we could not apply the headroom floor (ratio 0 and no
    // absolute floor) — fall back to the legacy used-ratio ceiling.
    if limits.min_headroom_ratio <= 0.0 && limits.min_free_memory_bytes.is_none() {
        let ceiling = limits.max_mem_used_ratio;
        let used_ratio = 1.0 - (avail as f64 / total as f64);
        let status = if ceiling > 0.0 && used_ratio > ceiling {
            ResourceStatus::Blocked
        } else {
            ResourceStatus::Ok
        };
        return ResourceReading {
            kind: ResourceKind::Memory,
            current: Some(total_bytes.saturating_sub(avail_bytes)),
            limit: Some((total_bytes as f64 * ceiling) as u64),
            headroom: Some(avail_bytes),
            used_pct: Some(used_ratio * 100.0),
            status,
            unknown_reason: String::new(),
        };
    }

    let status = if avail_bytes < floor {
        ResourceStatus::Blocked
    } else if limits.tight_headroom_ratio > 0.0
        && (avail_bytes as f64 / total_bytes as f64) < limits.tight_headroom_ratio
    {
        ResourceStatus::Tight
    } else {
        ResourceStatus::Ok
    };

    ResourceReading {
        kind: ResourceKind::Memory,
        current: Some(total_bytes.saturating_sub(avail_bytes)),
        limit: Some(floor),
        headroom: Some(headroom),
        used_pct,
        status,
        unknown_reason: String::new(),
    }
}

fn disk_reading(snapshot: &SystemInfo, limits: &ResourceLimits) -> ResourceReading {
    let Some(floor) = limits.min_free_disk_bytes else {
        return ResourceReading {
            kind: ResourceKind::Disk,
            current: Some(snapshot.disk.avail_bytes),
            limit: None,
            headroom: None,
            used_pct: None,
            status: ResourceStatus::Ok,
            unknown_reason: String::new(),
        };
    };
    let avail = snapshot.disk.avail_bytes;
    if snapshot.disk.total_bytes == 0 {
        return ResourceReading::unknown(
            ResourceKind::Disk,
            "statvfs unavailable for the workspace filesystem",
        );
    }
    let status = if avail > 0 && avail < floor {
        ResourceStatus::Blocked
    } else {
        ResourceStatus::Ok
    };
    ResourceReading {
        kind: ResourceKind::Disk,
        current: Some(avail),
        limit: Some(floor),
        headroom: Some(avail.saturating_sub(floor)),
        used_pct: None,
        status,
        unknown_reason: String::new(),
    }
}

fn cpu_reading(snapshot: &SystemInfo, limits: &ResourceLimits) -> ResourceReading {
    // No instantaneous CPU utilisation probe is wired in this phase; the
    // logical core count is the only reliable figure. Reported as advisory —
    // an unknown utilisation is NOT allowed to block admission.
    let cores = snapshot.cpu.count as u64;
    if cores == 0 {
        return ResourceReading::unknown(
            ResourceKind::Cpu,
            "/proc/cpuinfo unavailable: no processors reported",
        );
    }
    ResourceReading {
        kind: ResourceKind::Cpu,
        current: Some(cores),
        limit: Some(limits.max_cpu_percent as u64),
        headroom: None,
        used_pct: None,
        status: ResourceStatus::Ok,
        unknown_reason: String::new(),
    }
}

/// VRAM posture from a live [`crate::gpu::GpuObservation`].
///
/// Honesty rules, in priority order:
/// * nothing observed            → [`ResourceStatus::Unknown`] (unchanged),
/// * devices observed but usage  → aggregate used/total, but no floor exists
///   not fully reported            yet, so the dimension stays `Unknown` with
///                                the observation attached rather than blocking,
/// * fully reported              → `Unknown` (no floor) — still reported so the
///                                operator *sees* real numbers, never admitted
///                                on a guess.
fn vram_reading(gpu: &crate::gpu::GpuObservation) -> ResourceReading {
    if gpu.devices.is_empty() {
        return ResourceReading::unknown(
            ResourceKind::Vram,
            "no GPU/VRAM probe available on this host",
        );
    }
    match (gpu.total_bytes(), gpu.used_bytes()) {
        (Some(total), Some(used)) => ResourceReading {
            kind: ResourceKind::Vram,
            current: Some(used),
            // no admission floor is configured yet; limit stays None
            limit: None,
            headroom: Some(total.saturating_sub(used)),
            used_pct: Some(if total == 0 {
                0.0
            } else {
                ratio_pct(used, total)
            }),
            // VRAM admission requires an explicit floor (none exists), so the
            // dimension is reported, not granted admission authority.
            status: ResourceStatus::Unknown,
            unknown_reason: format!(
                "observed via {:?}; no VRAM admission floor configured",
                gpu.source
            ),
        },
        _ => ResourceReading::unknown(
            ResourceKind::Vram,
            format!(
                "GPU present but {:?} did not fully report usage",
                gpu.source
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::{CpuInfo, DiskUsage, MemInfo};

    fn snap(total_kb: u64, avail_kb: u64, free_disk: u64) -> SystemInfo {
        SystemInfo {
            os: "linux".into(),
            kernel: "6.0".into(),
            arch: "x86_64".into(),
            cpu: CpuInfo {
                count: 8,
                model: Some("test".into()),
            },
            uptime_secs: 1,
            mem: MemInfo {
                total_kb,
                available_kb: avail_kb,
            },
            disk: DiskUsage {
                total_bytes: 1 << 40,
                avail_bytes: free_disk,
            },
        }
    }

    fn limits() -> ResourceLimits {
        ResourceLimits::default()
    }

    #[test]
    fn preflight_success_reports_real_headroom() {
        // 16 GB total, 12 GB available -> 75% headroom, well above the floor.
        let p = ResourcePreflight::evaluate(
            &snap(16 * 1024 * 1024, 12 * 1024 * 1024, 1 << 30),
            &limits(),
        );
        assert!(p.admissible(), "{p:?}");
        assert_eq!(p.memory.status, ResourceStatus::Ok);
        assert_eq!(p.denial, None);
        assert_eq!(p.verdict(), PreflightVerdict::Admitted);
        // current = used bytes, limit = the required-headroom floor.
        assert_eq!(p.memory.current, Some(4 * 1024 * 1024 * 1024));
        assert!(p.memory.headroom.unwrap() > 0);
    }

    #[test]
    fn preflight_failure_is_structured_not_a_sentence() {
        // 16 GB total, 1 GB available -> below the 15% headroom floor.
        let p =
            ResourcePreflight::evaluate(&snap(16 * 1024 * 1024, 1024 * 1024, 1 << 30), &limits());
        assert!(!p.admissible());
        let d = p.denial.as_ref().expect("denial");
        assert_eq!(d.kind, ResourceKind::Memory);
        assert!(d.reason.contains("admission threshold"), "{}", d.reason);
        assert!(d.used_pct.is_some());
        assert!(d.offers(Remediation::Retry));
        assert!(d.offers(Remediation::Wait));
        assert!(d.offers(Remediation::SwitchModel));
        assert!(d.offers(Remediation::Cancel));
        assert!(d.to_line().contains("Host memory"));
    }

    #[test]
    fn unrelated_process_memory_does_not_refuse_a_small_call() {
        // The exact live-battery shape: a model server holds ~50.4% of a 15 GB
        // host. Under the OLD absolute `used < 50%` rule this was refused; with
        // headroom-based admission there is still ~49% available, so it admits.
        let total_kb: u64 = 15_551_000;
        let avail_kb: u64 = 7_700_000; // ~50.4% used, ~49.6% available
        let p = ResourcePreflight::evaluate(&snap(total_kb, avail_kb, 1 << 30), &limits());
        assert!(p.admissible(), "must not refuse small calls: {p:?}");
        assert!(
            p.memory.used_pct.unwrap() > 50.0,
            "still honestly reports >50% used"
        );
    }

    #[test]
    fn tight_headroom_admits_but_warns() {
        // 16 GB total, 3 GB available = 18.75% — above the 15% floor, below the
        // 30% "tight" mark.
        let l = ResourceLimits {
            tight_headroom_ratio: 0.30,
            ..ResourceLimits::default()
        };
        let p = ResourcePreflight::evaluate(&snap(16 * 1024 * 1024, 3 * 1024 * 1024, 1 << 30), &l);
        assert!(p.admissible(), "tight must still admit");
        assert_eq!(p.memory.status, ResourceStatus::Tight);
        assert!(matches!(
            p.verdict(),
            PreflightVerdict::Constrained(v) if v.contains(&ResourceKind::Memory)
        ));
    }

    #[test]
    fn unknown_dimensions_say_why_instead_of_guessing() {
        let mut s = snap(16 * 1024 * 1024, 12 * 1024 * 1024, 1 << 30);
        s.cpu.count = 0;
        let p = ResourcePreflight::evaluate(&s, &limits());
        assert_eq!(p.cpu.status, ResourceStatus::Unknown);
        assert!(
            p.cpu.unknown_reason.contains("cpuinfo"),
            "{}",
            p.cpu.unknown_reason
        );
        assert_eq!(p.vram.status, ResourceStatus::Unknown);
        assert!(p.vram.unknown_reason.contains("GPU"));
        // Unknown must NOT block: nothing was measured that could be short.
        assert!(p.admissible());
    }

    #[test]
    fn unmeasurable_memory_is_unknown_not_ok() {
        let s = snap(0, 0, 0);
        let p = ResourcePreflight::evaluate(&s, &limits());
        assert_eq!(p.memory.status, ResourceStatus::Unknown);
        assert!(p.memory.unknown_reason.contains("MemTotal"));
        assert!(
            p.admissible(),
            "an unreadable host must fail open to a clear error, not a silent denial"
        );
    }

    #[test]
    fn disk_floor_blocks_with_disk_denial() {
        let l = ResourceLimits {
            min_free_disk_bytes: Some((1 << 40) + 1),
            ..ResourceLimits::default()
        };
        let p = ResourcePreflight::evaluate(&snap(16 * 1024 * 1024, 12 * 1024 * 1024, 1024), &l);
        let d = p.denial.expect("disk denial");
        assert_eq!(d.kind, ResourceKind::Disk);
        assert!(d.reason.contains("disk"), "{}", d.reason);
    }

    #[test]
    fn disabling_the_headroom_guard_restores_the_legacy_ratio_rule() {
        let l = ResourceLimits {
            min_headroom_ratio: 0.0,
            min_free_memory_bytes: None,
            ..ResourceLimits::default()
        };
        // 90% used -> above the legacy 50% ceiling -> blocked.
        let p = ResourcePreflight::evaluate(&snap(100_000, 10_000, 1 << 30), &l);
        assert!(
            !p.admissible(),
            "legacy ceiling must still apply when headroom is off"
        );
        // 10% used -> fine.
        let p = ResourcePreflight::evaluate(&snap(100_000, 90_000, 1 << 30), &l);
        assert!(p.admissible());
    }

    #[test]
    fn absolute_memory_floor_wins_over_the_ratio() {
        let l = ResourceLimits {
            min_free_memory_bytes: Some(8 * 1024 * 1024 * 1024),
            ..ResourceLimits::default()
        };
        // 4 GB available on a 16 GB host is 25% (above the 15% ratio) but
        // below the explicit 8 GB absolute floor -> blocked.
        let p = ResourcePreflight::evaluate(&snap(16 * 1024 * 1024, 4 * 1024 * 1024, 1 << 30), &l);
        assert!(!p.admissible());
        assert_eq!(p.denial.unwrap().kind, ResourceKind::Memory);
    }

    #[test]
    fn verdict_never_blocks_selection_on_tight_alone() {
        let l = ResourceLimits {
            tight_headroom_ratio: 0.9,
            ..ResourceLimits::default()
        };
        let p = ResourcePreflight::evaluate(&snap(16 * 1024 * 1024, 3 * 1024 * 1024, 1 << 30), &l);
        assert!(p.verdict().is_admitted(), "constrained must still admit");
    }

    #[test]
    fn observed_vram_is_reported_but_never_gains_admission_authority() {
        use crate::gpu::{GpuObservationSource, GpuObserver, MockGpuObserver};
        let gpu = MockGpuObserver {
            rows: vec![("NVIDIA RTX 4070".into(), 12_288, 8_192)],
        }
        .observe();
        assert_eq!(gpu.source, GpuObservationSource::None, "fixture is a mock");
        let p = ResourcePreflight::evaluate_with_gpu(
            &snap(16 * 1024 * 1024, 12 * 1024 * 1024, 1 << 30),
            &limits(),
            &gpu,
        );
        // Real numbers are surfaced…
        assert_eq!(p.vram.current, Some(8_192 * 1024 * 1024));
        assert_eq!(p.vram.headroom, Some(4_096 * 1024 * 1024));
        assert_eq!(
            p.vram.used_pct,
            Some((8_192.0 / 12_288.0) * 100.0),
            "ratio_pct exact arithmetic"
        );
        // …but no floor exists yet, so the dimension stays Unknown, not Ok.
        assert_eq!(p.vram.status, ResourceStatus::Unknown);
        assert!(p.vram.unknown_reason.contains("floor"));
        assert!(p.admissible(), "observation must not start denying alone");
        assert_eq!(p.vram.limit, None);
    }

    #[test]
    fn partially_reported_vram_stays_unknown() {
        use crate::gpu::{GpuDevice, GpuKind, GpuObservation, GpuObservationSource};
        let gpu = GpuObservation {
            source: GpuObservationSource::None,
            devices: vec![GpuDevice {
                name: "gpu0".into(),
                kind: GpuKind::Nvidia,
                memory_mb: Some(12_288),
                used_mib: None,
            }],
            notes: vec![],
        };
        let p = ResourcePreflight::evaluate_with_gpu(
            &snap(16 * 1024 * 1024, 12 * 1024 * 1024, 1 << 30),
            &limits(),
            &gpu,
        );
        assert_eq!(p.vram.status, ResourceStatus::Unknown);
        assert!(p.vram.unknown_reason.contains("usage"));
        assert_eq!(p.vram.current, None);
    }
}
