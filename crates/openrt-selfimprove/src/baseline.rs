use openrt_workspace_assets::{ensure_path_safe_id, CanonicalForm, Provenance};
use serde::{Deserialize, Serialize};

use crate::ImproveError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricDirection {
    HigherIsBetter,
    LowerIsBetter,
}

impl MetricDirection {
    pub fn as_str(&self) -> &'static str {
        match self {
            MetricDirection::HigherIsBetter => "higher_is_better",
            MetricDirection::LowerIsBetter => "lower_is_better",
        }
    }

    pub fn beats(&self, baseline: f64, candidate: f64) -> bool {
        match self {
            MetricDirection::HigherIsBetter => candidate > baseline,
            MetricDirection::LowerIsBetter => candidate < baseline,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    pub id: String,
    pub metric: String,
    pub value: f64,
    pub direction: MetricDirection,
    pub run_id: String,
    pub provenance: Provenance,
}

impl Baseline {
    pub fn new(
        id: &str,
        metric: &str,
        value: f64,
        direction: MetricDirection,
        run_id: &str,
        provenance: Provenance,
    ) -> Result<Self, ImproveError> {
        ensure_path_safe_id(id).map_err(map_asset)?;
        if metric.trim().is_empty() {
            return Err(ImproveError::Malformed {
                detail: "baseline metric must not be empty".into(),
            });
        }
        if run_id.trim().is_empty() {
            return Err(ImproveError::Malformed {
                detail: "baseline run_id must not be empty".into(),
            });
        }
        provenance.validate().map_err(map_asset)?;
        Ok(Self {
            id: id.to_string(),
            metric: metric.to_string(),
            value,
            direction,
            run_id: run_id.to_string(),
            provenance,
        })
    }

    pub fn beats(&self, candidate: f64) -> bool {
        self.direction.beats(self.value, candidate)
    }
}

impl CanonicalForm for Baseline {
    fn asset_kind(&self) -> &'static str {
        "baseline"
    }

    fn asset_id(&self) -> String {
        self.id.clone()
    }

    fn canonical_pairs(&self) -> Vec<(String, String)> {
        vec![
            ("kind".into(), "baseline".into()),
            ("id".into(), self.id.clone()),
            ("metric".into(), self.metric.clone()),
            ("value_bits".into(), self.value.to_bits().to_string()),
            ("direction".into(), self.direction.as_str().into()),
            ("run_id".into(), self.run_id.clone()),
            ("provenance.origin".into(), self.provenance.origin.clone()),
            ("provenance.source".into(), self.provenance.source.clone()),
        ]
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvalOutcome {
    pub id: String,
    pub baseline_value: f64,
    pub candidate_value: f64,
    pub beats_baseline: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EvalResult {
    Compared(EvalOutcome),
    NoBaseline,
}

pub(crate) fn map_asset(e: openrt_workspace_assets::AssetError) -> ImproveError {
    use openrt_workspace_assets::AssetError as A;
    match e {
        A::DigestMismatch { kind, id } => ImproveError::DigestMismatch { kind, id },
        A::InvalidProvenance => ImproveError::InvalidProvenance,
        A::Malformed { detail } => ImproveError::Malformed { detail },
        A::Store(s) => ImproveError::Store(s),
        other => ImproveError::Malformed {
            detail: other.to_string(),
        },
    }
}
