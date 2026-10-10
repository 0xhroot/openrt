//! The eight-stage workflow compiler entry point (STEPS 1–11).
//!
//! Natural language → goal → decomposition → capability plan → graph →
//! validation → (optional) persistence → handoff to the existing orchestrator.
//! Bounded replanning is attempted once when the first decomposition fails
//! validation for a *correctable* reason; all other failures are final.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use openrt_orch::Workflow;
use openrt_persistence::{PersistenceError, PersistenceStore};

use crate::capability::{plan_capabilities, RegistryView};
use crate::decompose::decompose;
use crate::diagnostics::{CompileDiagnostic, DiagnosticCode};
use crate::goal::{GoalSpec, NaturalLanguageGoal};
use crate::graph::{build_workflow, Provenance};
use crate::modes::CompileMode;
use crate::persist::{persist_compiled, persist_dag_workflow};
use crate::validate::{validate_workflow, ModeContext, ResourceContext, ValidationReport};

/// Eight compilation stages, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompileStage {
    Understand,
    Decompose,
    PlanCapabilities,
    ConstructGraph,
    ValidateGraph,
    ValidatePolicy,
    Persist,
    Handoff,
}

impl CompileStage {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Understand => "understand",
            Self::Decompose => "decompose",
            Self::PlanCapabilities => "plan_capabilities",
            Self::ConstructGraph => "construct_graph",
            Self::ValidateGraph => "validate_graph",
            Self::ValidatePolicy => "validate_policy",
            Self::Persist => "persist",
            Self::Handoff => "handoff",
        }
    }
}

/// Everything the compiler needs from the live environment.
#[derive(Clone)]
pub struct CompileInput {
    pub goal: NaturalLanguageGoal,
    pub mode: CompileMode,
    /// Real registered tool names (`registry.specs()` names).
    pub tool_names: Vec<String>,
    /// Policy check closure: returns true when `perm` is allowed.
    pub policy_allows: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    pub network_allowed: bool,
    pub model_available: bool,
    pub resource: ResourceContext,
    pub mode_ctx: ModeContext,
    /// Optional LLM-produced plan JSON (schema-validated; used only when the
    /// deterministic decomposer is to be overridden).
    pub llm_plan_json: Option<String>,
    /// Max replanning attempts when validation fails correctably (default 1).
    pub max_replan: u32,
}

impl CompileInput {
    /// Default input with permissive-but-real policy (no closure → allow-all
    /// is NEVER assumed; callers must supply the live policy).
    pub fn new(goal: NaturalLanguageGoal) -> Self {
        Self {
            goal,
            mode: CompileMode::Compile,
            tool_names: Vec::new(),
            policy_allows: Arc::new(|_| false),
            network_allowed: true,
            model_available: true,
            resource: ResourceContext {
                summary: "unchecked".into(),
                denied: false,
                reason: None,
            },
            mode_ctx: ModeContext {
                network_allowed: true,
                sovereign: false,
            },
            llm_plan_json: None,
            max_replan: 1,
        }
    }

    pub fn with_tools(mut self, names: Vec<String>) -> Self {
        self.tool_names = names;
        self
    }

    pub fn with_policy(mut self, f: Arc<dyn Fn(&str) -> bool + Send + Sync>) -> Self {
        self.policy_allows = f;
        self
    }

    pub fn with_mode(mut self, mode: CompileMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_network(mut self, allowed: bool) -> Self {
        self.network_allowed = allowed;
        self.mode_ctx.network_allowed = allowed;
        self.mode_ctx.sovereign = !allowed;
        self
    }

    pub fn with_model_available(mut self, available: bool) -> Self {
        self.model_available = available;
        self
    }

    pub fn with_resource(mut self, resource: ResourceContext) -> Self {
        self.resource = resource;
        self
    }

    pub fn with_llm_plan(mut self, json: Option<String>) -> Self {
        self.llm_plan_json = json;
        self
    }

    /// Cap bounded replanning attempts (0 disables replan; default 1).
    pub fn with_max_replan(mut self, n: u32) -> Self {
        self.max_replan = n;
        self
    }
}

/// Record of one bounded replanning attempt (original plan + correction + final).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplanRecord {
    /// Decomposition from the first attempt (never discarded).
    pub original: crate::decompose::DecomposedGoal,
    /// Why the first plan was rejected (structured, human-readable).
    pub correction_reason: String,
    /// Number of replans performed (bounded by `max_replan`; always ≥ 1 here).
    pub attempts: u32,
    /// Decomposition kept after the successful replan.
    pub final_decomp: crate::decompose::DecomposedGoal,
}

/// Result of a successful compilation (failures return `Err(Vec<Diagnostic>)`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompileOutput {
    pub workflow_id: String,
    pub mode: CompileMode,
    pub stage_reached: CompileStage,
    pub goal: GoalSpec,
    pub workflow: Workflow,
    pub provenance: Provenance,
    pub validation: Option<ValidationReport>,
    pub diagnostics: Vec<CompileDiagnostic>,
    pub persisted: bool,
    /// Set when bounded replanning ran (original + reason + final preserved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replan: Option<ReplanRecord>,
}

