//! Local-storage observation for the Memory Fabric (M10).
//!
//! M1 fed the planner `nvme_bytes` as a **registry-declared proxy** — the
//! declared artifact size, which is metadata about where the bytes *should*
//! be. M10 adds the first-class alternative: a **direct read** of the local
//! filesystem when an artifact is actually installed there (`stat`, metadata
//! only — never a content read, never a guess).
//!
//! The two sources are kept distinct because they answer different questions:
//!
//! * [`StorageObservationSource::RegistryDeclared`] — "how big did the
//!   registry say this artifact is?" (provenance, cheap, always available),
//! * [`StorageObservationSource::DirectStat`] — "how many bytes are actually
//!   on this disk right now?" (truth on disk; unavailable when the file is
//!   missing, unreadable, or lives on another host).
//!
//! Honesty rules
//! -------------
//! * A failed `stat` degrades to [`StorageObservation::unavailable`] with a
//!   reason. It never returns zero and never reports a registry number as if
//!   it had been measured.
//! * An artifact the operator keeps elsewhere (a remote object store, another
//!   host) simply has no direct observation — the caller may fall back to the
//!   declared proxy explicitly, and the plan labels which source it used.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Where an NVMe/artifact byte observation came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageObservationSource {
    /// The registry's declared artifact size (metadata proxy).
    RegistryDeclared,
    /// A direct `stat` of a file present on local storage (M10).
    DirectStat,
}

/// A point-in-time answer to "how many artifact bytes are on local storage?".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageObservation {
    pub source: StorageObservationSource,
    /// Measured/declared bytes, or `None` when the observation failed.
    pub size_bytes: Option<u64>,
    /// Local path that was statted, when a direct read was attempted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl StorageObservation {
    /// The honest "nothing measured" state.
    pub fn unavailable(why: impl Into<String>) -> Self {
        Self {
            source: StorageObservationSource::RegistryDeclared,
            size_bytes: None,
            path: None,
            notes: vec![why.into()],
        }
    }

    /// Whether a real byte count is present.
    pub fn is_known(&self) -> bool {
        self.size_bytes.is_some()
    }
}

/// Read a file's size directly from the local filesystem (metadata only).
///
/// Returns `None` for any failure — missing file, unreadable, not a regular
/// file. It never returns a fabricated zero and never returns the registry's
/// declared number pretending to be a measurement.
pub fn stat_size_bytes(path: &Path) -> Option<u64> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    Some(metadata.len())
}

/// Directly observe one installed artifact file.
///
/// * `path == Some(p)` — the path is statted; success yields a direct
///   measured byte count, failure explains why.
/// * `path == None` — the caller (e.g. the runtime planner) has no local
///   path to observe; this call honestly reports nothing.
pub fn observe_local_artifact(path: Option<&Path>) -> StorageObservation {
    let Some(path) = path else {
        return StorageObservation::unavailable(
            "no local artifact path to observe; only the declared proxy is available",
        );
    };
    match stat_size_bytes(path) {
        Some(bytes) => StorageObservation {
            source: StorageObservationSource::DirectStat,
            size_bytes: Some(bytes),
            path: Some(path.display().to_string()),
            notes: vec!["bytes measured directly from local storage (stat)".into()],
        },
        None => StorageObservation::unavailable(format!(
            "{} is not a readable regular file on this host",
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_temp_file_statts_to_its_real_size() {
        let dir = std::env::temp_dir();
        let mut path = dir.join(format!("openrt-m10-{}", std::process::id()));
        path.set_extension("bin");
        std::fs::write(&path, vec![0u8; 4093]).unwrap();
        let bytes = stat_size_bytes(&path);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(bytes, Some(4093), "direct stat must match the real length");
    }

    #[test]
    fn missing_file_is_none_not_zero() {
        assert_eq!(
            stat_size_bytes(Path::new("/definitely/not/here-openrt-m10.bin")),
            None
        );
    }

    #[test]
    fn directory_is_not_an_artifact_bytes_measurement() {
        assert_eq!(stat_size_bytes(Path::new("/tmp")), None);
    }

    #[test]
    fn local_observation_labelled_direct() {
        let dir = std::env::temp_dir();
        let path = dir.join("openrt-m10-direct.bin");
        std::fs::write(&path, vec![7u8; 64]).unwrap();
        let obs = observe_local_artifact(Some(&path));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(obs.source, StorageObservationSource::DirectStat);
        assert_eq!(obs.size_bytes, Some(64));
        assert_eq!(obs.path.as_deref(), Some(path.to_str().unwrap()));
    }

    #[test]
    fn no_path_is_honestly_unavailable() {
        let obs = observe_local_artifact(None);
        assert_eq!(obs.size_bytes, None);
        assert!(!obs.notes.is_empty());
        assert!(!obs.is_known());
    }

    #[test]
    fn observation_round_trips() {
        let obs = observe_local_artifact(None);
        let json = serde_json::to_string(&obs).unwrap();
        let back: StorageObservation = serde_json::from_str(&json).unwrap();
        assert_eq!(back, obs);
    }
}
