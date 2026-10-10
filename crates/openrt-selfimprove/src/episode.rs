//! Experience episodes (M12.12) and promotion records (M12.17): sealed,
//! provenance-bearing records in the ONE improve store. Evaluation splits
//! MEASURED metrics (facts of the run) from INFERRED heuristics (labelled).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Provenance;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeOutcome {
    Success,
    Partial,
    Failure,
}

impl EpisodeOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            EpisodeOutcome::Success => "success",
            EpisodeOutcome::Partial => "partial",
            EpisodeOutcome::Failure => "failure",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verification {
    Passed,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperienceEpisode {
    pub id: String,
    pub task: String,
    pub context: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_ref: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub actions: Vec<String>,
    pub outcome: EpisodeOutcome,
    pub verification: Verification,
    pub errors: usize,
    pub retries: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    pub provenance: Provenance,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EpisodeEvaluation {
    pub episode_id: String,
    /// Numbers taken directly from the record — verifiable facts.
    #[serde(default)]
    pub measured: BTreeMap<String, f64>,
    /// Heuristic estimates — labelled, never presented as measurements.
    #[serde(default)]
    pub inferred: BTreeMap<String, f64>,
    pub method: String,
    pub evaluated_at: String,
    pub note: String,
}

/// Evaluate an episode. Measured set: verification, outcome, tool errors,
/// retries, latency. Inferred set: a deterministic heuristic quality
/// estimate derived from those measurements (documented, versioned).
pub fn evaluate(episode: &ExperienceEpisode) -> EpisodeEvaluation {
    let mut measured = BTreeMap::new();
    measured.insert(
        "verification_passed".to_string(),
        match episode.verification {
            Verification::Passed => 1.0,
            Verification::Failed => 0.0,
            Verification::Unknown => 0.5,
        },
    );
    measured.insert(
        "success".to_string(),
        match episode.outcome {
            EpisodeOutcome::Success => 1.0,
            EpisodeOutcome::Partial => 0.5,
            EpisodeOutcome::Failure => 0.0,
        },
    );
    measured.insert("tool_errors".to_string(), episode.errors as f64);
    measured.insert("retries".to_string(), episode.retries as f64);
    if let Some(l) = episode.latency_ms {
        measured.insert("latency_s".to_string(), l as f64 / 1000.0);
    }
    if let Some(f) = episode.feedback {
        measured.insert("operator_feedback".to_string(), f as f64);
    }

    let mut inferred = BTreeMap::new();
    let verification = measured["verification_passed"];
    let success = measured["success"];
    let error_penalty = (episode.errors as f64 * 0.1).min(0.5);
    let retry_penalty = (episode.retries as f64 * 0.05).min(0.3);
    let quality = (0.5 * verification + 0.4 * success + 0.1 * (1.0 - error_penalty)
        - retry_penalty)
        .clamp(0.0, 1.0);
    inferred.insert("heuristic_quality".to_string(), quality);

    EpisodeEvaluation {
        episode_id: episode.id.clone(),
        measured,
        inferred,
        method: "measured+inferred/v1".into(),
        evaluated_at: openrt_core::now().to_rfc3339(),
        note: "measured values are facts from the record; inferred values are labelled heuristics"
            .into(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromotionKind {
    Skill,
    Instinct,
    Genome,
    ModelExperiment,
    Routing,
}

impl PromotionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            PromotionKind::Skill => "skill",
            PromotionKind::Instinct => "instinct",
            PromotionKind::Genome => "genome",
            PromotionKind::ModelExperiment => "model_experiment",
            PromotionKind::Routing => "routing",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromotionDecision {
    Promote,
    Hold,
    Reject,
    Rollback,
}

impl PromotionDecision {
    pub fn as_str(&self) -> &'static str {
        match self {
            PromotionDecision::Promote => "promote",
            PromotionDecision::Hold => "hold",
            PromotionDecision::Reject => "reject",
            PromotionDecision::Rollback => "rollback",
        }
    }
}

/// Benchmark evidence behind a promotion decision. `Promote` is refused
/// without measured metrics (see `ImproveStore::record_promotion`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkEvidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_ref: Option<String>,
    #[serde(default)]
    pub candidate_metrics: BTreeMap<String, f64>,
    #[serde(default)]
    pub deltas: BTreeMap<String, f64>,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromotionRecord {
    pub id: String,
    pub kind: PromotionKind,
    pub artifact: String,
    pub decision: PromotionDecision,
    pub benchmark: BenchmarkEvidence,
    pub rationale: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator: Option<String>,
    pub provenance: Provenance,
    pub created_at: String,
}

impl openrt_workspace_assets::CanonicalForm for ExperienceEpisode {
    fn asset_kind(&self) -> &'static str {
        "episode"
    }

    fn asset_id(&self) -> String {
        self.id.clone()
    }

    fn canonical_pairs(&self) -> Vec<(String, String)> {
        vec![
            ("kind".into(), "episode".into()),
            ("id".into(), self.id.clone()),
            ("task".into(), self.task.clone()),
            ("context".into(), self.context.clone()),
            ("model".into(), self.model.clone()),
            (
                "outcome".into(),
                serde_json::to_string(&self.outcome).unwrap_or_default(),
            ),
            (
                "verification".into(),
                serde_json::to_string(&self.verification).unwrap_or_default(),
            ),
            ("errors".into(), self.errors.to_string()),
            ("retries".into(), self.retries.to_string()),
            (
                "latency_ms".into(),
                self.latency_ms.map(|v| v.to_string()).unwrap_or_default(),
            ),
            ("tools".into(), self.tools.join(",")),
            ("actions".into(), self.actions.join(",")),
            (
                "feedback".into(),
                self.feedback.map(|v| v.to_string()).unwrap_or_default(),
            ),
            ("artifact".into(), self.artifact.clone().unwrap_or_default()),
            ("provenance.origin".into(), self.provenance.origin.clone()),
            ("provenance.source".into(), self.provenance.source.clone()),
        ]
    }
}

impl openrt_workspace_assets::CanonicalForm for PromotionRecord {
    fn asset_kind(&self) -> &'static str {
        "promotion"
    }

    fn asset_id(&self) -> String {
        self.id.clone()
    }

    fn canonical_pairs(&self) -> Vec<(String, String)> {
        let metrics: Vec<(String, String)> = self
            .benchmark
            .candidate_metrics
            .iter()
            .map(|(k, v)| (k.clone(), v.to_bits().to_string()))
            .collect();
        let deltas: Vec<(String, String)> = self
            .benchmark
            .deltas
            .iter()
            .map(|(k, v)| (k.clone(), v.to_bits().to_string()))
            .collect();
        vec![
            ("kind".into(), "promotion".into()),
            ("id".into(), self.id.clone()),
            ("promotion_kind".into(), self.kind.as_str().into()),
            ("artifact".into(), self.artifact.clone()),
            ("decision".into(), self.decision.as_str().into()),
            (
                "baseline_ref".into(),
                self.benchmark.baseline_ref.clone().unwrap_or_default(),
            ),
            (
                "candidate_metrics".into(),
                serde_json::to_string(&metrics).unwrap_or_default(),
            ),
            (
                "deltas".into(),
                serde_json::to_string(&deltas).unwrap_or_default(),
            ),
            ("source".into(), self.benchmark.source.clone()),
            ("rationale".into(), self.rationale.clone()),
            ("operator".into(), self.operator.clone().unwrap_or_default()),
            ("provenance.origin".into(), self.provenance.origin.clone()),
            ("provenance.source".into(), self.provenance.source.clone()),
        ]
    }
}
