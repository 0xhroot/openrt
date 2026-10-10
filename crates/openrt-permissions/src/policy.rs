//! Policy model, rules, and evaluation.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::PermissionError;

/// A validated capability string of the form `domain.resource.action`
/// (e.g. `filesystem.read`, `shell.execute`, `network.local.connect`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Capability(String);

impl Capability {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn from_str_unchecked(s: &str) -> Self {
        Self(s.to_string())
    }

    /// Classify by a conservative domain allowlist. `false` does not mean
    /// dangerous, merely "not obviously harmless" — used only for hinting.
    pub fn is_harmless(&self) -> bool {
        const HARMLESS_PREFIXES: [&str; 4] =
            ["echo", "system.info", "filesystem.read", "network.local"];
        HARMLESS_PREFIXES.iter().any(|p| self.0.starts_with(p))
    }
}

impl FromStr for Capability {
    type Err = PermissionError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('.').filter(|p| !p.is_empty()).collect();
        if parts.len() != 3 || s.contains(char::is_whitespace) {
            return Err(PermissionError::InvalidCapability(s.to_string()));
        }
        Ok(Self(s.to_string()))
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Context available to the evaluator (session, tool, channel, …).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionContext {
    pub session_id: Option<String>,
    pub channel: Option<String>,
    pub task_id: Option<String>,
}

impl PermissionContext {
    pub fn new() -> Self {
        Self::default()
    }
}

/// What to do when a rule matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    Allow,
    Deny,
    /// Requires human/agent-in-the-loop approval.
    Ask,
}

/// One policy rule. The first matching rule in policy order wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRule {
    /// Pattern matched against the *effective* identifier: the requested
    /// capability when present, else the tool id. Supports `*` wildcards.
    pub pattern: String,
    pub action: RuleAction,
    /// Optional glob applied to the request scope (path/url/…).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl PolicyRule {
    pub fn allow(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            action: RuleAction::Allow,
            scope: None,
        }
    }
    pub fn deny(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            action: RuleAction::Deny,
            scope: None,
        }
    }
    pub fn ask(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            action: RuleAction::Ask,
            scope: None,
        }
    }

    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = Some(scope.into());
        self
    }
}

/// A permission request under evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    pub tool_id: String,
    pub capability: Option<Capability>,
    pub scope: Option<String>,
    pub context: PermissionContext,
}

impl PermissionRequest {
    /// Request evaluated purely by tool id (no declared capability). The
    /// caller-supplied context is preserved so policies can route the request
    /// session/channel-aware.
    pub fn tool(tool_id: &str, args: &serde_json::Value, ctx: &PermissionContext) -> Self {
        let scope = extract_scope(args);
        Self {
            tool_id: tool_id.to_string(),
            capability: None,
            scope,
            context: ctx.clone(),
        }
    }

    /// Request evaluated by declared capability + optional scope.
    pub fn capability(tool_id: &str, capability: Capability, scope: Option<String>) -> Self {
        Self {
            tool_id: tool_id.to_string(),
            capability: Some(capability),
            scope,
            context: PermissionContext::default(),
        }
    }
}

/// Public scope extractor used by the gate when evaluating `required_caps`
/// (same keys as tool-id requests).
pub fn extract_scope_public(args: &serde_json::Value) -> Option<String> {
    extract_scope(args)
}

fn extract_scope(args: &serde_json::Value) -> Option<String> {
    for key in ["path", "url", "address", "file"] {
        if let Some(v) = args.get(key).and_then(|v| v.as_str()) {
            return Some(v.to_string());
        }
    }
    None
}

/// The result of evaluating a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    Allow,
    Deny,
    Ask,
}

impl Decision {
    pub fn allows_execution(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// In-memory policy. Serialized like the on-disk `policy.json`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Policy {
    #[serde(default)]
    pub rules: Vec<PolicyRule>,
}

impl Policy {
    /// Everything denied unless explicitly allowed. Default policy of OpenRT.
    pub fn deny_by_default() -> Self {
        Self::default()
    }

    pub fn allow(&mut self, pattern: &str) -> &mut Self {
        self.rules.push(PolicyRule::allow(pattern));
        self
    }

    pub fn allow_with(&mut self, pattern: &str, action: RuleAction) -> &mut Self {
        self.rules.push(PolicyRule {
            pattern: pattern.to_string(),
            action,
            scope: None,
        });
        self
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// Evaluator over a [`Policy`] reference (stateless).
pub struct PolicyEvaluator<'a> {
    policy: &'a Policy,
}

impl<'a> PolicyEvaluator<'a> {
    pub fn new(policy: &'a Policy) -> Self {
        Self { policy }
    }

    pub fn policy(&self) -> &'a Policy {
        self.policy
    }

    /// Evaluate a request: first matching rule wins; no match ⇒ `Deny`.
    pub fn evaluate(&self, req: &PermissionRequest) -> Decision {
        let effective: &str = req
            .capability
            .as_ref()
            .map(|c| c.as_str())
            .unwrap_or(&req.tool_id);
        for rule in &self.policy.rules {
            if !glob_match(&rule.pattern, effective) {
                continue;
            }
            if let Some(scope_glob) = &rule.scope {
                let scope = req.scope.as_deref().unwrap_or_default();
                if !glob_match(scope_glob, scope) {
                    continue;
                }
            }
            return match rule.action {
                RuleAction::Allow => Decision::Allow,
                RuleAction::Deny => Decision::Deny,
                RuleAction::Ask => Decision::Ask,
            };
        }
        Decision::Deny
    }
}

/// Glob matching supporting `*` (any sequence, incl. empty) anywhere.
/// No `?`, no character classes. Case-sensitive.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_pi = Some(pi);
            pi += 1;
            star_ti = ti;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_basics() {
        assert!(glob_match("echo", "echo"));
        assert!(glob_match("*.txt", "notes.txt"));
        assert!(glob_match("filesystem.*", "filesystem.read"));
        assert!(glob_match("*", "anything"));
        assert!(!glob_match("*.txt", "notes.md"));
        assert!(!glob_match("shell*", "filesystem.read"));
    }

    #[test]
    fn capability_parsing() {
        assert!(Capability::from_str("filesystem.read.all").is_ok());
        assert!(Capability::from_str("filesystem.read.all.scope").is_err());
        assert!(Capability::from_str("filesystem").is_err());
    }

    #[test]
    fn first_rule_wins_with_scope() {
        let mut policy = Policy::deny_by_default();
        policy
            .rules
            .push(PolicyRule::allow("filesystem.read.all").with_scope("/tmp/**"));
        policy.rules.push(PolicyRule::deny("filesystem.read.*"));
        let e = PolicyEvaluator::new(&policy);
        let cap = Capability::from_str("filesystem.read.all").unwrap();
        let allow_req =
            PermissionRequest::capability("fs_read", cap.clone(), Some("/tmp/x.txt".into()));
        let deny_req = PermissionRequest::capability("fs_read", cap, Some("/etc/passwd".into()));
        assert_eq!(e.evaluate(&allow_req), Decision::Allow);
        assert_eq!(e.evaluate(&deny_req), Decision::Deny);
    }
}
