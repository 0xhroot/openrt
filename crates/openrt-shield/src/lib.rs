//! AgentShield (Phase 10, M10.9): the single security intelligence layer
//! that evaluates potentially dangerous agent behavior before it executes.
//!
//! # One authority, no parallel policy
//!
//! AgentShield **does not** replace any existing authority:
//!
//! * permission decisions stay with `openrt-permissions::PolicyEvaluator`
//!   evaluated by the runtime `GateExecutor`;
//! * network egress stays with `openrt-netpol` + `openrt-modes`;
//! * execution confinement stays with `openrt-sandbox`;
//! * halting stays with the ADR-008 `openrt_safety::KillSwitch` — AgentShield
//!   may *trip* it (`ShieldAction::Halt`) but never owns a second switch;
//! * prompt-injection detection is **reused** from the Phase 9 authority
//!   (`openrt_evidence::injection::detect_injection` + the shared
//!   `openrt-core` trust/injection types) — never re-implemented;
//! * the chained `openrt-audit` log stays the only audit chain.
//!
//! AgentShield adds: a `SecurityFinding` record (persisted under
//! `shield.findings`), a severity model, and a deterministic pre-execution
//! evaluation whose resulting action is mapped onto the existing
//! `Decision`/deny/ask/kill paths in the gate.
//!
//! # Model
//!
//! * [`Severity`] — `INFO < LOW < MEDIUM < HIGH < CRITICAL` (influences action).
//! * [`ShieldAction`] — `Allow < Warn < RequireApproval < Deny < Halt`.
//! * [`SecurityFinding`] — id, category, severity, source, target, reason,
//!   redacted evidence, recommended action, policy (the mapping that decided
//!   the action), status.
//! * [`ShieldEvaluator`] — pure, deterministic evaluation over a
//!   [`ShieldContext`]; recording/persistence happens through
//!   [`record_report`] / [`list_findings`].
//!
//! Findings never carry secrets: evidence is built from structured,
//! non-secret facts (paths, pattern ids, category names, counts) and is run
//! through `openrt_core::redaction` before persistence.

pub mod detectors;
pub mod scan;
pub mod store;

use serde::{Deserialize, Serialize};

pub use detectors::{command_segments, evaluate_command_strings};
pub use openrt_tools::ToolSpec;
pub use scan::{scan_extension, scan_tool_spec, ExtensionScanInput};
pub use store::{get_finding, list_findings, record_report, update_finding_status, SHIELD_NS};

/// Storage namespace for security findings (ADR-006; re-exported from
/// [`store::SHIELD_NS`] for convenience).
pub const SHIELD_FINDINGS_NS: &str = SHIELD_NS;

// ── severity ─────────────────────────────────────────────────────────────────

/// Severity of a finding. Ordering is meaningful (INFO lowest → CRITICAL
/// highest) and drives the default action mapping — severity is never
/// decorative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "INFO",
            Severity::Low => "LOW",
            Severity::Medium => "MEDIUM",
            Severity::High => "HIGH",
            Severity::Critical => "CRITICAL",
        }
    }

    /// One level lower, floored at `Info` (used for sandbox containment
    /// downgrades — a confined execution genuinely risks less host impact).
    pub fn lowered(self) -> Self {
        match self {
            Severity::Critical => Severity::High,
            Severity::High => Severity::Medium,
            Severity::Medium => Severity::Low,
            Severity::Low | Severity::Info => Severity::Info,
        }
    }
}

// ── action ───────────────────────────────────────────────────────────────────

/// What the shield recommends the enforcement point do. Mapped onto the
/// existing authorities: `Allow`/`Warn` proceed, `RequireApproval` routes
/// through the gate's approval (`Decision::Ask`) path, `Deny` becomes the
/// gate's permission denial, `Halt` trips the shared ADR-008 kill switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ShieldAction {
    Allow,
    Warn,
    RequireApproval,
    Deny,
    Halt,
}

