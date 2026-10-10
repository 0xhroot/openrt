//! Normalized Phase 7B error model.
//!
//! Every tool-fabric failure maps to one of the documented codes with a
//! structured `what` / `why` / `where` / `fix` payload. Operational outcomes
//! (backend down, verification failed, …) are returned as structured `ok:false`
//! tool results; contract violations (invalid input) become [`ToolError`].

use serde::Serialize;
use serde_json::{json, Value};

use openrt_tools::ToolError;

/// The normalized Phase 7B error codes (stable, machine-readable).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    /// The required backend/adapter/session is not available on this host.
    Unavailable,
    /// A permission/capability check refused the operation.
    PermissionDenied,
    /// Operating-mode / network-policy / trust policy refused the operation.
    PolicyDenied,
    /// Resource admission refused the operation.
    ResourceDenied,
    /// Caller-supplied input failed validation.
    InvalidInput,
    /// A referenced entity (session, table, tool, artifact, …) does not exist.
    NotFound,
    /// The operation exceeded its time budget.
    Timeout,
    /// The backend executed but failed (nonzero exit, protocol error, …).
    BackendFailure,
    /// Post-condition verification did not hold.
    VerificationFailed,
    /// Content digest / provenance / integrity check failed.
    IntegrityFailure,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "UNAVAILABLE",
            Self::PermissionDenied => "PERMISSION_DENIED",
            Self::PolicyDenied => "POLICY_DENIED",
            Self::ResourceDenied => "RESOURCE_DENIED",
            Self::InvalidInput => "INVALID_INPUT",
            Self::NotFound => "NOT_FOUND",
            Self::Timeout => "TIMEOUT",
            Self::BackendFailure => "BACKEND_FAILURE",
            Self::VerificationFailed => "VERIFICATION_FAILED",
            Self::IntegrityFailure => "INTEGRITY_FAILURE",
        }
    }
}

/// A normalized tool-fabric error: code + WHAT / WHY / WHERE / FIX.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricError {
    pub code: ErrorCode,
    pub what: String,
    pub why: String,
    pub where_: String,
    pub fix: String,
}

impl FabricError {
    pub fn new(
        code: ErrorCode,
        what: impl Into<String>,
        why: impl Into<String>,
        where_: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self {
            code,
            what: what.into(),
            why: why.into(),
            where_: where_.into(),
            fix: fix.into(),
        }
    }

    pub fn unavailable(
        what: impl Into<String>,
        why: impl Into<String>,
        where_: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self::new(ErrorCode::Unavailable, what, why, where_, fix)
    }

    pub fn invalid_input(
        what: impl Into<String>,
        why: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self::new(
            ErrorCode::InvalidInput,
            what,
            why,
            "tool input validation",
            fix,
        )
    }

    pub fn not_found(
        what: impl Into<String>,
        why: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self::new(ErrorCode::NotFound, what, why, "entity lookup", fix)
    }

    pub fn backend_failure(
        what: impl Into<String>,
        why: impl Into<String>,
        where_: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self::new(ErrorCode::BackendFailure, what, why, where_, fix)
    }

    pub fn timeout(
        what: impl Into<String>,
        why: impl Into<String>,
        where_: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self::new(ErrorCode::Timeout, what, why, where_, fix)
    }

    pub fn policy_denied(
        what: impl Into<String>,
        why: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self::new(
            ErrorCode::PolicyDenied,
            what,
            why,
            "mode / network / trust policy",
            fix,
        )
    }

    pub fn verification_failed(
        what: impl Into<String>,
        why: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self::new(
            ErrorCode::VerificationFailed,
            what,
            why,
            "post-condition verification",
            fix,
        )
    }

    /// Structured `{code, what, why, where, fix}` payload.
    pub fn to_json(&self) -> Value {
        json!({
            "code": self.code.as_str(),
            "what": self.what,
            "why": self.why,
            "where": self.where_,
            "fix": self.fix,
        })
    }

    /// Canonical `ok:false` tool result used by all 7B tools for operational
    /// failures (the agent sees the full structured error, not a bare string).
    pub fn to_output(&self) -> openrt_tools::ToolOutput {
        openrt_tools::ToolOutput::Json(json!({
            "ok": false,
            "error": self.to_json(),
        }))
    }

    /// Map to a [`ToolError`] for contract violations the gate/tool contract
    /// expects as hard errors (invalid input, kill/permission surfaces).
    pub fn into_tool_error(self, tool: &str) -> ToolError {
        match self.code {
            ErrorCode::InvalidInput => {
                ToolError::InvalidArguments(tool.to_string(), format!("{}; {}", self.why, self.fix))
            }
            ErrorCode::PermissionDenied => ToolError::PermissionDenied {
                tool: tool.to_string(),
                reason: self.why,
            },
            ErrorCode::ResourceDenied => ToolError::ResourceDenied {
                tool: tool.to_string(),
                reason: self.why,
            },
            other => ToolError::Internal(
                tool.to_string(),
                format!("{}: {} ({})", other.as_str(), self.why, self.fix),
            ),
        }
    }
}

impl std::fmt::Display for FabricError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {} — why: {} (at {}) — fix: {}",
            self.code.as_str(),
            self.what,
            self.why,
            self.where_,
            self.fix
        )
    }
}

/// Shorthand: structured failure result for a tool body.
pub fn fail(tool: &str, err: FabricError) -> Result<openrt_tools::ToolOutput, ToolError> {
    match err.code {
        ErrorCode::InvalidInput => Err(err.into_tool_error(tool)),
        _ => Ok(err.to_output()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable() {
        assert_eq!(ErrorCode::Unavailable.as_str(), "UNAVAILABLE");
        assert_eq!(ErrorCode::IntegrityFailure.as_str(), "INTEGRITY_FAILURE");
        assert_eq!(ErrorCode::Timeout.as_str(), "TIMEOUT");
    }

    #[test]
    fn error_json_has_four_fields() {
        let e = FabricError::unavailable(
            "browser backend",
            "no firefox binary",
            "browser.open",
            "install firefox or set fabric.browser.backend",
        );
        let v = e.to_json();
        assert_eq!(v["code"], "UNAVAILABLE");
        assert_eq!(v["what"], "browser backend");
        assert_eq!(v["why"], "no firefox binary");
        assert_eq!(v["where"], "browser.open");
        assert_eq!(v["fix"], "install firefox or set fabric.browser.backend");
    }

    #[test]
    fn invalid_input_becomes_tool_error() {
        let e = FabricError::invalid_input("database.query", "missing `sql`", "pass `sql`");
        let te = e.into_tool_error("database.query");
        assert!(matches!(te, ToolError::InvalidArguments(_, _)));
    }

    #[test]
    fn operational_failures_render_as_ok_false_output() {
        let e = FabricError::timeout("mcp call", "5s", "mcp.servers", "raise timeout");
        let out = fail("mcp.servers", e).unwrap();
        let v = out.render();
        assert!(v.contains("\"ok\": false"));
        assert!(v.contains("TIMEOUT"));
    }
}
