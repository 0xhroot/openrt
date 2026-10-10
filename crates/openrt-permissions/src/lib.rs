//! Permission model and policy evaluation.
//!
//! OpenRT is deny-by-default. A request is evaluated against a [`Policy`]:
//! the first matching rule decides. No matching rule ⇒ `Deny`. `Ask` is
//! reserved for interactive approvals and requires a resolver at the caller
//! (P2.2); without a resolver an `Ask` fails closed.
//!
//! [`path_scope`] provides the pure filesystem jail used by filesystem tools
//! (SEC-01); [`ask`] defines the approval-workflow interfaces.

mod ask;
mod error;
mod path_scope;
mod policy;

pub use ask::{ApprovalRequest, MockResolver, PermissionDecision, PermissionResolver};
pub use error::PermissionError;
pub use path_scope::{check_path, check_path_with_root, PathScopeError, ScopeRoot};
pub use policy::{
    extract_scope_public, Capability, Decision, PermissionContext, PermissionRequest, Policy,
    PolicyEvaluator, PolicyRule, RuleAction,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn ctx() -> PermissionContext {
        PermissionContext::default()
    }

    #[test]
    fn deny_by_default() {
        let policy = Policy::default();
        let evaluator = PolicyEvaluator::new(&policy);
        let req = PermissionRequest::tool("echo", &serde_json::json!({}), &ctx());
        assert_eq!(evaluator.evaluate(&req), Decision::Deny);
    }

    #[test]
    fn allowlist_match() {
        let mut policy = Policy::deny_by_default();
        policy.allow("echo");
        let evaluator = PolicyEvaluator::new(&policy);
        let req = PermissionRequest::tool("echo", &serde_json::json!({ "text": "hi" }), &ctx());
        assert_eq!(evaluator.evaluate(&req), Decision::Allow);
    }

    #[test]
    fn wildcard_scope_matches() {
        let mut policy = Policy::deny_by_default();
        policy
            .rules
            .push(PolicyRule::allow("filesystem.read").with_scope("/tmp/*"));
        let evaluator = PolicyEvaluator::new(&policy);
        let req = PermissionRequest::tool(
            "filesystem.read",
            &serde_json::json!({ "path": "/tmp/x/data.txt" }),
            &ctx(),
        );
        assert_eq!(evaluator.evaluate(&req), Decision::Allow);
        let denial = PermissionRequest::tool(
            "filesystem.read",
            &serde_json::json!({ "path": "/etc/passwd" }),
            &ctx(),
        );
        assert_eq!(evaluator.evaluate(&denial), Decision::Deny);
    }

    #[test]
    fn ask_requires_resolver() {
        let mut policy = Policy::deny_by_default();
        policy.allow_with("editor.open", RuleAction::Ask);
        let evaluator = PolicyEvaluator::new(&policy);
        let req = PermissionRequest::tool("editor.open", &serde_json::json!({}), &ctx());
        assert_eq!(evaluator.evaluate(&req), Decision::Ask);
    }

    #[test]
    fn first_matching_rule_wins() {
        let mut policy = Policy::deny_by_default();
        policy.allow_with("editor.open", RuleAction::Deny);
        policy.allow_with("editor.open", RuleAction::Allow);
        let evaluator = PolicyEvaluator::new(&policy);
        let req = PermissionRequest::tool("editor.open", &serde_json::json!({}), &ctx());
        assert_eq!(evaluator.evaluate(&req), Decision::Deny);
    }

    #[test]
    fn capabilities_classified() {
        let safe = Capability::from_str("filesystem.read.all").unwrap();
        assert!(safe.is_harmless());
        let risky = Capability::from_str("shell.execute.all").unwrap();
        assert!(!risky.is_harmless());
    }

    #[test]
    fn unknown_capability_denies() {
        let policy = Policy::deny_by_default();
        let evaluator = PolicyEvaluator::new(&policy);
        let req = PermissionRequest::tool("kube.exec", &serde_json::json!({}), &ctx());
        assert_eq!(evaluator.evaluate(&req), Decision::Deny);
    }

    #[test]
    fn tool_request_preserves_permission_context() {
        // SEC-09: the context supplied by the caller must thread through the
        // request so policies can route session/channel-aware (it was dropped
        // before P2.7).
        let caller_ctx = PermissionContext {
            session_id: Some("s-ctx-1".into()),
            channel: Some("cli".into()),
            task_id: Some("t-1".into()),
        };
        let req =
            PermissionRequest::tool("echo", &serde_json::json!({ "text": "hi" }), &caller_ctx);
        assert_eq!(req.context, caller_ctx);
    }
}
