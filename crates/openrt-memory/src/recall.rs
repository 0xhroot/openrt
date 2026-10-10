//! **Recall** — the ONE fail-closed recall oracle over the M5.3 layers.
//!
//! Contract (roadmap M5.3 DoD, exact — never weakened): *memory layers
//! answer recall queries **with citations to their origin runs/sources.***
//!
//! Fail-closed, deterministic, NEVER invented:
//! * the answer, the citation, the origin run/source, or a "verified" result
//!   whose citation digest did not re-derive byte-identically at recall time.
//! No evidence passes → the oracle returns the honest empty citation set
//! (never silently repairs, never presents tampered evidence).

use crate::digest::{canonical_digest, DigestError};
use std::collections::BTreeMap;

pub const RUNS_NS: &str = "openrt-memory/recall/origins";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryRecallKind {
    Verified,
    NoVerifiedEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRecall {
    pub origin_run: String,
    pub origin_source: String,
    pub citation_sha256: String,
    pub kind: MemoryRecallKind,
}

impl MemoryRecall {
    pub fn reopen(&self) -> Result<(), DigestError> {
        let want = canonical_digest(&[
            ("origin_run", &self.origin_run),
            ("origin_source", &self.origin_source),
        ]);
        if want == self.citation_sha256 {
            Ok(())
        } else {
            Err(DigestError::DigestMismatch)
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct MemoryRecallSet {
    items: BTreeMap<String, MemoryRecall>,
}

impl MemoryRecallSet {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn remember(&mut self, key: String, rec: MemoryRecall) {
        self.items.insert(key, rec);
    }
    pub fn recall(&self, limit: usize) -> Vec<MemoryRecall> {
        let mut out = Vec::new();
        for rec in self.items.values() {
            if out.len() >= limit {
                break;
            }
            if rec.reopen().is_ok() {
                out.push(rec.clone());
            }
        }
        out
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

pub fn recall(set: &MemoryRecallSet, limit: usize) -> Vec<MemoryRecall> {
    set.recall(limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deterministic_fail_closed() {
        let s = MemoryRecallSet::default();
        assert!(s.is_empty());
        assert!(s.recall(4).is_empty());
    }
}
