//! GPU detection — M9 wires a real observer (`nvidia-smi`) behind the same
//! trait abstraction the earlier phases reserved, while keeping a no-op
//! detector so platforms without a vendor probe report VRAM honestly unknown.

use serde::{Deserialize, Serialize};

const MIB: u64 = 1024 * 1024;

/// Coarse GPU vendor kind OpenRT reasons about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GpuKind {
    Cpu,
    Nvidia,
    Amd,
    Intel,
    Other,
}

/// One discovered accelerator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuDevice {
    pub name: String,
    pub kind: GpuKind,
    /// Device-local VRAM total in MiB, when known.
    pub memory_mb: Option<u64>,
    /// Device-local VRAM **used** in MiB, when the observer reports it.
    /// `None` stays the honest unknown — never a fabricated zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_mib: Option<u64>,
}

impl GpuDevice {
    fn new(name: impl Into<String>, memory_mb: Option<u64>, used_mib: Option<u64>) -> Self {
        Self {
            name: name.into(),
            kind: GpuKind::Other,
            memory_mb,
            used_mib,
        }
    }
}

/// Which observation source produced a [`GpuObservation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuObservationSource {
    /// No observer is configured; nothing was measured.
    None,
    /// Observed via the `nvidia-smi` CLI (read-only query).
    NvidiaSmi,
}

/// A point-in-time, whole-host VRAM observation.
///
/// Every field is optional because Unknown is a real value: a host without the
/// observer, or a query line the parser could not trust, must report what it
/// cannot know. Nothing here is ever guessed from system RAM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuObservation {
    pub source: GpuObservationSource,
    #[serde(default)]
    pub devices: Vec<GpuDevice>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl GpuObservation {
    /// The honest "nothing observed" state.
    pub fn empty() -> Self {
        Self {
            source: GpuObservationSource::None,
            devices: Vec::new(),
            notes: vec!["no GPU observation available".into()],
        }
    }

    /// True when at least one device reports a used amount.
    pub fn has_used(&self) -> bool {
        self.devices.iter().any(|d| d.used_mib.is_some())
    }

    /// Sum of device totals in bytes, when any device was observed and every
    /// one reports a total. An empty observation is `None` — never a zero.
    pub fn total_bytes(&self) -> Option<u64> {
        if self.devices.is_empty() {
            return None;
        }
        let mut sum = 0u64;
        for d in &self.devices {
            sum = sum.checked_add(d.memory_mb?.checked_mul(MIB)?)?;
        }
        Some(sum)
    }

    /// Sum of device used amounts in bytes, when any device was observed and
    /// every one reports usage. An empty observation is `None` — never a zero
    /// that a consumer could mistake for "measured, and idle".
    pub fn used_bytes(&self) -> Option<u64> {
        if self.devices.is_empty() {
            return None;
        }
        let mut sum = 0u64;
        for d in &self.devices {
            sum = sum.checked_add(d.used_mib?.checked_mul(MIB)?)?;
        }
        Some(sum)
    }
}

/// Abstraction over host GPU/accelerator probes.
pub trait GpuDetector: Send + Sync {
    fn detect(&self) -> Vec<GpuDevice>;
}

/// Empty detector — all-encompassing CPUs only.
#[derive(Debug, Default)]
pub struct NoGpuDetector;

impl GpuDetector for NoGpuDetector {
    fn detect(&self) -> Vec<GpuDevice> {
        Vec::new()
    }
}

/// A VRAM usage observer feeding the Memory Fabric planner (M9).
///
/// Observations are read-only and always best-effort: a missing binary, a
/// refused query, or an unparseable reply yields
/// [`GpuObservation::empty`] — never a guessed number.
pub trait GpuObserver: Send + Sync {
    fn observe(&self) -> GpuObservation;
}

/// No-observer default: VRAM stays honestly unknown.
#[derive(Debug, Default)]
pub struct NoGpuObserver;

impl GpuObserver for NoGpuObserver {
    fn observe(&self) -> GpuObservation {
        GpuObservation::empty()
    }
}

/// In-process resistor for tests and fixtures: deterministic values, clearly
/// attributed, never confused with a real measurement.
#[derive(Debug, Default)]
pub struct MockGpuObserver {
    /// `(name, total_mib, used_mib)` rows; public so tests build them inline.
    pub rows: Vec<(String, u64, u64)>,
}

impl GpuObserver for MockGpuObserver {
    fn observe(&self) -> GpuObservation {
        GpuObservation {
            source: GpuObservationSource::None,
            devices: self
                .rows
                .iter()
                .map(|(name, total, used)| GpuDevice::new(name.clone(), Some(*total), Some(*used)))
                .collect(),
            notes: vec!["deterministic test fixture".into()],
        }
    }
}

/// Observes VRAM through the vendor's read-only CLI (`nvidia-smi`).
///
/// Only used when the binary exists and returns well-formed data; any failure
/// degrades to an empty observation. Never invents aggregate numbers the query
/// did not actually produce.
#[derive(Debug, Clone)]
pub struct NvidiaSmiObserver {
    /// Program name/path; defaults to `nvidia-smi`.
    program: String,
    /// Parsed rows from the last successful query (for tests that want to
    /// observe the raw text without spawning).
    last_text: Option<String>,
}

