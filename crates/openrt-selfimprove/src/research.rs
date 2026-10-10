use openrt_workspace_assets::{ensure_path_safe_id, CanonicalForm, Provenance};
use serde::{Deserialize, Serialize};

use crate::baseline::map_asset;
use crate::ImproveError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDecision {
    Approve,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResearchNote {
    pub id: String,
    pub question: String,
    pub findings: Vec<String>,
    pub proposal_id: Option<String>,
    pub provenance: Provenance,
}

impl ResearchNote {
    pub fn new(
        id: &str,
        question: &str,
        findings: Vec<String>,
        proposal_id: Option<String>,
        provenance: Provenance,
    ) -> Result<Self, ImproveError> {
        ensure_path_safe_id(id).map_err(map_asset)?;
        if question.trim().is_empty() {
            return Err(ImproveError::Malformed {
                detail: "research question must not be empty".into(),
            });
        }
        if findings.is_empty() || findings.iter().any(|f| f.trim().is_empty()) {
            return Err(ImproveError::Malformed {
                detail: "research needs at least one non-empty finding".into(),
            });
        }
        provenance.validate().map_err(map_asset)?;
        Ok(Self {
            id: id.to_string(),
            question: question.to_string(),
            findings,
            proposal_id,
            provenance,
        })
    }
}

impl CanonicalForm for ResearchNote {
    fn asset_kind(&self) -> &'static str {
        "research"
    }

    fn asset_id(&self) -> String {
        self.id.clone()
    }

    fn canonical_pairs(&self) -> Vec<(String, String)> {
        let mut pairs = vec![
            ("kind".into(), "research".into()),
            ("id".into(), self.id.clone()),
            ("question".into(), self.question.clone()),
            ("findings".into(), self.findings.join("\n")),
            (
                "proposal_id".into(),
                self.proposal_id.clone().unwrap_or_default(),
            ),
            ("provenance.origin".into(), self.provenance.origin.clone()),
            ("provenance.source".into(), self.provenance.source.clone()),
        ];
        pairs.sort();
        pairs
    }
}
