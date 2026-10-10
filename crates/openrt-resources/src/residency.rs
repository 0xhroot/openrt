//! Residency telemetry for the Memory Fabric (M1).
//!
//! A [`ResidencySnapshot`] answers the question *"how much memory does this
//! process actually own, in each ownership class?"* without guessing.
//!
//! OpenRT is an orchestrator: it does not allocate model weights in-process
//! (`NoGpuDetector`, providers own execution), so nothing in this module
//! allocates, demotes, evicts, overrides, or authorizes anything. It observes
//! and reports only.
//!
//! # Ownership classes (distinct by construction)
//!
//! * `owned_ram_bytes` — anonymous pages the process actually owns
//!   (`RssAnon`). File-backed mapping pages are *not* owned memory: the
//!   kernel can reclaim them under pressure, so counting them as a cost of
//!   running a model is exactly the RssAnon-vs-VmRSS trap the Colibrì memory
//!   fabric documented (VmRSS inflates by gigabytes while not a single owned
//!   byte changes).
//! * `file_backed_bytes` — pages backed by a file mapping (`RssFile` /
//!   smaps `File`). Reclaimable; not owned.
//! * `device_bytes` — VRAM. OpenRT has no VRAM observer in M1 (GPU detection
//!   is `NoGpuDetector`), so this stays `None` unless a caller supplies a
//!   measured value via [`ResidencySnapshot::with_device_bytes`].
//! * `nvme_bytes` — bytes of model artifacts on local storage. OpenRT can
//!   observe declared artifact bytes (the registry's
//!   `artifact_bytes_declared`); the storage device class itself is not
//!   measured, so the field is the model-artifact proxy a caller may fill via
//!   [`ResidencySnapshot::with_nvme_bytes`].
//!
//! # Failure semantics
//!
//! Telemetry failure yields `None` / [`ResidencySource::Unsupported`], never a
//! panic, and never an error that could fail model registration or normal
//! execution. `Unknown` is always honest: we do not invent values a platform
//! cannot provide, and we do not fake parity across platforms.
//!
//! # Integration
//!
//! [`ResidencySnapshot::gauge_samples`] exposes known values as
//! `(name, value, unit)` triples that any existing telemetry surface (the
//! runtime's `MetricsRegistry`/`metrics_record`) can record without this
//! module referencing it — no second measurement or audit authority.

use serde::{Deserialize, Serialize};

/// Version of the residency snapshot schema. Bump additively only.
pub const RESIDENCY_SCHEMA_VERSION: u32 = 1;

/// Where the snapshot's values came from. Never guesses: every variant is a
/// real observer OpenRT exercised (or an explicit statement that none was).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResidencySource {
    /// `/proc/self/status` (`RssAnon` / `RssFile`) on Linux — the primary
    /// observer for owned vs file-backed memory.
    ProcStatus,
    /// `/proc/self/smaps_rollup` (`Anonymous` / `File`) on Linux — fallback
    /// when the status file lacks the fields.
    ProcSmapsRollup,
    /// Values merged from more than one observer (process + device + store).
    Composite,
    /// No observer could run; every value is `None`/unknown.
    Unsupported,
    /// Values supplied by a deterministic test fixture.
    #[cfg(test)]
    Fixture,
}

