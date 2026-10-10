//! OpenRT runtime: bootstrap, wiring, lifecycle, and the gated execution
//! pipeline.
//!
//! The runtime owns the *assembly*: config → store, event bus, provider,
//! registry, policy gate, resource gate, and agent. Tasks run through the
//! pipeline ([`crate::run`]), which emits the Phase 1 event surface and
//! persists every state transition to the foundation store (ADR-006).

pub mod capability_adapter;
pub mod checkpoint;
pub mod defense;
pub mod doctor;
pub(crate) mod exttool;
pub mod gate;
pub mod hooks;
pub mod locks;
#[cfg(test)]
mod memory_hints;
pub(crate) mod model_egress;
pub mod phase11;
pub mod phase12;
pub mod phase13;
pub mod phase5;
pub mod recovery;
pub mod resource_retry;
pub mod run;
pub mod runtime_memory;
pub mod shield;
pub mod sih;
pub mod skill;
pub mod timemachine;
pub mod workbench;

pub use defense::{EvidenceContextReport, EvidenceContextRequest};
pub mod runtime;
pub mod sinks;
pub mod verify;

pub use capability_adapter::PlannerCapabilityView;
pub use checkpoint::{CheckpointOptions, CheckpointWriter};
pub use doctor::{DoctorCheck, DoctorReport, DoctorStatus};
pub use gate::{AssetRuleLayer, GateExecutor, GateRuleEntry};
pub use hooks::HookLayer;
pub use recovery::{
    IncompleteRunInfo, InterruptedTaskInfo, RecoveryRecord, RunResumePoint, StaleSandboxInfo,
    TaskArtifact, TaskInspection, RECOVERY_NS,
};
pub use run::{run_pipeline, PipelineOutcome};
pub use runtime::{Runtime, RuntimeBuilder};
pub use runtime_memory::RuntimeMemoryPlan;
pub use sinks::PersistenceSink;
pub use skill::{SkillExecution, SkillLoadCheck, SkillLoadReport};
pub use timemachine::{
    BranchComparison, BranchDiffEntry, BranchEventSummary, CurrentView, SessionAsOf, StateAsOf,
    Timeline, TimelineOptions,
};

