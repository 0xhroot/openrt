pub mod asset;
pub mod canonical;
pub mod extension;
pub mod extstore;
pub mod generate;
pub mod genome;
pub mod hook;
pub mod instinct;
pub mod rule;
pub mod skill;
pub mod skilltest;
pub mod store;
pub mod workflow;

pub use asset::{
    sorted_join, ActivationState, AssetEnvelope, CanonicalForm, OperatorGrant, Provenance,
};
pub use canonical::{
    canonical_bytes, canonical_digest, ensure_path_safe_id, sha256_hex, verify_digest,
};
pub use extension::{
    Extension, ExtensionCompat, ExtensionDependency, ExtensionId, ExtensionMcpServer,
    ExtensionPackage, ExtensionTool, ExtensionVersion, EXTENSION_PACKAGE_SCHEMA, SECURITY_CLEAN,
    SECURITY_FLAGGED, SECURITY_UNSCANNED,
};
pub use generate::{ReviewState, SkillCandidate, SkillGenerator};
pub use genome::{AgentGenome, GenomeId};
pub use hook::{Hook, HookId, HookOutcome, HookTarget};
pub use instinct::{Instinct, InstinctGenerator, InstinctId};
pub use rule::{Rule, RuleDecision, RuleEffect, RuleId};
pub use skill::{
    Skill, SkillCompat, SkillDependency, SkillId, SkillVersion, TRUST_SIGNED, TRUST_UNVETTED,
    TRUST_VETTED,
};
pub use skilltest::{
    Marketplace, MarketplaceEntry, RemoteMarketplaceUnavailable, SkillTest, SkillTestKind,
    SKILL_TESTS_NS,
};
pub use store::{AssetStore, StoreMeta};
pub use workflow::{Workflow, WorkflowStep};

pub const SKILLS_NS: &str = "assets.skills";
pub const INSTINCTS_NS: &str = "assets.instincts";
pub const HOOKS_NS: &str = "assets.hooks";
pub const RULES_NS: &str = "assets.rules";
pub const GENOMES_NS: &str = "assets.genomes";
pub const CANDIDATES_NS: &str = "assets.candidates";
pub const WORKFLOWS_NS: &str = "assets.workflows";
pub const STORE_META_NS: &str = "assets.meta";
pub const EXTENSIONS_NS: &str = "extensions.packages";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AssetError {
    #[error("digest mismatch ({kind}/{id}) — fail closed, never silently repaired")]
    DigestMismatch { kind: String, id: String },
    #[error("unauthorized activation for {id}")]
    UnauthorizedActivation { id: String },
    #[error("unauthorized hook use for {id}")]
    UnauthorizedHook { id: String },
    #[error("hook {id} is not active")]
    HookNotActive { id: String },
    #[error("missing dependency {dep} required by {id}")]
    MissingDependency { id: String, dep: String },
    #[error("incompatible dependency {dep} for {id}: required >= {required}, found {found}")]
    IncompatibleDependency {
        id: String,
        dep: String,
        required: u32,
        found: u32,
    },
    #[error("dependency {dep} of {id} is not active")]
    DependencyNotActive { id: String, dep: String },
    #[error("dependency cycle involving {id}")]
    DependencyCycle { id: String },
    #[error("invalid or missing provenance — fail closed")]
    InvalidProvenance,
    #[error("malformed asset: {detail}")]
    Malformed { detail: String },
    #[error("invalid hook: {detail}")]
    InvalidHook { detail: String },
    #[error("rule conflict between {a} and {b} — explicit conflict, never silently resolved")]
    RuleConflict { a: String, b: String },
    #[error("invalid genome reference: {detail}")]
    InvalidGenomeReference { detail: String },
    #[error("policy violation: {detail}")]
    PolicyViolation { detail: String },
    #[error("sovereign mode violation: {detail}")]
    SovereignViolation { detail: String },
    #[error("asset {id} is invalidated")]
    Invalidated { id: String },
    #[error("asset {id} is not validated")]
    NotValidated { id: String },
    #[error("{kind} not found: {id}")]
    NotFound { kind: String, id: String },
    #[error("{kind} already registered: {id}")]
    Duplicate { kind: String, id: String },
    #[error("operator not authorized: {operator}")]
    UnauthorizedOperator { operator: String },
    #[error("candidate {id} is not approved")]
    CandidateNotApproved { id: String },
    #[error("candidate {id} was rejected")]
    CandidateRejected { id: String },
    #[error("store error: {0}")]
    Store(String),
}
