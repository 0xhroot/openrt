//! # openrt-checkpoint
//!
//! Checkpoint data model, recovery planner, persistence manager, and
//! rollback foundations for OpenRT P2.6.
//!
//! ## Architecture
//!
//! This crate owns **no** foundation store — it consumes [`openrt_persistence`]
//! via [`CheckpointManager`] and [`FileSnapshotStore`]. The engine never calls
//! this crate directly; the runtime orchestrates all checkpoint operations.

pub use openrt_core::CheckpointId;

pub mod classify;
pub mod manager;
pub mod model;
pub mod plan;
pub mod policy;
pub mod rollback;

// Re-exports for convenience.
pub use classify::{
    classify_agent_error, classify_persistence_error, classify_tool_error, FailureCategory,
};
pub use manager::{CheckpointBuild, CheckpointManager, DRYRUN_NS};
pub use model::{
    ArtifactRef, Checkpoint, CheckpointError, CheckpointExecution, CheckpointKind,
    CheckpointMetadata, CheckpointStatus, DependencyState, NodeEffect, NodeState, NodeStatus,
    Retryable, RunRefs, CHECKPOINT_SCHEMA_VERSION,
};
pub use plan::{
    plan_recovery, DeferredNode, DryRunAction, DryRunConclusion, DryRunNode, DryRunReport,
    PlanStatus, Preflight, RecoveryPlan, RecoveryScope,
};
pub use policy::{RecoveryPolicy, RetryCategories, UncertainPolicy};
pub use rollback::{
    FileSnapshotRecord, FileSnapshotStore, PolicyCompat, RollbackAuthorization,
    RollbackContinuation, RollbackOp, RollbackReport, RollbackResult, RollbackStatus,
    RollbackVerification,
};