/// The workflow compiler. Deterministic given unchanged inputs.
#[derive(Debug, Default, Clone)]
pub struct WorkflowCompiler;

impl WorkflowCompiler {
    pub fn new() -> Self {
        Self
    }

    /// Compile a natural-language goal into an executable [`Workflow`].
    ///
    /// Fails closed with structured diagnostics on any unsupported goal,
    /// capability gap, permission denial, Sovereign conflict, invalid LLM plan,
    /// or validation failure. Never fabricates a node for a missing capability.
    pub async fn compile(
        &self,
        input: &CompileInput,
        store: Option<&Arc<dyn PersistenceStore>>,
    ) -> Result<CompileOutput, Vec<CompileDiagnostic>> {
        let mut diagnostics: Vec<CompileDiagnostic> = Vec::new();
        let request = input.goal.request.trim().to_string();
        if request.is_empty() {
            return Err(vec![CompileDiagnostic::error(
                DiagnosticCode::EmptyGoal,
                "empty goal",
                "natural-language request is blank",
                "goal.request",
                "provide a non-empty request",
            )]);
        }

        // ---- Stage 1: understand ----
        let goal = understand(&request, input);
        if goal.objective.trim().is_empty() {
            return Err(vec![CompileDiagnostic::error(
                DiagnosticCode::UnsupportedGoal,
                "goal could not be understood",
                "objective normalization produced empty text",
                format!("goal.{}", goal.id),
                "restate the request with a clear verb and object",
            )]);
        }

        // ---- Stage 1b: reject ambiguous / unsupported goals (deterministic path).
        // When an LLM plan is supplied it is the plan source and is schema-
        // validated below; NL still must not be ambiguous if we later fall back.
        let used_llm_plan = input.llm_plan_json.is_some();
        if !used_llm_plan {
            if let Err(diag) = reject_unsupported_or_ambiguous(&goal) {
                return Err(vec![diag]);
            }
        }

        // ---- Stage 2: decompose (deterministic primary; LLM optional) ----
        let mut decomp = match &input.llm_plan_json {
            Some(raw) => match crate::capability::parse_llm_plan(&goal.id, raw) {
                Ok(d) => d,
                Err(e) => {
                    return Err(vec![CompileDiagnostic::error(
                            DiagnosticCode::InvalidLlmPlan,
                            "LLM plan rejected",
                            e,
                            "plan.llm",
                            "fix the model output schema (subgoals[{id,objective,depends_on}]) or disable LLM planning",
                        )]);
                }
            },
            None => decompose(&goal),
        };
        let original_decomp_for_replan = decomp.clone();

        if decomp.subgoals.is_empty() {
            return Err(vec![CompileDiagnostic::error(
                DiagnosticCode::CompilationFailure,
                "decomposition empty",
                "decomposer produced no subgoals",
                format!("goal.{}", goal.id),
                "rephrase the goal or supply a valid LLM plan",
            )]);
        }

        // Structural check of decomposition before capability planning.
        if let Err(e) = decomp.topological_order() {
            return Err(vec![CompileDiagnostic::error(
                DiagnosticCode::DependencyConflict,
                "decomposition has dependency errors",
                e,
                format!("goal.{}", goal.id),
                "fix depends_on edges so the decomposition is a DAG",
            )]);
        }

        // ---- Stages 3–5: plan capabilities → construct → validate graph ----
        // First build may return Err outright (capability gaps block steps).
        // Capture that so a bounded LLM-capability replan can still run.
        let first_attempt = self.plan_and_build(&goal, &decomp, input, &mut diagnostics);
        let mut replan: Option<ReplanRecord> = None;
        let (workflow, provenance, first_errors) = match first_attempt {
            Ok((wf, prov, diags)) => {
                let errs: Vec<CompileDiagnostic> =
                    diags.iter().filter(|d| d.is_error()).cloned().collect();
                diagnostics.extend(diags.iter().filter(|d| !d.is_error()).cloned());
                (wf, prov, errs)
            }
            Err(first_errors) => {
                // Correctable only when the LLM-sourced plan hit capability /
                // structure issues that deterministic decompose may avoid.
                let correctable = used_llm_plan
                    && input.max_replan > 0
                    && first_errors.iter().all(|d| {
                        matches!(
                            d.code,
                            DiagnosticCode::MissingCapability
                                | DiagnosticCode::DependencyConflict
                                | DiagnosticCode::UnavailableModel
                                | DiagnosticCode::PermissionDenied
                                | DiagnosticCode::InvalidWorkflow
                        )
                    });
                if !correctable {
                    return Err(first_errors);
                }
                let correction_reason = format!(
                    "first plan failed build ({}); replanning with deterministic decompose",
                    first_errors
                        .iter()
                        .map(|d| d.what.as_str())
                        .collect::<Vec<_>>()
                        .join("; ")
                );
                diagnostics.push(CompileDiagnostic::warning(
                    DiagnosticCode::CompilationFailure,
                    "replanning after build failure",
                    correction_reason.clone(),
                    format!("goal.{}", goal.id),
                    "inspect ReplanRecord.original if the simplified plan is too coarse",
                ));
                decomp = decompose(&goal);
                let (wf, prov, d_f) =
                    self.plan_and_build(&goal, &decomp, input, &mut diagnostics)?;
                let residual: Vec<CompileDiagnostic> =
                    d_f.iter().filter(|d| d.is_error()).cloned().collect();
                diagnostics.extend(d_f.iter().filter(|d| !d.is_error()).cloned());
                if !residual.is_empty() {
                    return Err(residual);
                }
                let plan_f = plan_capabilities(&decomp, &self.registry_view(input));
                let validation_f =
                    validate_workflow(&goal, &wf, &plan_f, &input.resource, &input.mode_ctx);
                if !validation_f.is_valid() {
                    let mut err = validation_f.to_diagnostics();
                    err.extend(diagnostics.iter().filter(|d| d.is_error()).cloned());
                    return Err(err);
                }
                replan = Some(ReplanRecord {
                    original: original_decomp_for_replan,
                    correction_reason,
                    attempts: 1,
                    final_decomp: decomp.clone(),
                });
                return self
                    .finish(
                        input,
                        goal,
                        decomp,
                        plan_f,
                        wf,
                        prov,
                        Some(validation_f),
                        diagnostics,
                        replan,
                        store,
                    )
                    .await;
            }
        };

        // ---- Stage 6: validate policy / resources / mode ----
        let plan = plan_capabilities(&decomp, &self.registry_view(input));
        let mut validation =
            validate_workflow(&goal, &workflow, &plan, &input.resource, &input.mode_ctx);

        // Bounded replanning for validation-stage failures (build succeeded
        // but checks failed). At most `max_replan` retries; never re-invokes LLM.
        let needs_replan = !validation.is_valid() || !first_errors.is_empty();
        if needs_replan && input.max_replan > 0 {
            let mut failure_labels: Vec<String> = validation
                .failures()
                .iter()
                .map(|c| format!("{}: {}", c.name, c.what))
                .collect();
            failure_labels.extend(first_errors.iter().map(|d| d.what.clone()));
            let structural_only = first_errors.is_empty()
                && validation.failures().iter().all(|c| {
                    matches!(
                        c.name.as_str(),
                        "graph.acyclic" | "graph.deps_resolve" | "graph.unique_ids"
                    )
                });
            let llm_capability_correctable = used_llm_plan
                && (first_errors.iter().all(|d| {
                    matches!(
                        d.code,
                        DiagnosticCode::MissingCapability
                            | DiagnosticCode::DependencyConflict
                            | DiagnosticCode::UnavailableModel
                            | DiagnosticCode::PermissionDenied
                    )
                }) || validation.failures().iter().all(|c| {
                    matches!(
                        c.name.as_str(),
                        "capability.no_gaps"
                            | "capability.coverage"
                            | "policy.permissions"
                            | "graph.acyclic"
                            | "graph.deps_resolve"
                            | "graph.unique_ids"
                    )
                }));
            let correctable = structural_only || (used_llm_plan && llm_capability_correctable);
            if correctable {
                let correction_reason = format!(
                    "first plan failed validation ({}); replanning with deterministic decompose",
                    failure_labels.join("; ")
                );
                diagnostics.push(CompileDiagnostic::warning(
                    DiagnosticCode::CompilationFailure,
                    "replanning after validation failure",
                    correction_reason.clone(),
                    format!("goal.{}", goal.id),
                    "inspect ReplanRecord.original if the simplified plan is too coarse",
                ));
                decomp = decompose(&goal);
                let (workflow_f, prov_f, d_f) =
                    self.plan_and_build(&goal, &decomp, input, &mut diagnostics)?;
                let final_errors: Vec<_> = d_f.iter().filter(|x| x.is_error()).cloned().collect();
                diagnostics.extend(d_f.iter().filter(|x| !x.is_error()).cloned());
                if !final_errors.is_empty() {
                    return Err(final_errors);
                }
                let plan_f = plan_capabilities(&decomp, &self.registry_view(input));
                validation = validate_workflow(
                    &goal,
                    &workflow_f,
                    &plan_f,
                    &input.resource,
                    &input.mode_ctx,
                );
                if !validation.is_valid() {
                    let mut err = validation.to_diagnostics();
                    err.extend(diagnostics.iter().filter(|d| d.is_error()).cloned());
                    return Err(err);
                }
                replan = Some(ReplanRecord {
                    original: original_decomp_for_replan,
                    correction_reason,
                    attempts: 1,
                    final_decomp: decomp.clone(),
                });
                return self
                    .finish(
                        input,
                        goal,
                        decomp,
                        plan_f,
                        workflow_f,
                        prov_f,
                        Some(validation),
                        diagnostics,
                        replan,
                        store,
                    )
                    .await;
            }
        }

        diagnostics.extend(first_errors);

        if !validation.is_valid() {
            let mut err = validation.to_diagnostics();
            err.extend(diagnostics.iter().filter(|d| d.is_error()).cloned());
            if err.is_empty() {
                err.push(CompileDiagnostic::error(
                    DiagnosticCode::CompilationFailure,
                    "validation failed",
                    "report has failing checks",
                    format!("workflow.{}", workflow.id),
                    "inspect ValidationReport and fix failing entries",
                ));
            }
            return Err(err);
        }

        if diagnostics.iter().any(|d| d.is_error()) {
            return Err(diagnostics);
        }

        diagnostics.extend(validation.to_diagnostics()); // normally empty here

        self.finish(
            input,
            goal,
            decomp,
            plan,
            workflow,
            provenance,
            Some(validation),
            diagnostics,
            replan,
            store,
        )
        .await
    }

