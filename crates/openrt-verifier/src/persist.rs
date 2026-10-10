//! Verdict persistence (`verify.verdicts`): one record per firewall run —
//! claim, evidence refs, backends, limits and result. Never model secrets,
//! never a verdict without the record (fail-closed write + read-back).

use openrt_knowledge::safe_key;
use openrt_persistence::PersistenceStore;
use serde::{Deserialize, Serialize};

use crate::deterministic::Grounding;
use crate::firewall::LimitsSnapshot;
use crate::types::{Basis, FirewallAction, Independence, Verdict};
use crate::VerifyError;

/// `openrt-verifier` owned namespace: firewall verdict records.
pub const VERDICTS_NS: &str = "verify.verdicts";

/// One cited span as recorded on a verdict (with its resolution outcome).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRefRecord {
    pub document_id: String,
    pub chunk_id: String,
    pub resolved: bool,
}

/// One persisted firewall verdict.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerdictRecord {
    /// Content-addressed id over (kb, claim, outcome, backends, timestamp).
    pub verdict_id: String,
    pub kb_id: String,
    /// The claim as submitted (OWNERSHIP: verdict records carry the claim).
    pub claim: String,
    pub claim_sha256: String,
    pub decision: FirewallAction,
    pub verdict: Verdict,
    pub basis: Basis,
    pub grounding: Grounding,
    pub evidence_refs: Vec<EvidenceRefRecord>,
    /// `provider/model` labels that actually produced model verdicts.
    pub backends: Vec<String>,
    pub independence: Independence,
    pub limits: LimitsSnapshot,
    pub reasons: Vec<String>,
    pub created_at: String,
}

impl VerdictRecord {
    pub(crate) fn key(kb_id: &str, verdict_id: &str) -> String {
        format!("{}.{}", safe_key(kb_id), verdict_id)
    }
}

/// Persist one verdict: write, read back and compare before returning `Ok`
/// (the caller may only treat the verdict as recorded after a matching round
/// trip). Returns the record's store key.
pub async fn record_verdict(
    store: &dyn PersistenceStore,
    record: &VerdictRecord,
) -> Result<String, VerifyError> {
    let key = VerdictRecord::key(&record.kb_id, &record.verdict_id);
    let value = serde_json::to_value(record)
        .map_err(|e| VerifyError::Serde(format!("serialize verdict `{key}`: {e}")))?;
    store.put_record(VERDICTS_NS, &key, &value).await?;
    let back = store
        .get_record(VERDICTS_NS, &key)
        .await?
        .ok_or_else(|| VerifyError::Corrupt(key.clone(), "read-back missing".into()))?;
    let verified: VerdictRecord = serde_json::from_value(back)
        .map_err(|e| VerifyError::Corrupt(key.clone(), e.to_string()))?;
    if verified != *record {
        return Err(VerifyError::Corrupt(
            key,
            "read-back does not match written verdict".into(),
        ));
    }
    Ok(key)
}

/// Deterministically list a KB's verdict records (sorted by record key).
/// Malformed records fail closed (`Corrupt`), never skipped silently.
pub async fn list_verdicts(
    store: &dyn PersistenceStore,
    kb_id: &str,
) -> Result<Vec<VerdictRecord>, VerifyError> {
    let prefix = format!("{}.", safe_key(kb_id));
    let mut out = Vec::new();
    for (key, value) in store.list_records(VERDICTS_NS).await? {
        if !key.starts_with(&prefix) {
            continue;
        }
        let rec: VerdictRecord = serde_json::from_value(value)
            .map_err(|e| VerifyError::Corrupt(key.clone(), e.to_string()))?;
        if rec.kb_id != kb_id {
            return Err(VerifyError::Corrupt(
                key,
                "verdict kb_id does not match its key".into(),
            ));
        }
        out.push((key, rec));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out.into_iter().map(|(_, r)| r).collect())
}
