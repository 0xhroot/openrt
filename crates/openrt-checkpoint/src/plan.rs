//! Recovery planner — transforms a checkpoint + policy into a [`RecoveryPlan`]
//! and (optionally) a [`DryRunReport`].

use std::collections::{HashMap, HashSet};

use openrt_core::CheckpointId;

use crate::classify::FailureCategory;
use crate::model::{Checkpoint, CheckpointError, NodeEffect, NodeState};
use crate::policy::{RecoveryPolicy, UncertainPolicy};

// ── scope ────────────────────────────────────────────────────────────────────

/// The kind of recovery operation requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryScope {
    /// Resume normal execution from where the checkpoint was taken (also
    /// retries eligible failed nodes).
    Resume,
    /// Only retry the failed/uncertain nodes; never enqueue new pending work.
    RetryOnly,
}

// ── preflight ────────────────────────────────────────────────────────────────

/// Gate checked **before** the planner runs. Every `Ok` field must be `true`
/// for a recovery plan to be produced; a single `Err` short-circuits to a
/// [`RecoveryPlan`] with status [`PlanStatus::Blocked`].
#[derive(Debug, Clone, Default)]
pub struct Preflight {
    /// Whether the engine is currently in a kill-switched / halted state.
    pub halted: bool,
    /// Whether required external resources (disk, network budget, etc.) are
    /// provisioned — always `true` for P2.6; reserved for resource gates.
    pub resources_ok: bool,
    /// Whether the session referenced by the checkpoint still exists.
    pub session_exists: bool,
    /// Whether the workflow definition still exists and is parseable.
    pub workflow_def_exists: bool,
    /// Whether the recovery policy is internally consistent (always `true`;
    /// kept for future structural validation).
    pub policy_ready: bool,
}

impl Preflight {
    /// Returns `Ok(())` if all gates pass, otherwise the first failing
    /// [`CheckpointError::Refused`] reason.
    pub fn ok(&self) -> Result<(), crate::model::CheckpointError> {
        if self.halted {
            return Err(crate::model::CheckpointError::Refused(
                "engine is halted; clear halt before resuming".into(),
            ));
        }
        if !self.resources_ok {
            return Err(crate::model::CheckpointError::Refused(
                "required resources are not available".into(),
            ));
        }
        if !self.session_exists {
            return Err(crate::model::CheckpointError::Refused(
                "checkpoint session no longer exists".into(),
            ));
        }
        if !self.workflow_def_exists {
            return Err(crate::model::CheckpointError::Refused(
                "workflow definition is missing or unparseable".into(),
            ));
        }
        Ok(())
    }
}

// ── deferred / intervention ───────────────────────────────────────────────────

/// A node that could not be automatically handled during recovery.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DeferredNode {
    pub step: String,
    pub state: NodeState,
    pub reason: String,
}

// ── plan status ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Ready,
    Blocked,
    NeedsIntervention,
}

// ── recovery plan ─────────────────────────────────────────────────────────────

/// The complete output of the recovery planner — a single, inspectable,
/// deterministic structure that the runtime can commit to or present for
/// operator review.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RecoveryPlan {
    pub checkpoint_id: CheckpointId,
    pub workflow_id: String,
    pub scope: RecoveryScope,
    pub status: PlanStatus,
    /// Steps the engine will **not** re-execute (already completed or
    /// explicitly skipped — deterministic, never rerun).
    pub skip_nodes: Vec<String>,
    /// Steps whose dependencies are satisfied and that will be enqueued for
    /// execution (sorted in topological order relative to the full workflow).
    pub resume_nodes: Vec<String>,
    /// Steps that failed and are eligible for a bounded retry.
    pub retry_nodes: Vec<String>,
    /// Steps that could not be auto-resumed (uncertain non-idempotent,
    /// resource-denied, exhausted, …).
    pub deferred_nodes: Vec<DeferredNode>,
    /// Steps that require manual operator action before they can proceed.
    pub intervention_nodes: Vec<String>,
    /// Descriptions of supported rollback operations for this plan.
    pub would_rollback: Vec<String>,
}

