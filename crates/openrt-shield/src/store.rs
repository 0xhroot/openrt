//! Persistence for [`SecurityFinding`]s under the ADR-006 namespace
//! `shield.findings` (one store, one namespace per concern — no side files).

use crate::{FindingStatus, SecurityFinding, ShieldReport};
use openrt_persistence::{PersistenceError, PersistenceStore};

/// Storage namespace for security findings (ADR-006).
pub const SHIELD_NS: &str = "shield.findings";

/// Key prefix so a namespace can never collide with other record families.
const KEY_PREFIX: &str = "find.";

/// Errors surfaced by shield persistence.
#[derive(Debug, thiserror::Error)]
pub enum ShieldError {
    #[error(transparent)]
    Store(#[from] PersistenceError),
    #[error("finding `{0}` not found")]
    Missing(String),
    #[error("corrupt finding `{0}`: {1}")]
    Corrupt(String, String),
}

fn key_for(id: &str) -> String {
    format!("{KEY_PREFIX}{id}")
}

fn decode(key: &str, value: serde_json::Value) -> Result<SecurityFinding, ShieldError> {
    serde_json::from_value(value).map_err(|e| ShieldError::Corrupt(key.to_string(), e.to_string()))
}

/// Persist every finding of a report (as-is — the caller has already stamped
/// the enforced status via [`ShieldReport`] findings before recording).
/// Returns how many findings were written.
pub async fn record_report(
    store: &dyn PersistenceStore,
    report: &ShieldReport,
) -> Result<usize, ShieldError> {
    for finding in &report.findings {
        let value = serde_json::to_value(finding)
            .map_err(|e| ShieldError::Corrupt(finding.id.clone(), e.to_string()))?;
        store
            .put_record(SHIELD_NS, &key_for(&finding.id), &value)
            .await?;
    }
    Ok(report.findings.len())
}

/// List findings, newest first, optionally filtered by status.
pub async fn list_findings(
    store: &dyn PersistenceStore,
    status: Option<FindingStatus>,
) -> Result<Vec<SecurityFinding>, ShieldError> {
    let mut out = Vec::new();
    for (key, value) in store.list_records(SHIELD_NS).await? {
        let finding = decode(&key, value)?;
        if status.map_or(true, |s| finding.status == s) {
            out.push(finding);
        }
    }
    // Deterministic order: newest first, id as tie-break.
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.id.cmp(&b.id)));
    Ok(out)
}

/// Fetch one finding by id.
pub async fn get_finding(
    store: &dyn PersistenceStore,
    id: &str,
) -> Result<Option<SecurityFinding>, ShieldError> {
    let Some(value) = store.get_record(SHIELD_NS, &key_for(id)).await? else {
        return Ok(None);
    };
    let finding = decode(&key_for(id), value)?;
    Ok(Some(finding))
}

/// Update a finding's status (operator acknowledges / enforcement records).
pub async fn update_finding_status(
    store: &dyn PersistenceStore,
    id: &str,
    status: FindingStatus,
) -> Result<SecurityFinding, ShieldError> {
    let mut finding = get_finding(store, id)
        .await?
        .ok_or_else(|| ShieldError::Missing(id.to_string()))?;
    finding.status = status;
    let value = serde_json::to_value(&finding)
        .map_err(|e| ShieldError::Corrupt(finding.id.clone(), e.to_string()))?;
    store.put_record(SHIELD_NS, &key_for(id), &value).await?;
    Ok(finding)
}

/// Convenience used by the gate: stamp every finding with the enforced
/// status, persist, and hand back the report for audit emission.
pub async fn record_enforced(
    store: &dyn PersistenceStore,
    report: &ShieldReport,
    status: FindingStatus,
) -> Result<ShieldReport, ShieldError> {
    let mut stamped = report.clone();
    for f in &mut stamped.findings {
        f.status = status;
    }
    record_report(store, &stamped).await?;
    Ok(stamped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FindingCategory, Severity, ShieldAction};
    use openrt_persistence::MemoryStore;

    fn finding(sev: Severity) -> SecurityFinding {
        SecurityFinding::new(
            FindingCategory::DangerousCommand,
            sev,
            "shield.dangerous_command",
            "terminal.run",
            "test finding",
            serde_json::json!({ "rule": "test" }),
        )
    }

    fn report(findings: Vec<SecurityFinding>) -> ShieldReport {
        ShieldReport::from_findings("gate", "terminal.run", findings)
    }

    #[tokio::test]
    async fn record_list_get_roundtrip() {
        let store = MemoryStore::default();
        let f = finding(Severity::High);
        let id = f.id.clone();
        let n = record_report(&store, &report(vec![f])).await.unwrap();
        assert_eq!(n, 1);

        let all = list_findings(&store, None).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, id);

        let got = get_finding(&store, &id).await.unwrap().expect("present");
        assert_eq!(got.severity, Severity::High);

        assert!(get_finding(&store, "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn status_filter_and_update() {
        let store = MemoryStore::default();
        let a = finding(Severity::Low);
        let b = finding(Severity::Critical);
        let b_id = b.id.clone();
        record_report(&store, &report(vec![a, b])).await.unwrap();

        let updated = update_finding_status(&store, &b_id, FindingStatus::Denied)
            .await
            .unwrap();
        assert_eq!(updated.status, FindingStatus::Denied);

        let denied = list_findings(&store, Some(FindingStatus::Denied))
            .await
            .unwrap();
        assert_eq!(denied.len(), 1);
        assert_eq!(denied[0].id, b_id);

        let open = list_findings(&store, Some(FindingStatus::Reported))
            .await
            .unwrap();
        assert_eq!(open.len(), 1);
    }

    #[tokio::test]
    async fn update_missing_finding_fails_closed() {
        let store = MemoryStore::default();
        let err = update_finding_status(&store, "ghost", FindingStatus::Open)
            .await
            .unwrap_err();
        assert!(matches!(err, ShieldError::Missing(_)));
    }

    #[tokio::test]
    async fn record_enforced_stamps_status() {
        let store = MemoryStore::default();
        let f = finding(Severity::Critical);
        let id = f.id.clone();
        let r = report(vec![f]);
        let stamped = record_enforced(&store, &r, FindingStatus::Denied)
            .await
            .unwrap();
        assert_eq!(stamped.findings[0].status, FindingStatus::Denied);
        // Original report untouched (immutability of the caller's value).
        assert_eq!(r.findings[0].status, FindingStatus::Reported);
        let got = get_finding(&store, &id).await.unwrap().unwrap();
        assert_eq!(got.status, FindingStatus::Denied);
        assert_eq!(got.recommended_action, ShieldAction::Deny);
    }
}
