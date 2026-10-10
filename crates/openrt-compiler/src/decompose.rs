//! Automatic goal decomposition (STEP 3).

use serde::{Deserialize, Serialize};

use crate::goal::GoalSpec;

/// One decomposed unit of work with explicit dependency edges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubGoal {
    pub id: String,
    pub objective: String,
    /// Subgoal ids that must complete first (executable edges, not comments).
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub required_inputs: Vec<String>,
    #[serde(default)]
    pub expected_outputs: Vec<String>,
    /// How success is determined (passed to the verifier role when present).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<String>,
    /// Failure implication (e.g. "blocks dependents").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_implication: Option<String>,
}

impl SubGoal {
    pub fn new(id: impl Into<String>, objective: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            objective: objective.into(),
            depends_on: Vec::new(),
            required_inputs: Vec::new(),
            expected_outputs: Vec::new(),
            verification: None,
            failure_implication: None,
        }
    }

    pub fn dep(mut self, id: &str) -> Self {
        self.depends_on.push(id.to_string());
        self
    }

    pub fn input(mut self, i: &str) -> Self {
        self.required_inputs.push(i.to_string());
        self
    }

    pub fn output(mut self, o: &str) -> Self {
        self.expected_outputs.push(o.to_string());
        self
    }

    pub fn verify(mut self, v: &str) -> Self {
        self.verification = Some(v.to_string());
        self
    }

    pub fn on_fail(mut self, implication: &str) -> Self {
        self.failure_implication = Some(implication.to_string());
        self
    }
}

/// Result of the `decompose` stage: ordered/partially-ordered subgoals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecomposedGoal {
    pub goal_id: String,
    pub subgoals: Vec<SubGoal>,
}

impl DecomposedGoal {
    pub fn find(&self, id: &str) -> Option<&SubGoal> {
        self.subgoals.iter().find(|s| s.id == id)
    }

    /// Topological order of subgoal ids (Kahn). Errors on cycles / unknown deps.
    pub fn topological_order(&self) -> Result<Vec<String>, String> {
        let ids: Vec<&str> = self.subgoals.iter().map(|s| s.id.as_str()).collect();
        let mut indegree: std::collections::HashMap<&str, usize> =
            ids.iter().map(|id| (*id, 0usize)).collect();
        let mut edges: std::collections::HashMap<&str, Vec<&str>> =
            std::collections::HashMap::new();
        for sg in &self.subgoals {
            for dep in &sg.depends_on {
                if !ids.contains(&dep.as_str()) {
                    return Err(format!(
                        "subgoal `{}` depends on unknown subgoal `{dep}`",
                        sg.id
                    ));
                }
                edges.entry(dep.as_str()).or_default().push(sg.id.as_str());
                *indegree.get_mut(sg.id.as_str()).unwrap() += 1;
            }
        }
        let mut ready: Vec<&str> = indegree
            .iter()
            .filter(|(_, d)| **d == 0)
            .map(|(id, _)| *id)
            .collect();
        ready.sort_unstable(); // deterministic
        let mut order = Vec::new();
        while let Some(id) = ready.pop() {
            order.push(id.to_string());
            let mut nexts: Vec<&str> = edges.get(id).map(|v| v.as_slice()).unwrap_or(&[]).to_vec();
            nexts.sort_unstable();
            for next in nexts {
                let d = indegree.get_mut(next).unwrap();
                *d -= 1;
                if *d == 0 {
                    ready.push(next);
                }
            }
        }
        if order.len() != self.subgoals.len() {
            return Err("decomposition contains a dependency cycle".into());
        }
        Ok(order)
    }
}

