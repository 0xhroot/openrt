//! Capability planning against the real tool registry, policy, and modes
//! (STEPS 4–5). Never invents capabilities that are not registered.

use serde::{Deserialize, Serialize};

use crate::decompose::{DecomposedGoal, SubGoal};

/// High-level capability class a subgoal may need.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityClass {
    FilesystemRead,
    FilesystemWrite,
    Terminal,
    Python,
    ModelReasoning,
    ModelExtraction,
    Verification,
    Workflow,
    /// Creating a named artifact through its real registry backend(s)
    /// (DOCX/PDF/PPTX/XLSX/chart/…); image/video/audio fall through with an
    /// honest UNAVAILABLE status because no generator is registered.
    ArtifactWrite,
    Unknown,
}

impl CapabilityClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FilesystemRead => "filesystem.read",
            Self::FilesystemWrite => "filesystem.write",
            Self::Terminal => "terminal",
            Self::Python => "python",
            Self::ModelReasoning => "model.reasoning",
            Self::ModelExtraction => "model.extraction",
            Self::Verification => "verification",
            Self::Workflow => "workflow",
            Self::ArtifactWrite => "artifact.write",
            Self::Unknown => "unknown",
        }
    }
}

/// Whether a selected capability is available under the current registry/policy/mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStatus {
    Available,
    /// Registered but policy-denied.
    PermissionDenied,
    /// Required by Sovereign Mode (network) but mode forbids it.
    SovereignBlocked,
    /// Not present in the tool registry / model inventory.
    Unavailable,
}

/// One planned capability assignment for a subgoal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityAssignment {
    pub subgoal_id: String,
    pub capability: CapabilityClass,
    /// Concrete registered tool name when a tool is selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Role the existing orchestrator will bind for this node.
    pub role: String,
    /// Required permission capabilities (strings as policy sees them).
    #[serde(default)]
    pub required_permissions: Vec<String>,
    pub status: CapabilityStatus,
    /// Reason when not Available (always set for non-Available).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Artifact kind when this assignment is an [`CapabilityClass::ArtifactWrite`]
    /// (`pdf`/`docx`/`pptx`/`xlsx`/`chart`/… — absent for other classes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    /// Expected inputs / outputs carried from the subgoal.
    #[serde(default)]
    pub inputs: Vec<String>,
    #[serde(default)]
    pub outputs: Vec<String>,
    /// Verification strategy for this node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<String>,
    /// Why this capability/role was selected (explainability).
    pub why: String,
}

/// Full capability plan for a decomposition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityPlan {
    pub goal_id: String,
    pub assignments: Vec<CapabilityAssignment>,
}

impl CapabilityPlan {
    pub fn gaps(&self) -> Vec<&CapabilityAssignment> {
        self.assignments
            .iter()
            .filter(|a| a.status != CapabilityStatus::Available)
            .collect()
    }

    pub fn has_blocking_gap(&self) -> bool {
        !self.gaps().is_empty()
    }
}

/// Minimal view of the live tool registry the planner consults (avoids a
/// hard dependency on openrt-tools types in the public plan surface).
pub struct RegistryView<'a> {
    pub tool_names: &'a [String],
    /// Whether a policy evaluator would allow the given tool id / capability.
    pub policy_allows: &'a dyn Fn(&str) -> bool,
    /// Network allowed under current operating mode.
    pub network_allowed: bool,
    /// Whether any model provider is bound (role agents always need one).
    pub model_available: bool,
}

/// Artifacts with a dedicated registry backend (classified as
/// [`CapabilityClass::ArtifactWrite`]); everything else (report/csv/markdown/
/// html/json) is an ordinary filesystem write.
const BACKEND_ARTIFACTS: &[crate::intent::ArtifactKind] = &[
    crate::intent::ArtifactKind::Pdf,
    crate::intent::ArtifactKind::Docx,
    crate::intent::ArtifactKind::Pptx,
    crate::intent::ArtifactKind::Xlsx,
    crate::intent::ArtifactKind::Chart,
    crate::intent::ArtifactKind::Image,
    crate::intent::ArtifactKind::Video,
    crate::intent::ArtifactKind::Audio,
];

/// Create-class verbs that mark a subgoal as *producing* its artifact
/// (guards the artifact branch against read/analyze subgoals that merely
/// mention an artifact in passing).
const SUBGOAL_CREATE_VERBS: &[&str] = &[
    "generate", "create", "persist", "write", "save", "export", "render", "make", "draft",
    "prepare", "compile", "produc", "build", "author", "assembl", "compos",
];