impl Default for NvidiaSmiObserver {
    fn default() -> Self {
        Self {
            program: "nvidia-smi".to_string(),
            last_text: None,
        }
    }
}

impl NvidiaSmiObserver {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            last_text: None,
        }
    }

    /// Query the vendor CLI and parse every well-formed device row.
    pub fn query(&mut self) -> GpuObservation {
        let Some(text) = run_nvidia_smi(&self.program) else {
            return GpuObservation::empty();
        };
        self.last_text = Some(text.clone());
        let devices = parse_nvidia_smi_csv(&text)
            .into_iter()
            .map(|(name, total, used)| GpuDevice::new(name, Some(total), Some(used)))
            .collect::<Vec<_>>();
        let notes = if devices.is_empty() {
            vec!["nvidia-smi ran but returned no well-formed device rows".into()]
        } else {
            vec![]
        };
        GpuObservation {
            source: GpuObservationSource::NvidiaSmi,
            devices,
            notes,
        }
    }
}

impl GpuObserver for NvidiaSmiObserver {
    fn observe(&self) -> GpuObservation {
        self.clone().query()
    }
}

/// Run `nvidia-smi --query-gpu=name,memory.total,memory.used` and return the
/// CSV body when the process succeeded. Read-only; any failure is `None`.
fn run_nvidia_smi(program: &str) -> Option<String> {
    let output = std::process::Command::new(program)
        .arg("--query-gpu=name,memory.total,memory.used")
        .arg("--format=csv,noheader,nounits")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Parse `nvidia-smi` CSV rows into `(name, total_mib, used_mib)` triples.
///
/// A row is trusted only when all three fields parse; every other row is
/// skipped (the device is not reported at all rather than mis-reported).
pub fn parse_nvidia_smi_csv(text: &str) -> Vec<(String, u64, u64)> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let mut it = line.split(',');
            let name = it.next()?.trim().to_string();
            let total = it.next()?.trim().parse::<u64>().ok()?;
            let used = it.next()?.trim().parse::<u64>().ok()?;
            if name.is_empty() {
                return None;
            }
            Some((name, total, used))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMI_CSV: &str = "NVIDIA GeForce RTX 4070, 12288, 2048\n\
                           NVIDIA A100-SXM4-40GB, 40960, 4096\n";

    #[test]
    fn no_gpu_detector_is_empty() {
        assert!(NoGpuDetector.detect().is_empty());
        assert!(NoGpuObserver.observe().devices.is_empty());
    }

    #[test]
    fn smi_csv_parser_reads_only_well_formed_rows() {
        let rows = parse_nvidia_smi_csv(SMI_CSV);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], ("NVIDIA GeForce RTX 4070".into(), 12288, 2048));
        assert_eq!(rows[1], ("NVIDIA A100-SXM4-40GB".into(), 40960, 4096));
        // A garbage line is skipped, not mis-parsed.
        let mut messy = SMI_CSV.to_string();
        messy.push_str("broken row without numbers\n");
        messy.push_str("missing-used, 10,\n");
        assert_eq!(parse_nvidia_smi_csv(&messy).len(), 2);
    }

    #[test]
    fn empty_or_garbage_text_yields_no_devices() {
        assert!(parse_nvidia_smi_csv("").is_empty());
        assert!(parse_nvidia_smi_csv("not,a,row\nx,y,z\n").is_empty());
    }

    #[test]
    fn observation_aggregates_bytes_when_all_known() {
        let obs = MockGpuObserver {
            rows: vec![("gpu0".into(), 12_288, 2_048)],
        }
        .observe();
        assert!(obs.has_used());
        assert_eq!(obs.total_bytes(), Some(12_288 * MIB));
        assert_eq!(obs.used_bytes(), Some(2_048 * MIB));
    }

    #[test]
    fn partial_observation_is_unknown_not_a_partial_sum() {
        let obs = MockGpuObserver {
            rows: vec![("gpu0".into(), 12_288, 2_048)],
        }
        .observe();
        let _ = obs;
        let unknown = GpuObservation {
            source: GpuObservationSource::None,
            devices: vec![GpuDevice {
                name: "gpu0".into(),
                kind: GpuKind::Other,
                memory_mb: Some(12_288),
                used_mib: None,
            }],
            notes: vec![],
        };
        assert!(!unknown.has_used());
        assert_eq!(unknown.total_bytes(), Some(12_288 * MIB));
        assert_eq!(unknown.used_bytes(), None);
    }

    #[test]
    fn observation_round_trips() {
        let obs = NvidiaSmiObserver::default().clone().query();
        // On any host this never panics; with no nvidia binary it is empty.
        if obs.source == GpuObservationSource::None {
            assert!(obs.devices.is_empty());
        }
        let json = serde_json::to_string(&obs).unwrap();
        let back: GpuObservation = serde_json::from_str(&json).unwrap();
        assert_eq!(back, obs);
    }

    #[test]
    fn gpu_round_trips() {
        let d = GpuDevice {
            name: "v100".into(),
            kind: GpuKind::Nvidia,
            memory_mb: Some(16_384),
            used_mib: None,
        };
        let v = serde_json::to_string(&d).unwrap();
        assert!(v.contains("nvidia"));
        assert!(!v.contains("used_mib"), "None used stays absent");
        let back: GpuDevice = serde_json::from_str(&v).unwrap();
        assert_eq!(back.used_mib, None);
    }
}
