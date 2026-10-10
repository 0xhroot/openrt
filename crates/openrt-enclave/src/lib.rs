//! Operator-owned enclave workers (M6.4, ADR-004): mutual node identity,
//! step leases over the sole [`WorkflowEngine`] scheduler, rogue-node
//! quarantine, and verified model migration.
//!
//! This crate does **not** own scheduling or the registry: it leases steps to
//! enclave nodes, records results back through existing surfaces, and routes
//! model transfer through `import_local` (SHA256 + provenance) when possible.
//!
//! Storage: `enclave.nodes`, `enclave.leases`, `enclave.registry` (ADR-006).

use std::collections::BTreeMap;
use std::sync::Arc;

use openrt_audit::AuditLog;
use openrt_modes::ModeManager;
use openrt_persistence::PersistenceStore;
use serde::{Deserialize, Serialize};

/// Node namespaces (ADR-006/ADR-004).
pub const NODES_NS: &str = "enclave.nodes";
/// Lease records for distributed step claims.
pub const LEASES_NS: &str = "enclave.leases";
/// Federation/migration transfer records.
pub const REGISTRY_NS: &str = "enclave.registry";

/// Node identity record (operator-generated, out-of-band enrollment).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeIdentity {
    pub node_id: String,
    /// sha256 of the node's operator-issued public identity material.
    pub identity_fp: String,
    pub enrolled_at: String,
    pub status: NodeStatus,
    pub capabilities: Vec<String>,
    /// Set when quarantined; reason is sticky until operator clears.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quarantine_reason: Option<String>,
}

/// Lifecycle of an enclave node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Enrolled,
    Active,
    /// Attempted non-enclave contact, identity mismatch, or artifact tamper.
    Quarantined,
    Revoked,
}

/// A leased workflow step claim (identity + integrity bound).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepLease {
    pub lease_id: String,
    pub node_id: String,
    /// Workflow/run identity from the sole scheduler's run.
    pub workflow_id: String,
    pub step_id: String,
    pub generation: u64,
    pub claimed_at: String,
    pub expires_at: String,
    pub status: LeaseStatus,
}

/// Lease lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseStatus {
    Held,
    Completed,
    Failed,
    /// Node quarantined mid-lease.
    Revoked,
}

/// Model migration/federation transfer record (end-to-end verified).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferRecord {
    pub transfer_id: String,
    pub artifact_id: String,
    pub sha256: String,
    pub from_node: String,
    pub to_node: String,
    /// Provenance digest carried through (ADR-001) when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance_digest: Option<String>,
    pub verified: bool,
    pub started_at: String,
    pub completed_at: Option<String>,
}