    async fn finish(
        &self,
        input: &CompileInput,
        goal: GoalSpec,
        decomp: crate::decompose::DecomposedGoal,
        plan: crate::capability::CapabilityPlan,
        workflow: Workflow,
        provenance: Provenance,
        validation: Option<ValidationReport>,
        mut diagnostics: Vec<CompileDiagnostic>,
        replan: Option<ReplanRecord>,
        store: Option<&Arc<dyn PersistenceStore>>,
    ) -> Result<CompileOutput, Vec<CompileDiagnostic>> {
        let mut stage_reached = CompileStage::ValidatePolicy;
        let mut persisted = false;
        if input.mode.persists() {
            if let Some(store) = store {
                // Persist executable graph under `dag` first (orchestrator
                // path), then the compiled record under `compiled`.
                if let Err(e) = persist_dag_workflow(store, &workflow).await {
                    return Err(vec![persist_diag(e, &workflow.id, "dag")]);
                }
                match persist_compiled(
                    store,
                    &workflow.id,
                    goal.clone(),
                    decomp.clone(),
                    plan,
                    provenance.clone(),
                    validation.clone(),
                    diagnostics.clone(),
                    replan.clone(),
                )
                .await
                {
                    Ok(()) => {
                        persisted = true;
                        stage_reached = CompileStage::Persist;
                    }
                    Err(e) => return Err(vec![persist_diag(e, &workflow.id, COMPILED_NS_REF)]),
                }
            } else {
                diagnostics.push(CompileDiagnostic::warning(
                    DiagnosticCode::CompilationFailure,
                    "persistence skipped",
                    "no PersistenceStore supplied",
                    format!("workflow.{}", workflow.id),
                    "pass a store to CompileInput workflow when CompileMode persists",
                ));
            }
        }

        let stage_reached = if input.mode.executes() {
            CompileStage::Handoff
        } else {
            stage_reached
        };

        if diagnostics.iter().any(|d| d.is_error()) {
            return Err(diagnostics);
        }

        Ok(CompileOutput {
            workflow_id: workflow.id.clone(),
            mode: input.mode,
            stage_reached,
            goal,
            workflow,
            provenance,
            validation,
            diagnostics,
            persisted,
            replan,
        })
    }

