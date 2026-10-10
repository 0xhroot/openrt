//! Default-deny network egress policies per worker/workflow scope (M6.3).
//!
//! This crate stores *policy intent* (which scopes may egress, to where).
//! Enforcement always consults the single [`ModeManager`]: Sovereign mode
//! blocks all non-enclave egress regardless of grants; Provisioning may allow
//! only explicitly granted destinations. There is no second network authority
//! and no network canaries (ADR-003).
//!
//! Storage: `netpolicy.grants` under the foundation store (ADR-006).

use std::sync::Arc;

use openrt_audit::AuditLog;
use openrt_modes::{ExecutionMode, ModeError, ModeManager};
use serde::{Deserialize, Serialize};

/// Store namespace for egress grants (ADR-006).
pub const NETPOLICY_NS: &str = "netpolicy.grants";

/// One default-deny grant for a scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EgressGrant {
    /// Scope id: worker id, workflow id, or `default`.
    pub scope: String,
    /// Allowed destinations as host globs (`*.internal`, `10.0.0.0/8` as
    /// opaque strings — matching is the caller's OS-level netfilter job; this
    /// layer records operator intent and fails closed on deny).
    pub allow: Vec<String>,
    /// Operator who granted (audit attribution).
    pub granted_by: String,
    pub granted_at: String,
    pub note: String,
}

/// Outcome of an egress check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum EgressDecision {
    /// Mode + grant allow this destination.
    Allow { scope: String, destination: String },
    /// Sovereign mode (or no grant) refuses.
    Deny {
        scope: String,
        destination: String,
        reason: String,
    },
}

