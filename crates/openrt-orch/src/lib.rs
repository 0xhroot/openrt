//! Multi-agent orchestration (P2.3, namespaces `exec.runs` / `exec.dag`).
//!
//! Three layers:
//!
//! * [`SubAgentKind`] — the typed role the roadmap fixes for M2.1:
//!   planner, researcher, coder, verifier, critic;
//! * [`Workflow`] / [`WorkflowStep`] — a declarative DAG over those roles with
//!   per-step dependencies, retries and serializable [`Condition`]s;
//! * [`WorkflowEngine`] — a deterministic wave scheduler (tokio `JoinSet`)
//!   that runs independent steps concurrently, retries failed steps, writes
//!   per-step artifacts to the shared [`Blackboard`] and emits the P2.3
//!   orchestration events (`WorkflowStarted` .. `WorkflowCompleted`).
//!
//! The engine persists the workflow definition (`dag/`) and the run report
//! (`runs/`) through `openrt-persistence` (ADR-006 single store).

pub mod agent;
pub mod engine;
pub mod workflow;

pub use agent::{room_role_sub_agent, SubAgent, SubAgentKind};
pub use engine::{
    CheckpointHook, ResumeSeed, RunNodeProgress, RunNodeState, RunProgress, StepReport,
    StepSandbox, WorkflowEngine, WorkflowError, WorkflowRunReport,
};
pub use workflow::{Condition, Workflow, WorkflowStatus, WorkflowStep};

/// Persistence namespace for workflow run reports (OWNERSHIP `exec.runs`).
pub const RUNS_NS: &str = "runs";
/// Persistence namespace for workflow definitions (OWNERSHIP `exec.dag`).
pub const DAG_NS: &str = "dag";
