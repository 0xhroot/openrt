//! Append-only evidence graph: content-addressed claims, grounding edges
//! (claim → passage → document) with a per-KB hash chain.
//!
//! Append-only is enforced at THIS layer (the raw store's `put_record`
//! silently overwrites): every append first proves the slot is free
//! (`AppendViolation` otherwise), writes, then verifies the round-trip
//! read-back before returning `Ok`.

use openrt_core::{canonical_pair_digest, sha256_hex};
use openrt_knowledge::{safe_key, Citation};
use openrt_persistence::PersistenceStore;
use serde::{Deserialize, Serialize};

use crate::{map_knowledge, EvidenceError, EVIDENCE_NS};

/// A claim recorded against a KB. Content-addressed: `claim_id` is
/// `sha256(text)`, so recording identical text twice is idempotent and any
/// tampering with `text` fails the hash check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimRecord {
    pub kb_id: String,
    pub claim_id: String,
    pub text: String,
    pub text_sha256: String,
    pub created_at: String,
}

impl ClaimRecord {
    pub(crate) fn claim_id_for(text: &str) -> String {
        sha256_hex(text.as_bytes())
    }

    pub(crate) fn key(kb_id: &str, claim_id: &str) -> String {
        format!("{}.claim.{}", safe_key(kb_id), claim_id)
    }
}

/// The two edge kinds of the evidence triangle. The citation itself carries
/// the passage ↔ document side; `PassageExtractedFrom` records it explicitly
/// as the companion of each grounding edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    ClaimGroundedIn,
    PassageExtractedFrom,
}

impl EdgeKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            EdgeKind::ClaimGroundedIn => "claim_grounded_in",
            EdgeKind::PassageExtractedFrom => "passage_extracted_from",
        }
    }
}

/// One append-only edge. `edge_hash = sha256` over the canonical field pairs
/// (including `prev_hash`, excluding `edge_hash` itself); `prev_hash` links
/// each edge to its predecessor in the per-KB chain (`""` for genesis).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceEdge {
    pub kb_id: String,
    pub kind: EdgeKind,
    /// `Some` for `ClaimGroundedIn`, `None` for the passage companion.
    pub claim_id: Option<String>,
    pub citation: Citation,
    pub seq: u64,
    pub created_at: String,
    pub prev_hash: String,
    pub edge_hash: String,
}

impl EvidenceEdge {
    pub(crate) fn key(kb_id: &str, seq: u64) -> String {
        format!("{}.edge.{:012}", safe_key(kb_id), seq)
    }

    pub(crate) fn compute_hash(&self) -> String {
        canonical_pair_digest(&[
            ("kb_id".into(), self.kb_id.clone()),
            ("kind".into(), self.kind.as_str().into()),
            ("claim_id".into(), self.claim_id.clone().unwrap_or_default()),
            ("citation_kb_id".into(), self.citation.kb_id.clone()),
            ("document_id".into(), self.citation.document_id.clone()),
            (
                "document_version".into(),
                self.citation.document_version.to_string(),
            ),
            ("chunk_id".into(), self.citation.chunk_id.clone()),
            ("block_keys".into(), self.citation.block_keys.join("\u{1f}")),
            (
                "page".into(),
                self.citation
                    .page
                    .map(|p| p.to_string())
                    .unwrap_or_default(),
            ),
            ("text_sha256".into(), self.citation.text_sha256.clone()),
            ("seq".into(), self.seq.to_string()),
            ("created_at".into(), self.created_at.clone()),
            ("prev_hash".into(), self.prev_hash.clone()),
        ])
    }
}

/// Result of [`verify_integrity`]. `intact()` is true only when no problem
/// was found — never assumed, always checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrityReport {
    pub edges_checked: usize,
    pub claims_checked: usize,
    pub problems: Vec<String>,
}

impl IntegrityReport {
    pub fn intact(&self) -> bool {
        self.problems.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordClass {
    Claim,
    Edge(u64),
}

fn classify(rest: &str) -> Option<RecordClass> {
    if let Some(hex) = rest.strip_prefix("claim.") {
        if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Some(RecordClass::Claim);
        }
        return None;
    }
    if let Some(digits) = rest.strip_prefix("edge.") {
        if digits.len() == 12 && digits.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(seq) = digits.parse::<u64>() {
                return Some(RecordClass::Edge(seq));
            }
        }
    }
    None
}

