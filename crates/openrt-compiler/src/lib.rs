//! OpenRT Phase 6 workflow compiler.
//!
//! Transforms a natural-language goal into a validated, executable
//! [`openrt_orch::Workflow`] that the **existing** orchestrator runs. This
//! crate never executes tools or models itself and never creates a second DAG
//! engine — it produces an executable PLAN; `openrt-orch` remains the sole
//! execution authority.
//!
//! # Pipeline
//!
//! ```text
//! NaturalLanguageGoal
//!     → GoalSpec            (understand)
//!     → DecomposedGoal      (decompose)
//!     → CapabilityPlan      (plan capabilities; registry + policy + mode)
//!     → Workflow            (construct graph — existing openrt_orch types)
//!     → validate graph      (topology, deps, cycles, inputs)
//!     → validate policy/resources
//!     → CompiledWorkflow    (persist under `compiled`)
//!     → existing orchestrator
//! ```
//!
//! # Invariants
//!
//! * Fail closed: unsupported goals, missing capabilities, permission denials
//!   and Sovereign conflicts surface as structured diagnostics — never a
//!   fabricated node.
//! * Provenance: every compiled node records why it exists and why its
//!   capability/role was selected; the original request is always attached.
//! * Determinism: with unchanged inputs the compiler is deterministic; any
//!   future LLM planner is a *planning component only* whose output must pass
//!   schema/capability/permission/DAG validation before it can become a plan.
//! * One persistence backbone (ADR-006): compiled records live in the
//!   foundation store namespace `compiled`; the executable graph is the same
//!   `Workflow` the orchestrator already persists under `dag`.

pub mod capability;
pub mod compile;
pub mod decompose;
pub mod diagnostics;
pub mod goal;
pub mod graph;
pub mod intent;
pub mod modes;
pub mod persist;
pub mod validate;

pub use capability::{CapabilityAssignment, CapabilityClass, CapabilityPlan, CapabilityStatus};
pub use compile::{CompileInput, CompileOutput, CompileStage, ReplanRecord, WorkflowCompiler};
pub use decompose::{DecomposedGoal, SubGoal};
pub use diagnostics::{CompileDiagnostic, DiagnosticCode, DiagnosticSeverity};
pub use goal::{GoalSpec, NaturalLanguageGoal};
pub use graph::{NodeProvenance, Provenance};
pub use modes::CompileMode;
pub use persist::{
    list_compiled, load_compiled, load_dag_workflow, persist_compiled, persist_dag_workflow,
    CompiledRecord,
};
pub use validate::{ModeContext, ResourceContext, ValidationReport, ValidationSeverity};

/// Persistence namespace for compiled workflow records (OWNERSHIP Phase 6).
pub const COMPILED_NS: &str = "compiled";

/// Compiler contract version recorded on every compiled record.
pub const COMPILER_VERSION: &str = "0.1.0";
