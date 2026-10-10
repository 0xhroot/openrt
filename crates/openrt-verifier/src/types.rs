//! Core verification types shared across the deterministic, model and
//! firewall layers (M9.8).

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Semantic verdict on a claim. Produced only by a layer that actually ran:
/// deterministic evidence checks, a real model response, or both — never by
/// echoing the claim back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Evidence (and, when run, a real model) supports the claim.
    Supported,
    /// Evidence or a real model contradicts the claim.
    Unsupported,
    /// Cannot be decided honestly with what ran (gap, disagreement,
    /// unreachable backend).
    Insufficient,
}

/// Hallucination-firewall action at a production answer path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FirewallAction {
    /// Claim may be presented as grounded.
    Pass,
    /// Claim must be surfaced for review (inconclusive, degraded, partial).
    Flag,
    /// Claim must not be presented as grounded.
    Block,
}

/// Which layer produced the final verdict (recorded on every verdict so a
/// deterministic pass is never mistaken for a model-backed one).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    Deterministic,
    Model,
    CrossModel,
}

/// Resource/time caps for verification (M9.8: explicit limits — no
/// unbounded model calls, no unbounded evidence context).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyLimits {
    /// Wall-clock cap for each individual model call.
    pub timeout: Duration,
    /// Total evidence characters handed to a model (clipped honestly, with
    /// the clip recorded on the report).
    pub max_evidence_chars: usize,
    /// Maximum claims accepted in one batch verification request.
    pub max_claims: usize,
}

impl Default for VerifyLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            max_evidence_chars: 4_000,
            max_claims: 16,
        }
    }
}

/// Honest cross-model independence (M9.8): `Independent` only when at least
/// two genuinely distinct model ids each produced a verdict. The same model
/// run twice is never independent — it is reported as an explicit
/// degradation instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Independence {
    Independent { backends: Vec<String> },
    Degraded { reason: String },
}

impl Independence {
    /// True only for a genuinely independent cross-model run.
    pub fn is_independent(&self) -> bool {
        matches!(self, Independence::Independent { .. })
    }

    /// Human-true one-line description for reports/CLI output.
    pub fn describe(&self) -> String {
        match self {
            Independence::Independent { backends } => {
                format!("independent ({} distinct models)", backends.len())
            }
            Independence::Degraded { reason } => format!("degraded: {reason}"),
        }
    }
}