async fn scan_raw(
    store: &dyn PersistenceStore,
    kb_id: &str,
) -> Result<Vec<(String, RecordClass, serde_json::Value)>, EvidenceError> {
    let prefix = format!("{}.", safe_key(kb_id));
    let mut out = Vec::new();
    for (key, value) in store.list_records(EVIDENCE_NS).await? {
        let Some(rest) = key.strip_prefix(prefix.as_str()) else {
            continue;
        };
        let Some(class) = classify(rest) else {
            continue;
        };
        match value.get("kb_id").and_then(|v| v.as_str()) {
            Some(field) if field == kb_id => out.push((key, class, value)),
            Some(_) => {}
            None => out.push((key, class, value)),
        }
    }
    Ok(out)
}

fn parse_claim(key: &str, value: serde_json::Value) -> Result<ClaimRecord, EvidenceError> {
    serde_json::from_value(value)
        .map_err(|e| EvidenceError::Corrupt(key.to_string(), e.to_string()))
}

fn parse_edge(
    key: &str,
    class: RecordClass,
    value: serde_json::Value,
) -> Result<EvidenceEdge, EvidenceError> {
    let RecordClass::Edge(key_seq) = class else {
        return Err(EvidenceError::Corrupt(
            key.to_string(),
            "expected edge record".into(),
        ));
    };
    let edge: EvidenceEdge = serde_json::from_value(value)
        .map_err(|e| EvidenceError::Corrupt(key.to_string(), e.to_string()))?;
    if edge.seq != key_seq {
        return Err(EvidenceError::Corrupt(
            key.to_string(),
            format!("record seq {} does not match key seq {key_seq}", edge.seq),
        ));
    }
    Ok(edge)
}

/// List a KB's claim records (sorted by `claim_id`). Malformed records fail
/// closed (`Corrupt`); records whose key shares our prefix but belong to
/// another KB are skipped.
pub async fn list_claims(
    store: &dyn PersistenceStore,
    kb_id: &str,
) -> Result<Vec<ClaimRecord>, EvidenceError> {
    let mut claims = Vec::new();
    for (key, class, value) in scan_raw(store, kb_id).await? {
        if class == RecordClass::Claim {
            claims.push(parse_claim(&key, value)?);
        }
    }
    claims.sort_by(|a, b| a.claim_id.cmp(&b.claim_id));
    Ok(claims)
}

/// List a KB's edges in chain order (`seq` ascending). Malformed records
/// fail closed (`Corrupt`).
pub async fn list_edges(
    store: &dyn PersistenceStore,
    kb_id: &str,
) -> Result<Vec<EvidenceEdge>, EvidenceError> {
    let mut edges = Vec::new();
    for (key, class, value) in scan_raw(store, kb_id).await? {
        if let RecordClass::Edge(_) = class {
            edges.push(parse_edge(&key, class, value)?);
        }
    }
    edges.sort_by_key(|e| e.seq);
    Ok(edges)
}

/// Record a claim. Idempotent: the same text maps to the same `claim_id` and
/// key, so re-recording returns the existing record without a new write.
/// An existing slot whose content does NOT match its content-addressed key
/// is an `AppendViolation` (tamper), never silently overwritten.
pub async fn record_claim(
    store: &dyn PersistenceStore,
    kb_id: &str,
    text: &str,
) -> Result<ClaimRecord, EvidenceError> {
    let claim_id = ClaimRecord::claim_id_for(text);
    let key = ClaimRecord::key(kb_id, &claim_id);
    if let Some(value) = store.get_record(EVIDENCE_NS, &key).await? {
        let existing = parse_claim(&key, value)?;
        if existing.claim_id != claim_id
            || existing.text != text
            || existing.kb_id != kb_id
            || existing.text_sha256 != claim_id
        {
            return Err(EvidenceError::AppendViolation(
                key,
                "existing claim does not match its content-addressed key".into(),
            ));
        }
        return Ok(existing);
    }
    let claim = ClaimRecord {
        kb_id: kb_id.to_string(),
        claim_id,
        text: text.to_string(),
        text_sha256: ClaimRecord::claim_id_for(text),
        created_at: openrt_core::now().to_rfc3339(),
    };
    let value = serde_json::to_value(&claim)
        .map_err(|e| EvidenceError::Serde(format!("serialize claim `{key}`: {e}")))?;
    store.put_record(EVIDENCE_NS, &key, &value).await?;
    let back = store
        .get_record(EVIDENCE_NS, &key)
        .await?
        .ok_or_else(|| EvidenceError::AppendViolation(key.clone(), "read-back missing".into()))?;
    let verified: ClaimRecord = serde_json::from_value(back)
        .map_err(|e| EvidenceError::Corrupt(key.clone(), e.to_string()))?;
    if verified != claim {
        return Err(EvidenceError::AppendViolation(
            key,
            "read-back does not match written claim".into(),
        ));
    }
    Ok(claim)
}