    fn registry_view<'a>(&self, input: &'a CompileInput) -> RegistryView<'a> {
        RegistryView {
            tool_names: &input.tool_names,
            policy_allows: input.policy_allows.as_ref(),
            network_allowed: input.network_allowed,
            model_available: input.model_available,
        }
    }

    fn plan_and_build(
        &self,
        goal: &GoalSpec,
        decomp: &crate::decompose::DecomposedGoal,
        input: &CompileInput,
        diagnostics: &mut Vec<CompileDiagnostic>,
    ) -> Result<(Workflow, Provenance, Vec<CompileDiagnostic>), Vec<CompileDiagnostic>> {
        let plan = plan_capabilities(decomp, &self.registry_view(input));
        let workflow_id = format!("wf.{}", goal.id.trim_start_matches("goal."));
        match build_workflow(goal, decomp, &plan, &workflow_id) {
            Ok((wf, prov, d)) => {
                // Do NOT auto-merge error diagnostics here — the caller decides
                // whether a bounded replan discards first-attempt errors.
                let _ = diagnostics;
                Ok((wf, prov, d))
            }
            Err(errs) => Err(errs),
        }
    }
}

const COMPILED_NS_REF: &str = crate::COMPILED_NS;

fn persist_diag(e: PersistenceError, workflow_id: &str, ns: &str) -> CompileDiagnostic {
    CompileDiagnostic::error(
        DiagnosticCode::CompilationFailure,
        "persistence failed",
        e.to_string(),
        format!("{ns}/{workflow_id}"),
        "ensure the store directory is writable and the namespace key is path-safe (a-z0-9._-)",
    )
}