/// Real registered tool backends per artifact kind. Empty ⇒ no generator is
/// registered (image/video/audio) and planning fails closed with an honest
/// UNAVAILABLE reason instead of fabricating a node. `pdf` requires the whole
/// docx pipeline (create + soffice-backed export) — ALL backends must be
/// registered and allowed.
fn artifact_backends(kind: crate::intent::ArtifactKind) -> &'static [&'static str] {
    use crate::intent::ArtifactKind;
    match kind {
        ArtifactKind::Pdf => &["docx.create", "docx.export_pdf"],
        ArtifactKind::Docx => &["docx.create"],
        ArtifactKind::Pptx => &["ppt.create"],
        ArtifactKind::Xlsx => &["excel.create"],
        ArtifactKind::Chart => &["python.run"],
        ArtifactKind::Image | ArtifactKind::Video | ArtifactKind::Audio => &[],
        _ => &[],
    }
}

/// The backend-specific artifact this subgoal is producing (reading order),
/// if any.
fn backend_artifact(subgoal: &SubGoal) -> Option<crate::intent::ArtifactKind> {
    let text = format!(
        "{} {}",
        subgoal.objective,
        subgoal.expected_outputs.join(" ")
    );
    crate::intent::Intent::artifacts_in(&text)
        .into_iter()
        .find(|a| BACKEND_ARTIFACTS.contains(a))
}

/// Infer the capability class for a subgoal from its objective/outputs.
fn classify(subgoal: &SubGoal) -> (CapabilityClass, &'static str /* role */) {
    let o = subgoal.objective.to_lowercase();
    if o.contains("verify") || o.contains("check") || o.contains("confirm") {
        return (CapabilityClass::Verification, "verifier");
    }
    if o.contains("plan") || o.contains("clarify") || o.contains("criteri") {
        return (CapabilityClass::ModelReasoning, "planner");
    }
    // Producing a named artifact binds its real backend (docx/pptx/excel/
    // chart/python/…), not a generic filesystem write; image/video/audio
    // resolve to ArtifactWrite with no backend so planning reports UNAVAILABLE.
    if SUBGOAL_CREATE_VERBS.iter().any(|v| o.contains(v)) && backend_artifact(subgoal).is_some() {
        return (CapabilityClass::ArtifactWrite, "coder");
    }
    if o.contains("write")
        || o.contains("create")
        || o.contains("generate")
        || o.contains("persist")
        || o.contains("save")
    {
        // Writing a durable artifact prefers filesystem.write when registered;
        // otherwise the coder role still produces text via the model.
        return (CapabilityClass::FilesystemWrite, "coder");
    }
    if o.contains("read")
        || o.contains("ingest")
        || o.contains("extract")
        || o.contains("discover")
        || o.contains("list")
    {
        return (CapabilityClass::FilesystemRead, "researcher");
    }
    if o.contains("analy")
        || o.contains("compare")
        || o.contains("summar")
        || o.contains("aggregate")
        || o.contains("review")
    {
        return (CapabilityClass::ModelReasoning, "researcher");
    }
    if o.contains("python") || o.contains("script") {
        return (CapabilityClass::Python, "coder");
    }
    if o.contains("terminal") || o.contains("shell") || o.contains("command") {
        return (CapabilityClass::Terminal, "coder");
    }
    (CapabilityClass::ModelReasoning, "researcher")
}

/// Preferred tool for a capability class (first registered match wins).
fn preferred_tool(class: CapabilityClass, registry: &RegistryView<'_>) -> Option<String> {
    let has = |name: &str| registry.tool_names.iter().any(|t| t == name);
    match class {
        CapabilityClass::FilesystemRead => ["filesystem.read", "filesystem.list"]
            .into_iter()
            .find(|n| has(n))
            .map(str::to_string),
        CapabilityClass::FilesystemWrite => ["filesystem.write", "echo"]
            .into_iter()
            .find(|n| has(n))
            .map(str::to_string),
        CapabilityClass::Terminal => has("terminal.run").then(|| "terminal.run".to_string()),
        CapabilityClass::Python => has("python.run").then(|| "python.run".to_string()),
        CapabilityClass::ArtifactWrite => {
            // Tool selection needs the concrete artifact; handled in
            // plan_capabilities (never reached here).
            None
        }
        CapabilityClass::Verification
        | CapabilityClass::ModelReasoning
        | CapabilityClass::ModelExtraction
        | CapabilityClass::Workflow
        | CapabilityClass::Unknown => {
            // Role agents execute via the model; no discrete tool required.
            None
        }
    }
}

