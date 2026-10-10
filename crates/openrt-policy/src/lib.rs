//! Versioned, append-only security policy document (M6.2).
//!
//! The document is the *operator-only* source of truth for the deny-by-default
//! rule set enforced by the single [`openrt_permissions::PolicyEvaluator`].
//! There is no second evaluator: converting a [`PolicyDocument`] yields the
//! same [`openrt_permissions::Policy`] the runtime gate already uses.
//!
//! Persistence (ADR-006 one-store): revisions live under the
//! `policy.engine` namespace as `v{version}` records plus a `current` pointer.
//! There is no overwrite/delete surface — an append creates version N+1 from
//! version N; the previous revision remains readable for audit.

use std::sync::Arc;

use openrt_permissions::{PermissionError, Policy, PolicyRule, RuleAction};
use serde::{Deserialize, Serialize};

/// Store namespace for the versioned policy document (ADR-006).
pub const POLICY_ENGINE_NS: &str = "policy.engine";
/// Key of the pointer to the active revision.
pub const CURRENT_KEY: &str = "current";
/// Key prefix for immutable revision records (`v1`, `v2`, …).
pub const REVISION_PREFIX: &str = "v";

/// Current document schema version written by this build.
pub const DOCUMENT_SCHEMA_VERSION: u32 = 1;

/// One rule as stored in the document (mirrors [`PolicyRule`] wire form).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentRule {
    pub pattern: String,
    pub action: RuleAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl From<&PolicyRule> for DocumentRule {
    fn from(r: &PolicyRule) -> Self {
        Self {
            pattern: r.pattern.clone(),
            action: r.action,
            scope: r.scope.clone(),
        }
    }
}

impl From<&DocumentRule> for PolicyRule {
    fn from(r: &DocumentRule) -> Self {
        PolicyRule {
            pattern: r.pattern.clone(),
            action: r.action,
            scope: r.scope.clone(),
        }
    }
}

/// Immutable revision of the security policy document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDocument {
    /// Monotonic revision number (starts at 1).
    pub version: u64,
    /// Document schema version (this build writes [`DOCUMENT_SCHEMA_VERSION`]).
    pub schema: u32,
    /// RFC 3339 timestamp of the revision.
    pub created_at: String,
    /// Operator identity that authored the revision.
    pub author: String,
    /// Free-form operator note (reason for the change).
    pub note: String,
    /// Ordered rules; evaluation order is vector order (first match wins).
    pub rules: Vec<DocumentRule>,
}

impl PolicyDocument {
    /// Empty deny-by-default document at revision 1.
    pub fn empty(author: &str, note: &str) -> Self {
        Self {
            version: 1,
            schema: DOCUMENT_SCHEMA_VERSION,
            created_at: openrt_core::now().to_rfc3339(),
            author: author.to_string(),
            note: note.to_string(),
            rules: Vec::new(),
        }
    }

    /// Convert to the evaluator [`Policy`] (the sole evaluation authority).
    pub fn to_policy(&self) -> Policy {
        Policy {
            rules: self.rules.iter().map(PolicyRule::from).collect(),
        }
    }

    /// Append a rule, returning the next revision (caller persists it).
    pub fn appended(
        &self,
        author: &str,
        note: &str,
        pattern: &str,
        action: RuleAction,
        scope: Option<String>,
    ) -> Result<Self, PermissionError> {
        if !pattern.contains('*')
            && (pattern.trim().is_empty() || pattern.chars().any(char::is_whitespace))
        {
            return Err(PermissionError::InvalidRule(pattern.to_string()));
        }
        if pattern.trim().is_empty() {
            return Err(PermissionError::InvalidRule("empty pattern".into()));
        }
        let mut rules = self.rules.clone();
        rules.push(DocumentRule {
            pattern: pattern.to_string(),
            action,
            scope,
        });
        Ok(Self {
            version: self.version + 1,
            schema: DOCUMENT_SCHEMA_VERSION,
            created_at: openrt_core::now().to_rfc3339(),
            author: author.to_string(),
            note: note.to_string(),
            rules,
        })
    }
}

