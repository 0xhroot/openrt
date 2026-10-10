//! Compilation modes (STEP 10).

use serde::{Deserialize, Serialize};

/// Distinct conceptual compilation modes. Execution is impossible unless
/// validation succeeded (enforced by [`crate::WorkflowCompiler`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompileMode {
    /// Produce the intended workflow without persistence or execution.
    Explain,
    /// Produce and persist the workflow (no execution).
    Compile,
    /// Compile, fully validate, and persist — do not execute.
    Validate,
    /// Compile, validate, persist, then hand off to the existing orchestrator.
    Execute,
}

impl CompileMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Explain => "explain",
            Self::Compile => "compile",
            Self::Validate => "validate",
            Self::Execute => "execute",
        }
    }

    pub fn persists(&self) -> bool {
        !matches!(self, Self::Explain)
    }

    pub fn validates(&self) -> bool {
        matches!(self, Self::Validate | Self::Execute)
    }

    pub fn executes(&self) -> bool {
        matches!(self, Self::Execute)
    }
}

impl std::str::FromStr for CompileMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "explain" => Ok(Self::Explain),
            "compile" => Ok(Self::Compile),
            "validate" => Ok(Self::Validate),
            "execute" => Ok(Self::Execute),
            other => Err(format!(
                "unknown compile mode `{other}` (expected explain|compile|validate|execute)"
            )),
        }
    }
}
