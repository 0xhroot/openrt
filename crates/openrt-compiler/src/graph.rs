//! Graph construction: subgoals + capability plan → existing
//! [`openrt_orch::Workflow`] (STEPS 6–7). Edges are real `depends_on`
//! relationships, never comments.

use serde::{Deserialize, Serialize};

use openrt_orch::{SubAgentKind, Workflow, WorkflowStep};

use crate::capability::{CapabilityPlan, CapabilityStatus};
use crate::decompose::DecomposedGoal;
use crate::diagnostics::{CompileDiagnostic, DiagnosticCode};
use crate::goal::GoalSpec;

/// Why a compiled node exists (STEP 10 explainability).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeProvenance {
    pub step_id: String,
    pub subgoal_id: String,
    pub why_exists: String,
    pub why_capability: String,
    pub dependencies: Vec<String>,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<String>,
}

/// Full provenance blob attached to a compiled workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// Original natural-language request (always attached).
    pub original_request: String,
    pub goal_id: String,
    pub compiler_version: String,
    pub nodes: Vec<NodeProvenance>,
    /// Source of any prior history that influenced the plan (empty = none).
    #[serde(default)]
    pub history_sources: Vec<String>,
}

fn role_kind(role: &str) -> Result<SubAgentKind, String> {
    match role {
        "planner" => Ok(SubAgentKind::Planner),
        "researcher" => Ok(SubAgentKind::Researcher),
        "coder" => Ok(SubAgentKind::Coder),
        "verifier" => Ok(SubAgentKind::Verifier),
        "critic" => Ok(SubAgentKind::Critic),
        other => Err(format!("unknown role `{other}`")),
    }
}

/// Prompt for a compiled node: includes objective, inputs, outputs, and
/// verification so the role agent is fully instructed without extra state.
fn node_prompt(
    objective: &str,
    inputs: &[String],
    outputs: &[String],
    verification: Option<&str>,
    original: &str,
) -> String {
    let mut p = String::new();
    p.push_str(objective);
    if !inputs.is_empty() {
        p.push_str("\nInputs: ");
        p.push_str(&inputs.join(", "));
    }
    if !outputs.is_empty() {
        p.push_str("\nExpected outputs: ");
        p.push_str(&outputs.join(", "));
    }
    if let Some(v) = verification {
        p.push_str("\nSuccess when: ");
        p.push_str(v);
    }
    p.push_str("\nOriginal user request: ");
    p.push_str(original);
    p
}

