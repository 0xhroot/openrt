use openrt_workspace_assets::{ensure_path_safe_id, CanonicalForm, Provenance};
use serde::{Deserialize, Serialize};

use crate::baseline::map_asset;
use crate::ImproveError;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DebugReport {
    pub id: String,
    pub run_id: String,
    pub failure_signature: String,
    pub reproduction_steps: Vec<String>,
    pub diagnosis: String,
    pub proposal_id: Option<String>,
    pub provenance: Provenance,
}

impl DebugReport {
    pub fn new(
        id: &str,
        run_id: &str,
        failure_signature: &str,
        reproduction_steps: Vec<String>,
        diagnosis: &str,
        proposal_id: Option<String>,
        provenance: Provenance,
    ) -> Result<Self, ImproveError> {
        ensure_path_safe_id(id).map_err(map_asset)?;
        if run_id.trim().is_empty() {
            return Err(ImproveError::Malformed {
                detail: "debug report run_id must not be empty".into(),
            });
        }
        if failure_signature.trim().is_empty() {
            return Err(ImproveError::Malformed {
                detail: "debug report failure_signature must not be empty".into(),
            });
        }
        if reproduction_steps.is_empty() || reproduction_steps.iter().any(|s| s.trim().is_empty()) {
            return Err(ImproveError::Malformed {
                detail: "debug report needs at least one non-empty reproduction step".into(),
            });
        }
        if diagnosis.trim().is_empty() {
            return Err(ImproveError::Malformed {
                detail: "debug report diagnosis must not be empty".into(),
            });
        }
        provenance.validate().map_err(map_asset)?;
        Ok(Self {
            id: id.to_string(),
            run_id: run_id.to_string(),
            failure_signature: failure_signature.to_string(),
            reproduction_steps,
            diagnosis: diagnosis.to_string(),
            proposal_id,
            provenance,
        })
    }
}

impl CanonicalForm for DebugReport {
    fn asset_kind(&self) -> &'static str {
        "debug"
    }

    fn asset_id(&self) -> String {
        self.id.clone()
    }

    fn canonical_pairs(&self) -> Vec<(String, String)> {
        let mut pairs = vec![
            ("kind".into(), "debug".into()),
            ("id".into(), self.id.clone()),
            ("run_id".into(), self.run_id.clone()),
            ("failure_signature".into(), self.failure_signature.clone()),
            (
                "reproduction_steps".into(),
                self.reproduction_steps.join("\n"),
            ),
            ("diagnosis".into(), self.diagnosis.clone()),
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