impl ShieldAction {
    pub fn as_str(self) -> &'static str {
        match self {
            ShieldAction::Allow => "ALLOW",
            ShieldAction::Warn => "WARN",
            ShieldAction::RequireApproval => "REQUIRE_APPROVAL",
            ShieldAction::Deny => "DENY",
            ShieldAction::Halt => "HALT",
        }
    }

    /// The default action for a severity — the single mapping table that
    /// makes severity influence policy action.
    pub fn for_severity(sev: Severity) -> Self {
        match sev {
            Severity::Info | Severity::Low => ShieldAction::Allow,
            Severity::Medium => ShieldAction::Warn,
            Severity::High => ShieldAction::RequireApproval,
            Severity::Critical => ShieldAction::Deny,
        }
    }
}

// ── categories ───────────────────────────────────────────────────────────────

/// The twelve detection categories AgentShield covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingCategory {
    DangerousCommand,
    NetworkEscalation,
    FilesystemEscape,
    CredentialAccess,
    PromptInjection,
    MaliciousTool,
    DependencyRisk,
    ExcessivePermissions,
    UnsafeMcp,
    SecretAccess,
    SandboxBypass,
    SuspiciousSkill,
}

impl FindingCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            FindingCategory::DangerousCommand => "dangerous_command",
            FindingCategory::NetworkEscalation => "network_escalation",
            FindingCategory::FilesystemEscape => "filesystem_escape",
            FindingCategory::CredentialAccess => "credential_access",
            FindingCategory::PromptInjection => "prompt_injection",
            FindingCategory::MaliciousTool => "malicious_tool",
            FindingCategory::DependencyRisk => "dependency_risk",
            FindingCategory::ExcessivePermissions => "excessive_permissions",
            FindingCategory::UnsafeMcp => "unsafe_mcp",
            FindingCategory::SecretAccess => "secret_access",
            FindingCategory::SandboxBypass => "sandbox_bypass",
            FindingCategory::SuspiciousSkill => "suspicious_skill",
        }
    }
}

// ── status ───────────────────────────────────────────────────────────────────

/// Lifecycle of a recorded finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingStatus {
    /// Recorded, action allowed the call (INFO/LOW).
    Open,
    /// Recorded, the call proceeded with a WARN action.
    Warned,
    /// Recorded, the call required operator approval (granted/denied there).
    ApprovalRequired,
    /// Recorded, the call was denied.
    Denied,
    /// Recorded, the shield halted the call and tripped the kill switch.
    Halted,
    /// Recorded by an extension/package scan (pre-activation, advisory until
    /// an operator acts on it).
    Reported,
}

impl FindingStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            FindingStatus::Open => "open",
            FindingStatus::Warned => "warned",
            FindingStatus::ApprovalRequired => "approval_required",
            FindingStatus::Denied => "denied",
            FindingStatus::Halted => "halted",
            FindingStatus::Reported => "reported",
        }
    }

    /// Status implied by an action when a finding is enforced at execution.
    pub fn for_action(action: ShieldAction) -> Self {
        match action {
            ShieldAction::Allow => FindingStatus::Open,
            ShieldAction::Warn => FindingStatus::Warned,
            ShieldAction::RequireApproval => FindingStatus::ApprovalRequired,
            ShieldAction::Deny => FindingStatus::Denied,
            ShieldAction::Halt => FindingStatus::Halted,
        }
    }
}

// ── finding ──────────────────────────────────────────────────────────────────

/// One security finding. Evidence is a JSON value of *structured, redacted*
/// facts only (paths, pattern ids, category names, counts) — never raw
/// argument values that could contain secrets, never corpus text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecurityFinding {
    pub id: String,
    pub category: FindingCategory,
    pub severity: Severity,
    /// Detector that produced it (e.g. `shield.dangerous_command`).
    pub source: String,
    /// What was evaluated (tool id, skill id, `mcp:<server>`, path…).
    pub target: String,
    /// Human-readable reason built from pattern ids and sanitized targets —
    /// never raw secret material.
    pub reason: String,
    /// Structured, redacted evidence.
    pub evidence: serde_json::Value,
    pub recommended_action: ShieldAction,
    /// The mapping/policy that produced the action (e.g.
    /// `severity:CRITICAL->DENY` or an explicit detector override).
    pub policy: String,
    pub status: FindingStatus,
    pub created_at: String,
}

