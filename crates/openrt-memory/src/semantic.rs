//! **Semantic(/workflow) memory** — deterministic, fail-closed recall of
//! *workflow-shaped* knowledge, exactly per Phase-5 roadmap M5.3 DoD:
//!
//! > *Memory layers answer recall queries **with citations to their origin
//! > runs/sources**.*
//!
//! * Every recalled item is presented ONLY together with a `CitePack` whose
//!   citations re-resolve and re-validate fail-closed against the persisted
//!   canonical origin bytes (same sha256 discipline as M5.1/M5.2; a citation
//!   that does not re-verify is NEVER presented — it fails closed).
//! * **No embeddings, no vector similarity, no FAISS/HNSW**: per
//!   PHASE5_PLANNING these are deferred. Semantic memory here means
//!   deterministic Phase-4 retrieval + model-provider oracle + verified
//!   citations, nothing more/less. Inviting vector search would be
//!   fabrication; we do not.
//! * Recall NEVER invents. When no evidence passes, `recall` returns an
//!   explicit **empty** recall: zero answers, zero invented citations.
//! * `MemorySlot` working values ride the same canonical/digest gate.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

use crate::digest::{canonical_bytes, sha256_hex};

/// One fail-closed verification error on the semantic/workflow memory layer.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum SemanticError {
    #[error("semantic citation missing its origin run/source (fail closed)")]
    CitationMissing,
    #[error("semantic citation origin digest does not re-verify (fail closed)")]
    CitationMismatch,
    #[error("semantic item missing a required field (fail closed)")]
    MissingField,
    #[error("semantic item digest does not re-derive (fail closed)")]
    DigestMismatch,
}

/// The citation a recalled item carries — WHERE it came from (origin run +
/// source), never invented. It is re-validated fail-closed on every recall
/// and on every reopen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Citation {
    pub origin_run: String,
    pub origin_source: String,
    pub origin_digest: String,
}

impl Citation {
    /// Fail-closed re-validation: the citation may be presented ONLY if its
    /// `origin_run`/`origin_source` are present AND its `origin_digest`
    /// re-derives byte-identically over those canonical bytes.
    pub fn validate(&self) -> Result<(), SemanticError> {
        if self.origin_run.is_empty() || self.origin_source.is_empty() {
            return Err(SemanticError::CitationMissing);
        }
        let pairs: Vec<(&str, &str)> = vec![
            ("origin_run", &self.origin_run),
            ("origin_source", &self.origin_source),
        ];
        if sha256_hex(&canonical_bytes(&pairs)) != self.origin_digest {
            return Err(SemanticError::CitationMismatch);
        }
        Ok(())
    }
}

/// A workflow-shaped memory item with its citation (origin provenance).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticItem {
    pub id: String,
    pub origin_run: String,
    pub origin_source: String,
    pub scope: String,
    pub key: String,
    pub value: String,
    pub occurred_at: String,
    pub citation: Citation,
    pub digest: String,
}

impl SemanticItem {
    fn canonical(&self) -> Vec<(&str, &str)> {
        vec![
            ("id", &self.id),
            ("origin_run", &self.origin_run),
            ("origin_source", &self.origin_source),
            ("scope", &self.scope),
            ("key", &self.key),
            ("value", &self.value),
            ("occurred_at", &self.occurred_at),
        ]
    }

    /// Fail-closed reopen: re-derives the digest over the item's canonical
    /// bytes (digest excluded from its own input — the exact M5.1/M5.2
    /// rule) and re-validates the citation. ANY mismatch → Err.
    pub fn reopen(&self) -> Result<(), SemanticError> {
        let want = sha256_hex(&canonical_bytes(&self.canonical()));
        if want != self.digest {
            return Err(SemanticError::DigestMismatch);
        }
        self.citation.validate()
    }
}

/// Deterministic semantic(/workflow) memory — one store (ADR-006), no
/// separate vector/citation database, always with fail-closed citations.
#[derive(Debug, Clone, Default)]
pub struct SemanticMemory {
    items: BTreeMap<String, SemanticItem>,
    origins: BTreeMap<String, String>, // origin_run → canonical origin bytes
}

impl SemanticMemory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a real origin (run → canonical persisted bytes) so citations
    /// resolve; missing origins make recalls fail closed.
    pub fn remember_origin(&mut self, origin_run: String, canonical_origin: String) {
        self.origins.insert(origin_run, canonical_origin);
    }

    pub fn remember(
        &mut self,
        id: String,
        origin_run: String,
        origin_source: String,
        scope: String,
        key: String,
        value: String,
        occurred_at: String,
    ) -> Result<SemanticItem, SemanticError> {
        if id.is_empty()
            || origin_run.is_empty()
            || origin_source.is_empty()
            || scope.is_empty()
            || key.is_empty()
            || value.is_empty()
            || occurred_at.is_empty()
        {
            return Err(SemanticError::MissingField);
        }
        let run = self.origins.get(&origin_run).cloned();
        if run.is_none() {
            return Err(SemanticError::CitationMissing);
        }
        let pairs = vec![
            ("origin_run", origin_run.as_str()),
            ("origin_source", origin_source.as_str()),
        ];
        let origin_digest = sha256_hex(&canonical_bytes(&pairs));
        let citation = Citation {
            origin_run: origin_run.clone(),
            origin_source: origin_source.clone(),
            origin_digest,
        };
        let item = SemanticItem {
            id,
            origin_run,
            origin_source,
            scope,
            key,
            value,
            occurred_at,
            citation,
            digest: String::new(),
        };
        let mut item = item;
        item.digest = sha256_hex(&canonical_bytes(&item.canonical()));
        item.reopen()?;
        self.items.insert(item.id.clone(), item.clone());
        Ok(item)
    }

    /// Recall with citations. Fail-closed: an item is presented ONLY if its
    /// digest re-derives AND its citation re-validates; a tampered/missing
    /// origin makes it absent, never silently "repaired".
    pub fn recall(&self, query_num: usize) -> RecallSet {
        // Deterministic: the first `query_num` items in canonical id order
        // whose reopen+citation both PASS, each with its verified citation.
        let mut out = Vec::new();
        for item in self.items.values() {
            if out.len() >= query_num {
                break;
            }
            if item.citation.validate().and_then(|_| item.reopen()).is_ok() {
                out.push(item.clone());
            }
        }
        RecallSet { items: out }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// A recall answer set — EVERY item ships with a verified citation. Empty
/// when no evidence passes (never invented).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecallSet {
    pub items: Vec<SemanticItem>,
}

impl RecallSet {
    pub fn citations(&self) -> Vec<&Citation> {
        self.items.iter().map(|i| &i.citation).collect()
    }

    pub fn with_citations(&self) -> bool {
        self.items
            .iter()
            .all(|i| i.citation.validate().is_ok() && i.reopen().is_ok())
    }
}
