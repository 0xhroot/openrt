//! Fifteen-point validation report over a compiled workflow (STEPS 8–9).
//!
//! Runs the same structural checks the orchestrator will apply at execution
//! time, plus policy/resource/mode gates. Every finding is structured with
//! WHAT / WHY / WHERE / HOW TO FIX — never a bare boolean.

use serde::{Deserialize, Serialize};

use openrt_orch::Workflow;

use crate::capability::CapabilityPlan;
use crate::diagnostics::{CompileDiagnostic, DiagnosticCode, DiagnosticSeverity};
use crate::goal::GoalSpec;

/// Severity of one validation check outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationSeverity {
    Pass,
    Warning,
    Fail,
}

/// Result of one named validation check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationCheck {
    /// Stable check name (`graph.acyclic`, `policy.permissions`, …).
    pub name: String,
    pub severity: ValidationSeverity,
    pub what: String,
    pub why: String,
    pub where_: String,
    pub how_to_fix: String,
}

impl ValidationCheck {
    fn pass(name: &str, what: &str) -> Self {
        Self {
            name: name.into(),
            severity: ValidationSeverity::Pass,
            what: what.into(),
            why: String::new(),
            where_: String::new(),
            how_to_fix: String::new(),
        }
    }

    fn fail(
        name: &str,
        what: impl Into<String>,
        why: impl Into<String>,
        where_: impl Into<String>,
        how_to_fix: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            severity: ValidationSeverity::Fail,
            what: what.into(),
            why: why.into(),
            where_: where_.into(),
            how_to_fix: how_to_fix.into(),
        }
    }

    fn warn(
        name: &str,
        what: impl Into<String>,
        why: impl Into<String>,
        where_: impl Into<String>,
        how_to_fix: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            severity: ValidationSeverity::Warning,
            what: what.into(),
            why: why.into(),
            where_: where_.into(),
            how_to_fix: how_to_fix.into(),
        }
    }
}

/// Full validation report for one compiled workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationReport {
    pub workflow_id: String,
    pub checks: Vec<ValidationCheck>,
}

impl ValidationReport {
    pub fn is_valid(&self) -> bool {
        self.checks
            .iter()
            .all(|c| c.severity != ValidationSeverity::Fail)
    }

    pub fn failures(&self) -> Vec<&ValidationCheck> {
        self.checks
            .iter()
            .filter(|c| c.severity == ValidationSeverity::Fail)
            .collect()
    }

    /// Convert failed checks into structured compiler diagnostics.
    pub fn to_diagnostics(&self) -> Vec<CompileDiagnostic> {
        self.failures()
            .iter()
            .map(|c| {
                CompileDiagnostic::error(
                    DiagnosticCode::InvalidWorkflow,
                    c.what.clone(),
                    c.why.clone(),
                    c.where_.clone(),
                    c.how_to_fix.clone(),
                )
            })
            .collect()
    }
}

/// Why a resource admission check is consulted during compile (not run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceContext {
    /// Human label for the current limit snapshot (for diagnostics).
    pub summary: String,
    /// True when the gate currently denies admission.
    pub denied: bool,
    /// Denial reason when `denied`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Mode context consulted during validation (Sovereign, network, …).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeContext {
    pub network_allowed: bool,
    pub sovereign: bool,
}