async fn append_edge(
    store: &dyn PersistenceStore,
    kb_id: &str,
    kind: EdgeKind,
    claim_id: Option<String>,
    citation: &Citation,
) -> Result<EvidenceEdge, EvidenceError> {
    // Sequence allocation is a read-modify-write over the edge list: hold
    // the store's exclusive section so concurrent appenders (tasks or
    // processes) can't race on `next_seq` / `prev_hash` and fork the chain.
    let _section = store.lock_exclusive().await?;
    let existing = list_edges(store, kb_id).await?;
    let next_seq = existing.iter().map(|e| e.seq).max().map_or(0, |m| m + 1);
    let prev_hash = existing
        .last()
        .map_or_else(String::new, |e| e.edge_hash.clone());
    let key = EvidenceEdge::key(kb_id, next_seq);
    if store.get_record(EVIDENCE_NS, &key).await?.is_some() {
        return Err(EvidenceError::AppendViolation(
            key,
            "edge slot already occupied".into(),
        ));
    }
    let mut edge = EvidenceEdge {
        kb_id: kb_id.to_string(),
        kind,
        claim_id,
        citation: citation.clone(),
        seq: next_seq,
        created_at: openrt_core::now().to_rfc3339(),
        prev_hash,
        edge_hash: String::new(),
    };
    edge.edge_hash = edge.compute_hash();
    let value = serde_json::to_value(&edge)
        .map_err(|e| EvidenceError::Serde(format!("serialize edge `{key}`: {e}")))?;
    store.put_record(EVIDENCE_NS, &key, &value).await?;
    let back = store
        .get_record(EVIDENCE_NS, &key)
        .await?
        .ok_or_else(|| EvidenceError::AppendViolation(key.clone(), "read-back missing".into()))?;
    let verified = parse_edge(&key, RecordClass::Edge(next_seq), back)?;
    if verified != edge {
        return Err(EvidenceError::AppendViolation(
            key,
            "read-back does not match written edge".into(),
        ));
    }
    Ok(edge)
}

fn fmt_citation(c: &Citation) -> String {
    format!("{}#{}", c.document_id, c.chunk_id)
}

/// Append the grounding triangle for `claim_id` + `citation`: a
/// `ClaimGroundedIn` edge plus its `PassageExtractedFrom` companion.
///
/// Fail-closed gates, in order: citation `kb_id` must match; the citation
/// must re-resolve against persisted `corpus.chunks`
/// (`Citation::validate == Ok(true)` — knowledge stays THE citation
/// authority); the claim must exist. Re-linking the same
/// (claim, citation) is idempotent — the existing edge is returned and a
/// missing companion is repaired. Returns the `ClaimGroundedIn` edge.
pub async fn link_claim(
    store: &dyn PersistenceStore,
    kb_id: &str,
    claim_id: &str,
    citation: &Citation,
) -> Result<EvidenceEdge, EvidenceError> {
    if citation.kb_id != kb_id {
        return Err(EvidenceError::Invalid(format!(
            "citation kb `{}` does not match link kb `{kb_id}`",
            citation.kb_id
        )));
    }
    if !citation.validate(store).await.map_err(map_knowledge)? {
        return Err(EvidenceError::UnresolvedCitation(fmt_citation(citation)));
    }
    let claim_key = ClaimRecord::key(kb_id, claim_id);
    if store.get_record(EVIDENCE_NS, &claim_key).await?.is_none() {
        return Err(EvidenceError::NotFound(claim_key));
    }
    let existing = list_edges(store, kb_id).await?;
    let has_companion = |edges: &[EvidenceEdge]| {
        edges.iter().any(|e| {
            e.kind == EdgeKind::PassageExtractedFrom
                && e.claim_id.is_none()
                && e.citation == *citation
        })
    };
    if let Some(found) = existing.iter().find(|e| {
        e.kind == EdgeKind::ClaimGroundedIn
            && e.claim_id.as_deref() == Some(claim_id)
            && e.citation == *citation
    }) {
        if !has_companion(&existing) {
            append_edge(store, kb_id, EdgeKind::PassageExtractedFrom, None, citation).await?;
        }
        return Ok(found.clone());
    }
    let edge = append_edge(
        store,
        kb_id,
        EdgeKind::ClaimGroundedIn,
        Some(claim_id.to_string()),
        citation,
    )
    .await?;
    if !has_companion(&existing) {
        append_edge(store, kb_id, EdgeKind::PassageExtractedFrom, None, citation).await?;
    }
    Ok(edge)
}