/// Pointer record under [`CURRENT_KEY`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentPointer {
    pub version: u64,
}

/// Errors from the policy document store.
#[derive(Debug, thiserror::Error)]
pub enum PolicyDocumentError {
    #[error("persistence error: {0}")]
    Persistence(#[from] openrt_persistence::PersistenceError),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("policy document not initialized (fail closed)")]
    NotInitialized,
    #[error("revision v{0} not found")]
    MissingRevision(u64),
    #[error("invalid policy rule: {0}")]
    InvalidRule(String),
}

/// Append-only document store over the single [`PersistenceStore`].
pub struct PolicyDocumentStore {
    store: Arc<dyn openrt_persistence::PersistenceStore>,
    lock: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for PolicyDocumentStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicyDocumentStore")
            .finish_non_exhaustive()
    }
}

impl PolicyDocumentStore {
    pub fn new(store: Arc<dyn openrt_persistence::PersistenceStore>) -> Self {
        Self {
            store,
            lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Current revision, or `NotInitialized` (fail closed).
    pub async fn load_current(&self) -> Result<PolicyDocument, PolicyDocumentError> {
        let ptr = self
            .store
            .get_record(POLICY_ENGINE_NS, CURRENT_KEY)
            .await?
            .ok_or(PolicyDocumentError::NotInitialized)?;
        let cur: CurrentPointer = serde_json::from_value(ptr)?;
        self.load_revision(cur.version).await
    }

    /// Load a specific immutable revision.
    pub async fn load_revision(&self, version: u64) -> Result<PolicyDocument, PolicyDocumentError> {
        let key = format!("{REVISION_PREFIX}{version}");
        let v = self
            .store
            .get_record(POLICY_ENGINE_NS, &key)
            .await?
            .ok_or(PolicyDocumentError::MissingRevision(version))?;
        Ok(serde_json::from_value(v)?)
    }

    /// Ensure a deny-by-default revision 1 exists (idempotent bootstrap).
    pub async fn ensure_initialized(
        &self,
        author: &str,
        note: &str,
    ) -> Result<PolicyDocument, PolicyDocumentError> {
        match self.load_current().await {
            Ok(doc) => Ok(doc),
            Err(PolicyDocumentError::NotInitialized) => {
                let _g = self.lock.lock().await;
                if let Ok(doc) = self.load_current().await {
                    return Ok(doc);
                }
                let doc = PolicyDocument::empty(author, note);
                self.write_revision(&doc).await?;
                Ok(doc)
            }
            Err(e) => Err(e),
        }
    }

    /// Append a rule as a new revision (operator-only production path).
    pub async fn append_rule(
        &self,
        author: &str,
        note: &str,
        pattern: &str,
        action: RuleAction,
        scope: Option<String>,
    ) -> Result<PolicyDocument, PolicyDocumentError> {
        let _g = self.lock.lock().await;
        let base = match self.load_current().await {
            Ok(d) => d,
            Err(PolicyDocumentError::NotInitialized) => PolicyDocument::empty(author, "bootstrap"),
            Err(e) => return Err(e),
        };
        let next = base
            .appended(author, note, pattern, action, scope)
            .map_err(|e| PolicyDocumentError::InvalidRule(e.to_string()))?;
        self.write_revision(&next).await?;
        Ok(next)
    }

    /// All known revision numbers (ascending), for audit surfaces.
    pub async fn list_versions(&self) -> Result<Vec<u64>, PolicyDocumentError> {
        let rows = self.store.list_records(POLICY_ENGINE_NS).await?;
        let mut versions = Vec::new();
        for (key, _) in rows {
            if let Some(rest) = key.strip_prefix(REVISION_PREFIX) {
                if let Ok(v) = rest.parse::<u64>() {
                    versions.push(v);
                }
            }
        }
        versions.sort_unstable();
        Ok(versions)
    }

    async fn write_revision(&self, doc: &PolicyDocument) -> Result<(), PolicyDocumentError> {
        let key = format!("{REVISION_PREFIX}{}", doc.version);
        let value = serde_json::to_value(doc)?;
        self.store
            .put_record(POLICY_ENGINE_NS, &key, &value)
            .await?;
        let ptr = serde_json::to_value(CurrentPointer {
            version: doc.version,
        })?;
        self.store
            .put_record(POLICY_ENGINE_NS, CURRENT_KEY, &ptr)
            .await?;
        Ok(())
    }
}

/// Parse a rule action from operator input (`allow` | `deny` | `ask`).
pub fn parse_rule_action(s: &str) -> Result<RuleAction, PermissionError> {
    match s.to_ascii_lowercase().as_str() {
        "allow" => Ok(RuleAction::Allow),
        "deny" => Ok(RuleAction::Deny),
        "ask" => Ok(RuleAction::Ask),
        other => Err(PermissionError::InvalidRule(format!(
            "unknown rule action `{other}` (allow|deny|ask)"
        ))),
    }
}

/// Convenience: load current document rules as an evaluator [`Policy`].
pub async fn policy_from_store(store: &PolicyDocumentStore) -> Result<Policy, PolicyDocumentError> {
    Ok(store.load_current().await?.to_policy())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_permissions::{Decision, PermissionContext, PermissionRequest, PolicyEvaluator};
    use openrt_persistence::MemoryStore;

    fn store() -> PolicyDocumentStore {
        PolicyDocumentStore::new(Arc::new(MemoryStore::new()))
    }

    #[tokio::test]
    async fn uninitialized_fails_closed() {
        let s = store();
        assert!(matches!(
            s.load_current().await,
            Err(PolicyDocumentError::NotInitialized)
        ));
    }

    #[tokio::test]
    async fn ensure_bootstrap_writes_deny_by_default() {
        let s = store();
        let doc = s.ensure_initialized("operator", "init").await.unwrap();
        assert_eq!(doc.version, 1);
        assert!(doc.rules.is_empty());
        let policy = doc.to_policy();
        let eval = PolicyEvaluator::new(&policy);
        let req = PermissionRequest::tool(
            "echo",
            &serde_json::json!({}),
            &PermissionContext::default(),
        );
        assert_eq!(eval.evaluate(&req), Decision::Deny);
    }

    #[tokio::test]
    async fn append_rule_creates_new_revision_and_keeps_history() {
        let s = store();
        s.ensure_initialized("operator", "init").await.unwrap();
        let v2 = s
            .append_rule("operator", "allow echo", "echo", RuleAction::Allow, None)
            .await
            .unwrap();
        assert_eq!(v2.version, 2);
        assert_eq!(v2.rules.len(), 1);
        let v1 = s.load_revision(1).await.unwrap();
        assert_eq!(v1.version, 1);
        assert!(v1.rules.is_empty());
        let policy = s.load_current().await.unwrap().to_policy();
        let eval = PolicyEvaluator::new(&policy);
        let req = PermissionRequest::tool(
            "echo",
            &serde_json::json!({}),
            &PermissionContext::default(),
        );
        assert_eq!(eval.evaluate(&req), Decision::Allow);
        let other = PermissionRequest::tool(
            "shell.exec",
            &serde_json::json!({}),
            &PermissionContext::default(),
        );
        assert_eq!(eval.evaluate(&other), Decision::Deny);
        let versions = s.list_versions().await.unwrap();
        assert_eq!(versions, vec![1, 2]);
    }

    #[tokio::test]
    async fn invalid_pattern_rejected() {
        let s = store();
        s.ensure_initialized("operator", "init").await.unwrap();
        assert!(s
            .append_rule("operator", "bad", "  ", RuleAction::Allow, None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn consecutive_appends_bump_versions() {
        let s = store();
        s.ensure_initialized("o", "i").await.unwrap();
        let a = s
            .append_rule("o", "n", "echo", RuleAction::Allow, None)
            .await
            .unwrap();
        let b = s
            .append_rule("o", "n2", "system.info", RuleAction::Allow, None)
            .await
            .unwrap();
        assert_eq!(a.version + 1, b.version);
        assert_eq!(b.rules.len(), 2);
    }
}