impl SecurityFinding {
    pub fn new(
        category: FindingCategory,
        severity: Severity,
        source: &str,
        target: impl Into<String>,
        reason: impl Into<String>,
        evidence: serde_json::Value,
    ) -> Self {
        let recommended_action = ShieldAction::for_severity(severity);
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            category,
            severity,
            source: source.to_string(),
            target: target.into(),
            reason: reason.into(),
            evidence: redact_evidence(evidence),
            recommended_action,
            policy: format!(
                "severity:{}->{}",
                severity.as_str(),
                recommended_action.as_str()
            ),
            status: FindingStatus::Reported,
            created_at: openrt_core::now().to_rfc3339(),
        }
    }

    /// Override the recommended action (detector context, e.g. a destructive
    /// command escalating to `Halt`), keeping the policy note honest.
    pub fn with_action(mut self, action: ShieldAction, policy: &str) -> Self {
        self.recommended_action = action;
        self.policy = policy.to_string();
        self
    }

    /// Downgrade severity (sandbox containment), re-deriving the default
    /// action unless one was explicitly overridden.
    pub fn downgraded(mut self) -> Self {
        self.severity = self.severity.lowered();
        self.recommended_action = ShieldAction::for_severity(self.severity);
        self.policy = format!(
            "severity:{}->{} (sandbox containment)",
            self.severity.as_str(),
            self.recommended_action.as_str()
        );
        self
    }

    pub fn with_status(mut self, status: FindingStatus) -> Self {
        self.status = status;
        self
    }
}

/// Run evidence through the SEC-05 redaction policy so a finding can never
/// persist a secret-shaped value, whatever a detector put there.
fn redact_evidence(mut value: serde_json::Value) -> serde_json::Value {
    openrt_core::redaction::RedactionPolicy::new().redact_value(&mut value);
    value
}

// ── context / report ─────────────────────────────────────────────────────────

/// Everything the shield may consider for one evaluation. Built by the gate
/// (tool calls) or by the extension scanner (skill/MCP/plugin packages).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShieldContext {
    /// Where the evaluation happens (`gate`, `skill.execute`, `scan`).
    pub source: String,
    /// Tool id being executed (empty for package scans).
    pub tool: String,
    /// Invocation arguments (untrusted, scanned; never persisted raw).
    pub args: serde_json::Value,
    /// Capabilities the call requires (tool spec `required_caps`).
    pub required_caps: Vec<String>,
    /// Permissions claimed by the caller on behalf of a skill/package
    /// (empty for plain tool calls) — the basis for over-privilege checks.
    pub claimed_permissions: Vec<String>,
    /// Whether execution is confined by a required sandbox profile
    /// (containment may lower severity one level).
    pub sandboxed: bool,
    /// Whether Sovereign Mode is active (network/egress escalation checks).
    pub sovereign: bool,
    pub session_id: Option<String>,
    pub task_id: Option<String>,
}

impl ShieldContext {
    /// Default context for a gate-side tool evaluation.
    pub fn gate(tool: &str, args: serde_json::Value) -> Self {
        Self {
            source: "gate".into(),
            tool: tool.to_string(),
            args,
            required_caps: Vec::new(),
            claimed_permissions: Vec::new(),
            sandboxed: false,
            sovereign: false,
            session_id: None,
            task_id: None,
        }
    }
}

/// The outcome of one shield evaluation: every finding plus the combined
/// action (the most restrictive recommendation wins).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShieldReport {
    pub context_source: String,
    pub target: String,
    pub findings: Vec<SecurityFinding>,
    pub action: ShieldAction,
    pub highest_severity: Option<Severity>,
}