/// A point-in-time answer to "what does this process memory footprint look
/// like, by ownership class?".
///
/// All byte fields are `Option` because Unknown is a real value: a platform,
/// kernel, or deployment without an observer must report what it cannot know
/// rather than fabricate a number the planner could act on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResidencySnapshot {
    pub schema_version: u32,
    /// RFC 3339 timestamp of the observation.
    pub timestamp: String,
    /// Anonymous pages the process actually owns. `None` = not observed.
    pub owned_ram_bytes: Option<u64>,
    /// File-backed (page-cache) pages mapped into the process. Reclaimable.
    pub file_backed_bytes: Option<u64>,
    /// VRAM bytes. `None` = no VRAM observer (the M1 default).
    pub device_bytes: Option<u64>,
    /// Model-artifact bytes on local storage (registry-declared proxy).
    pub nvme_bytes: Option<u64>,
    /// Which observer(s) produced the values.
    pub source: ResidencySource,
    /// Human-readable notes on why a value is unknown. Empty by default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl ResidencySnapshot {
    /// A snapshot whose every value is Unknown.
    pub fn unknown() -> Self {
        Self {
            schema_version: RESIDENCY_SCHEMA_VERSION,
            timestamp: openrt_core::now().to_rfc3339(),
            owned_ram_bytes: None,
            file_backed_bytes: None,
            device_bytes: None,
            nvme_bytes: None,
            source: ResidencySource::Unsupported,
            notes: vec!["no residency observer available".into()],
        }
    }

    /// True when every byte field is unknown — the honest "nothing measured"
    /// state. Callers MUST NOT treat this as a zero-usage model.
    pub fn is_unknown(&self) -> bool {
        self.owned_ram_bytes.is_none()
            && self.file_backed_bytes.is_none()
            && self.device_bytes.is_none()
            && self.nvme_bytes.is_none()
    }

    /// Attach a measured (never guessed) VRAM byte count.
    ///
    /// The value must come from an actual observer (a future GPU detector or a
    /// provider-reported number); this setter does not invent one.
    pub fn with_device_bytes(mut self, bytes: Option<u64>) -> Self {
        self.device_bytes = bytes;
        if bytes.is_some() {
            self.source = ResidencySource::Composite;
        }
        self
    }

    /// Attach a measured (never guessed) model-artifact byte count on local
    /// storage. The natural source is the registry's declared artifact bytes.
    pub fn with_nvme_bytes(mut self, bytes: Option<u64>) -> Self {
        self.nvme_bytes = bytes;
        if bytes.is_some() {
            self.source = ResidencySource::Composite;
        }
        self
    }

    /// Sample the current process from the host.
    ///
    /// Never fails and never panics: a platform without a usable observer
    /// yields an `Unsupported`/unknown snapshot.
    pub fn sample() -> Self {
        Self::sample_from(&|name| std::fs::read_to_string(format!("/proc/self/{name}")).ok())
    }

    /// Sample through an injectable file reader (tests, unusual mounts).
    ///
    /// The reader is asked for `/proc` basenames (`status`, `smaps_rollup`)
    /// and may return `None` for any of them; every `None` degrades to Unknown
    /// for that source, never to an error.
    pub fn sample_from(read: &dyn Fn(&str) -> Option<String>) -> Self {
        let status = read("status").map(|t| parse_proc_status(&t));
        let smaps = read("smaps_rollup").map(|t| parse_smaps_rollup(&t));

        let owned = status
            .and_then(|s| s.rss_anon_kb)
            .or_else(|| smaps.and_then(|s| s.anon_kb));
        let file = status
            .and_then(|s| s.rss_file_kb)
            .or_else(|| smaps.and_then(|s| s.file_kb));

        // Track where each observed field came from so the source label says
        // the truth instead of whichever observer happened to match first.
        let owned_from = if status.and_then(|s| s.rss_anon_kb).is_some() {
            Some(ResidencySource::ProcStatus)
        } else if smaps.and_then(|s| s.anon_kb).is_some() {
            Some(ResidencySource::ProcSmapsRollup)
        } else {
            None
        };
        let file_from = if status.and_then(|s| s.rss_file_kb).is_some() {
            Some(ResidencySource::ProcStatus)
        } else if smaps.and_then(|s| s.file_kb).is_some() {
            Some(ResidencySource::ProcSmapsRollup)
        } else {
            None
        };
        let source = match (owned_from, file_from) {
            (None, None) => ResidencySource::Unsupported,
            (Some(a), Some(b)) if a == b => a,
            _ => ResidencySource::Composite,
        };

        let mut notes = Vec::new();
        if owned.is_some() && file_from.is_none() {
            notes.push(
                "owned bytes observed but file-backed bytes were not; named field is Unknown"
                    .into(),
            );
        }
        if owned.is_none() && file.is_none() {
            notes.push("no anonymous or file-backed residency counter was readable".into());
        }
        // KiB counters are integers in /proc; convert to exact bytes. Missing
        // counters stay None (unknown), never fabricated as zero.

        Self {
            schema_version: RESIDENCY_SCHEMA_VERSION,
            timestamp: openrt_core::now().to_rfc3339(),
            owned_ram_bytes: owned.map(|kb| kb.saturating_mul(1024)),
            file_backed_bytes: file.map(|kb| kb.saturating_mul(1024)),
            device_bytes: None,
            nvme_bytes: None,
            source,
            notes,
        }
    }

    /// Known bytes as gauge samples for an existing telemetry surface.
    ///
    /// Only observed (`Some`) values appear; Unknown fields produce no gauge,
    /// so a consumer cannot mistake an unobserved value for a measured zero.
    pub fn gauge_samples(&self) -> Vec<(&'static str, f64, &'static str)> {
        let mut out = Vec::new();
        for (name, value) in [
            ("residency.owned_ram_bytes", self.owned_ram_bytes),
            ("residency.file_backed_bytes", self.file_backed_bytes),
            ("residency.device_bytes", self.device_bytes),
            ("residency.nvme_bytes", self.nvme_bytes),
        ] {
            if let Some(v) = value {
                out.push((name, v as f64, "bytes"));
            }
        }
        out
    }
}

