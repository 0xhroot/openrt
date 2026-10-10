//! Declarative workflow DAG model (P2.3, M2.2).

use serde::{Deserialize, Serialize};

use crate::SubAgentKind;

/// Terminal-ish status of a workflow run. `Paused` (Phase 11) is NOT a
/// failure: the run stopped cooperatively at a checkpoint with partial
/// history preserved and stays resumable — never marked failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStatus {
    Completed,
    Failed,
    Cancelled,
    /// Cooperative checkpoint pause: partial history preserved, resumable
    /// by explicit operator action (never auto-resumed at boot).
    Paused,
}

impl std::fmt::Display for WorkflowStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Paused => "paused",
        };
        f.write_str(s)
    }
}

/// A serializable gate on a step (P2.3): the step only runs when its
/// `depends_on` outputs are ready *and* the condition holds against those
/// outputs or the blackboard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Condition {
    /// `blackboard.key` equals the given value.
    BlackboardEquals {
        key: String,
        value: serde_json::Value,
    },
    /// `blackboard.key` exists (any value).
    BlackboardExists { key: String },
    /// Output text of a completed dependency (`output.<dep>`) contains the
    /// needle (substring, case-sensitive).
    OutputContains { step: String, needle: String },
}

impl Condition {
    pub fn eval(
        &self,
        outputs: &std::collections::HashMap<String, String>,
        blackboard: &openrt_blackboard::Blackboard,
    ) -> Result<bool, crate::WorkflowError> {
        match self {
            Condition::BlackboardEquals { key, value } => {
                Ok(blackboard.get(key)?.as_ref() == Some(value))
            }
            Condition::BlackboardExists { key } => Ok(blackboard.get(key)?.is_some()),
            Condition::OutputContains { step, needle } => {
                let text = outputs.get(step).ok_or_else(|| {
                    crate::WorkflowError::Condition(format!("dependency `{step}` has no output"))
                })?;
                Ok(text.contains(needle))
            }
        }
    }
}

/// One node of the workflow DAG.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowStep {
    /// Stable id, used as the blackboard artifact key (`output.<id>`), so it
    /// must be a valid [`Blackboard`] key (`a-z0-9._-`).
    pub id: String,
    pub kind: SubAgentKind,
    /// Task given to the role agent.
    pub prompt: String,
    /// Steps whose artifacts must exist before this one runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// Retries after a failed agent run (0 = no retry).
    #[serde(default)]
    pub max_retries: u32,
    /// Only run when the condition holds (None = always run, once deps met).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only_if: Option<Condition>,
    /// Phase 8: sandbox profile this step's tools must run under. Declared
    /// requirements are validated against the sandbox authority BEFORE the
    /// first step executes (fail closed); per-tool enforcement still happens
    /// at the gate on every tool call. `None` = no declared requirement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
}

impl WorkflowStep {
    pub fn new(id: impl Into<String>, kind: SubAgentKind, prompt: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind,
            prompt: prompt.into(),
            depends_on: Vec::new(),
            max_retries: 0,
            only_if: None,
            sandbox: None,
        }
    }

    /// Declare the sandbox profile this step runs under (Phase 8).
    pub fn sandboxed(mut self, profile: &str) -> Self {
        self.sandbox = Some(profile.to_string());
        self
    }

    pub fn dep(mut self, id: &str) -> Self {
        self.depends_on.push(id.to_string());
        self
    }

    pub fn retries(mut self, n: u32) -> Self {
        self.max_retries = n;
        self
    }

    pub fn when(mut self, condition: Condition) -> Self {
        self.only_if = Some(condition);
        self
    }
}

/// A DAG of role steps. Serialized to the `dag/` store namespace as the
/// durable record of what a run was.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workflow {
    pub id: String,
    pub name: Option<String>,
    pub steps: Vec<WorkflowStep>,
}

impl Workflow {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: None,
            steps: Vec::new(),
        }
    }

    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn step(mut self, step: WorkflowStep) -> Self {
        self.steps.push(step);
        self
    }

    /// Index a step id → step (None when unknown).
    pub fn find_step(&self, id: &str) -> Option<&WorkflowStep> {
        self.steps.iter().find(|s| s.id == id)
    }

    /// Kahn topological order, or an error for unknown deps / cycles.
    pub fn topological_sort(&self) -> Result<Vec<&WorkflowStep>, super::WorkflowError> {
        let ids: Vec<&str> = self.steps.iter().map(|s| s.id.as_str()).collect();
        if self.steps.len() != ids.iter().collect::<std::collections::HashSet<_>>().len() {
            return Err(super::WorkflowError::Condition("duplicate step id".into()));
        }
        let mut indegree: std::collections::HashMap<&str, usize> =
            ids.iter().map(|id| (*id, 0usize)).collect();
        let mut edges: std::collections::HashMap<&str, Vec<&str>> =
            std::collections::HashMap::new();
        for step in &self.steps {
            for dep in &step.depends_on {
                if !ids.contains(&dep.as_str()) {
                    return Err(super::WorkflowError::Condition(format!(
                        "step `{}` depends on unknown step `{dep}`",
                        step.id
                    )));
                }
                edges
                    .entry(dep.as_str())
                    .or_default()
                    .push(step.id.as_str());
                *indegree.get_mut(step.id.as_str()).unwrap() += 1;
            }
        }
        let mut ready: Vec<&str> = indegree
            .iter()
            .filter(|(_, d)| **d == 0)
            .map(|(id, _)| *id)
            .collect();
        let mut order = Vec::new();
        while let Some(id) = ready.pop() {
            order.push(id);
            for &next in edges.get(id).map(|v| v.as_slice()).unwrap_or(&[]) {
                let d = indegree.get_mut(next).unwrap();
                *d -= 1;
                if *d == 0 {
                    ready.push(next);
                }
            }
        }
        if order.len() != self.steps.len() {
            return Err(super::WorkflowError::Condition(
                "workflow contains a dependency cycle".into(),
            ));
        }
        Ok(order
            .into_iter()
            .map(|id| self.steps.iter().find(|s| s.id == id).unwrap())
            .collect())
    }
}
