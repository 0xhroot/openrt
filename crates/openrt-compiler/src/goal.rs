//! Natural language → structured goal representation (STEP 2).

use serde::{Deserialize, Serialize};

/// Raw user request plus ambient context the compiler may consult.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NaturalLanguageGoal {
    /// Original user request — ALWAYS preserved as provenance.
    pub request: String,
    /// Session the goal belongs to (for history/workspace context).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Workspace root hint (never used to bypass jails).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

impl NaturalLanguageGoal {
    pub fn new(request: impl Into<String>) -> Self {
        Self {
            request: request.into(),
            session_id: None,
            workspace: None,
        }
    }

    pub fn with_session(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }

    pub fn with_workspace(mut self, w: impl Into<String>) -> Self {
        self.workspace = Some(w.into());
        self
    }
}

/// Structured goal produced by the `understand` stage.
///
/// Never invents missing requirements: fields that cannot be determined from
/// the request stay empty rather than being fabricated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalSpec {
    /// Stable id derived deterministically from the request digest.
    pub id: String,
    /// Original request (provenance — always attached).
    pub original_request: String,
    /// Normalized objective (trimmed, single-line summary of intent).
    pub objective: String,
    /// Input references extracted from the request (files, paths, topics).
    #[serde(default)]
    pub inputs: Vec<String>,
    /// Expected outputs the request explicitly asks for.
    #[serde(default)]
    pub expected_outputs: Vec<String>,
    /// Constraints stated or implied (offline, local-only, format, …).
    #[serde(default)]
    pub constraints: Vec<String>,
    /// Session/workspace context captured at compile time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Whether the goal requires network (conflicts with Sovereign Mode).
    #[serde(default)]
    pub requires_network: bool,
    /// Whether the goal explicitly asks for verification of its output.
    #[serde(default)]
    pub requires_verification: bool,
}

impl GoalSpec {
    /// Deterministic id from the original request (stable across recompiles
    /// of the same text).
    pub fn id_for(request: &str) -> String {
        let digest = openrt_core::sha256_hex(request.trim().as_bytes());
        format!("goal.{}", &digest[..12])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_id_is_deterministic() {
        let a = GoalSpec::id_for("  Create a report. ");
        let b = GoalSpec::id_for("Create a report.");
        assert_eq!(a, b);
        assert!(a.starts_with("goal."));
    }

    #[test]
    fn original_request_preserved() {
        let nl = NaturalLanguageGoal::new("Analyze PDFs and write a report");
        assert_eq!(nl.request, "Analyze PDFs and write a report");
    }
}