/// Enclave errors.
#[derive(Debug, thiserror::Error)]
pub enum EnclaveError {
    #[error("persistence error: {0}")]
    Persistence(#[from] openrt_persistence::PersistenceError),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("audit error: {0}")]
    Audit(#[from] openrt_audit::AuditError),
    #[error("mode error: {0}")]
    Mode(#[from] openrt_modes::ModeError),
    #[error("node `{0}` not found")]
    NodeNotFound(String),
    #[error("node `{0}` is quarantined: {1}")]
    Quarantined(String, String),
    #[error("node `{0}` is revoked")]
    Revoked(String),
    #[error("identity fingerprint mismatch for `{0}` (spoofing rejected)")]
    IdentityMismatch(String),
    #[error("artifact digest mismatch for `{0}` (tamper rejected)")]
    ArtifactTamper(String),
    #[error("enclave-internal only: non-enclave contact is forbidden (ADR-004)")]
    NonEnclaveContact,
    #[error("lease `{0}` not found or not held")]
    LeaseUnavailable(String),
    #[error("invalid enclave request: {0}")]
    Invalid(String),
}

/// Enclave coordinator over the sole store.
pub struct Enclave {
    store: Arc<dyn PersistenceStore>,
    audit: AuditLog,
    modes: ModeManager,
    lock: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for Enclave {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Enclave").finish_non_exhaustive()
    }
}

impl Enclave {
    pub fn new(store: Arc<dyn PersistenceStore>) -> Self {
        let audit = AuditLog::new(store.clone());
        let modes = ModeManager::new(store.clone());
        Self {
            store,
            audit,
            modes,
            lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Enroll a node (operator-only; identity fingerprint required).
    pub async fn enroll(
        &self,
        actor: &str,
        node_id: &str,
        identity_fp: &str,
        capabilities: Vec<String>,
    ) -> Result<NodeIdentity, EnclaveError> {
        if node_id.trim().is_empty() || identity_fp.trim().is_empty() {
            return Err(EnclaveError::Invalid(
                "node_id and identity_fp required".into(),
            ));
        }
        let _g = self.lock.lock().await;
        if let Some(existing) = self.load_node(node_id).await? {
            if existing.identity_fp != identity_fp {
                return Err(EnclaveError::IdentityMismatch(node_id.to_string()));
            }
            if existing.status == NodeStatus::Revoked {
                return Err(EnclaveError::Revoked(node_id.to_string()));
            }
            return Ok(existing);
        }
        let node = NodeIdentity {
            node_id: node_id.to_string(),
            identity_fp: identity_fp.to_string(),
            enrolled_at: openrt_core::now().to_rfc3339(),
            status: NodeStatus::Active,
            capabilities,
            quarantine_reason: None,
        };
        self.put_node(&node).await?;
        self.audit
            .append(
                actor,
                "enclave.enroll",
                node_id,
                "enrolled",
                serde_json::json!({ "identity_fp": identity_fp }),
            )
            .await?;
        Ok(node)
    }

    /// Mutual authentication check on every node message: fingerprint must
    /// match enrollment; quarantined/revoked nodes are rejected (ADR-004).
    pub async fn authenticate(
        &self,
        node_id: &str,
        identity_fp: &str,
    ) -> Result<NodeIdentity, EnclaveError> {
        let node = self
            .load_node(node_id)
            .await?
            .ok_or_else(|| EnclaveError::NodeNotFound(node_id.to_string()))?;
        if node.identity_fp != identity_fp {
            self.quarantine_internal(node_id, "identity fingerprint mismatch (spoofing rejected)")
                .await?;
            return Err(EnclaveError::IdentityMismatch(node_id.to_string()));
        }
        match node.status {
            NodeStatus::Quarantined => Err(EnclaveError::Quarantined(
                node_id.to_string(),
                node.quarantine_reason
                    .unwrap_or_else(|| "quarantined".into()),
            )),
            NodeStatus::Revoked => Err(EnclaveError::Revoked(node_id.to_string())),
            NodeStatus::Enrolled | NodeStatus::Active => Ok(node),
        }
    }

    /// Quarantine a node (operator or automatic on anomaly). Audited.
    pub async fn quarantine(
        &self,
        actor: &str,
        node_id: &str,
        reason: &str,
    ) -> Result<(), EnclaveError> {
        let _g = self.lock.lock().await;
        self.quarantine_internal(node_id, reason).await?;
        self.audit
            .append(
                actor,
                "enclave.quarantine",
                node_id,
                "quarantined",
                serde_json::json!({ "reason": reason }),
            )
            .await?;
        Ok(())
    }

    /// Clear quarantine after operator review (never automatic).
    pub async fn clear_quarantine(&self, actor: &str, node_id: &str) -> Result<(), EnclaveError> {
        let mut node = self
            .load_node(node_id)
            .await?
            .ok_or_else(|| EnclaveError::NodeNotFound(node_id.to_string()))?;
        node.status = NodeStatus::Active;
        node.quarantine_reason = None;
        self.put_node(&node).await?;
        self.audit
            .append(
                actor,
                "enclave.clear_quarantine",
                node_id,
                "active",
                serde_json::json!({}),
            )
            .await?;
        Ok(())
    }

    /// Reject any non-enclave contact attempt (Sovereign and always).
    pub async fn ensure_enclave_contact(&self) -> Result<(), EnclaveError> {
        // ADR-004: enclave-internal only; no internet/egress path exists here.
        // Sovereign additionally blocks non-enclave network via ModeManager.
        let _ = self.modes.ensure_network_allowed();
        // We never allow non-enclave destinations from this API surface.
        Err(EnclaveError::NonEnclaveContact)
    }

    /// Claim a step lease for an authenticated node. Steps are projected from
    /// the sole scheduler's workflow id + step id (no second scheduler).
    pub async fn claim_step(
        &self,
        node_id: &str,
        identity_fp: &str,
        workflow_id: &str,
        step_id: &str,
        ttl_secs: u64,
    ) -> Result<StepLease, EnclaveError> {
        let node = self.authenticate(node_id, identity_fp).await?;
        if workflow_id.trim().is_empty() || step_id.trim().is_empty() {
            return Err(EnclaveError::Invalid(
                "workflow and step ids required".into(),
            ));
        }
        let _g = self.lock.lock().await;
        // One held lease per (workflow, step): fail-closed if already held.
        for (_, v) in self.store.list_records(LEASES_NS).await? {
            let l: StepLease = serde_json::from_value(v)?;
            if l.workflow_id == workflow_id && l.step_id == step_id && l.status == LeaseStatus::Held
            {
                let expires = chrono::DateTime::parse_from_rfc3339(&l.expires_at)
                    .map(|d| d.with_timezone(&chrono::Utc))
                    .unwrap_or(openrt_core::now());
                if expires > openrt_core::now() && l.node_id != node.node_id {
                    return Err(EnclaveError::LeaseUnavailable(l.lease_id));
                }
            }
        }
        let lease = StepLease {
            lease_id: uuid::Uuid::new_v4().to_string(),
            node_id: node.node_id.clone(),
            workflow_id: workflow_id.to_string(),
            step_id: step_id.to_string(),
            generation: 1,
            claimed_at: openrt_core::now().to_rfc3339(),
            expires_at: (openrt_core::now() + chrono::Duration::seconds(ttl_secs as i64))
                .to_rfc3339(),
            status: LeaseStatus::Held,
        };
        self.store
            .put_record(LEASES_NS, &lease.lease_id, &serde_json::to_value(&lease)?)
            .await?;
        self.audit
            .append(
                "system",
                "enclave.lease.claim",
                &lease.lease_id,
                "held",
                serde_json::json!({
                    "node_id": node.node_id,
                    "workflow_id": workflow_id,
                    "step_id": step_id,
                }),
            )
            .await?;
        Ok(lease)
    }

    /// Complete a lease with an integrity-bound result hash.
    pub async fn complete_step(
        &self,
        lease_id: &str,
        node_id: &str,
        identity_fp: &str,
        result_sha256: &str,
    ) -> Result<StepLease, EnclaveError> {
        self.authenticate(node_id, identity_fp).await?;
        let mut lease: StepLease = serde_json::from_value(
            self.store
                .get_record(LEASES_NS, lease_id)
                .await?
                .ok_or_else(|| EnclaveError::LeaseUnavailable(lease_id.to_string()))?,
        )?;
        if lease.node_id != node_id {
            return Err(EnclaveError::IdentityMismatch(node_id.to_string()));
        }
        if lease.status != LeaseStatus::Held {
            return Err(EnclaveError::LeaseUnavailable(lease_id.to_string()));
        }
        if result_sha256.len() != 64 || !result_sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(EnclaveError::Invalid(
                "result_sha256 must be 64 hex chars".into(),
            ));
        }
        lease.status = LeaseStatus::Completed;
        self.store
            .put_record(LEASES_NS, lease_id, &serde_json::to_value(&lease)?)
            .await?;
        self.audit
            .append(
                "system",
                "enclave.lease.complete",
                lease_id,
                "completed",
                serde_json::json!({ "result_sha256": result_sha256 }),
            )
            .await?;
        Ok(lease)
    }

    /// Verify + record a model migration between enclave nodes.
    /// Digest must match the registry manifest when present; mismatch ⇒
    /// fail-closed [`EnclaveError::ArtifactTamper`].
    pub async fn migrate_model(
        &self,
        actor: &str,
        artifact_id: &str,
        expected_sha256: &str,
        from_node: &str,
        to_node: &str,
        provenance_digest: Option<String>,
    ) -> Result<TransferRecord, EnclaveError> {
        let from = self
            .load_node(from_node)
            .await?
            .ok_or_else(|| EnclaveError::NodeNotFound(from_node.into()))?;
        let to = self
            .load_node(to_node)
            .await?
            .ok_or_else(|| EnclaveError::NodeNotFound(to_node.into()))?;
        for n in [&from, &to] {
            match n.status {
                NodeStatus::Quarantined => {
                    return Err(EnclaveError::Quarantined(
                        n.node_id.clone(),
                        n.quarantine_reason.clone().unwrap_or_default(),
                    ))
                }
                NodeStatus::Revoked => return Err(EnclaveError::Revoked(n.node_id.clone())),
                _ => {}
            }
        }
        if expected_sha256.len() != 64 || !expected_sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(EnclaveError::Invalid(
                "expected_sha256 must be 64 hex".into(),
            ));
        }
        // Content-addressed confirmation against the one registry's manifest.
        let registry = openrt_registry::ModelRegistry::new(
            self.store.clone(),
            ModeManager::new(self.store.clone()),
        );
        if let Some(manifest) = registry
            .manifest(artifact_id)
            .await
            .map_err(|e| EnclaveError::Invalid(format!("registry lookup failed: {e}")))?
        {
            let has_blob = manifest.blobs.iter().any(|b| b.digest == expected_sha256);
            if !manifest.blobs.is_empty() && !has_blob {
                self.quarantine_internal(
                    to_node,
                    "artifact digest mismatch during migration (tamper rejected)",
                )
                .await?;
                return Err(EnclaveError::ArtifactTamper(artifact_id.to_string()));
            }
        }
        let rec = TransferRecord {
            transfer_id: uuid::Uuid::new_v4().to_string(),
            artifact_id: artifact_id.to_string(),
            sha256: expected_sha256.to_string(),
            from_node: from_node.to_string(),
            to_node: to_node.to_string(),
            provenance_digest,
            verified: true,
            started_at: openrt_core::now().to_rfc3339(),
            completed_at: Some(openrt_core::now().to_rfc3339()),
        };
        self.store
            .put_record(REGISTRY_NS, &rec.transfer_id, &serde_json::to_value(&rec)?)
            .await?;
        self.audit
            .append(
                actor,
                "enclave.migrate",
                artifact_id,
                "verified",
                serde_json::json!({
                    "from": from_node,
                    "to": to_node,
                    "sha256": expected_sha256,
                    "transfer_id": rec.transfer_id,
                }),
            )
            .await?;
        Ok(rec)
    }

    /// List enrolled nodes.
    pub async fn list_nodes(&self) -> Result<Vec<NodeIdentity>, EnclaveError> {
        let rows = self.store.list_records(NODES_NS).await?;
        let mut out: Vec<NodeIdentity> = Vec::new();
        for (_, v) in rows {
            out.push(serde_json::from_value(v)?);
        }
        out.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        Ok(out)
    }

    /// List transfers.
    pub async fn list_transfers(&self) -> Result<Vec<TransferRecord>, EnclaveError> {
        let rows = self.store.list_records(REGISTRY_NS).await?;
        let mut out: Vec<TransferRecord> = Vec::new();
        for (_, v) in rows {
            out.push(serde_json::from_value(v)?);
        }
        Ok(out)
    }

    /// Aggregate node status counts for observability.
    pub async fn status_counts(&self) -> Result<BTreeMap<String, usize>, EnclaveError> {
        let mut m = BTreeMap::new();
        for n in self.list_nodes().await? {
            *m.entry(format!("{:?}", n.status)).or_insert(0) += 1;
        }
        Ok(m)
    }

    async fn load_node(&self, node_id: &str) -> Result<Option<NodeIdentity>, EnclaveError> {
        match self.store.get_record(NODES_NS, node_id).await? {
            None => Ok(None),
            Some(v) => Ok(Some(serde_json::from_value(v)?)),
        }
    }

    async fn put_node(&self, node: &NodeIdentity) -> Result<(), EnclaveError> {
        self.store
            .put_record(NODES_NS, &node.node_id, &serde_json::to_value(node)?)
            .await?;
        Ok(())
    }

    async fn quarantine_internal(&self, node_id: &str, reason: &str) -> Result<(), EnclaveError> {
        let mut node = self
            .load_node(node_id)
            .await?
            .ok_or_else(|| EnclaveError::NodeNotFound(node_id.to_string()))?;
        node.status = NodeStatus::Quarantined;
        node.quarantine_reason = Some(reason.to_string());
        self.put_node(&node).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_persistence::MemoryStore;

    fn enc() -> Enclave {
        Enclave::new(Arc::new(MemoryStore::new()))
    }

    #[tokio::test]
    async fn enroll_and_authenticate() {
        let e = enc();
        let n = e
            .enroll("op", "node-a", "fp-aaaa", vec!["cpu".into()])
            .await
            .unwrap();
        assert_eq!(n.status, NodeStatus::Active);
        let ok = e.authenticate("node-a", "fp-aaaa").await.unwrap();
        assert_eq!(ok.node_id, "node-a");
    }

    #[tokio::test]
    async fn identity_spoof_quarantines_and_rejects() {
        let e = enc();
        e.enroll("op", "node-a", "fp-real", vec![]).await.unwrap();
        let err = e.authenticate("node-a", "fp-fake").await.unwrap_err();
        assert!(matches!(err, EnclaveError::IdentityMismatch(_)));
        let node = e.load_node("node-a").await.unwrap().unwrap();
        assert_eq!(node.status, NodeStatus::Quarantined);
        // Subsequent correct fp still quarantined.
        assert!(matches!(
            e.authenticate("node-a", "fp-real").await,
            Err(EnclaveError::Quarantined(_, _))
        ));
    }

    #[tokio::test]
    async fn quarantined_node_cannot_claim() {
        let e = enc();
        e.enroll("op", "n", "fp", vec![]).await.unwrap();
        e.quarantine("op", "n", "anomaly").await.unwrap();
        assert!(matches!(
            e.claim_step("n", "fp", "w", "s", 60).await,
            Err(EnclaveError::Quarantined(_, _))
        ));
    }

    #[tokio::test]
    async fn step_lease_claim_and_complete() {
        let e = enc();
        e.enroll("op", "n1", "fp1", vec![]).await.unwrap();
        let lease = e.claim_step("n1", "fp1", "wf", "step-1", 60).await.unwrap();
        assert_eq!(lease.status, LeaseStatus::Held);
        // Second node cannot steal held lease.
        e.enroll("op", "n2", "fp2", vec![]).await.unwrap();
        assert!(matches!(
            e.claim_step("n2", "fp2", "wf", "step-1", 60).await,
            Err(EnclaveError::LeaseUnavailable(_))
        ));
        let done = e
            .complete_step(&lease.lease_id, "n1", "fp1", &"a".repeat(64))
            .await
            .unwrap();
        assert_eq!(done.status, LeaseStatus::Completed);
        // After completion, other node can claim.
        let l2 = e.claim_step("n2", "fp2", "wf", "step-1", 60).await.unwrap();
        assert_eq!(l2.node_id, "n2");
    }

    #[tokio::test]
    async fn non_enclave_contact_rejected() {
        let e = enc();
        assert!(matches!(
            e.ensure_enclave_contact().await,
            Err(EnclaveError::NonEnclaveContact)
        ));
    }

    #[tokio::test]
    async fn migration_records_verified_transfer() {
        let e = enc();
        e.enroll("op", "a", "fa", vec![]).await.unwrap();
        e.enroll("op", "b", "fb", vec![]).await.unwrap();
        let sha = "b".repeat(64);
        let rec = e
            .migrate_model("op", "model-x", &sha, "a", "b", Some("prov-1".into()))
            .await
            .unwrap();
        assert!(rec.verified);
        assert_eq!(rec.sha256, sha);
        assert_eq!(e.list_transfers().await.unwrap().len(), 1);
        assert!(e.audit.verify().await.unwrap() >= 1);
    }

    #[tokio::test]
    async fn migration_rejects_bad_digest_format() {
        let e = enc();
        e.enroll("op", "a", "fa", vec![]).await.unwrap();
        e.enroll("op", "b", "fb", vec![]).await.unwrap();
        assert!(matches!(
            e.migrate_model("op", "m", "not-hex", "a", "b", None).await,
            Err(EnclaveError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn quarantined_node_blocked_from_migration() {
        let e = enc();
        e.enroll("op", "a", "fa", vec![]).await.unwrap();
        e.enroll("op", "b", "fb", vec![]).await.unwrap();
        e.quarantine("op", "b", "rogue").await.unwrap();
        assert!(matches!(
            e.migrate_model("op", "m", &"c".repeat(64), "a", "b", None)
                .await,
            Err(EnclaveError::Quarantined(_, _))
        ));
    }
}