/// Stage 1b: fail closed on goals the compiler cannot honestly plan.
///
/// Acceptance combines the historic needle check with structured intent
/// normalization ([`crate::intent::normalize`]): a request is actionable when
/// it carries a recognized action class, an artifact intent (PDF/DOCX/report/
/// chart/…), a file reference, or a network marker. Rejection text and the
/// fail-closed contract are unchanged.
///
/// * **Unsupported** — no actionable signal (verbs/file/network/artifact
///   tokens): we refuse to invent a workflow.
/// * **Ambiguous** — conflicting intents (e.g. create **or** delete) without
///   sequencing, so any plan would be a guess.
fn reject_unsupported_or_ambiguous(goal: &GoalSpec) -> Result<(), CompileDiagnostic> {
    let lower = format!(
        "{} {}",
        goal.objective.to_lowercase(),
        goal.original_request.to_lowercase()
    );

    let mut intent = crate::intent::normalize(&goal.original_request);
    intent.merge(crate::intent::normalize(&goal.objective));

    const ACTION: &[&str] = &[
        "read",
        "load",
        "open",
        "ingest",
        "parse",
        "extract",
        "analy",
        "compare",
        "review",
        "inspect",
        "identify",
        "find",
        "write",
        "create",
        "generate",
        "produce",
        "save",
        "make",
        "build",
        "summar",
        "digest",
        "overview",
        "verify",
        "validat",
        "check",
        "confirm",
        "report",
        "document",
        "list",
        "download",
        "fetch",
        "http",
        "https",
        "web ",
        "run",
        "execute",
        "test",
        "search",
        "scan",
        "monitor",
        "watch",
        "collect",
        "gather",
        "transform",
        "convert",
        "clean",
        "sort",
        "merge",
        "split",
        "encrypt",
        "decrypt",
        "deploy",
        "build",
    ];
    let has_action = ACTION.iter().any(|v| lower.contains(v)) || intent.actionable();
    let has_fileish = goal.inputs.iter().any(|_| true)
        || !intent.file_refs.is_empty()
        || lower.contains(".txt")
        || lower.contains(".md")
        || lower.contains(".json")
        || lower.contains(".toml")
        || lower.contains(".yaml")
        || lower.contains(".yml")
        || lower.contains(".rs")
        || lower.contains(".py")
        || lower.contains(".pdf");

    if !has_action && !has_fileish {
        return Err(CompileDiagnostic::error(
            DiagnosticCode::UnsupportedGoal,
            "unsupported goal",
            "request has no actionable verb, artifact, file reference, or network marker the compiler can plan",
            format!("goal.{}", goal.id),
            "restate with an explicit action (read/write/analyze/verify/…), a named artifact (PDF/DOCX/report/chart/…), a file reference, or a network marker",
        ));
    }

    // Ambiguous: mutually exclusive create/write vs delete/remove without sequencing.
    let creates = ["create", "write", "generate", "save", "make"]
        .iter()
        .any(|v| lower.contains(v))
        || intent.has(crate::intent::ActionClass::Create);
    let deletes = ["delete", "remove", "destroy", "erase", "wipe"]
        .iter()
        .any(|v| lower.contains(v))
        || intent.has(crate::intent::ActionClass::Delete);
    let sequenced = [
        " then ",
        " after ",
        " first ",
        " next ",
        " finally ",
        " and then ",
    ]
    .iter()
    .any(|v| lower.contains(v))
        || intent.sequenced;
    if creates && deletes && !sequenced {
        return Err(CompileDiagnostic::error(
            DiagnosticCode::AmbiguousGoal,
            "ambiguous goal",
            "request mixes create/write intent with delete/remove intent and no sequencing words",
            format!("goal.{}", goal.id),
            "state one primary outcome, or sequence explicitly (e.g. \"create X then delete Y\")",
        ));
    }

    Ok(())
}