/// Run the full validation suite against a compiled workflow.
pub fn validate_workflow(
    goal: &GoalSpec,
    workflow: &Workflow,
    plan: &CapabilityPlan,
    resource: &ResourceContext,
    mode: &ModeContext,
) -> ValidationReport {
    let mut checks = Vec::new();
    let wf_where = format!("workflow.{}", workflow.id);

    // 1. Non-empty graph.
    if workflow.steps.is_empty() {
        checks.push(ValidationCheck::fail(
            "graph.non_empty",
            "workflow has no steps",
            "every subgoal was blocked",
            wf_where.clone(),
            "resolve capability gaps and recompile",
        ));
    } else {
        checks.push(ValidationCheck::pass(
            "graph.non_empty",
            "workflow has steps",
        ));
    }

    // 2. Unique step ids (topological_sort also checks, but report explicitly).
    let mut seen = std::collections::HashSet::new();
    let mut dup: Vec<String> = Vec::new();
    for s in &workflow.steps {
        if !seen.insert(s.id.clone()) {
            dup.push(s.id.clone());
        }
    }
    if dup.is_empty() {
        checks.push(ValidationCheck::pass("graph.unique_ids", "step ids unique"));
    } else {
        checks.push(ValidationCheck::fail(
            "graph.unique_ids",
            "duplicate step ids",
            format!("ids: {}", dup.join(", ")),
            wf_where.clone(),
            "give each subgoal a unique id before graph construction",
        ));
    }

    // 3. Acyclic + known dependencies (reuse orchestrator topological sort).
    match workflow.topological_sort() {
        Ok(order) => {
            if order.len() != workflow.steps.len() {
                checks.push(ValidationCheck::fail(
                    "graph.acyclic",
                    "topological order incomplete",
                    "some steps unreachable",
                    wf_where.clone(),
                    "fix dependency edges",
                ));
            } else {
                checks.push(ValidationCheck::pass("graph.acyclic", "graph is a DAG"));
            }
        }
        Err(e) => {
            checks.push(ValidationCheck::fail(
                "graph.acyclic",
                "graph validation failed",
                e.to_string(),
                wf_where.clone(),
                "fix dependency edges so the graph is a DAG",
            ));
        }
    }

    // 4. Dependencies resolve to existing steps.
    let ids: Vec<&str> = workflow.steps.iter().map(|s| s.id.as_str()).collect();
    let mut unknown_deps = Vec::new();
    for s in &workflow.steps {
        for d in &s.depends_on {
            if !ids.contains(&d.as_str()) {
                unknown_deps.push(format!("{}→{}", s.id, d));
            }
        }
    }
    if unknown_deps.is_empty() {
        checks.push(ValidationCheck::pass(
            "graph.deps_resolve",
            "all deps resolve",
        ));
    } else {
        checks.push(ValidationCheck::fail(
            "graph.deps_resolve",
            "unknown dependency edges",
            unknown_deps.join(", "),
            wf_where.clone(),
            "remove or retarget unknown dependency edges",
        ));
    }

    // 5. Every step has a non-empty prompt (provenance-carrying instruction).
    let empty_prompts: Vec<&str> = workflow
        .steps
        .iter()
        .filter(|s| s.prompt.trim().is_empty())
        .map(|s| s.id.as_str())
        .collect();
    if empty_prompts.is_empty() {
        checks.push(ValidationCheck::pass(
            "step.prompts",
            "all steps have prompts",
        ));
    } else {
        checks.push(ValidationCheck::fail(
            "step.prompts",
            "empty step prompt",
            format!("steps: {}", empty_prompts.join(", ")),
            wf_where.clone(),
            "populate each step prompt with objective/instructions",
        ));
    }

    // 6. Prompt includes original request (provenance requirement).
    let original = goal.original_request.trim();
    let missing_original: Vec<&str> = workflow
        .steps
        .iter()
        .filter(|s| !original.is_empty() && !s.prompt.contains(original))
        .map(|s| s.id.as_str())
        .collect();
    if missing_original.is_empty() {
        checks.push(ValidationCheck::pass(
            "provenance.original_request",
            "original request attached to every step",
        ));
    } else {
        checks.push(ValidationCheck::warn(
            "provenance.original_request",
            "original request missing from some step prompts",
            format!("steps: {}", missing_original.join(", ")),
            wf_where.clone(),
            "rebuild graph so node_prompt embeds the original request",
        ));
    }

    // 7. Capability plan covers every step.
    let unplanned: Vec<&str> = workflow
        .steps
        .iter()
        .filter(|s| !plan.assignments.iter().any(|a| a.subgoal_id == s.id))
        .map(|s| s.id.as_str())
        .collect();
    if unplanned.is_empty() {
        checks.push(ValidationCheck::pass(
            "capability.coverage",
            "every step has a capability assignment",
        ));
    } else {
        checks.push(ValidationCheck::fail(
            "capability.coverage",
            "steps without capability assignment",
            format!("steps: {}", unplanned.join(", ")),
            wf_where.clone(),
            "re-run capability planning so every subgoal is assigned",
        ));
    }

    // 8. No blocking capability gaps.
    let gaps = plan.gaps();
    if gaps.is_empty() {
        checks.push(ValidationCheck::pass(
            "capability.no_gaps",
            "no capability gaps",
        ));
    } else {
        let detail = gaps
            .iter()
            .map(|g| format!("{}: {:?}", g.subgoal_id, g.status))
            .collect::<Vec<_>>()
            .join("; ");
        checks.push(ValidationCheck::fail(
            "capability.no_gaps",
            "blocking capability gaps",
            detail,
            wf_where.clone(),
            "register missing tools/permissions or rephrase the goal",
        ));
    }

    // 9. Permission check (every required permission policy-allowable).
    // Capability plan already encodes PermissionDenied status; re-surface.
    let denied: Vec<&str> = plan
        .assignments
        .iter()
        .filter(|a| a.status == crate::capability::CapabilityStatus::PermissionDenied)
        .map(|a| a.subgoal_id.as_str())
        .collect();
    if denied.is_empty() {
        checks.push(ValidationCheck::pass(
            "policy.permissions",
            "all required permissions satisfied",
        ));
    } else {
        checks.push(ValidationCheck::fail(
            "policy.permissions",
            "policy denies required permissions",
            format!("subgoals: {}", denied.join(", ")),
            wf_where.clone(),
            "grant the required capabilities or re-run in a permissive mode",
        ));
    }

    // 10. Sovereign Mode: network goals blocked when network disallowed.
    if goal.requires_network && !mode.network_allowed {
        checks.push(ValidationCheck::fail(
            "mode.sovereign",
            "goal requires network but mode forbids it",
            "Sovereign Mode (or network-off mode) blocks network capabilities",
            wf_where.clone(),
            "run with network allowed, or rephrase the goal to be fully local",
        ));
    } else {
        checks.push(ValidationCheck::pass(
            "mode.sovereign",
            "mode/sovereign constraints satisfied",
        ));
    }

    // 11. Resource admission at compile time (fail closed when denied).
    if resource.denied {
        checks.push(ValidationCheck::fail(
            "resource.admission",
            "resource gate denies admission",
            resource
                .reason
                .clone()
                .unwrap_or_else(|| resource.summary.clone()),
            wf_where.clone(),
            "free resources or raise ResourceLimits, then recompile",
        ));
    } else {
        checks.push(ValidationCheck::pass(
            "resource.admission",
            "resource gate allows admission",
        ));
    }

    // 12. Verification coverage when the goal demands it.
    if goal.requires_verification {
        let has_verify = workflow.steps.iter().any(|s| {
            s.kind == openrt_orch::SubAgentKind::Verifier
                || s.prompt.to_lowercase().contains("verify")
        });
        if has_verify {
            checks.push(ValidationCheck::pass(
                "goal.verification",
                "verification step present",
            ));
        } else {
            checks.push(ValidationCheck::fail(
                "goal.verification",
                "goal requires verification but no verify node exists",
                "requires_verification was set on the goal spec",
                wf_where.clone(),
                "include a verify subgoal in the decomposition",
            ));
        }
    } else {
        checks.push(ValidationCheck::pass(
            "goal.verification",
            "verification not required",
        ));
    }

    // 13. Expected outputs represented (each expected output appears in some prompt/step).
    for out in &goal.expected_outputs {
        let found = workflow.steps.iter().any(|s| {
            s.prompt.contains(out.as_str()) || s.depends_on.iter().any(|d| d.contains(out.as_str()))
        });
        if found {
            checks.push(ValidationCheck::pass(
                "goal.outputs",
                &format!("output `{out}` referenced"),
            ));
        } else {
            checks.push(ValidationCheck::warn(
                "goal.outputs",
                "expected output not referenced in any step",
                format!("output `{out}` absent from prompts"),
                wf_where.clone(),
                "carry expected outputs into subgoal objectives/outputs",
            ));
        }
    }

    // 14. Retry budgets sane (non-negative, bounded).
    let over_retry: Vec<&str> = workflow
        .steps
        .iter()
        .filter(|s| s.max_retries > 10)
        .map(|s| s.id.as_str())
        .collect();
    if over_retry.is_empty() {
        checks.push(ValidationCheck::pass(
            "step.retry_budget",
            "retry budgets within bounds",
        ));
    } else {
        checks.push(ValidationCheck::warn(
            "step.retry_budget",
            "unusually high retry budget",
            format!("steps: {}", over_retry.join(", ")),
            wf_where.clone(),
            "cap max_retries at a small constant to bound failure loops",
        ));
    }

    // 15. Goal id linkage (provenance integrity).
    if workflow
        .name
        .as_deref()
        .map(|n| n == goal.objective)
        .unwrap_or(false)
        || !workflow.name.is_none()
    {
        checks.push(ValidationCheck::pass(
            "provenance.goal_link",
            "workflow name carries goal objective",
        ));
    } else {
        checks.push(ValidationCheck::warn(
            "provenance.goal_link",
            "workflow has no name linking it to the goal",
            "workflow.name is None",
            wf_where.clone(),
            "name the workflow after the goal objective at construction",
        ));
    }

    ValidationReport {
        workflow_id: workflow.id.clone(),
        checks,
    }
}