/// Parsed owned/file-backed counters from `/proc/self/status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProcStatusMemory {
    /// `RssAnon` (kB). Anonymous pages the process owns.
    pub rss_anon_kb: Option<u64>,
    /// `RssFile` (kB). File-backed (page-cache) pages.
    pub rss_file_kb: Option<u64>,
}

/// Parse `/proc/self/status` residency fields.
///
/// Pure and resilient: a missing/renamed/distorted field is `None`, never an
/// error, never a panic. Only `RssAnon`/`RssFile` are read — VmRSS deliberately
/// is not, because it also counts reclaimable file-backed pages and would
/// present them as owned memory.
pub fn parse_proc_status(text: &str) -> ProcStatusMemory {
    let mut out = ProcStatusMemory::default();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let Some(key) = it.next() else { continue };
        let Some(value) = it.next().and_then(|v| v.parse::<u64>().ok()) else {
            continue;
        };
        match key.trim_end_matches(':') {
            "RssAnon" => out.rss_anon_kb = Some(value),
            "RssFile" => out.rss_file_kb = Some(value),
            _ => {}
        }
    }
    out
}

/// Parsed counters from `/proc/self/smaps_rollup` (fallback observer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SmapsRollupMemory {
    /// `Rss` (kB). Total resident set, all classes.
    pub rss_kb: Option<u64>,
    /// `Anonymous` (kB). Anonymous pages, including shared anon.
    pub anon_kb: Option<u64>,
    /// `File` (kB). File-backed pages.
    pub file_kb: Option<u64>,
}

