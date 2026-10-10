pub mod apply;
pub mod baseline;
pub mod debug;
pub mod episode;
pub mod learn;
pub mod proposal;
pub mod research;
pub mod store;

pub use apply::{AppliedChange, ApplyTarget};
pub use baseline::{Baseline, EvalOutcome, EvalResult, MetricDirection};
pub use debug::DebugReport;
pub use episode::{
    evaluate, BenchmarkEvidence, EpisodeEvaluation, EpisodeOutcome, ExperienceEpisode,
    PromotionDecision, PromotionKind, PromotionRecord, Verification,
};
pub use learn::{LearningEngine, LearningKind, LearningRecord, RunSummary};
pub use proposal::{ImprovementProposal, ProposalKind, ProposalState, TargetKind};
pub use research::{ResearchNote, ReviewDecision};
pub use store::{ImproveMeta, ImproveStore};

pub use openrt_workspace_assets::{OperatorGrant, Provenance};

pub const BASELINES_NS: &str = "improve.baselines";
pub const PROPOSALS_NS: &str = "improve.proposals";
pub const DEBUG_NS: &str = "improve.debug";
pub const RESEARCH_NS: &str = "improve.research";
pub const LEARNINGS_NS: &str = "improve.learnings";
pub const META_NS: &str = "improve.meta";
pub const EPISODES_NS: &str = "improve.episodes";
pub const PROMOTIONS_NS: &str = "improve.promotions";

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ImproveError {
    #[error("digest mismatch ({kind}/{id}) — fail closed, never silently repaired")]
    DigestMismatch { kind: String, id: String },
    #[error("{kind} not found: {id}")]
    NotFound { kind: String, id: String },
    #[error("{kind} already registered: {id}")]
    Duplicate { kind: String, id: String },
    #[error("invalid or missing provenance — fail closed")]
    InvalidProvenance,
    #[error("malformed improve record: {detail}")]
    Malformed { detail: String },
    #[error("proposal {id} does not beat baseline: baseline {baseline}, candidate {candidate}")]
    RegressionDetected {
        id: String,
        baseline: f64,
        candidate: f64,
    },
    #[error("proposal {id} has not passed evaluation")]
    EvalNotRun { id: String },
    #[error("operator approval required for proposal {id}")]
    UnauthorizedApproval { id: String },
    #[error("operator not authorized: {operator}")]
    UnauthorizedOperator { operator: String },
    #[error("proposal {id} is not approved")]
    NotApproved { id: String },
    #[error("proposal {id} in state {state}, expected {expected}")]
    InvalidState {
        id: String,
        state: String,
        expected: String,
    },
    #[error("target {target} of proposal {id} is operator-only — self-improvement cannot modify policies, security scanning, or verification metadata")]
    ProtectedTarget { id: String, target: String },
    #[error("sovereign mode violation: {detail}")]
    SovereignViolation { detail: String },
    #[error("store error: {0}")]
    Store(String),
}