/// Build the executable workflow graph. Fails closed with diagnostics when a
/// capability gap, unknown role, or structural issue is present.
pub fn build_workflow(
    goal: &GoalSpec,
    decomp: &DecomposedGoal,
    plan: &CapabilityPlan,
    workflow_id: &str,
) -> Result<(Workflow, Provenance, Vec<CompileDiagnostic>), Vec<CompileDiagnostic>> {
    let mut diagnostics = Vec::new();
    let mut steps = Vec::new();
    let mut nodes = Vec::new();

    // Map subgoal → assignment (order-preserving).
    let assign = |id: &str| plan.assignments.iter().find(|a| a.subgoal_id == id);

    for sg in &decomp.subgoals {
        let Some(a) = assign(&sg.id) else {
            diagnostics.push(CompileDiagnostic::error(
                DiagnosticCode::MissingCapability,
                "capability plan incomplete",
                format!("no capability assignment for subgoal `{}`", sg.id),
                format!("subgoal.{}", sg.id),
                "re-run capability planning so every subgoal has an assignment",
            ));
            continue;
        };

        if a.status != CapabilityStatus::Available {
            let code = match a.status {
                CapabilityStatus::PermissionDenied => DiagnosticCode::PermissionDenied,
                CapabilityStatus::SovereignBlocked => DiagnosticCode::SovereignConflict,
                CapabilityStatus::Unavailable => {
                    if a.capability == crate::capability::CapabilityClass::ArtifactWrite {
                        // Artifact backends (or a missing generator for
                        // image/video/audio) are a tool-availability problem,
                        // reported honestly as unavailable_tool.
                        DiagnosticCode::UnavailableTool
                    } else if a.tool.is_none() && a.capability.as_str().starts_with("model.") {
                        DiagnosticCode::UnavailableModel
                    } else {
                        DiagnosticCode::MissingCapability
                    }
                }
                CapabilityStatus::Available => unreachable!(),
            };
            diagnostics.push(CompileDiagnostic::error(
                code,
                format!("capability unavailable for `{}`", sg.id),
                a.reason.clone().unwrap_or_else(|| "unknown".into()),
                format!("subgoal.{}", sg.id),
                format!(
                    "register the required capability/tool or rephrase the goal; assignment: {}",
                    a.why
                ),
            ));
            continue;
        }

        let kind = match role_kind(&a.role) {
            Ok(k) => k,
            Err(e) => {
                diagnostics.push(CompileDiagnostic::error(
                    DiagnosticCode::CompilationFailure,
                    "invalid role binding",
                    e,
                    format!("subgoal.{}", sg.id),
                    "map subgoals only to planner|researcher|coder|verifier|critic",
                ));
                continue;
            }
        };

        // Dependencies: only subgoals that also produced a step.
        let deps: Vec<String> = sg
            .depends_on
            .iter()
            .filter(|d| assign(d).map(|x| x.status == CapabilityStatus::Available) == Some(true))
            .cloned()
            .collect();
        // Dropped deps (blocked dependents) are a diagnostic, not a silent edge removal.
        for d in &sg.depends_on {
            if !deps.contains(d) {
                if let Some(bad) = assign(d) {
                    if bad.status != CapabilityStatus::Available {
                        diagnostics.push(CompileDiagnostic::error(
                            DiagnosticCode::DependencyConflict,
                            "dependency blocked",
                            format!("subgoal `{}` depends on blocked subgoal `{d}`", sg.id),
                            format!("subgoal.{}", sg.id),
                            "resolve the dependency's capability gap or remove the dependency",
                        ));
                    }
                }
            }
        }

        let prompt = node_prompt(
            &sg.objective,
            &sg.required_inputs,
            &sg.expected_outputs,
            sg.verification.as_deref(),
            &goal.original_request,
        );

        let mut step = WorkflowStep::new(&sg.id, kind, prompt);
        step.depends_on = deps.clone();
        // Default retry: one retry for model/tool transient failures (engine
        // treats max_retries as additional attempts after the first).
        step.max_retries = 1;
        steps.push(step);

        nodes.push(NodeProvenance {
            step_id: sg.id.clone(),
            subgoal_id: sg.id.clone(),
            why_exists: format!("decomposed from goal `{}`: {}", goal.id, sg.objective),
            why_capability: a.why.clone(),
            dependencies: deps,
            inputs: sg.required_inputs.clone(),
            outputs: sg.expected_outputs.clone(),
            verification: sg.verification.clone(),
        });
    }

    if steps.is_empty() {
        diagnostics.push(CompileDiagnostic::error(
            DiagnosticCode::CompilationFailure,
            "no executable nodes",
            "every subgoal was blocked or produced no workflow step",
            format!("goal.{}", goal.id),
            "resolve capability gaps listed above and recompile",
        ));
        return Err(diagnostics);
    }

    let workflow = Workflow::new(workflow_id).named(goal.objective.clone());
    let mut workflow = workflow;
    for s in steps {
        workflow = workflow.step(s);
    }

    // Structural pre-check via the orchestrator's own topological sort.
    if let Err(e) = workflow.topological_sort() {
        diagnostics.push(CompileDiagnostic::error(
            DiagnosticCode::InvalidWorkflow,
            "graph validation failed",
            e.to_string(),
            format!("workflow.{workflow_id}"),
            "fix dependency edges so the graph is a DAG",
        ));
        return Err(diagnostics);
    }

    let provenance = Provenance {
        original_request: goal.original_request.clone(),
        goal_id: goal.id.clone(),
        compiler_version: crate::COMPILER_VERSION.to_string(),
        nodes,
        history_sources: Vec::new(),
    };

    // Errors already returned; remaining diagnostics are warnings only.
    if diagnostics.iter().any(|d| d.is_error()) {
        return Err(diagnostics);
    }
    Ok((workflow, provenance, diagnostics))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{plan_capabilities, RegistryView};
    use crate::decompose::decompose;

    fn allow(_p: &str) -> bool {
        true
    }

    fn make() -> (GoalSpec, DecomposedGoal, CapabilityPlan) {
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
        (goal, decomp, plan)
    }

    #[test]
    fn builds_valid_dag_with_provenance() {
        let (goal, decomp, plan) = make();
        let (wf, prov, diags) = build_workflow(&goal, &decomp, &plan, "wf.test").unwrap();
        assert!(diags.iter().all(|d| !d.is_error()));
        assert!(!wf.steps.is_empty());
        assert_eq!(prov.original_request, goal.original_request);
        assert_eq!(prov.nodes.len(), wf.steps.len());
        for n in &prov.nodes {
            assert!(wf.find_step(&n.step_id).is_some());
            assert!(!n.why_exists.is_empty());
            assert!(!n.why_capability.is_empty());
        }
        wf.topological_sort().unwrap();
    }

    #[test]
    fn blocked_capability_fails_closed() {
        let (goal, decomp, mut plan) = make();
        for a in &mut plan.assignments {
            a.status = CapabilityStatus::Unavailable;
            a.reason = Some("missing".into());
        }
        let err = build_workflow(&goal, &decomp, &plan, "wf.bad").unwrap_err();
        assert!(err.iter().any(|d| d.is_error()));
        assert!(err.iter().any(|d| matches!(
            d.code,
            DiagnosticCode::MissingCapability | DiagnosticCode::UnavailableModel
        )));
    }
}
