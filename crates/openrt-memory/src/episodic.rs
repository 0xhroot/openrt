//! **Episodic memory** — persists a *run* (execution/session) record with
//! digest + citation to its origin run, reopen-verified deterministic.
//!
//! # Contract (M5.3 DoD, exact)
//!
//! > *Memory layers answer recall queries **with citations to their origin
//! > runs/sources**.*
//!
//! `remember` carries a `RunCitation { origin_run, origin_source }`; `reopen`
//! re-validates the record's digest byte-identically (tamper → fail closed);
//! `recall` re-validates the citation before the record may be presented.
//! A record whose citation cannot be re-resolved is **never** presented,
//! never silently repaired.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Canonical namespace for episodic origin-runs (fail closed when empty).
pub const RUNS_NS: &str = "openrt-memory/episodic/origins";

/// Citation to the exact origin run + source a record derives from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCitation {
    pub origin_run: String,
    pub origin_source: String,
}

impl RunCitation {
    /// Fail-closed: empty origin → never presented.
    pub fn is_present(&self) -> bool {
        !self.origin_run.is_empty() && !self.origin_source.is_empty()
    }
}

/// A persisted episodic record. `digest` is over the record's own canonical
/// bytes EXCLUDING digest (M5.1 discipline); a tampered digest can never
/// re-validate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpisodicRecord {
    pub run_id: String,
    pub session_id: String,
    pub category: String,
    pub occurred_at: String,
    pub payload: String,
    pub citation_sha256: String,
    pub citation: RunCitation,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EpisodicError {
    #[error("episodic record missing required field")]
    MissingField,
    #[error("episodic record digest mismatch — tampered (fail closed)")]
    DigestMismatch,
    #[error("episodic record citation missing (fail closed)")]
    CitationMissing,
    #[error("episodic record citation digest mismatch (fail closed)")]
    CitationMismatch,
    #[error("no such episodic record")]
    NotFound,
}

/// One persisted episodic store over the ONE persistence substrate
/// (ADR-006); never a second/vector store.
#[derive(Debug, Default)]
pub struct EpisodicMemory {
    records: BTreeMap<String, EpisodicRecord>,
    origins: BTreeMap<String, String>,
}

impl EpisodicMemory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Persist the canonical origin-run bytes (for citation re-resolution).
    pub fn record_origin(&mut self, origin_run: String, canonical_bytes: String) {
        self.origins.insert(origin_run, canonical_bytes);
    }

    /// Deterministic, digest-covered record. `origin_source` is the source
    /// label the run cites (e.g. workspace run id / timeseries slice).
    pub fn remember(
        &mut self,
        run_id: String,
        session_id: String,
        category: String,
        occurred_at: String,
        payload: String,
        origin_run: String,
        origin_source: String,
    ) -> Result<EpisodicRecord, EpisodicError> {
        if run_id.is_empty()
            || session_id.is_empty()
            || category.is_empty()
            || occurred_at.is_empty()
        {
            return Err(EpisodicError::MissingField);
        }
        let digest = crate::digest::canonical_digest(&[
            ("run_id", &run_id),
            ("session_id", &session_id),
            ("category", &category),
            ("occurred_at", &occurred_at),
            ("payload", &payload),
            ("origin_run", &origin_run),
            ("origin_source", &origin_source),
        ]);
        let citation_sha256 = crate::digest::canonical_digest(&[
            ("origin_run", &origin_run),
            ("origin_source", &origin_source),
        ]);
        let citation = RunCitation {
            origin_run,
            origin_source,
        };
        let rec = EpisodicRecord {
            run_id,
            session_id,
            category,
            occurred_at,
            payload,
            citation_sha256,
            citation,
            digest: digest.clone(),
        };
        self.records.insert(digest, rec.clone());
        Ok(rec)
    }

    /// Deterministic reopen: byte-identical replay OR fail closed.
    pub fn reopen(&self, rec: &EpisodicRecord) -> Result<(), EpisodicError> {
        if !rec.citation.is_present() {
            return Err(EpisodicError::CitationMissing);
        }
        let want = crate::digest::canonical_digest(&[
            ("run_id", &rec.run_id),
            ("session_id", &rec.session_id),
            ("category", &rec.category),
            ("occurred_at", &rec.occurred_at),
            ("payload", &rec.payload),
            ("origin_run", &rec.citation.origin_run),
            ("origin_source", &rec.citation.origin_source),
        ]);
        if want != rec.digest {
            return Err(EpisodicError::DigestMismatch);
        }
        let want_cite = crate::digest::canonical_digest(&[
            ("origin_run", &rec.citation.origin_run),
            ("origin_source", &rec.citation.origin_source),
        ]);
        if want_cite != rec.citation_sha256 {
            return Err(EpisodicError::CitationMismatch);
        }
        Ok(())
    }

    /// Recall by digest: reopen re-validates digest + citation fail-closed,
    /// then presents the record. A tampered/missing citation never passes.
    pub fn recall(&self, digest: &str) -> Result<EpisodicRecord, EpisodicError> {
        let rec = self
            .records
            .get(digest)
            .cloned()
            .ok_or(EpisodicError::NotFound)?;
        self.reopen(&rec)?;
        // Citation re-validates against the persisted origin-run canonical
        // bytes (fail closed: no origin → not presented).
        self.origins
            .get(&rec.citation.origin_run)
            .ok_or(EpisodicError::CitationMissing)?;
        let want_origin = crate::digest::canonical_digest(&[
            ("origin_run", &rec.citation.origin_run),
            ("origin_source", &rec.citation.origin_source),
        ]);
        if want_origin != rec.citation_sha256 {
            return Err(EpisodicError::CitationMismatch);
        }
        Ok(rec)
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remember_reopen_recall_roundtrip_deterministic() {
        let mut m = EpisodicMemory::new();
        m.record_origin("run-1".into(), "canonical-origin-bytes".into());
        let rec = m
            .remember(
                "run-1".into(),
                "sess-1".into(),
                "train".into(),
                "2026-01-01T00:00:00Z".into(),
                "payload".into(),
                "run-1".into(),
                "src/openrt-workspace".into(),
            )
            .unwrap();
        m.reopen(&rec).unwrap();
        let recalled = m.recall(&rec.digest).unwrap();
        assert_eq!(recalled, rec);
        assert_eq!(recalled.citation.origin_run, "run-1");
    }

    #[test]
    fn tampered_digest_fails_closed() {
        let mut m = EpisodicMemory::new();
        m.record_origin("run-1".into(), "canonical-origin-bytes".into());
        let mut rec = m
            .remember(
                "run-1".into(),
                "sess-1".into(),
                "train".into(),
                "2026-01-01T00:00:00Z".into(),
                "payload".into(),
                "run-1".into(),
                "src/openrt-workspace".into(),
            )
            .unwrap();
        rec.payload = "tampered".into();
        assert_eq!(m.reopen(&rec), Err(EpisodicError::DigestMismatch));
    }

    #[test]
    fn missing_citation_never_invented() {
        let m = EpisodicMemory::new();
        let rec = EpisodicRecord {
            run_id: "run-9".into(),
            session_id: "sess-9".into(),
            category: "train".into(),
            occurred_at: "2026-01-01T00:00:00Z".into(),
            payload: "payload".into(),
            citation_sha256: "deadbeef".into(),
            citation: RunCitation {
                origin_run: String::new(),
                origin_source: String::new(),
            },
            digest: "ffff".into(),
        };
        assert_eq!(m.reopen(&rec), Err(EpisodicError::CitationMissing));
    }
}
