//! Structured compilation diagnostics (WHAT / WHY / WHERE / HOW TO FIX).

use serde::{Deserialize, Serialize};

/// Machine-readable diagnostic codes. Never collapse distinct failures into a
/// generic "Compilation failed."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCode {
    UnsupportedGoal,
    AmbiguousGoal,
    MissingCapability,
    UnavailableModel,
    UnavailableTool,
    MissingInput,
    ResourceConflict,
    PermissionDenied,
    SovereignConflict,
    DependencyConflict,
    InvalidWorkflow,
    UnsatisfiedVerification,
    CompilationFailure,
    InvalidLlmPlan,
    EmptyGoal,
}

impl DiagnosticCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UnsupportedGoal => "unsupported_goal",
            Self::AmbiguousGoal => "ambiguous_goal",
            Self::MissingCapability => "missing_capability",
            Self::UnavailableModel => "unavailable_model",
            Self::UnavailableTool => "unavailable_tool",
            Self::MissingInput => "missing_input",
            Self::ResourceConflict => "resource_conflict",
            Self::PermissionDenied => "permission_denied",
            Self::SovereignConflict => "sovereign_conflict",
            Self::DependencyConflict => "dependency_conflict",
            Self::InvalidWorkflow => "invalid_workflow",
            Self::UnsatisfiedVerification => "unsatisfied_verification",
            Self::CompilationFailure => "compilation_failure",
            Self::InvalidLlmPlan => "invalid_llm_plan",
            Self::EmptyGoal => "empty_goal",
        }
    }
}

/// Severity: errors block compilation; warnings are recorded but do not fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
}

/// One structured diagnostic. Every field is required so callers can never
/// receive a bare "failed" message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompileDiagnostic {
    pub code: DiagnosticCode,
    pub severity: DiagnosticSeverity,
    /// WHAT failed (short label).
    pub what: String,
    /// WHY it failed (root cause).
    pub why: String,
    /// WHERE: goal id, subgoal id, step id, or stage name.
    pub where_: String,
    /// HOW TO FIX: actionable remediation.
    pub how_to_fix: String,
}

impl CompileDiagnostic {
    pub fn error(
        code: DiagnosticCode,
        what: impl Into<String>,
        why: impl Into<String>,
        where_: impl Into<String>,
        how_to_fix: impl Into<String>,
    ) -> Self {
        Self {
            code,
            severity: DiagnosticSeverity::Error,
            what: what.into(),
            why: why.into(),
            where_: where_.into(),
            how_to_fix: how_to_fix.into(),
        }
    }

    pub fn warning(
        code: DiagnosticCode,
        what: impl Into<String>,
        why: impl Into<String>,
        where_: impl Into<String>,
        how_to_fix: impl Into<String>,
    ) -> Self {
        Self {
            code,
            severity: DiagnosticSeverity::Warning,
            what: what.into(),
            why: why.into(),
            where_: where_.into(),
            how_to_fix: how_to_fix.into(),
        }
    }

    pub fn is_error(&self) -> bool {
        self.severity == DiagnosticSeverity::Error
    }
}

impl std::fmt::Display for CompileDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{:?}] {}: {} (at {}) — fix: {}",
            self.code, self.what, self.why, self.where_, self.how_to_fix
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_serializes_all_fields() {
        let d = CompileDiagnostic::error(
            DiagnosticCode::MissingCapability,
            "capability OCR",
            "ocr.extract is not registered in the tool registry",
            "subgoal.extract_text",
            "register an OCR tool or rephrase the goal without OCR",
        );
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["code"], "missing_capability");
        assert_eq!(v["severity"], "error");
        assert!(v["how_to_fix"].as_str().unwrap().contains("OCR"));
        assert!(d.is_error());
    }
}
