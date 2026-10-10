use openrt_workspace_assets::{ensure_path_safe_id, CanonicalForm, Provenance};
use serde::{Deserialize, Serialize};

use crate::baseline::map_asset;
use crate::ImproveError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    SelfDebug,
    SelfOptimize,
    ResearchChange,
    /// M10.8: an approval-gated agent-genome evolution.
    GenomeEvolution,
}

impl ProposalKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProposalKind::SelfDebug => "self_debug",
            ProposalKind::SelfOptimize => "self_optimize",
            ProposalKind::ResearchChange => "research_change",
            ProposalKind::GenomeEvolution => "genome_evolution",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    Router,
    Tool,
    Plan,
    Prompt,
    Workflow,
    Policy,
    PermissionPolicy,
    SecurityScan,
    Verification,
    /// M10.8: an agent genome (evolution applies through the runtime's
    /// validated genome-apply path — never a raw mutation).
    Genome,
}

impl TargetKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TargetKind::Router => "router",
            TargetKind::Tool => "tool",
            TargetKind::Plan => "plan",
            TargetKind::Prompt => "prompt",
            TargetKind::Workflow => "workflow",
            TargetKind::Policy => "policy",
            TargetKind::PermissionPolicy => "permission_policy",
            TargetKind::SecurityScan => "security_scan",
            TargetKind::Verification => "verification",
            TargetKind::Genome => "genome",
        }
    }

    pub fn is_protected(&self) -> bool {
        matches!(
            self,
            TargetKind::Policy
                | TargetKind::PermissionPolicy
                | TargetKind::SecurityScan
                | TargetKind::Verification
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalState {
    Proposed,
    EvalPassed,
    Approved,
    Rejected,
    Applied,
}

impl ProposalState {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProposalState::Proposed => "proposed",
            ProposalState::EvalPassed => "eval_passed",
            ProposalState::Approved => "approved",
            ProposalState::Rejected => "rejected",
            ProposalState::Applied => "applied",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImprovementProposal {
    pub id: String,
    pub kind: ProposalKind,
    pub summary: String,
    pub target_kind: TargetKind,
    pub target_id: String,
    pub baseline_id: Option<String>,
    pub expected_value: Option<f64>,
    pub state: ProposalState,
    pub research_id: Option<String>,
    /// Non-empty evidence/provenance for production proposals: source
    /// run/history ids, baseline reference, evaluation context, metrics.
    /// Empty only for legacy/test records created before this field existed
    /// (serde default); runtime `improve_propose` always fills it.
    #[serde(default)]
    pub evidence: Vec<String>,
    /// Post-apply measured result (bits of f64), recorded by the runtime
    /// after a real target mutation + re-measurement. `None` = not yet
    /// post-evaluated (or post-eval failed before a number was produced).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_value_bits: Option<u64>,
    /// Operator decision note captured at approve/reject time (persisted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_decision: Option<String>,
    /// Structured mutation the apply target must perform, e.g.
    /// `{"op":"activate_skill"}` or `{"op":"deactivate_skill"}` for
    /// `TargetKind::Tool`, `{"op":"activate_instinct"}` for `Plan`.
    /// Required for production apply (Runtime rejects apply without it
    /// for non-protected mutable kinds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<serde_json::Value>,
    pub provenance: Provenance,
}

impl ImprovementProposal {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: &str,
        kind: ProposalKind,
        summary: &str,
        target_kind: TargetKind,
        target_id: &str,
        baseline_id: Option<String>,
        expected_value: Option<f64>,
        research_id: Option<String>,
        provenance: Provenance,
    ) -> Result<Self, ImproveError> {
        Self::with_evidence(
            id,
            kind,
            summary,
            target_kind,
            target_id,
            baseline_id,
            expected_value,
            research_id,
            provenance,
            Vec::new(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_evidence(
        id: &str,
        kind: ProposalKind,
        summary: &str,
        target_kind: TargetKind,
        target_id: &str,
        baseline_id: Option<String>,
        expected_value: Option<f64>,
        research_id: Option<String>,
        provenance: Provenance,
        evidence: Vec<String>,
    ) -> Result<Self, ImproveError> {
        ensure_path_safe_id(id).map_err(map_asset)?;
        if summary.trim().is_empty() {
            return Err(ImproveError::Malformed {
                detail: "proposal summary must not be empty".into(),
            });
        }
        if target_id.trim().is_empty() {
            return Err(ImproveError::Malformed {
                detail: "proposal target_id must not be empty".into(),
            });
        }
        if kind == ProposalKind::SelfOptimize && (baseline_id.is_none() || expected_value.is_none())
        {
            return Err(ImproveError::Malformed {
                detail: format!(
                    "self-optimization {id} must record a baseline and an expected value"
                ),
            });
        }
        provenance.validate().map_err(map_asset)?;
        Ok(Self {
            id: id.to_string(),
            kind,
            summary: summary.to_string(),
            target_kind,
            target_id: target_id.to_string(),
            baseline_id,
            expected_value,
            state: ProposalState::Proposed,
            research_id,
            evidence,
            post_value_bits: None,
            operator_decision: None,
            change: None,
            provenance,
        })
    }

    /// Attach a structured change payload (builder style for runtime propose).
    pub fn with_change(mut self, change: serde_json::Value) -> Self {
        self.change = Some(change);
        self
    }

    /// Attach a change only when `Some` (used by runtime propose).
    pub fn map_change(mut self, change: Option<serde_json::Value>) -> Self {
        self.change = change;
        self
    }

    /// Post-apply measured candidate (as f64), if recorded.
    pub fn post_value(&self) -> Option<f64> {
        self.post_value_bits.map(f64::from_bits)
    }

    pub fn set_post_value(&mut self, value: f64) {
        self.post_value_bits = Some(value.to_bits());
    }
}

impl CanonicalForm for ImprovementProposal {
    fn asset_kind(&self) -> &'static str {
        "proposal"
    }

    fn asset_id(&self) -> String {
        self.id.clone()
    }

    fn canonical_pairs(&self) -> Vec<(String, String)> {
        let mut pairs = vec![
            ("kind".into(), "proposal".into()),
            ("id".into(), self.id.clone()),
            ("proposal_kind".into(), self.kind.as_str().into()),
            ("summary".into(), self.summary.clone()),
            ("target_kind".into(), self.target_kind.as_str().into()),
            ("target_id".into(), self.target_id.clone()),
            (
                "baseline_id".into(),
                self.baseline_id.clone().unwrap_or_default(),
            ),
            (
                "expected_value_bits".into(),
                self.expected_value
                    .map(|v| v.to_bits().to_string())
                    .unwrap_or_default(),
            ),
            ("state".into(), self.state.as_str().into()),
            (
                "research_id".into(),
                self.research_id.clone().unwrap_or_default(),
            ),
            ("evidence".into(), {
                let mut e = self.evidence.clone();
                e.sort();
                e.join("\n")
            }),
            (
                "post_value_bits".into(),
                self.post_value_bits
                    .map(|b| b.to_string())
                    .unwrap_or_default(),
            ),
            (
                "operator_decision".into(),
                self.operator_decision.clone().unwrap_or_default(),
            ),
            (
                "change".into(),
                self.change
                    .as_ref()
                    .map(|c| c.to_string())
                    .unwrap_or_default(),
            ),
            ("provenance.origin".into(), self.provenance.origin.clone()),
            ("provenance.source".into(), self.provenance.source.clone()),
        ];
        pairs.sort();
        pairs
    }
}