/// Deterministic, generalizable decomposer.
///
/// Does NOT hardcode one giant workflow for one demo: intent patterns map to
/// reusable subgoal templates that compose from the request's own inputs and
/// expected outputs. Unsupported/empty goals fail closed at the caller.
pub fn decompose(goal: &GoalSpec) -> DecomposedGoal {
    let lower = goal.objective.to_lowercase();
    let request = goal.original_request.to_lowercase();

    // Structured intent overlay: the historic needles below stay authoritative
    // for summarize/verify/report detection; intent classes widen read/analyze/
    // write detection to the full normalized vocabulary (export/render/prepare/
    // draft/…) without letting summary markers create analysis/write flags the
    // historic decomposer never produced (behavior stays a superset only where
    // the new vocabulary genuinely adds signal).
    let mut intent = crate::intent::normalize(&goal.original_request);
    intent.merge(crate::intent::normalize(&goal.objective));
    let summary_marker = contains_any(&lower, &["summar", "digest", "overview"]);
    let summary_marker = summary_marker || intent.summary;

    let mentions_read = contains_any(
        &lower,
        &["read", "load", "open", "ingest", "parse", "extract"],
    ) || intent.has(crate::intent::ActionClass::Read)
        || intent.has(crate::intent::ActionClass::Navigate);
    let mentions_analyze = contains_any(
        &lower,
        &["analy", "compare", "review", "inspect", "identify", "find"],
    ) || (intent.has(crate::intent::ActionClass::Analyze)
        && !summary_marker);
    let mentions_write = contains_any(
        &lower,
        &[
            "write", "create", "generate", "produce", "save", "make", "build",
        ],
    ) || (intent.has(crate::intent::ActionClass::Create) && !summary_marker);
    let mentions_summar = summary_marker;
    let mentions_verify = goal.requires_verification
        || contains_any(&lower, &["verify", "validate", "check", "confirm"])
        || contains_any(&request, &["verified", "validated"]);
    let mentions_parallel = contains_any(
        &lower,
        &["independently", "each", "all of", "parallel", "separately"],
    );
    let mentions_report = contains_any(&lower, &["report", "document", "write-up", "writeup"]);

    let inputs = if goal.inputs.is_empty() {
        vec!["workspace files".to_string()]
    } else {
        goal.inputs.clone()
    };
    let outputs = if goal.expected_outputs.is_empty() {
        if mentions_report {
            vec!["report artifact".to_string()]
        } else {
            vec!["goal output".to_string()]
        }
    } else {
        goal.expected_outputs.clone()
    };

    let mut subgoals: Vec<SubGoal> = Vec::new();

    // Always start from understanding/planning unless the goal is trivially atomic.
    let needs_plan = mentions_analyze || mentions_write || mentions_summar || inputs.len() > 1;
    if needs_plan {
        subgoals.push(
            SubGoal::new("plan", "Clarify objective, inputs, and success criteria")
                .output("plan")
                .verify("plan lists concrete steps and inputs")
                .on_fail("blocks all downstream work"),
        );
    }
    let plan_id = if needs_plan { Some("plan") } else { None };

    // Discover / ingest inputs.
    let needs_discover =
        !goal.inputs.is_empty() || mentions_read || mentions_analyze || mentions_write;
    if needs_discover {
        let mut sg = SubGoal::new("discover", "Discover and list required input sources")
            .output("input inventory")
            .verify("every referenced input is listed or explicitly missing")
            .on_fail("blocks analysis and generation");
        if let Some(p) = plan_id {
            sg = sg.dep(p);
        }
        for i in &inputs {
            sg = sg.input(i);
        }
        subgoals.push(sg);
    }
    let discover_id = if needs_discover {
        Some("discover")
    } else {
        None
    };

    // Read / ingest content.
    let needs_read = mentions_read || mentions_analyze || mentions_summar;
    if needs_read {
        let mut sg = SubGoal::new("ingest", "Read and extract content from inputs")
            .output("extracted content")
            .verify("extraction covers each discovered input")
            .on_fail("blocks analysis");
        if let Some(d) = discover_id {
            sg = sg.dep(d);
        } else if let Some(p) = plan_id {
            sg = sg.dep(p);
        }
        for i in &inputs {
            sg = sg.input(i);
        }
        subgoals.push(sg);
    }
    let ingest_id = if needs_read { Some("ingest") } else { None };

    // Independent parallel analysis branches when the request asks for it.
    if mentions_analyze && mentions_parallel && inputs.len() >= 2 {
        for (idx, input) in inputs.iter().enumerate() {
            let id = format!("analyze.{}", idx + 1);
            let mut sg = SubGoal::new(&id, format!("Independently analyze `{input}`"))
                .output(&format!("findings for {input}"))
                .verify("findings cite the analyzed input")
                .on_fail("does not block unrelated analysis branches");
            if let Some(i) = ingest_id {
                sg = sg.dep(i);
            } else if let Some(d) = discover_id {
                sg = sg.dep(d);
            }
            sg = sg.input(input);
            subgoals.push(sg);
        }
        // Aggregate after all parallel branches.
        let mut agg = SubGoal::new("aggregate", "Aggregate independent findings")
            .output("combined findings")
            .verify("aggregation references every analysis branch")
            .on_fail("blocks final generation");
        for (idx, _) in inputs.iter().enumerate() {
            agg = agg.dep(&format!("analyze.{}", idx + 1));
        }
        subgoals.push(agg);
    } else if mentions_analyze {
        let mut sg = SubGoal::new("analyze", "Analyze inputs against requirements")
            .output("findings")
            .verify("findings address the stated objective")
            .on_fail("blocks generation");
        if let Some(i) = ingest_id {
            sg = sg.dep(i);
        } else if let Some(d) = discover_id {
            sg = sg.dep(d);
        } else if let Some(p) = plan_id {
            sg = sg.dep(p);
        }
        for i in &inputs {
            sg = sg.input(i);
        }
        subgoals.push(sg);
    }
    let analyze_id = if mentions_analyze {
        if mentions_parallel && inputs.len() >= 2 {
            Some("aggregate")
        } else {
            Some("analyze")
        }
    } else {
        None
    };
    let has_parallel_branches = mentions_analyze && mentions_parallel && inputs.len() >= 2;

    // Generate the primary artifact.
    if mentions_write || mentions_summar || mentions_report {
        let mut sg = SubGoal::new(
            "generate",
            if mentions_summar && !mentions_report {
                "Generate the requested summary".to_string()
            } else {
                "Generate the requested output artifact".to_string()
            },
        )
        .output(
            &outputs
                .first()
                .cloned()
                .unwrap_or_else(|| "artifact".into()),
        )
        .verify("artifact exists and matches the requested form")
        .on_fail("blocks verification and persistence");
        if has_parallel_branches {
            sg = sg.dep("aggregate");
        } else if let Some(a) = analyze_id {
            sg = sg.dep(a);
        } else if let Some(i) = ingest_id {
            sg = sg.dep(i);
        } else if let Some(d) = discover_id {
            sg = sg.dep(d);
        } else if let Some(p) = plan_id {
            sg = sg.dep(p);
        }
        subgoals.push(sg);
    }
    let generate_id = if mentions_write || mentions_summar || mentions_report {
        Some("generate")
    } else {
        None
    };

    // Verify.
    if mentions_verify || (mentions_write && goal.requires_verification) || mentions_report {
        let mut sg = SubGoal::new("verify", "Verify the output against the objective")
            .input("generated artifact")
            .output("verification result")
            .verify("pass only when the artifact satisfies the stated criteria")
            .on_fail("final artifact is not trusted");
        if let Some(g) = generate_id {
            sg = sg.dep(g);
        } else if has_parallel_branches {
            sg = sg.dep("aggregate");
        } else if let Some(a) = analyze_id {
            sg = sg.dep(a);
        }
        subgoals.push(sg);
    }
    let verify_id = if mentions_verify || mentions_report {
        Some("verify")
    } else {
        None
    };

    // Persist final artifact when the goal asks to save/create something durable.
    if mentions_write || mentions_report {
        let mut sg = SubGoal::new("persist", "Persist the final artifact")
            .input("verified output")
            .output("durable artifact")
            .verify("artifact is readable after write")
            .on_fail("result is lost");
        if let Some(v) = verify_id {
            sg = sg.dep(v);
        } else if let Some(g) = generate_id {
            sg = sg.dep(g);
        }
        for o in &outputs {
            sg = sg.output(o);
        }
        subgoals.push(sg);
    }

    // Fallback: single-step goal when no pattern matched (still generalizable).
    if subgoals.is_empty() {
        subgoals.push(
            SubGoal::new("execute", format!("Work toward: {}", goal.objective))
                .input(&goal.original_request)
                .output("result")
                .verify("result addresses the original request"),
        );
    }

    DecomposedGoal {
        goal_id: goal.id.clone(),
        subgoals,
    }
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal::GoalSpec;

    fn goal(text: &str) -> GoalSpec {
        GoalSpec {
            id: GoalSpec::id_for(text),
            original_request: text.into(),
            objective: text.into(),
            inputs: vec![],
            expected_outputs: vec![],
            constraints: vec![],
            session_id: None,
            workspace: None,
            requires_network: false,
            requires_verification: false,
        }
    }

    #[test]
    fn decomposes_report_goal_with_deps() {
        let mut g = goal(
            "Analyze all inspection PDFs independently, compare findings, and create a verified report",
        );
        g.inputs = vec!["inspection1.pdf".into(), "inspection2.pdf".into()];
        let d = decompose(&g);
        assert!(d.find("plan").is_some());
        assert!(d.find("analyze.1").is_some());
        assert!(d.find("analyze.2").is_some());
        assert!(d.find("aggregate").is_some());
        assert!(d.find("generate").is_some());
        assert!(d.find("verify").is_some());
        assert!(d.find("persist").is_some());
        let order = d.topological_order().unwrap();
        let pos = |id: &str| order.iter().position(|x| x == id).unwrap();
        assert!(pos("analyze.1") < pos("aggregate"));
        assert!(pos("aggregate") < pos("generate"));
        assert!(pos("generate") < pos("verify"));
    }

    #[test]
    fn atomic_goal_still_produces_subgoals() {
        let d = decompose(&goal("Create a file containing the current project status"));
        assert!(!d.subgoals.is_empty());
        d.topological_order().unwrap();
    }

    #[test]
    fn parallel_inputs_create_independent_branches() {
        let mut g = goal("Analyze files A, B, and C independently then combine");
        g.inputs = vec!["A".into(), "B".into(), "C".into()];
        let d = decompose(&g);
        assert_eq!(
            d.subgoals
                .iter()
                .filter(|s| s.id.starts_with("analyze."))
                .count(),
            3
        );
        let agg = d.find("aggregate").unwrap();
        assert_eq!(agg.depends_on.len(), 3);
    }
}