impl RecoveryPlan {
    /// Whether the plan can proceed without operator intervention.
    pub fn is_blocked(&self) -> bool {
        self.status != PlanStatus::Ready
    }

    /// Whether a specific step is resumable (in `resume_nodes` or
    /// `retry_nodes`).
    pub fn is_resumable(&self, step: &str) -> bool {
        self.resume_nodes.iter().any(|s| s == step) || self.retry_nodes.iter().any(|s| s == step)
    }

    /// Whether a specific step was skipped.
    pub fn is_skipped(&self, step: &str) -> bool {
        self.skip_nodes.iter().any(|s| s == step)
    }
}

// ── dry run ───────────────────────────────────────────────────────────────────

/// A per-node description of what recovery *would* do, without executing
/// anything.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DryRunNode {
    pub step: String,
    pub action: DryRunAction,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DryRunAction {
    Skipped,
    Resumed,
    Retried,
    Deferred,
    Intervention,
}

/// The overall conclusion of a dry run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DryRunConclusion {
    /// All nodes would complete automatically.
    WouldComplete,
    /// At least one node needs operator intervention.
    NeedsIntervention,
    /// Recovery is blocked (preflight failed).
    Blocked,
}

/// Immutable snapshot of the recovery plan plus its per-node breakdown,
/// produced by a dry run. This is persisted in the `dryrun` foundation store
/// namespace (checkpoint-id keyed).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DryRunReport {
    pub checkpoint_id: CheckpointId,
    pub workflow_id: String,
    pub plan_status: PlanStatus,
    pub nodes: Vec<DryRunNode>,
    pub rollback_ops: Vec<String>,
    pub overall: DryRunConclusion,
}

impl DryRunReport {
    /// Build a report from an existing plan (deterministic — no I/O).
    pub fn from_plan(plan: &RecoveryPlan) -> Self {
        let mut nodes = Vec::new();
        for step in &plan.skip_nodes {
            nodes.push(DryRunNode {
                step: step.clone(),
                action: DryRunAction::Skipped,
                reason: "already completed or skipped; never rerun".into(),
            });
        }
        for step in &plan.resume_nodes {
            nodes.push(DryRunNode {
                step: step.clone(),
                action: DryRunAction::Resumed,
                reason: "deps satisfied; enqueued for execution".into(),
            });
        }
        for step in &plan.retry_nodes {
            nodes.push(DryRunNode {
                step: step.clone(),
                action: DryRunAction::Retried,
                reason: "failed but retryable; re-enqueued".into(),
            });
        }
        for d in &plan.deferred_nodes {
            nodes.push(DryRunNode {
                step: d.step.clone(),
                action: DryRunAction::Deferred,
                reason: d.reason.clone(),
            });
        }
        for step in &plan.intervention_nodes {
            nodes.push(DryRunNode {
                step: step.clone(),
                action: DryRunAction::Intervention,
                reason: "needs operator intervention".into(),
            });
        }
        // Stable order: sort alphabetically by step (determinism).
        nodes.sort_by(|a, b| a.step.cmp(&b.step));

        let overall = match plan.status {
            PlanStatus::Blocked => DryRunConclusion::Blocked,
            PlanStatus::NeedsIntervention => DryRunConclusion::NeedsIntervention,
            PlanStatus::Ready => DryRunConclusion::WouldComplete,
        };

        DryRunReport {
            checkpoint_id: plan.checkpoint_id,
            workflow_id: plan.workflow_id.clone(),
            plan_status: plan.status,
            nodes,
            rollback_ops: plan.would_rollback.clone(),
            overall,
        }
    }
}

// ── planner core ──────────────────────────────────────────────────────────────

