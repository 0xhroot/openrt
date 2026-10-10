//! Approval workflow interfaces (P2.2 ASK boundary).
//!
//! A policy rule with `RuleAction::Ask` produces a `Decision::Ask`. The gate
//! refuses to run the tool on its own: it builds an [`ApprovalRequest`] and
//! consults an optional [`PermissionResolver`]. Rules:
//!
//! * **No resolver configured ⇒ fail closed** (treated as denied).
//! * **`Ask` is never implicitly `Allow`** — only an explicit resolver
//!   `PermissionDecision::Allow` lets the call proceed.
//! * **A resolver never overrides a policy `Deny`** — it is only consulted
//!   after the evaluator already returned `Ask`, so it can only release a
//!   decision the policy made conditional; it cannot create or override
//!   allowances.
//! * A resolver error or `Deny` is a hard denial.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;

use crate::error::PermissionError;
use crate::policy::PermissionContext;

/// One approval request handed to a resolver.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ApprovalRequest {
    /// Opportunistic unique id (the gate supplies a UUID).
    pub request_id: String,
    /// Tool id that is asking (e.g. `filesystem.read`).
    pub tool: String,
    /// The exact invocation arguments (the gate has already validated them).
    pub args: serde_json::Value,
    /// Extracted scope (e.g. the path being touched).
    pub scope: Option<String>,
    /// Caller context (session/task/channel).
    pub context: PermissionContext,
}

/// How a resolver answered. Distinct from [`crate::Decision`] (which also has
/// `Ask`): a resolver is only ever asked "allow or deny this already-Ask
/// request". The two `Allow…Session`/`Allow…Workspace` tiers express an
/// operator's scoped permission memory: the gate records them as a grant for
/// the exact `(session?, tool, scope)` that was approved, so identical
/// follow-up requests are released without re-prompting. They can never
/// override a policy `Deny` (grants are consulted only after the evaluator
/// already returned `Ask`) and they never widen beyond the approved scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionDecision {
    Allow,
    Deny,
    /// Allow this exact request and every identical request from the SAME
    /// session (in-memory; dies with the process/session).
    #[serde(rename = "allow_session")]
    AllowSession,
    /// Allow this exact request and every identical request from the current
    /// workspace (in-memory; dies with the process, any session).
    #[serde(rename = "allow_workspace")]
    AllowWorkspace,
}

/// A human-in-the-loop / policy harness that answers `Ask` requests.
///
/// Implementations decide on the basis of the [`ApprovalRequest`]; they cannot
/// invent permissions the policy did not already evaluate as `Ask`.
#[async_trait]
pub trait PermissionResolver: Send + Sync {
    async fn resolve(&self, req: &ApprovalRequest) -> Result<PermissionDecision, PermissionError>;
}

/// Deterministic resolver for tests and offline harnesses. Serves a scripted
/// verdict per requested tool and counts consult/allow/deny for assertions.
#[derive(Debug, Default)]
pub struct MockResolver {
    allow_tools: Mutex<Vec<String>>,
    consult_count: AtomicU64,
    allow_count: AtomicU64,
    deny_count: AtomicU64,
}

impl MockResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Approve requests for the given tool ids; deny everything else.
    pub fn allow_only(tools: &[&str]) -> Self {
        Self {
            allow_tools: Mutex::new(tools.iter().map(|s| (*s).to_string()).collect()),
            consult_count: AtomicU64::new(0),
            allow_count: AtomicU64::new(0),
            deny_count: AtomicU64::new(0),
        }
    }

    /// Approve every request this resolver is actually asked.
    pub fn allow_all() -> Self {
        Self::allow_only(&["*"])
    }

    /// Reject every request (still consulted! — proves fail-closed wiring).
    pub fn deny_all() -> Self {
        Self::new()
    }

    pub fn consult_count(&self) -> u64 {
        self.consult_count.load(Ordering::Relaxed)
    }

    pub fn allow_count(&self) -> u64 {
        self.allow_count.load(Ordering::Relaxed)
    }

    pub fn deny_count(&self) -> u64 {
        self.deny_count.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl PermissionResolver for MockResolver {
    async fn resolve(&self, req: &ApprovalRequest) -> Result<PermissionDecision, PermissionError> {
        self.consult_count.fetch_add(1, Ordering::Relaxed);
        let allow = {
            let allowed = self.allow_tools.lock().unwrap();
            allowed.iter().any(|t| t == "*" || t == &req.tool)
        };
        if allow {
            self.allow_count.fetch_add(1, Ordering::Relaxed);
            Ok(PermissionDecision::Allow)
        } else {
            self.deny_count.fetch_add(1, Ordering::Relaxed);
            Ok(PermissionDecision::Deny)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(tool: &str) -> ApprovalRequest {
        ApprovalRequest {
            request_id: "r1".into(),
            tool: tool.into(),
            args: serde_json::json!({}),
            scope: None,
            context: PermissionContext::default(),
        }
    }

    #[tokio::test]
    async fn allow_only_script() {
        let r = MockResolver::allow_only(&["echo"]);
        assert_eq!(
            r.resolve(&ask("echo")).await.unwrap(),
            PermissionDecision::Allow
        );
        assert_eq!(
            r.resolve(&ask("rm")).await.unwrap(),
            PermissionDecision::Deny
        );
        assert_eq!(r.consult_count(), 2);
        assert_eq!(r.allow_count(), 1);
        assert_eq!(r.deny_count(), 1);
    }

    #[tokio::test]
    async fn allow_all_and_deny_all() {
        let r = MockResolver::allow_all();
        assert_eq!(
            r.resolve(&ask("anything")).await.unwrap(),
            PermissionDecision::Allow
        );
        let d = MockResolver::deny_all();
        assert_eq!(
            d.resolve(&ask("anything")).await.unwrap(),
            PermissionDecision::Deny
        );
    }
}