/// Aggregate error type for the runtime surface. Each owned domain crate
/// keeps its own `*Error`; this type maps them all into one boundary.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(#[from] openrt_config::ConfigurationError),
    #[error("persistence error: {0}")]
    Persistence(#[from] openrt_persistence::PersistenceError),
    #[error("model provider error: {0}")]
    Model(#[from] openrt_models::ModelError),
    #[error("tool error: {0}")]
    Tool(#[from] openrt_tools::ToolError),
    #[error("agent error: {0}")]
    Agent(#[from] openrt_agents::AgentError),
    #[error("task lifecycle error: {0}")]
    Task(#[from] openrt_core::TaskError),
    #[error("session `{0}` not found")]
    SessionNotFound(openrt_core::SessionId),
    #[error("provider `{0}` not configured or unsupported: {1}")]
    ProviderUnavailable(String, String),
    #[error("`ollama` provider requires the `ollama` feature (build with --features ollama)")]
    OllamaFeatureDisabled,
    #[error("background task failed: {0}")]
    Join(String),
    #[error("orchestration error: {0}")]
    Orchestration(openrt_orch::WorkflowError),
    #[error("checkpoint error: {0}")]
    Checkpoint(#[from] openrt_checkpoint::CheckpointError),
    #[error("audit log error: {0}")]
    Audit(#[from] openrt_audit::AuditError),
    #[error("operating-mode error: {0}")]
    Modes(#[from] openrt_modes::ModeError),
    #[error("model registry error: {0}")]
    Registry(#[from] openrt_registry::RegistryError),
    #[error("history ledger error: {0}")]
    History(#[from] openrt_timeseries::HistoryError),
    #[error("memory store error: {0}")]
    Memory(#[from] openrt_memory::MemoryStoreError),
    #[error("workspace asset error: {0}")]
    Asset(#[from] openrt_workspace_assets::AssetError),
    #[error("self-improvement error: {0}")]
    Improve(#[from] openrt_selfimprove::ImproveError),
    #[error("working memory error: {0}")]
    Working(#[from] openrt_memory::WorkingError),
    #[error("workflow compilation failed:\n{}", render_diags(.0))]
    Compile(Vec<openrt_compiler::CompileDiagnostic>),
    #[error("policy document error: {0}")]
    Policy(#[from] openrt_policy::PolicyDocumentError),
    #[error("secrets error: {0}")]
    Secrets(#[from] openrt_secrets::SecretsError),
    #[error("network policy error: {0}")]
    NetPol(#[from] openrt_netpol::NetPolError),
    #[error("enclave error: {0}")]
    Enclave(#[from] openrt_enclave::EnclaveError),
    #[error("observability error: {0}")]
    Observability(#[from] openrt_observability::ObsError),
    #[error("supervisor error: {0}")]
    Supervisor(#[from] openrt_supervisor::SupervisorError),
    #[error("verification error: {0}")]
    Verifier(#[from] openrt_verifier::VerifyError),
    #[error("knowledge layer error: {0}")]
    Knowledge(#[from] openrt_knowledge::KnowledgeError),
    #[error("evidence layer error: {0}")]
    Evidence(#[from] openrt_evidence::EvidenceError),
    /// Phase 10 boot recovery: sandbox catalog scan failures (fail-closed —
    /// a runtime never boots silently blind to stale sandboxes).
    #[error("sandbox error: {0}")]
    Sandbox(String),
    /// M10.6 skill loader/execution refusals (fail-closed — named checks).
    #[error("skill error: {0}")]
    Skill(String),
    /// Phase 11 extension wiring failures (fail-closed — an extension tool
    /// that cannot bind through the one registry never half-registers).
    #[error("extension error: {0}")]
    Extension(String),
    /// Phase 11 M11.2 MCP registry failures (fail-closed — a registry
    /// operation that cannot complete never half-applies).
    #[error("mcp registry error: {0}")]
    Mcp(String),
    /// Phase 11 workbench/room/material failures (fail-closed — the
    /// workspace overlay never fabricates records).
    #[error("workbench error: {0}")]
    Workbench(#[from] openrt_workspace::WorkspaceError),
    /// M10.9 AgentShield finding persistence failures (fail-closed — the
    /// gate maps them into blocked calls, never silent skips).
    #[error("AgentShield error: {0}")]
    Shield(#[from] openrt_shield::store::ShieldError),
    /// Phase 12 advisory industrial intelligence failures (fail-closed —
    /// a refused analysis never fabricates a report).
    #[error("industrial intelligence error: {0}")]
    Industrial(#[from] openrt_industrial::IndustrialError),
    /// Phase 12 research-only trading failures (includes the one broker
    /// refusal; no order path exists).
    #[error("trading research error: {0}")]
    Trading(#[from] openrt_trading::TradingError),
    /// Phase 12 training-experiment failures (honest — no backend means
    /// the run fails with an explicit reason, never a fake success).
    #[error("training error: {0}")]
    Train(#[from] openrt_train::TrainError),
    /// Phase 12 model-cache persistence failures.
    #[error("model cache error: {0}")]
    Cache(#[from] openrt_model_cache::CacheError),
    /// Phase 12 router failures (signal overlay included; base routing
    /// stays authoritative).
    #[error("router error: {0}")]
    Router(#[from] openrt_router::RouterError),
    /// Phase 12 honest-unavailability marker — a capability with no
    /// configured backend says so instead of faking output.
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// Host resource admission refused new work. Carries the structured denial
    /// so the TUI can offer Retry / Wait / Switch Model / Details / Cancel and
    /// the session survives instead of losing the transcript.
    ///
    /// This is deliberately **not** folded into `Tool(ToolError::ResourceDenied)`:
    /// a host-level refusal is a recoverable admission decision, not a broken
    /// tool and not a permission failure.
    #[error("resource limit: {0}")]
    ResourceBlocked(#[from] openrt_resources::ResourceDenial),
}

/// Render structured compiler diagnostics as multi-line WHAT/WHY/WHERE/FIX.
fn render_diags(diags: &[openrt_compiler::CompileDiagnostic]) -> String {
    diags
        .iter()
        .map(|d| {
            format!(
                "[{}] {} | why: {} | at: {} | fix: {}",
                d.code.as_str(),
                d.what,
                d.why,
                d.where_,
                d.how_to_fix
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