/// Required permission strings for a class (policy keys).
fn required_perms(class: CapabilityClass, tool: Option<&str>) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(t) = tool {
        v.push(t.to_string());
    }
    match class {
        CapabilityClass::FilesystemRead => v.push("filesystem.read".into()),
        CapabilityClass::FilesystemWrite => v.push("filesystem.write".into()),
        CapabilityClass::Terminal => v.push("terminal.run".into()),
        CapabilityClass::Python => v.push("python.run".into()),
        _ => {}
    }
    v
}

/// Plan capabilities for every subgoal against the live registry/policy/mode.
pub fn plan_capabilities(decomp: &DecomposedGoal, registry: &RegistryView<'_>) -> CapabilityPlan {
    let mut assignments = Vec::new();
    for sg in &decomp.subgoals {
        let (class, role) = classify(sg);
        let artifact = (class == CapabilityClass::ArtifactWrite)
            .then(|| backend_artifact(sg))
            .flatten();
        let backends: Vec<&'static str> = artifact
            .map(artifact_backends)
            .map(<[&'static str]>::to_vec)
            .unwrap_or_default();
        let tool = match class {
            CapabilityClass::ArtifactWrite => backends.first().map(|t| t.to_string()),
            _ => preferred_tool(class, registry),
        };
        let perms = match class {
            CapabilityClass::ArtifactWrite => {
                // ALL backends must be allowed (e.g. the pdf pipeline needs
                // both docx.create and docx.export_pdf).
                let mut p: Vec<String> = backends.iter().map(|t| t.to_string()).collect();
                if let Some(t) = &tool {
                    if !p.contains(t) {
                        p.push(t.clone());
                    }
                }
                p
            }
            _ => required_perms(class, tool.as_deref()),
        };

        let (status, reason) = if class == CapabilityClass::ArtifactWrite {
            match artifact {
                None => (
                    CapabilityStatus::Unavailable,
                    Some("artifact backend could not be determined".to_string()),
                ),
                Some(art) if backends.is_empty() => (
                    CapabilityStatus::Unavailable,
                    Some(format!(
                        "{} generation backend is not available.",
                        art.label()
                    )),
                ),
                Some(_) => {
                    let missing: Vec<&str> = backends
                        .iter()
                        .copied()
                        .filter(|t| !registry.tool_names.iter().any(|n| n == t))
                        .collect();
                    if !missing.is_empty() {
                        (
                            CapabilityStatus::Unavailable,
                            Some(format!(
                                "required tool(s) not registered: {}",
                                missing.join(", ")
                            )),
                        )
                    } else if perms.iter().any(|p| !(registry.policy_allows)(p)) {
                        (
                            CapabilityStatus::PermissionDenied,
                            Some(format!(
                                "policy denies required permission(s): {}",
                                perms.join(", ")
                            )),
                        )
                    } else if !registry.network_allowed && class_needs_network(class) {
                        (
                            CapabilityStatus::SovereignBlocked,
                            Some("capability requires network; Sovereign Mode blocks it".into()),
                        )
                    } else {
                        (CapabilityStatus::Available, None)
                    }
                }
            }
        } else if class == CapabilityClass::ModelReasoning
            || class == CapabilityClass::Verification
            || class == CapabilityClass::ModelExtraction
        {
            if !registry.model_available {
                (
                    CapabilityStatus::Unavailable,
                    Some("no model provider is bound to the runtime".to_string()),
                )
            } else if perms.iter().any(|p| !(registry.policy_allows)(p)) {
                (
                    CapabilityStatus::PermissionDenied,
                    Some(format!(
                        "policy denies required permission(s): {}",
                        perms.join(", ")
                    )),
                )
            } else {
                (CapabilityStatus::Available, None)
            }
        } else if let Some(t) = tool.as_deref() {
            if !registry.tool_names.iter().any(|n| n == t) {
                (
                    CapabilityStatus::Unavailable,
                    Some(format!("tool `{t}` is not registered")),
                )
            } else if perms.iter().any(|p| !(registry.policy_allows)(p)) {
                (
                    CapabilityStatus::PermissionDenied,
                    Some(format!(
                        "policy denies required permission(s): {}",
                        perms.join(", ")
                    )),
                )
            } else {
                // Network-class tools (none currently preferred) would check mode here.
                if !registry.network_allowed && class_needs_network(class) {
                    (
                        CapabilityStatus::SovereignBlocked,
                        Some("capability requires network; Sovereign Mode blocks it".into()),
                    )
                } else {
                    (CapabilityStatus::Available, None)
                }
            }
        } else {
            // No concrete tool selected. Tool-backed classes fail closed (a
            // missing write/read/terminal/python tool is NOT satisfied by the
            // model alone); model classes fall through above.
            let tool_backed = matches!(
                class,
                CapabilityClass::FilesystemRead
                    | CapabilityClass::FilesystemWrite
                    | CapabilityClass::Terminal
                    | CapabilityClass::Python
            );
            if tool_backed {
                (
                    CapabilityStatus::Unavailable,
                    Some(format!(
                        "no registered tool satisfies capability `{}`",
                        class.as_str()
                    )),
                )
            } else if !registry.model_available {
                (
                    CapabilityStatus::Unavailable,
                    Some("no model provider is bound to the runtime".to_string()),
                )
            } else {
                (CapabilityStatus::Available, None)
            }
        };

        let why = if class == CapabilityClass::ArtifactWrite {
            let label = artifact.map(|a| a.label()).unwrap_or("artifact");
            let via = if backends.is_empty() {
                "no registered backend".to_string()
            } else {
                format!(" via tool(s) {}", backends.join(", "))
            };
            format!(
                "subgoal `{}` creates a {label} artifact → capability `{}` bound to role `{role}`{via}",
                sg.id,
                class.as_str(),
            )
        } else {
            format!(
                "subgoal `{}` objective maps to capability `{}` bound to role `{}`{}",
                sg.id,
                class.as_str(),
                role,
                tool.as_ref()
                    .map(|t| format!(" via tool `{t}`"))
                    .unwrap_or_default()
            )
        };

        assignments.push(CapabilityAssignment {
            subgoal_id: sg.id.clone(),
            capability: class,
            tool,
            role: role.to_string(),
            required_permissions: perms,
            status,
            reason,
            artifact: artifact.map(|a| a.as_str().to_string()),
            inputs: sg.required_inputs.clone(),
            outputs: sg.expected_outputs.clone(),
            verification: sg.verification.clone(),
            why,
        });
    }
    CapabilityPlan {
        goal_id: decomp.goal_id.clone(),
        assignments,
    }
}

/// Network-class capabilities (none of the preferred local tools need this
/// today; OCR/browser-fetch style capabilities would when registered).
fn class_needs_network(_class: CapabilityClass) -> bool {
    false
}

/// Parse an optional LLM-produced plan JSON into a `DecomposedGoal`.
///
/// Fails closed on any schema violation — invalid model output never becomes
/// an executable plan (STEP 26).
pub fn parse_llm_plan(goal_id: &str, raw: &str) -> Result<DecomposedGoal, String> {
    let v: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("invalid JSON: {e}"))?;
    let arr = v
        .get("subgoals")
        .and_then(|a| a.as_array())
        .ok_or_else(|| "missing `subgoals` array".to_string())?;
    if arr.is_empty() {
        return Err("`subgoals` must be non-empty".into());
    }
    let mut subgoals = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for item in arr {
        let id = item
            .get("id")
            .and_then(|x| x.as_str())
            .ok_or_else(|| "subgoal missing string `id`".to_string())?;
        if id.is_empty()
            || !id.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
            })
        {
            return Err(format!("subgoal id `{id}` invalid (a-z0-9._- required)"));
        }
        if !seen.insert(id.to_string()) {
            return Err(format!("duplicate subgoal id `{id}`"));
        }
        let objective = item
            .get("objective")
            .and_then(|x| x.as_str())
            .ok_or_else(|| format!("subgoal `{id}` missing `objective`"))?;
        if objective.trim().is_empty() {
            return Err(format!("subgoal `{id}` has empty objective"));
        }
        let mut sg = SubGoal::new(id, objective);
        if let Some(deps) = item.get("depends_on").and_then(|d| d.as_array()) {
            for d in deps {
                let d = d
                    .as_str()
                    .ok_or_else(|| format!("subgoal `{id}` depends_on must be strings"))?;
                sg = sg.dep(d);
            }
        }
        if let Some(inputs) = item.get("inputs").and_then(|d| d.as_array()) {
            for i in inputs {
                if let Some(s) = i.as_str() {
                    sg = sg.input(s);
                }
            }
        }
        if let Some(outputs) = item.get("outputs").and_then(|d| d.as_array()) {
            for o in outputs {
                if let Some(s) = o.as_str() {
                    sg = sg.output(s);
                }
            }
        }
        if let Some(v) = item.get("verification").and_then(|x| x.as_str()) {
            sg = sg.verify(v);
        }
        subgoals.push(sg);
    }
    // Dependency references must resolve (fail closed before graph build).
    let ids: Vec<&str> = subgoals.iter().map(|s| s.id.as_str()).collect();
    for sg in &subgoals {
        for d in &sg.depends_on {
            if !ids.contains(&d.as_str()) {
                return Err(format!(
                    "subgoal `{}` depends on unknown `{d}` (available: {})",
                    sg.id,
                    ids.join(", ")
                ));
            }
        }
    }
    let d = DecomposedGoal {
        goal_id: goal_id.to_string(),
        subgoals,
    };
    // Cycle check via topological sort.
    d.topological_order()?;
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decompose::decompose;
    use crate::goal::GoalSpec;

    fn goal(text: &str) -> GoalSpec {
        GoalSpec {
            id: GoalSpec::id_for(text),
            original_request: text.into(),
            objective: text.into(),
            inputs: vec!["report.pdf".into()],
            expected_outputs: vec!["report.md".into()],
            constraints: vec![],
            session_id: None,
            workspace: None,
            requires_network: false,
            requires_verification: true,
        }
    }

    fn allow_all(_p: &str) -> bool {
        true
    }
    fn deny_all(_p: &str) -> bool {
        false
    }

    #[test]
    fn plans_from_real_registry_names() {
        let tools = vec![
            "filesystem.read".to_string(),
            "filesystem.write".to_string(),
            "echo".to_string(),
        ];
        let reg = RegistryView {
            tool_names: &tools,
            policy_allows: &allow_all,
            network_allowed: true,
            model_available: true,
        };
        let d = decompose(&goal("Read the PDF and create a verified report"));
        let plan = plan_capabilities(&d, &reg);
        assert!(!plan.assignments.is_empty());
        assert!(!plan.has_blocking_gap());
        let write = plan
            .assignments
            .iter()
            .find(|a| a.subgoal_id == "generate")
            .unwrap();
        assert_eq!(write.capability, CapabilityClass::FilesystemWrite);
        assert_eq!(write.tool.as_deref(), Some("filesystem.write"));
        assert_eq!(write.role, "coder");
        assert!(write.why.contains("filesystem.write"));
    }

    #[test]
    fn missing_tool_reports_unavailable_not_fabricated() {
        let tools = vec!["filesystem.read".to_string()];
        let reg = RegistryView {
            tool_names: &tools,
            policy_allows: &allow_all,
            network_allowed: true,
            model_available: true,
        };
        let d = decompose(&goal("Create a report file"));
        let plan = plan_capabilities(&d, &reg);
        let gen = plan
            .assignments
            .iter()
            .find(|a| a.subgoal_id == "generate")
            .unwrap();
        // Preferred write tool missing → falls back to echo if present, else unavailable.
        assert_ne!(gen.status, CapabilityStatus::Available);
        assert!(gen.reason.is_some());
    }

    #[test]
    fn permission_denial_surfaces() {
        let tools = vec![
            "filesystem.read".to_string(),
            "filesystem.write".to_string(),
        ];
        let reg = RegistryView {
            tool_names: &tools,
            policy_allows: &deny_all,
            network_allowed: true,
            model_available: true,
        };
        let d = decompose(&goal("Read files and write a report"));
        let plan = plan_capabilities(&d, &reg);
        assert!(plan.has_blocking_gap());
        assert!(plan
            .assignments
            .iter()
            .any(|a| a.status == CapabilityStatus::PermissionDenied));
    }

    #[test]
    fn llm_plan_rejects_malformed() {
        assert!(parse_llm_plan("g", "not json").is_err());
        assert!(parse_llm_plan("g", "{}").is_err());
        assert!(parse_llm_plan("g", r#"{"subgoals":[]}"#).is_err());
        assert!(parse_llm_plan(
            "g",
            r#"{"subgoals":[{"id":"a","objective":"x","depends_on":["missing"]}]}"#
        )
        .is_err());
        assert!(parse_llm_plan(
            "g",
            r#"{"subgoals":[{"id":"a","objective":"x","depends_on":["b"]},{"id":"b","objective":"y","depends_on":["a"]}]}"#
        )
        .is_err());
    }

    #[test]
    fn llm_plan_accepts_valid() {
        let d = parse_llm_plan(
            "goal.x",
            r#"{"subgoals":[{"id":"read","objective":"Read inputs"},{"id":"write","objective":"Write output","depends_on":["read"]}]}"#,
        )
        .unwrap();
        assert_eq!(d.subgoals.len(), 2);
        assert_eq!(d.topological_order().unwrap(), vec!["read", "write"]);
    }
}