/// Parse `/proc/self/smaps_rollup` residency fields.
///
/// Pure and resilient. `Anonymous` is the owned class and `File` the
/// file-backed class; both must actually be observed or the field is `None`.
pub fn parse_smaps_rollup(text: &str) -> SmapsRollupMemory {
    let mut out = SmapsRollupMemory::default();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let Some(key) = it.next() else { continue };
        let Some(value) = it.next().and_then(|v| v.parse::<u64>().ok()) else {
            continue;
        };
        match key.trim_end_matches(':') {
            "Rss" => out.rss_kb = Some(value),
            "Anonymous" => out.anon_kb = Some(value),
            "File" => out.file_kb = Some(value),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS_FIXTURE: &str = "\
Name:	openrt
VmRSS:	  1000000 kB
RssAnon:	  600000 kB
RssFile:	  250000 kB
RssShmem:	  150000 kB";

    #[test]
    fn proc_status_distinguishes_owned_from_file_backed() {
        let m = parse_proc_status(STATUS_FIXTURE);
        // RssAnon (owned) and RssFile (file-backed) must stay distinct; VmRSS
        // is deliberately ignored and must not leak into either field.
        assert_eq!(m.rss_anon_kb, Some(600_000));
        assert_eq!(m.rss_file_kb, Some(250_000));
        assert_ne!(m.rss_anon_kb, m.rss_file_kb);
    }

    #[test]
    fn proc_status_missing_fields_stay_unknown() {
        // No RssAnon/RssFile lines at all.
        let m = parse_proc_status("Name:\topenrt\nVmRSS:  7777 kB\n");
        assert_eq!(m.rss_anon_kb, None);
        assert_eq!(m.rss_file_kb, None);

        let m2 = parse_proc_status("garbage-without-numbers");
        assert_eq!(m2.rss_anon_kb, None);
        assert_eq!(m2.rss_file_kb, None);
    }

    #[test]
    fn smaps_rollup_parser_and_fallback_observer() {
        let m = parse_smaps_rollup(
            "\
Rss:     900000 kB
Pss:     450000 kB
Anonymous:   500000 kB
File:    300000 kB",
        );
        assert_eq!(m.rss_kb, Some(900_000));
        assert_eq!(m.anon_kb, Some(500_000));
        assert_eq!(m.file_kb, Some(300_000));
    }

    #[test]
    fn smaps_missing_file_field_is_unknown_not_zero() {
        let m = parse_smaps_rollup("Rss: 100 kB\nAnonymous: 99 kB\n");
        assert_eq!(m.anon_kb, Some(99));
        assert_eq!(m.file_kb, None);
    }

    #[test]
    fn snapshot_merges_status_and_reports_distinct_classes() {
        let s = ResidencySnapshot::sample_from(&|name| match name {
            "status" => Some(STATUS_FIXTURE.to_string()),
            _ => None,
        });
        assert_eq!(s.owned_ram_bytes, Some(600_000 * 1024));
        assert_eq!(s.file_backed_bytes, Some(250_000 * 1024));
        assert_ne!(s.owned_ram_bytes, s.file_backed_bytes);
        assert_eq!(s.device_bytes, None, "no VRAM observer in M1");
        assert_eq!(s.nvme_bytes, None, "no artifact observer attached");
        assert_eq!(s.source, ResidencySource::ProcStatus);
    }

    #[test]
    fn snapshot_falls_back_to_smaps_rollup() {
        let s = ResidencySnapshot::sample_from(&|name| match name {
            "status" => Some("Name:\topenrt\n".to_string()),
            "smaps_rollup" => Some("Rss: 100 kB\nAnonymous: 60 kB\nFile: 30 kB\n".to_string()),
            _ => None,
        });
        assert_eq!(s.owned_ram_bytes, Some(60 * 1024));
        assert_eq!(s.file_backed_bytes, Some(30 * 1024));
        assert_eq!(s.source, ResidencySource::ProcSmapsRollup);
    }

    #[test]
    fn unobservable_platform_yields_unknown_not_fabrication() {
        let s = ResidencySnapshot::sample_from(&|_| None);
        assert!(s.is_unknown());
        assert_eq!(s.owned_ram_bytes, None);
        assert_eq!(s.file_backed_bytes, None);
        assert_eq!(s.device_bytes, None);
        assert_eq!(s.source, ResidencySource::Unsupported);
        assert!(!s.notes.is_empty());
    }

    #[test]
    fn unknown_snapshot_is_unknown() {
        let s = ResidencySnapshot::unknown();
        assert!(s.is_unknown());
        assert_eq!(s.schema_version, RESIDENCY_SCHEMA_VERSION);
    }

    #[test]
    fn attach_only_measured_values() {
        let base = ResidencySnapshot::unknown();
        let s = base
            .with_device_bytes(Some(2_000_000_000))
            .with_nvme_bytes(Some(9_350_000_000));
        assert_eq!(s.device_bytes, Some(2_000_000_000));
        assert_eq!(s.nvme_bytes, Some(9_350_000_000));
        assert_eq!(s.source, ResidencySource::Composite);
        assert!(!s.is_unknown());
    }

    #[test]
    fn serialization_roundtrip() {
        let s = ResidencySnapshot::sample_from(&|name| match name {
            "status" => Some(STATUS_FIXTURE.to_string()),
            _ => None,
        })
        .with_device_bytes(Some(8))
        .with_nvme_bytes(Some(16));
        let json = serde_json::to_value(&s).unwrap();
        let back: ResidencySnapshot = serde_json::from_value(json).unwrap();
        assert_eq!(back, s);
        assert_eq!(back.schema_version, RESIDENCY_SCHEMA_VERSION);
    }

    #[test]
    fn serialization_of_unknown_is_explicit() {
        let s = ResidencySnapshot::unknown();
        let json = serde_json::to_value(&s).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(obj["owned_ram_bytes"], serde_json::Value::Null);
        assert_eq!(obj["source"], "unsupported");
        let back: ResidencySnapshot = serde_json::from_value(json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn gauge_samples_only_include_observed_values() {
        let s = ResidencySnapshot::sample_from(&|name| match name {
            "status" => Some(STATUS_FIXTURE.to_string()),
            _ => None,
        });
        let g = s.gauge_samples();
        let names: Vec<_> = g.iter().map(|(n, _, _)| *n).collect();
        assert_eq!(
            names,
            vec!["residency.owned_ram_bytes", "residency.file_backed_bytes"]
        );
        assert!(
            g.iter().all(|(_, _, u)| *u == "bytes"),
            "all residency gauges are byte counts"
        );
        assert!(!g.iter().any(|(n, _, _)| *n == "residency.device_bytes"));
    }

    #[test]
    fn unknown_snapshots_emit_no_gauges() {
        assert!(ResidencySnapshot::unknown().gauge_samples().is_empty());
    }

    #[test]
    fn sample_never_panics_on_host_and_device_is_unknown() {
        // Real-host smoke: even where procfs is unavailable this must not
        // panic, and M1 never fabricates a VRAM number.
        let s = ResidencySnapshot::sample();
        assert_eq!(s.schema_version, RESIDENCY_SCHEMA_VERSION);
        assert_eq!(s.device_bytes, None);
    }

    #[test]
    fn openrt_mock_environment_is_transparent() {
        // OPENRT_MOCK selects the mock model provider downstream; residency
        // sampling is orthogonal and must be identical either way. Two live
        // samples of a busy test process would legitimately differ, so compare
        // through a deterministic fixture reader instead.
        let reader = |name: &str| -> Option<String> {
            match name {
                "status" => Some(STATUS_FIXTURE.to_string()),
                _ => None,
            }
        };
        std::env::set_var("OPENRT_MOCK", "1");
        let with_mock = ResidencySnapshot::sample_from(&reader);
        std::env::remove_var("OPENRT_MOCK");
        let without_mock = ResidencySnapshot::sample_from(&reader);

        let n = |s: &ResidencySnapshot| {
            (
                s.owned_ram_bytes,
                s.file_backed_bytes,
                s.device_bytes,
                s.nvme_bytes,
                s.source,
                s.notes.clone(),
            )
        };
        assert_eq!(n(&with_mock), n(&without_mock), "mock env is transparent");
        assert_eq!(
            with_mock.device_bytes, None,
            "mock env never fabricates VRAM"
        );
        assert_eq!(with_mock.owned_ram_bytes, Some(600_000 * 1024));
        assert_eq!(with_mock.file_backed_bytes, Some(250_000 * 1024));
    }
}