/// Stage 1: normalize the raw request into a [`GoalSpec`].
///
/// Does not invent requirements: inputs/outputs are only those explicitly
/// detectable in the text (quoted paths, `.ext` tokens); everything else stays
/// empty. Network/verification flags come from the request text and ambient
/// mode.
fn understand(request: &str, input: &CompileInput) -> GoalSpec {
    let lower = request.to_lowercase();
    let id = GoalSpec::id_for(request);
    let objective = request.trim().to_string();

    // Extract simple file-like tokens as inputs (no fabrication of absences).
    let mut inputs = Vec::new();
    for tok in request.split_whitespace() {
        let clean = tok.trim_matches(|c: char| c == '\'' || c == '"' || c == ',' || c == ';');
        if clean.contains('.') && clean.len() > 2 {
            let ext = clean.rsplit('.').next().unwrap_or("");
            if ext.len() <= 6 && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
                inputs.push(clean.to_string());
            }
        }
    }
    inputs.sort();
    inputs.dedup();

    let mut expected_outputs = Vec::new();
    for tok in request.split_whitespace() {
        let clean = tok.trim_matches(|c: char| c == '\'' || c == '"' || c == ',' || c == ';');
        if lower.contains("file") || lower.contains("write") || lower.contains("create") {
            let ext = clean.rsplit('.').next().unwrap_or("");
            if clean.contains('.')
                && ext.len() <= 6
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
                && !inputs.iter().any(|i| i == clean)
            {
                expected_outputs.push(clean.to_string());
            }
        }
    }
    expected_outputs.sort();
    expected_outputs.dedup();

    // Structured artifact intent: when the request *creates* a named artifact
    // (PDF/DOCX/report/chart/…) and no concrete dotted output was detected,
    // record the artifact as the expected output so decomposition and
    // capability planning can bind the real backend tool instead of a
    // generic text step. Never invents an output when no create verb or
    // artifact is present.
    if expected_outputs.is_empty() {
        if let Some(a) = crate::intent::output_artifact_in(request) {
            expected_outputs.push(a.expected_output());
        }
    }

    let requires_network = lower.contains("http")
        || lower.contains("https")
        || lower.contains("fetch ")
        || lower.contains("download")
        || lower.contains("web ")
        || !input.network_allowed && false; // ambient mode alone does not set the flag

    let requires_verification = lower.contains("verif")
        || lower.contains("validat")
        || lower.contains("confirmed")
        || lower.contains("checked");

    let mut constraints = Vec::new();
    if !input.network_allowed {
        constraints.push("network disabled by mode".into());
    }
    if input.mode_ctx.sovereign {
        constraints.push("Sovereign Mode: local-only".into());
    }

    GoalSpec {
        id,
        original_request: request.to_string(),
        objective,
        inputs,
        expected_outputs,
        constraints,
        session_id: input.goal.session_id.clone(),
        workspace: input.goal.workspace.clone(),
        requires_network,
        requires_verification,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_persistence::MemoryStore;

    fn input(req: &str) -> CompileInput {
        CompileInput::new(NaturalLanguageGoal::new(req))
            .with_tools(vec![
                "filesystem.read".to_string(),
                "filesystem.write".to_string(),
                "echo".to_string(),
            ])
            .with_policy(Arc::new(|_| true))
            .with_network(true)
            .with_model_available(true)
    }

    #[tokio::test]
    async fn compiles_simple_goal_to_valid_dag() {
        let out = WorkflowCompiler::new()
            .compile(
                &input("Create a file containing the current project status"),
                None,
            )
            .await
            .unwrap();
        assert!(!out.workflow.steps.is_empty());
        assert_eq!(out.stage_reached, CompileStage::ValidatePolicy);
        assert!(out.validation.as_ref().unwrap().is_valid());
        assert!(!out.persisted);
        assert!(out.provenance.original_request.contains("status"));
        for n in &out.provenance.nodes {
            assert!(out.workflow.find_step(&n.step_id).is_some());
        }
    }

    #[tokio::test]
    async fn empty_goal_fails_closed() {
        let err = WorkflowCompiler::new()
            .compile(&input("   "), None)
            .await
            .unwrap_err();
        assert!(err.iter().any(|d| d.code == DiagnosticCode::EmptyGoal));
        assert!(err.iter().all(|d| !d.how_to_fix.is_empty()));
    }

    #[tokio::test]
    async fn permission_denial_fails_closed_with_diagnostic() {
        let mut inp = input("Create a report file");
        inp.policy_allows = Arc::new(|_| false);
        let err = WorkflowCompiler::new()
            .compile(&inp, None)
            .await
            .unwrap_err();
        assert!(err.iter().any(|d| {
            matches!(
                d.code,
                DiagnosticCode::PermissionDenied | DiagnosticCode::MissingCapability
            )
        }));
        for d in &err {
            assert!(d.is_error());
            assert!(!d.what.is_empty());
            assert!(!d.why.is_empty());
            assert!(!d.where_.is_empty());
            assert!(!d.how_to_fix.is_empty());
        }
    }

    #[tokio::test]
    async fn sovereign_network_goal_blocked_when_network_off() {
        let mut inp = input("Download the latest report from the web");
        inp = inp.with_network(false);
        // understand() sets requires_network from text; network off should fail.
        let result = WorkflowCompiler::new().compile(&inp, None).await;
        match result {
            Err(errs) => {
                assert!(errs.iter().any(|d| {
                    d.code == DiagnosticCode::SovereignConflict
                        || d.where_.contains("mode.sovereign")
                        || d.what.contains("network")
                }));
            }
            Ok(out) => {
                // If understand didn't flag requires_network (wording miss),
                // at least validation must not claim sovereign pass incorrectly.
                if out.goal.requires_network {
                    panic!("should have failed closed for network goal");
                }
            }
        }
    }

    #[tokio::test]
    async fn invalid_llm_plan_rejected() {
        let mut inp = input("do anything");
        inp.llm_plan_json = Some("{not json".into());
        let err = WorkflowCompiler::new()
            .compile(&inp, None)
            .await
            .unwrap_err();
        assert!(err.iter().any(|d| d.code == DiagnosticCode::InvalidLlmPlan));
    }

    #[tokio::test]
    async fn persists_when_store_supplied_and_mode_compiles() {
        let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
        let mut inp = input("Create a file containing the current project status");
        inp.mode = CompileMode::Compile;
        let out = WorkflowCompiler::new()
            .compile(&inp, Some(&store))
            .await
            .unwrap();
        assert!(out.persisted);
        assert_eq!(out.stage_reached, CompileStage::Persist);
        let rec = crate::persist::load_compiled(&store, &out.workflow_id)
            .await
            .unwrap()
            .expect("compiled record present");
        assert_eq!(rec.goal.original_request, out.goal.original_request);
        let dag = crate::persist::load_dag_workflow(&store, &out.workflow_id)
            .await
            .unwrap()
            .expect("dag workflow present");
        assert_eq!(dag.steps.len(), out.workflow.steps.len());
    }

    #[tokio::test]
    async fn explain_mode_does_not_persist() {
        let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
        let mut inp = input("Create a file containing the current project status");
        inp.mode = CompileMode::Explain;
        let out = WorkflowCompiler::new()
            .compile(&inp, Some(&store))
            .await
            .unwrap();
        assert!(!out.persisted);
        assert_eq!(out.stage_reached, CompileStage::ValidatePolicy);
        assert!(crate::persist::load_compiled(&store, &out.workflow_id)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn llm_plan_is_used_when_valid() {
        let mut inp = input("Analyze A then write B");
        inp.llm_plan_json = Some(
            r#"{"subgoals":[{"id":"read","objective":"Read A"},{"id":"write","objective":"Write B","depends_on":["read"]}]}"#
                .into(),
        );
        let out = WorkflowCompiler::new().compile(&inp, None).await.unwrap();
        assert_eq!(out.workflow.steps.len(), 2);
        assert!(out.workflow.find_step("read").is_some());
        assert!(out.workflow.find_step("write").is_some());
        let w = out.workflow.find_step("write").unwrap();
        assert_eq!(w.depends_on, vec!["read".to_string()]);
    }

    #[tokio::test]
    async fn unsupported_goal_fails_with_structured_diagnostic() {
        let err = WorkflowCompiler::new()
            .compile(
                &input("Transmute lead into unobtanium with psychic waves"),
                None,
            )
            .await
            .unwrap_err();
        assert!(err
            .iter()
            .any(|d| d.code == DiagnosticCode::UnsupportedGoal));
        assert!(err
            .iter()
            .all(|d| !d.what.is_empty() && !d.how_to_fix.is_empty()));
    }

    #[tokio::test]
    async fn ambiguous_goal_fails_with_structured_diagnostic() {
        let err = WorkflowCompiler::new()
            .compile(&input("Create or delete the report file"), None)
            .await
            .unwrap_err();
        assert!(err.iter().any(|d| d.code == DiagnosticCode::AmbiguousGoal));
        assert!(err
            .iter()
            .all(|d| !d.why.is_empty() && !d.how_to_fix.is_empty()));
    }

    // ---- PART F: artifact-intent plans (structured intent → real backends) ----

    /// Create-synonym goals for the same artifact must all compile to a plan
    /// bound to the real docx/pdf backends, with provenance explaining why.
    #[tokio::test]
    async fn artifact_create_synonyms_compile_to_backend_plan() {
        for goal in [
            "make a pdf on microprocessor 8051",
            "generate a pdf about microprocessor 8051",
            "produce a pdf for microprocessor 8051",
        ] {
            let mut inp = input(goal);
            inp.tool_names = vec![
                "filesystem.read".to_string(),
                "filesystem.write".to_string(),
                "echo".to_string(),
                "docx.create".to_string(),
                "docx.export_pdf".to_string(),
            ];
            let out = WorkflowCompiler::new()
                .compile(&inp, None)
                .await
                .unwrap_or_else(|e| panic!("{goal} must compile: {e:?}"));
            assert!(out.workflow.find_step("generate").is_some(), "{goal}");
            assert!(
                out.goal.expected_outputs == vec!["PDF artifact".to_string()],
                "{goal}: {:?}",
                out.goal.expected_outputs
            );
            let node = out
                .provenance
                .nodes
                .iter()
                .find(|n| n.step_id == "generate")
                .expect("generate provenance");
            assert!(
                node.why_capability.contains("artifact.write"),
                "{}",
                node.why_capability
            );
            assert!(
                node.why_capability.contains("docx.create"),
                "{}",
                node.why_capability
            );
            assert!(out.validation.as_ref().unwrap().is_valid());
        }
    }

    /// The pdf pipeline needs BOTH docx tools; with none registered the plan
    /// fails closed as unavailable_tool — never a fabricated text-only node.
    #[tokio::test]
    async fn pdf_without_office_backend_fails_closed_as_unavailable_tool() {
        let err = WorkflowCompiler::new()
            .compile(&input("make a pdf on microprocessor 8051"), None)
            .await
            .unwrap_err();
        assert!(
            err.iter()
                .any(|d| d.code == DiagnosticCode::UnavailableTool),
            "{err:?}"
        );
        assert!(
            err.iter().any(|d| d.why.contains("not registered")),
            "{err:?}"
        );
    }

    /// No image generator exists in the registry — the compiler says so
    /// honestly instead of pretending a model step will draw pixels.
    #[tokio::test]
    async fn image_generation_backend_is_honestly_unavailable() {
        let mut inp = input("create an image of the block diagram");
        inp.tool_names.push("docx.create".to_string());
        inp.tool_names.push("python.run".to_string());
        let err = WorkflowCompiler::new()
            .compile(&inp, None)
            .await
            .unwrap_err();
        assert!(
            err.iter()
                .any(|d| d.code == DiagnosticCode::UnavailableTool),
            "{err:?}"
        );
        assert!(
            err.iter()
                .any(|d| d.why.contains("image generation backend is not available.")),
            "{err:?}"
        );
    }

    /// Structured-intent expansion must not weaken fail-closed rejection:
    /// a bare topic with no action, artifact, file, or network marker still
    /// fails as unsupported_goal.
    #[tokio::test]
    async fn bare_topic_without_action_or_artifact_fails_closed() {
        let err = WorkflowCompiler::new()
            .compile(&input("microprocessor 8051"), None)
            .await
            .unwrap_err();
        assert!(
            err.iter()
                .any(|d| d.code == DiagnosticCode::UnsupportedGoal),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn different_goals_produce_different_structures() {
        let a = WorkflowCompiler::new()
            .compile(&input("List files in the current directory"), None)
            .await
            .unwrap();
        let b = WorkflowCompiler::new()
            .compile(
                &input(
                    "Analyze report1.txt and report2.txt independently then create a verified report",
                ),
                None,
            )
            .await
            .unwrap();
        let c = WorkflowCompiler::new()
            .compile(
                &input("Create a file containing the current project status"),
                None,
            )
            .await
            .unwrap();
        let ids = |o: &CompileOutput| {
            let mut v: Vec<String> = o.workflow.steps.iter().map(|s| s.id.clone()).collect();
            v.sort();
            v.join(",")
        };
        let (ia, ib, ic) = (ids(&a), ids(&b), ids(&c));
        assert_ne!(ia, ib, "parallel goal must differ from list goal");
        assert_ne!(ia, ic, "list goal must differ from create-file goal");
        assert_ne!(ib, ic, "parallel goal must differ from create-file goal");
        assert!(a.validation.as_ref().unwrap().is_valid());
        assert!(b.validation.as_ref().unwrap().is_valid());
        assert!(c.validation.as_ref().unwrap().is_valid());
    }

    #[tokio::test]
    async fn llm_capability_gap_triggers_bounded_replan_preserving_original() {
        // LLM plan forces an unavailable terminal tool; deterministic fallback
        // for the same goal is model-only and valid. Original + reason kept.
        let mut inp = input("Analyze the project status");
        inp.llm_plan_json = Some(
            r#"{"subgoals":[
                {"id":"shell","objective":"Run shell commands to inspect the system"},
                {"id":"analyze","objective":"Analyze findings","depends_on":["shell"]}
            ]}"#
            .into(),
        );
        inp.tool_names = vec!["filesystem.read".to_string(), "echo".to_string()];
        let out = WorkflowCompiler::new().compile(&inp, None).await.unwrap();
        let rep = out.replan.expect("replan must be recorded");
        assert_eq!(rep.attempts, 1);
        assert!(
            rep.correction_reason.contains("failed build")
                || rep.correction_reason.contains("failed validation"),
            "reason: {}",
            rep.correction_reason
        );
        assert!(
            rep.original.find("shell").is_some(),
            "original plan preserved"
        );
        assert!(
            rep.final_decomp.find("shell").is_none(),
            "final plan is the deterministic fallback"
        );
        assert!(!out.workflow.find_step("shell").is_some());
        assert!(out.validation.as_ref().unwrap().is_valid());
        // Provenance still points at the original request.
        assert!(out.provenance.original_request.contains("project status"));
        // Invalid plan never executed: only final valid workflow is returned.
        assert!(!out.workflow.steps.is_empty());
    }

    #[tokio::test]
    async fn max_replan_zero_disables_replan_and_fails_closed() {
        let mut inp = input("Analyze the project status");
        inp.llm_plan_json = Some(
            r#"{"subgoals":[
                {"id":"shell","objective":"Run shell commands to inspect the system"},
                {"id":"analyze","objective":"Analyze findings","depends_on":["shell"]}
            ]}"#
            .into(),
        );
        inp.tool_names = vec!["filesystem.read".to_string()];
        inp.max_replan = 0;
        let err = WorkflowCompiler::new()
            .compile(&inp, None)
            .await
            .unwrap_err();
        assert!(!err.is_empty());
        // Must fail closed rather than silently accept the bad plan.
        assert!(err.iter().any(|d| d.is_error()));
    }

    #[tokio::test]
    async fn resource_denied_fails_closed_not_replanned_away() {
        let mut inp = input("Analyze the project status");
        inp.resource = ResourceContext {
            summary: "memory exhausted".into(),
            denied: true,
            reason: Some("available memory below floor".into()),
        };
        let err = WorkflowCompiler::new()
            .compile(&inp, None)
            .await
            .unwrap_err();
        assert!(err
            .iter()
            .any(|d| d.where_.contains("resource") || d.what.to_lowercase().contains("resource")));
    }

    #[tokio::test]
    async fn unavailable_model_fails_closed() {
        let mut inp = input("Analyze the project status and summarize findings");
        inp.model_available = false;
        let err = WorkflowCompiler::new()
            .compile(&inp, None)
            .await
            .unwrap_err();
        assert!(
            err.iter().any(|d| {
                matches!(
                    d.code,
                    DiagnosticCode::UnavailableModel | DiagnosticCode::MissingCapability
                )
            }),
            "expected model/capability diagnostic: {err:?}"
        );
    }
}