/// Errors from policy grant persistence.
#[derive(Debug, thiserror::Error)]
pub enum NetPolError {
    #[error("persistence error: {0}")]
    Persistence(#[from] openrt_persistence::PersistenceError),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("mode error: {0}")]
    Mode(#[from] ModeError),
    #[error("audit error: {0}")]
    Audit(#[from] openrt_audit::AuditError),
    #[error("empty scope or destination")]
    Invalid,
}

/// Policy store + enforcement helper over ModeManager (sole authority).
pub struct NetworkPolicy {
    store: Arc<dyn openrt_persistence::PersistenceStore>,
    modes: ModeManager,
    audit: AuditLog,
    lock: tokio::sync::Mutex<()>,
}

impl NetworkPolicy {
    pub fn new(store: Arc<dyn openrt_persistence::PersistenceStore>) -> Self {
        let modes = ModeManager::new(store.clone());
        let audit = AuditLog::new(store.clone());
        Self {
            store,
            modes,
            audit,
            lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Grant egress for `scope` (operator-only; audited). Does not bypass
    /// Sovereign: grants only matter when mode allows network.
    pub async fn grant(
        &self,
        actor: &str,
        scope: &str,
        allow: Vec<String>,
        note: &str,
    ) -> Result<EgressGrant, NetPolError> {
        if scope.trim().is_empty() || allow.iter().any(|a| a.trim().is_empty()) {
            return Err(NetPolError::Invalid);
        }
        let _g = self.lock.lock().await;
        let g = EgressGrant {
            scope: scope.to_string(),
            allow,
            granted_by: actor.to_string(),
            granted_at: openrt_core::now().to_rfc3339(),
            note: note.to_string(),
        };
        self.store
            .put_record(NETPOLICY_NS, scope, &serde_json::to_value(&g)?)
            .await?;
        self.audit
            .append(
                actor,
                "netpol.grant",
                scope,
                "granted",
                serde_json::json!({ "allow": g.allow, "note": note }),
            )
            .await?;
        Ok(g)
    }

    /// Revoke a grant (operator; audited).
    pub async fn revoke(&self, actor: &str, scope: &str) -> Result<(), NetPolError> {
        let _g = self.lock.lock().await;
        if self.store.get_record(NETPOLICY_NS, scope).await?.is_none() {
            return Ok(());
        }
        self.store.delete_record(NETPOLICY_NS, scope).await?;
        self.audit
            .append(
                actor,
                "netpol.revoke",
                scope,
                "revoked",
                serde_json::json!({}),
            )
            .await?;
        Ok(())
    }

    /// Load a grant if present.
    pub async fn grant_for(&self, scope: &str) -> Result<Option<EgressGrant>, NetPolError> {
        match self.store.get_record(NETPOLICY_NS, scope).await? {
            None => Ok(None),
            Some(v) => Ok(Some(serde_json::from_value(v)?)),
        }
    }

    /// List all grants.
    pub async fn list(&self) -> Result<Vec<EgressGrant>, NetPolError> {
        let rows = self.store.list_records(NETPOLICY_NS).await?;
        let mut out: Vec<EgressGrant> = Vec::new();
        for (_, v) in rows {
            out.push(serde_json::from_value(v)?);
        }
        out.sort_by(|a, b| a.scope.cmp(&b.scope));
        Ok(out)
    }

    /// Enforce egress for `destination` under `scope`.
    ///
    /// Order: (1) ModeManager must allow network (Sovereign ⇒ deny, no client
    /// touch); (2) scope grant must list a matching allow entry (default-deny).
    pub async fn ensure_egress(
        &self,
        scope: &str,
        destination: &str,
    ) -> Result<EgressDecision, NetPolError> {
        if scope.trim().is_empty() || destination.trim().is_empty() {
            return Err(NetPolError::Invalid);
        }
        // Sole network authority first.
        match self.modes.ensure_network_allowed().await {
            Ok(()) => {}
            Err(ModeError::NetworkBlocked) => {
                return Ok(EgressDecision::Deny {
                    scope: scope.to_string(),
                    destination: destination.to_string(),
                    reason: "sovereign mode blocks non-enclave egress (ADR-003)".into(),
                });
            }
            Err(e) => return Err(e.into()),
        }
        let grant = self.grant_for(scope).await?;
        let allowed = match &grant {
            Some(g) => g.allow.iter().any(|pat| dest_match(pat, destination)),
            None => false,
        };
        if allowed {
            Ok(EgressDecision::Allow {
                scope: scope.to_string(),
                destination: destination.to_string(),
            })
        } else {
            Ok(EgressDecision::Deny {
                scope: scope.to_string(),
                destination: destination.to_string(),
                reason: format!(
                    "no egress grant for scope `{scope}` covering `{destination}` (default-deny)"
                ),
            })
        }
    }

    /// Current mode (for doctor / status surfaces).
    pub async fn mode(&self) -> Result<ExecutionMode, NetPolError> {
        Ok(self.modes.current().await?.mode)
    }
}

/// Simple destination match: exact, `*` suffix/prefix globs, or `*` catch-all.
fn dest_match(pattern: &str, dest: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix("*.") {
        return dest == suffix || dest.ends_with(&format!(".{suffix}")) || dest.ends_with(suffix);
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return dest.starts_with(prefix);
    }
    dest == pattern
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_persistence::MemoryStore;

    fn np() -> NetworkPolicy {
        NetworkPolicy::new(Arc::new(MemoryStore::new()))
    }

    #[tokio::test]
    async fn sovereign_denies_even_with_grant() {
        let n = np();
        n.grant("op", "wf-1", vec!["models.internal".into()], "ok")
            .await
            .unwrap();
        let d = n.ensure_egress("wf-1", "models.internal").await.unwrap();
        match d {
            EgressDecision::Deny { reason, .. } => assert!(reason.contains("sovereign")),
            EgressDecision::Allow { .. } => panic!("sovereign must deny"),
        }
    }

    #[tokio::test]
    async fn provisioning_without_grant_denies() {
        let n = np();
        n.modes
            .transition("op", ExecutionMode::Provisioning, "test")
            .await
            .unwrap();
        let d = n.ensure_egress("wf-x", "example.com").await.unwrap();
        assert!(matches!(d, EgressDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn provisioning_with_matching_grant_allows() {
        let n = np();
        n.modes
            .transition("op", ExecutionMode::Provisioning, "test")
            .await
            .unwrap();
        n.grant("op", "wf-1", vec!["models.internal".into()], "ok")
            .await
            .unwrap();
        let ok = n.ensure_egress("wf-1", "models.internal").await.unwrap();
        assert!(matches!(ok, EgressDecision::Allow { .. }));
        let bad = n.ensure_egress("wf-1", "evil.example").await.unwrap();
        assert!(matches!(bad, EgressDecision::Deny { .. }));
        // Wrong scope still denies.
        let other = n.ensure_egress("wf-2", "models.internal").await.unwrap();
        assert!(matches!(other, EgressDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn revoke_restores_deny() {
        let n = np();
        n.modes
            .transition("op", ExecutionMode::Provisioning, "t")
            .await
            .unwrap();
        n.grant("op", "s", vec!["*".into()], "wide").await.unwrap();
        assert!(matches!(
            n.ensure_egress("s", "anywhere").await.unwrap(),
            EgressDecision::Allow { .. }
        ));
        n.revoke("op", "s").await.unwrap();
        assert!(matches!(
            n.ensure_egress("s", "anywhere").await.unwrap(),
            EgressDecision::Deny { .. }
        ));
    }

    #[test]
    fn dest_match_globs() {
        assert!(dest_match("*", "x"));
        assert!(dest_match("a.example", "a.example"));
        assert!(dest_match("*.internal", "foo.internal"));
        assert!(dest_match("10.*", "10.0.0.1"));
        assert!(!dest_match("*.internal", "evil.com"));
    }
}