/// Resolve a citation against the evidence graph. The citation must first
/// re-resolve against persisted `corpus.chunks` (`Ok(true)`), otherwise
/// `UnresolvedCitation` — evidence never papering over an unverified
/// citation. Returns every edge referencing the citation.
pub async fn resolve_citation(
    store: &dyn PersistenceStore,
    kb_id: &str,
    citation: &Citation,
) -> Result<Vec<EvidenceEdge>, EvidenceError> {
    if citation.kb_id != kb_id {
        return Err(EvidenceError::Invalid(format!(
            "citation kb `{}` does not match `{kb_id}`",
            citation.kb_id
        )));
    }
    if !citation.validate(store).await.map_err(map_knowledge)? {
        return Err(EvidenceError::UnresolvedCitation(fmt_citation(citation)));
    }
    Ok(list_edges(store, kb_id)
        .await?
        .into_iter()
        .filter(|e| e.citation == *citation)
        .collect())
}

/// All `ClaimGroundedIn` edges for one claim.
pub async fn evidence_for_claim(
    store: &dyn PersistenceStore,
    kb_id: &str,
    claim_id: &str,
) -> Result<Vec<EvidenceEdge>, EvidenceError> {
    Ok(list_edges(store, kb_id)
        .await?
        .into_iter()
        .filter(|e| e.kind == EdgeKind::ClaimGroundedIn && e.claim_id.as_deref() == Some(claim_id))
        .collect())
}

/// The passages a claim is grounded in (deduplicated citations).
pub async fn passages_for_claim(
    store: &dyn PersistenceStore,
    kb_id: &str,
    claim_id: &str,
) -> Result<Vec<Citation>, EvidenceError> {
    let mut out: Vec<Citation> = Vec::new();
    for e in evidence_for_claim(store, kb_id, claim_id).await? {
        if !out.contains(&e.citation) {
            out.push(e.citation);
        }
    }
    Ok(out)
}