impl ShieldReport {
    /// Combine findings into the most restrictive action.
    pub fn from_findings(source: &str, target: &str, mut findings: Vec<SecurityFinding>) -> Self {
        let mut action = ShieldAction::Allow;
        let mut highest: Option<Severity> = None;
        for f in &findings {
            if f.recommended_action > action {
                action = f.recommended_action;
            }
            if highest.map_or(true, |h| f.severity > h) {
                highest = Some(f.severity);
            }
        }
        findings.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then(a.category.as_str().cmp(b.category.as_str()))
        });
        Self {
            context_source: source.to_string(),
            target: target.to_string(),
            findings,
            action,
            highest_severity: highest,
        }
    }

    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

/// One status a caller may attach before recording (used by the gate).
pub fn status_for_action(action: ShieldAction) -> FindingStatus {
    FindingStatus::for_action(action)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_orders_and_lowers() {
        assert!(Severity::Info < Severity::Low);
        assert!(Severity::Medium < Severity::High);
        assert!(Severity::Critical > Severity::High);
        assert_eq!(Severity::Critical.lowered(), Severity::High);
        assert_eq!(Severity::Info.lowered(), Severity::Info);
    }

    #[test]
    fn severity_drives_default_action() {
        assert_eq!(
            ShieldAction::for_severity(Severity::Info),
            ShieldAction::Allow
        );
        assert_eq!(
            ShieldAction::for_severity(Severity::Low),
            ShieldAction::Allow
        );
        assert_eq!(
            ShieldAction::for_severity(Severity::Medium),
            ShieldAction::Warn
        );
        assert_eq!(
            ShieldAction::for_severity(Severity::High),
            ShieldAction::RequireApproval
        );
        assert_eq!(
            ShieldAction::for_severity(Severity::Critical),
            ShieldAction::Deny
        );
    }

    #[test]
    fn report_combines_most_restrictive_action_and_sorts() {
        let low = SecurityFinding::new(
            FindingCategory::DangerousCommand,
            Severity::Low,
            "t",
            "terminal.run",
            "n",
            serde_json::json!({}),
        );
        let high = SecurityFinding::new(
            FindingCategory::CredentialAccess,
            Severity::High,
            "t",
            "terminal.run",
            "n",
            serde_json::json!({}),
        );
        let report = ShieldReport::from_findings("gate", "terminal.run", vec![low, high]);
        assert_eq!(report.action, ShieldAction::RequireApproval);
        assert_eq!(report.highest_severity, Some(Severity::High));
        assert_eq!(
            report.findings[0].category,
            FindingCategory::CredentialAccess
        );
        assert!(!report.is_clean());
    }

    #[test]
    fn clean_report_allows() {
        let report = ShieldReport::from_findings("gate", "echo", vec![]);
        assert_eq!(report.action, ShieldAction::Allow);
        assert!(report.is_clean());
        assert_eq!(report.highest_severity, None);
    }

    #[test]
    /// CANARY, NOT A CREDENTIAL: sequential placeholder digits, used to prove
    /// the finding's evidence is redacted before it can be serialized.
    fn evidence_is_redacted_at_construction() {
        let f = SecurityFinding::new(
            FindingCategory::SecretAccess,
            Severity::High,
            "t",
            "args",
            "secret-shaped value seen",
            serde_json::json!({ "value": "sk-abc12345678901234567890123456789" }),
        );
        let s = f.evidence.to_string();
        assert!(!s.contains("sk-abc"), "secret must be redacted: {s}");
    }

    #[test]
    fn explicit_action_override_records_policy() {
        let f = SecurityFinding::new(
            FindingCategory::DangerousCommand,
            Severity::Critical,
            "t",
            "terminal.run",
            "n",
            serde_json::json!({}),
        )
        .with_action(
            ShieldAction::Halt,
            "detector:irreversible_host_destruction->HALT",
        );
        assert_eq!(f.recommended_action, ShieldAction::Halt);
        assert!(f.policy.contains("HALT"));
    }
}