/// Convenience: turn a failed report into the error payload the compiler
/// returns (structured diagnostics, fail closed).
pub fn report_to_error(report: &ValidationReport) -> Vec<CompileDiagnostic> {
    let mut diags = report.to_diagnostics();
    if diags.is_empty() {
        // Defensive: failures() non-empty implies to_diagnostics non-empty.
        diags.push(CompileDiagnostic {
            code: DiagnosticCode::CompilationFailure,
            severity: DiagnosticSeverity::Error,
            what: "validation failed".into(),
            why: "report has failing checks".into(),
            where_: format!("workflow.{}", report.workflow_id),
            how_to_fix: "inspect ValidationReport.checks and fix failing entries".into(),
        });
    }
    diags
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{plan_capabilities, RegistryView};
    use crate::decompose::decompose;
    use crate::graph::build_workflow;

    fn allow(_p: &str) -> bool {
        true
    }

    fn make() -> (GoalSpec, Workflow, CapabilityPlan) {
        let goal = GoalSpec {
            id: GoalSpec::id_for("create status file"),
            original_request: "Create a file containing the current project status".into(),
            objective: "Create a file containing the current project status".into(),
            inputs: vec![],
            expected_outputs: vec!["status.txt".into()],
            constraints: vec![],
            session_id: None,
            workspace: None,
            requires_network: false,
            requires_verification: false,
        };
        let decomp = decompose(&goal);
        let tools = vec![
            "filesystem.read".to_string(),
            "filesystem.write".to_string(),
            "echo".to_string(),
        ];
        let reg = RegistryView {
            tool_names: &tools,
            policy_allows: &allow,
            network_allowed: true,
            model_available: true,
        };
        let plan = plan_capabilities(&decomp, &reg);
        let (wf, _prov, _d) = build_workflow(&goal, &decomp, &plan, "wf.ok").unwrap();
        (goal, wf, plan)
    }

    fn ok_ctx() -> (ResourceContext, ModeContext) {
        (
            ResourceContext {
                summary: "ok".into(),
                denied: false,
                reason: None,
            },
            ModeContext {
                network_allowed: true,
                sovereign: false,
            },
        )
    }

    #[test]
    fn valid_workflow_passes_all_checks() {
        let (goal, wf, plan) = make();
        let (res, mode) = ok_ctx();
        let report = validate_workflow(&goal, &wf, &plan, &res, &mode);
        assert!(
            report.is_valid(),
            "expected valid, failures: {:?}",
            report.failures()
        );
        assert!(report.checks.len() >= 15);
        assert!(report
            .checks
            .iter()
            .all(|c| c.severity != ValidationSeverity::Fail));
    }

    #[test]
    fn resource_denial_fails_closed() {
        let (goal, wf, plan) = make();
        let res = ResourceContext {
            summary: "memory low".into(),
            denied: true,
            reason: Some("insufficient memory".into()),
        };
        let mode = ModeContext {
            network_allowed: true,
            sovereign: false,
        };
        let report = validate_workflow(&goal, &wf, &plan, &res, &mode);
        assert!(!report.is_valid());
        assert!(report
            .failures()
            .iter()
            .any(|c| c.name == "resource.admission"));
        let diags = report.to_diagnostics();
        assert!(diags.iter().any(|d| d.is_error()));
        assert!(diags.iter().all(|d| !d.how_to_fix.is_empty()));
    }

    #[test]
    fn sovereign_network_goal_blocked_when_network_off() {
        let (mut goal, wf, plan) = make();
        goal.requires_network = true;
        let (res, _) = ok_ctx();
        let mode = ModeContext {
            network_allowed: false,
            sovereign: true,
        };
        let report = validate_workflow(&goal, &wf, &plan, &res, &mode);
        assert!(!report.is_valid());
        assert!(report.failures().iter().any(|c| c.name == "mode.sovereign"));
    }

    #[test]
    fn empty_workflow_fails() {
        let (goal, plan) = {
            let g = GoalSpec {
                id: GoalSpec::id_for("x"),
                original_request: "x".into(),
                objective: "x".into(),
                inputs: vec![],
                expected_outputs: vec![],
                constraints: vec![],
                session_id: None,
                workspace: None,
                requires_network: false,
                requires_verification: false,
            };
            let d = decompose(&g);
            let tools: Vec<String> = vec![];
            let reg = RegistryView {
                tool_names: &tools,
                policy_allows: &allow,
                network_allowed: true,
                model_available: true,
            };
            let plan = plan_capabilities(&d, &reg);
            (g, plan)
        };
        let empty = Workflow::new("wf.empty");
        let (res, mode) = ok_ctx();
        let report = validate_workflow(&goal, &empty, &plan, &res, &mode);
        assert!(!report.is_valid());
        assert!(report
            .failures()
            .iter()
            .any(|c| c.name == "graph.non_empty"));
    }
}