/// Plan recovery from a checkpoint.
///
/// # Errors
///
/// Returns `Err` only for schema/corruption errors. Blocked or
/// intervention-required outcomes are reflected in the plan's `status` field.
pub fn plan_recovery(
    checkpoint: &Checkpoint,
    workflow_steps: &[String],
    deps_by_step: &HashMap<String, Vec<String>>,
    policy: &RecoveryPolicy,
    scope: RecoveryScope,
) -> Result<RecoveryPlan, CheckpointError> {
    checkpoint.validate()?;

    let mut skip_nodes = Vec::new();
    let mut resume_nodes = Vec::new();
    let mut retry_nodes = Vec::new();
    let mut deferred_nodes = Vec::new();
    let mut intervention_nodes = Vec::new();

    let completed_set: HashSet<&str> = checkpoint.completed.iter().map(|s| s.as_str()).collect();
    let skipped_set: HashSet<&str> = checkpoint
        .node_states
        .iter()
        .filter(|n| n.state == NodeState::Skipped)
        .map(|n| n.step.as_str())
        .collect();

    let _ = workflow_steps; // engine orders execution; planner classifies state

    for ns in &checkpoint.node_states {
        match ns.state {
            NodeState::Completed | NodeState::Skipped => {
                skip_nodes.push(ns.step.clone());
            }
            NodeState::Failed => {
                let cat = ns
                    .failure_category
                    .parse::<FailureCategory>()
                    .unwrap_or(FailureCategory::Fatal);
                let policy_retryable = policy.retryable.allows(&cat);
                let within_budget = ns.attempts < policy.max_attempts;
                let in_scope = matches!(scope, RecoveryScope::Resume);

                if policy_retryable && within_budget && in_scope {
                    retry_nodes.push(ns.step.clone());
                } else {
                    let reason = if !policy_retryable {
                        format!("non-retryable category: {cat}")
                    } else if !within_budget {
                        format!(
                            "exhausted {attempts} attempts (max {max_attempts})",
                            attempts = ns.attempts,
                            max_attempts = policy.max_attempts
                        )
                    } else {
                        "retry-only scope; not eligible for retry".into()
                    };
                    intervention_nodes.push(ns.step.clone());
                    deferred_nodes.push(DeferredNode {
                        step: ns.step.clone(),
                        state: ns.state,
                        reason,
                    });
                }
            }
            NodeState::Pending => {
                // Pending node: check if all deps are satisfied.
                let deps = deps_by_step
                    .get(&ns.step)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]);
                let deps_satisfied = deps.iter().all(|d| {
                    completed_set.contains(d.as_str()) || skipped_set.contains(d.as_str())
                });
                if deps_satisfied && matches!(scope, RecoveryScope::Resume) {
                    resume_nodes.push(ns.step.clone());
                } else if deps_satisfied {
                    // Retry-only scope never enqueues new pending work.
                    deferred_nodes.push(DeferredNode {
                        step: ns.step.clone(),
                        state: ns.state,
                        reason: "retry-only scope; pending nodes are deferred".into(),
                    });
                } else {
                    deferred_nodes.push(DeferredNode {
                        step: ns.step.clone(),
                        state: ns.state,
                        reason: "deps not satisfied yet (resolved by engine)".into(),
                    });
                }
            }
            NodeState::Running => match policy.uncertain_policy {
                UncertainPolicy::AlwaysIntervene => {
                    intervention_nodes.push(ns.step.clone());
                    deferred_nodes.push(DeferredNode {
                        step: ns.step.clone(),
                        state: ns.state,
                        reason: "uncertain outcome; policy requires intervention".into(),
                    });
                }
                UncertainPolicy::AutoWhenSafe => match ns.effect {
                    NodeEffect::ReadOnly | NodeEffect::Idempotent => {
                        retry_nodes.push(ns.step.clone());
                    }
                    _ => {
                        let reason = format!(
                            "uncertain outcome with {:?} effect; not safe to auto-resume",
                            ns.effect
                        );
                        intervention_nodes.push(ns.step.clone());
                        deferred_nodes.push(DeferredNode {
                            step: ns.step.clone(),
                            state: ns.state,
                            reason,
                        });
                    }
                },
            },
        }
    }

    let intervention_set: HashSet<&str> = intervention_nodes.iter().map(|s| s.as_str()).collect();
    resume_nodes = resume_nodes
        .into_iter()
        .filter(|s| !intervention_set.contains(s.as_str()))
        .collect::<Vec<_>>();

    resume_nodes.sort();
    retry_nodes.sort();
    skip_nodes.sort();
    intervention_nodes.sort();
    deferred_nodes.sort_by(|a, b| a.step.cmp(&b.step));

    let status = if intervention_nodes.is_empty() {
        PlanStatus::Ready
    } else {
        PlanStatus::NeedsIntervention
    };

    let mut would_rollback = Vec::new();
    for art in &checkpoint.artifact_refs {
        if art.kind == "file_snapshot" {
            if let (Some(sid), Some(path)) = (&art.snapshot_id, &art.path) {
                would_rollback.push(format!("restore file `{path}` from snapshot `{sid}`"));
            }
        }
    }
    if checkpoint.execution.run_snapshot.is_some() {
        would_rollback.push("restore workflow run metadata to prior checkpoint".into());
    }
    if !checkpoint.blackboard.entries.is_empty() {
        would_rollback.push("restore blackboard execution state to checkpoint snapshot".into());
    }
    would_rollback.sort();

    Ok(RecoveryPlan {
        checkpoint_id: checkpoint.checkpoint_id,
        workflow_id: checkpoint.workflow_id.clone(),
        scope,
        status,
        skip_nodes,
        resume_nodes,
        retry_nodes,
        deferred_nodes,
        intervention_nodes,
        would_rollback,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        CheckpointExecution, CheckpointKind, CheckpointStatus, DependencyState, NodeStatus,
    };
    use openrt_blackboard::BlackboardSnapshot;
    use openrt_core::{now, SessionId};
    use std::collections::HashMap;

    fn mk_cp(node_states: Vec<NodeStatus>, deps: HashMap<String, Vec<String>>) -> Checkpoint {
        let mut completed = Vec::new();
        let mut pending = Vec::new();
        let mut failed = Vec::new();
        let mut uncertain = Vec::new();
        for ns in &node_states {
            match ns.state {
                NodeState::Completed | NodeState::Skipped => completed.push(ns.step.clone()),
                NodeState::Pending => pending.push(ns.step.clone()),
                NodeState::Failed => failed.push(ns.step.clone()),
                NodeState::Running => uncertain.push(ns.step.clone()),
            }
        }
        Checkpoint {
            schema_version: crate::model::CHECKPOINT_SCHEMA_VERSION,
            checkpoint_id: openrt_core::CheckpointId::new(),
            session_id: SessionId::new(),
            task_id: None,
            workflow_id: "wf-plan-test".into(),
            created_at: now(),
            kind: CheckpointKind::AfterNode,
            status: CheckpointStatus::Running,
            node_states,
            completed,
            pending,
            failed,
            uncertain,
            current_node: None,
            dependency_state: DependencyState {
                ready: Vec::new(),
                deps_by_step: deps.clone(),
            },
            retry_counters: HashMap::new(),
            blackboard: BlackboardSnapshot::default(),
            artifact_refs: vec![],
            execution: CheckpointExecution::default(),
            refs: crate::model::RunRefs::default(),
            digest: None,
        }
    }

    fn ns(step: &str, state: NodeState, attempts: u32) -> NodeStatus {
        NodeStatus {
            step: step.into(),
            state,
            attempts,
            artifact_key: None,
            error: None,
            failure_category: String::new(),
            effect: NodeEffect::Unknown,
        }
    }

    #[test]
    fn plan_all_completed() {
        let cp = mk_cp(
            vec![
                ns("a", NodeState::Completed, 1),
                ns("b", NodeState::Completed, 1),
            ],
            HashMap::new(),
        );
        let plan = plan_recovery(
            &cp,
            &["a".into(), "b".into()],
            &HashMap::new(),
            &RecoveryPolicy::default(),
            RecoveryScope::Resume,
        )
        .unwrap();
        assert_eq!(plan.status, PlanStatus::Ready);
        assert_eq!(plan.skip_nodes, vec!["a", "b"]);
        assert!(plan.resume_nodes.is_empty());
        assert!(plan.retry_nodes.is_empty());
        assert!(plan.intervention_nodes.is_empty());
    }

    #[test]
    fn plan_pending_nodes_resume() {
        let deps: HashMap<String, Vec<String>> =
            vec![("b".into(), vec!["a".into()])].into_iter().collect();
        let cp = mk_cp(
            vec![
                ns("a", NodeState::Completed, 1),
                ns("b", NodeState::Pending, 0),
            ],
            deps.clone(),
        );
        let plan = plan_recovery(
            &cp,
            &["a".into(), "b".into()],
            &deps,
            &RecoveryPolicy::default(),
            RecoveryScope::Resume,
        )
        .unwrap();
        assert_eq!(plan.status, PlanStatus::Ready);
        assert!(plan.skip_nodes.contains(&"a".into()));
        assert!(plan.resume_nodes.contains(&"b".into()));
    }

    #[test]
    fn plan_retryable_failure() {
        let cp = mk_cp(
            vec![
                ns("a", NodeState::Completed, 1),
                ns("b", NodeState::Failed, 1),
            ],
            HashMap::new(),
        );
        let mut node = cp.node_states[1].clone();
        node.failure_category = "transient".into();
        let mut cp2 = cp.clone();
        cp2.node_states[1] = node;
        let plan = plan_recovery(
            &cp2,
            &["a".into(), "b".into()],
            &HashMap::new(),
            &RecoveryPolicy::default(),
            RecoveryScope::Resume,
        )
        .unwrap();
        assert!(plan.retry_nodes.contains(&"b".into()));
    }

    #[test]
    fn plan_exhausted_failure_needs_intervention() {
        let cp = mk_cp(vec![ns("a", NodeState::Failed, 3)], HashMap::new());
        let mut node = cp.node_states[0].clone();
        node.failure_category = "transient".into();
        let mut cp2 = cp.clone();
        cp2.node_states[0] = node;
        let policy = RecoveryPolicy {
            max_attempts: 3,
            ..Default::default()
        };
        let plan = plan_recovery(
            &cp2,
            &["a".into()],
            &HashMap::new(),
            &policy,
            RecoveryScope::Resume,
        )
        .unwrap();
        assert_eq!(plan.status, PlanStatus::NeedsIntervention);
        assert!(plan.intervention_nodes.contains(&"a".into()));
    }

    #[test]
    fn plan_uncertain_auto_when_safe() {
        let mut ns = ns("a", NodeState::Running, 1);
        ns.effect = NodeEffect::ReadOnly;
        let cp = mk_cp(vec![ns], HashMap::new());
        let plan = plan_recovery(
            &cp,
            &["a".into()],
            &HashMap::new(),
            &RecoveryPolicy::default(),
            RecoveryScope::Resume,
        )
        .unwrap();
        assert!(plan.retry_nodes.contains(&"a".into()));
    }

    #[test]
    fn plan_uncertain_non_idempotent_needs_intervention() {
        let mut ns = ns("a", NodeState::Running, 1);
        ns.effect = NodeEffect::NonIdempotent;
        let cp = mk_cp(vec![ns], HashMap::new());
        let plan = plan_recovery(
            &cp,
            &["a".into()],
            &HashMap::new(),
            &RecoveryPolicy::default(),
            RecoveryScope::Resume,
        )
        .unwrap();
        assert_eq!(plan.status, PlanStatus::NeedsIntervention);
        assert!(plan.intervention_nodes.contains(&"a".into()));
    }

    #[test]
    fn dry_run_report_from_plan() {
        let plan = RecoveryPlan {
            checkpoint_id: openrt_core::CheckpointId::new(),
            workflow_id: "wf".into(),
            scope: RecoveryScope::Resume,
            status: PlanStatus::Ready,
            skip_nodes: vec!["a".into()],
            resume_nodes: vec!["b".into()],
            retry_nodes: vec![],
            deferred_nodes: vec![],
            intervention_nodes: vec![],
            would_rollback: vec!["restore blackboard".into()],
        };
        let report = DryRunReport::from_plan(&plan);
        assert_eq!(report.overall, DryRunConclusion::WouldComplete);
        assert_eq!(report.nodes.len(), 2);
    }
}