/// Distinct claim ids grounded in any passage of `document_id` (sorted).
pub async fn claims_for_document(
    store: &dyn PersistenceStore,
    kb_id: &str,
    document_id: &str,
) -> Result<Vec<String>, EvidenceError> {
    let mut ids: Vec<String> = Vec::new();
    for e in list_edges(store, kb_id).await? {
        if e.kind != EdgeKind::ClaimGroundedIn || e.citation.document_id != document_id {
            continue;
        }
        if let Some(id) = e.claim_id {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids.sort();
    Ok(ids)
}

/// Verify the KB's evidence chain end to end:
///
/// * claim records hash-consistent (`claim_id == text_sha256 == sha256(text)`)
///   and matching their keys;
/// * edge keys ↔ record `seq`, `edge_hash` recomputes, `prev_hash` linkage
///   and contiguous `seq` from 0 (gaps = deletion);
/// * every `ClaimGroundedIn` has its `PassageExtractedFrom` companion and
///   vice versa (no orphans);
/// * every grounding edge's citation still re-resolves against persisted
///   `corpus.chunks`, and its claim record still exists.
///
/// Store failures propagate as `Err`; record-level tampering is collected
/// as `problems` (never silently ignored, never a false `intact()`).
pub async fn verify_integrity(
    store: &dyn PersistenceStore,
    kb_id: &str,
) -> Result<IntegrityReport, EvidenceError> {
    let mut problems: Vec<String> = Vec::new();
    let mut claims_checked = 0usize;
    let mut claims: Vec<ClaimRecord> = Vec::new();
    let mut edges_checked = 0usize;
    let mut edges: Vec<EvidenceEdge> = Vec::new();
    for (key, class, value) in scan_raw(store, kb_id).await? {
        match class {
            RecordClass::Claim => {
                claims_checked += 1;
                match parse_claim(&key, value) {
                    Ok(c) => claims.push(c),
                    Err(e) => problems.push(format!("unreadable claim: {e}")),
                }
            }
            RecordClass::Edge(_) => {
                edges_checked += 1;
                match parse_edge(&key, class, value) {
                    Ok(e) => edges.push(e),
                    Err(e) => problems.push(format!("unreadable edge: {e}")),
                }
            }
        }
    }
    edges.sort_by_key(|e| e.seq);

    let mut seen_claim_ids: Vec<String> = Vec::new();
    for c in &claims {
        let recomputed = ClaimRecord::claim_id_for(&c.text);
        if c.claim_id != recomputed || c.text_sha256 != recomputed {
            problems.push(format!(
                "claim `{}` content hash mismatch (text was altered)",
                c.claim_id
            ));
        }
        let key = ClaimRecord::key(kb_id, &c.claim_id);
        let suffix = key.rsplit(".claim.").next().unwrap_or_default();
        if suffix != c.claim_id {
            problems.push(format!(
                "claim record key does not match claim_id `{}`",
                c.claim_id
            ));
        }
        if seen_claim_ids.contains(&c.claim_id) {
            problems.push(format!("duplicate claim record `{}`", c.claim_id));
        }
        seen_claim_ids.push(c.claim_id.clone());
    }

    for (i, e) in edges.iter().enumerate() {
        if e.seq != i as u64 {
            problems.push(format!(
                "edge sequence gap: expected seq {i}, found {}",
                e.seq
            ));
        }
        let expected_prev = if i == 0 {
            String::new()
        } else {
            edges[i - 1].edge_hash.clone()
        };
        if e.prev_hash != expected_prev {
            problems.push(format!(
                "edge seq {} prev_hash linkage broken (chain tampered or reordered)",
                e.seq
            ));
        }
        let recomputed = e.compute_hash();
        if recomputed != e.edge_hash {
            problems.push(format!(
                "edge seq {} hash mismatch (record was modified)",
                e.seq
            ));
        }
    }

    for e in edges.iter().filter(|e| e.kind == EdgeKind::ClaimGroundedIn) {
        match &e.claim_id {
            None => problems.push(format!("grounding edge seq {} has no claim_id", e.seq)),
            Some(id) if !seen_claim_ids.contains(id) => problems.push(format!(
                "grounding edge seq {} references missing claim `{id}`",
                e.seq
            )),
            Some(_) => {}
        }
        match e.citation.validate(store).await {
            Ok(true) => {}
            Ok(false) => problems.push(format!(
                "grounding edge seq {} citation no longer resolves against persisted evidence",
                e.seq
            )),
            Err(ke) => return Err(map_knowledge(ke)),
        }
    }

    let grounded: Vec<&Citation> = edges
        .iter()
        .filter(|e| e.kind == EdgeKind::ClaimGroundedIn)
        .map(|e| &e.citation)
        .collect();
    let companions: Vec<&Citation> = edges
        .iter()
        .filter(|e| e.kind == EdgeKind::PassageExtractedFrom)
        .map(|e| &e.citation)
        .collect();
    for c in &grounded {
        if !companions.iter().any(|x| x == c) {
            problems.push(format!(
                "missing PassageExtractedFrom companion for citation {}",
                fmt_citation(c)
            ));
        }
    }
    for c in &companions {
        if !grounded.iter().any(|x| x == c) {
            problems.push(format!(
                "orphan PassageExtractedFrom edge for citation {}",
                fmt_citation(c)
            ));
        }
    }

    Ok(IntegrityReport {
        edges_checked,
        claims_checked,
        problems,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_knowledge::{persist_chunk, PersistedChunk};
    use openrt_persistence::{MemoryStore, PersistenceStore};

    fn store() -> std::sync::Arc<dyn PersistenceStore> {
        std::sync::Arc::new(MemoryStore::new())
    }

    fn citation(kb: &str, doc: &str, chunk: &str) -> Citation {
        Citation {
            kb_id: kb.into(),
            document_id: doc.into(),
            document_version: 1,
            chunk_id: chunk.into(),
            block_keys: vec!["b0".into()],
            page: Some(1),
            text_sha256: openrt_core::sha256_hex(b"real chunk text"),
        }
    }

    async fn persist_on(
        store: &dyn PersistenceStore,
        kb: &str,
        doc: &str,
        chunk: &str,
    ) -> Citation {
        let cit = citation(kb, doc, chunk);
        let rec = PersistedChunk {
            chunk_id: cit.chunk_id.clone(),
            document_id: cit.document_id.clone(),
            document_version: cit.document_version,
            block_keys: cit.block_keys.clone(),
            page: cit.page,
            text_sha256: cit.text_sha256.clone(),
            text: Some("real chunk text".into()),
        };
        persist_chunk(store, kb, &rec).await.unwrap();
        cit
    }

    #[tokio::test]
    async fn claim_record_is_content_addressed_and_idempotent() {
        let s = store();
        let a = record_claim(&*s, "kb1", "the launch window is dawn")
            .await
            .unwrap();
        let b = record_claim(&*s, "kb1", "the launch window is dawn")
            .await
            .unwrap();
        assert_eq!(a, b);
        assert_eq!(a.claim_id, a.text_sha256);
        assert_eq!(a.claim_id.len(), 64);
        let claims = list_claims(&*s, "kb1").await.unwrap();
        assert_eq!(claims.len(), 1);
    }

    #[tokio::test]
    async fn link_requires_persisted_citation_and_existing_claim() {
        let s = store();
        let cit = citation("kb1", "doc1", "c1");
        let claim = record_claim(&*s, "kb1", "grounded").await.unwrap();
        // citation never persisted -> fail closed
        let err = link_claim(&*s, "kb1", &claim.claim_id, &cit)
            .await
            .unwrap_err();
        assert!(matches!(err, EvidenceError::UnresolvedCitation(_)));
        // persist it, now link with a claim that does not exist -> NotFound
        persist_on(&*s, "kb1", "doc1", "c1").await;
        let err = link_claim(&*s, "kb1", "deadbeef", &cit).await.unwrap_err();
        assert!(matches!(err, EvidenceError::NotFound(_)));
        // happy path
        let edge = link_claim(&*s, "kb1", &claim.claim_id, &cit).await.unwrap();
        assert_eq!(edge.kind, EdgeKind::ClaimGroundedIn);
        assert_eq!(edge.seq, 0);
        let edges = list_edges(&*s, "kb1").await.unwrap();
        assert_eq!(edges.len(), 2); // grounding + companion
        assert_eq!(edges[1].kind, EdgeKind::PassageExtractedFrom);
        assert_eq!(edges[1].prev_hash, edges[0].edge_hash);
    }

    #[tokio::test]
    async fn relink_is_idempotent_and_repairs_missing_companion() {
        let s = store();
        let claim = record_claim(&*s, "kb1", "stable claim").await.unwrap();
        let cit = persist_on(&*s, "kb1", "doc1", "c1").await;
        let first = link_claim(&*s, "kb1", &claim.claim_id, &cit).await.unwrap();
        let second = link_claim(&*s, "kb1", &claim.claim_id, &cit).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(list_edges(&*s, "kb1").await.unwrap().len(), 2);
        // delete the companion, then relink -> repaired, no duplicate grounding
        let edges = list_edges(&*s, "kb1").await.unwrap();
        let companion_key = EvidenceEdge::key("kb1", edges[1].seq);
        s.delete_record(EVIDENCE_NS, &companion_key).await.unwrap();
        let third = link_claim(&*s, "kb1", &claim.claim_id, &cit).await.unwrap();
        assert_eq!(third.seq, 0);
        let edges = list_edges(&*s, "kb1").await.unwrap();
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[1].kind, EdgeKind::PassageExtractedFrom);
    }

    #[tokio::test]
    async fn verify_detects_edge_tamper_and_deletion() {
        let s = store();
        let claim = record_claim(&*s, "kb1", "claim A").await.unwrap();
        let cit_a = persist_on(&*s, "kb1", "doc1", "c1").await;
        link_claim(&*s, "kb1", &claim.claim_id, &cit_a)
            .await
            .unwrap();
        let claim2 = record_claim(&*s, "kb1", "claim B").await.unwrap();
        let cit_b = persist_on(&*s, "kb1", "doc2", "c2").await;
        link_claim(&*s, "kb1", &claim2.claim_id, &cit_b)
            .await
            .unwrap();
        // clean chain is intact
        let report = verify_integrity(&*s, "kb1").await.unwrap();
        assert!(report.intact(), "problems: {:?}", report.problems);
        assert_eq!(report.claims_checked, 2);
        assert_eq!(report.edges_checked, 4);
        // tamper: rewrite an edge's created_at in place
        let edges = list_edges(&*s, "kb1").await.unwrap();
        let mut victim = edges[1].clone();
        victim.created_at = "1999-01-01T00:00:00+00:00".into();
        let key = EvidenceEdge::key("kb1", victim.seq);
        let value = serde_json::to_value(&victim).unwrap();
        s.put_record(EVIDENCE_NS, &key, &value).await.unwrap();
        let report = verify_integrity(&*s, "kb1").await.unwrap();
        assert!(!report.intact());
        assert!(report.problems.iter().any(|p| p.contains("hash mismatch")));
        // restore by writing the original back, then delete edge seq 2 (gap)
        let value = serde_json::to_value(&edges[1]).unwrap();
        s.put_record(EVIDENCE_NS, &key, &value).await.unwrap();
        assert!(verify_integrity(&*s, "kb1").await.unwrap().intact());
        let gap_key = EvidenceEdge::key("kb1", edges[2].seq);
        s.delete_record(EVIDENCE_NS, &gap_key).await.unwrap();
        let report = verify_integrity(&*s, "kb1").await.unwrap();
        assert!(!report.intact());
        assert!(
            report.problems.iter().any(|p| p.contains("sequence gap"))
                && report.problems.iter().any(|p| p.contains("orphan"))
        );
    }

    #[tokio::test]
    async fn verify_detects_claim_text_tamper_and_unresolved_citation() {
        let s = store();
        let claim = record_claim(&*s, "kb1", "original claim text")
            .await
            .unwrap();
        let cit = persist_on(&*s, "kb1", "doc1", "c1").await;
        link_claim(&*s, "kb1", &claim.claim_id, &cit).await.unwrap();
        assert!(verify_integrity(&*s, "kb1").await.unwrap().intact());
        // tamper with claim text in place (same key, changed text)
        let key = ClaimRecord::key("kb1", &claim.claim_id);
        let mut bad = claim.clone();
        bad.text = "forged claim text".into();
        s.put_record(EVIDENCE_NS, &key, &serde_json::to_value(&bad).unwrap())
            .await
            .unwrap();
        let report = verify_integrity(&*s, "kb1").await.unwrap();
        assert!(!report.intact());
        assert!(report
            .problems
            .iter()
            .any(|p| p.contains("content hash mismatch")));
        // restore, then delete the chunk record -> citation no longer resolves
        s.put_record(EVIDENCE_NS, &key, &serde_json::to_value(&claim).unwrap())
            .await
            .unwrap();
        let chunk_key = format!(
            "{}.{}",
            openrt_knowledge::safe_key("kb1"),
            openrt_knowledge::record_key("doc1", "c1")
        );
        s.delete_record(openrt_knowledge::KB_CHUNKS_NS, &chunk_key)
            .await
            .unwrap();
        let report = verify_integrity(&*s, "kb1").await.unwrap();
        assert!(!report.intact());
        assert!(report
            .problems
            .iter()
            .any(|p| p.contains("no longer resolves")));
        // resolve_citation also fails closed now
        let err = resolve_citation(&*s, "kb1", &cit).await.unwrap_err();
        assert!(matches!(err, EvidenceError::UnresolvedCitation(_)));
    }

    #[tokio::test]
    async fn traversal_queries_return_claims_and_documents() {
        let s = store();
        let claim = record_claim(&*s, "kb1", "traversed claim").await.unwrap();
        let cit = persist_on(&*s, "kb1", "doc7", "c7").await;
        link_claim(&*s, "kb1", &claim.claim_id, &cit).await.unwrap();
        let passages = passages_for_claim(&*s, "kb1", &claim.claim_id)
            .await
            .unwrap();
        assert_eq!(passages, vec![cit.clone()]);
        let docs = claims_for_document(&*s, "kb1", "doc7").await.unwrap();
        assert_eq!(docs, vec![claim.claim_id.clone()]);
        let resolved = resolve_citation(&*s, "kb1", &cit).await.unwrap();
        assert_eq!(resolved.len(), 2);
        // other kb sees nothing
        assert!(claims_for_document(&*s, "kb-other", "doc7")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn kb_prefix_collision_is_skipped_not_parsed() {
        let s = store();
        record_claim(&*s, "a.b", "other kb claim").await.unwrap();
        record_claim(&*s, "a", "own kb claim").await.unwrap();
        let claims = list_claims(&*s, "a").await.unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].kb_id, "a");
        assert!(verify_integrity(&*s, "a").await.unwrap().intact());
    }
}
