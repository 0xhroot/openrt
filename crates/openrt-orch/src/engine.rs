//! Workflow DAG execution engine (P2.3) with P2.6 checkpoint hooks and resume.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use openrt_agents::Agent;
use openrt_blackboard::Blackboard;
use openrt_core::{AgentId, CancellationToken, EventKind, ExecutionId, SessionId, TaskId};
use openrt_persistence::PersistenceStore;
use openrt_telemetry::EventBus;

use crate::workflow::WorkflowStatus;
use crate::{SubAgentKind, Workflow, WorkflowStep, DAG_NS, RUNS_NS};

/// Error the DAG engine produces.
#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    #[error("no agent bound to role `{0}`")]
    UnknownRole(String),
    #[error("workflow has no steps")]
    EmptyWorkflow,
    #[error("workflow condition failed: {0}")]
    Condition(String),
    #[error("workflow was cancelled")]
    Cancelled,
    #[error("persistence error: {0}")]
    Persistence(#[from] openrt_persistence::PersistenceError),
    #[error("step `{0}` failed after {1} attempt(s): {2}")]
    StepFailed(String, u32, String),
    #[error("checkpoint hook failed (run aborted, foundation state preserved): {0}")]
    Checkpoint(String),
    #[error("step `{step}` sandbox admission failed: {reason}")]
    SandboxDenied { step: String, reason: String },
}

/// Phase 8: per-step sandbox admission, delegated to the runtime's unified
/// sandbox authority. `openrt-orch` holds only this trait — no second policy
/// model or backend logic lives here.
#[async_trait::async_trait]
pub trait StepSandbox: Send + Sync {
    /// Validate a declared step sandbox profile BEFORE execution begins.
    /// `Err(reason)` fails the run closed.
    async fn admit_profile(&self, profile: &str) -> Result<(), String>;
}

/// Per-node lifecycle state reported to a [`CheckpointHook`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunNodeState {
    Completed,
    Skipped,
    Failed,
    /// Spawned and still executing when this progress snapshot was taken —
    /// its outcome is unknown (Phase 10: durable in-flight marking so a
    /// crash lands in `UncertainPolicy`, never silent re-execution).
    Running,
}

/// One node's durable progress within the current run.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RunNodeProgress {
    pub step: String,
    pub state: RunNodeState,
    pub attempts: u32,
    #[serde(default)]
    pub artifact_key: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    /// Canonical failure-category name (see `openrt-checkpoint::classify`);
    /// only meaningful when `state == Failed`. Values are stable strings:
    /// model_failure / tool_failure / cancellation / transient.
    #[serde(default)]
    pub failure_category: Option<String>,
}

/// Point-in-time execution progress handed to a checkpoint hook.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RunProgress {
    pub workflow_id: String,
    pub session_id: Option<SessionId>,
    pub task_id: Option<TaskId>,
    pub execution_id: ExecutionId,
    pub nodes: Vec<RunNodeProgress>,
    /// Steps eligible to run whose deps are satisfied and not yet started.
    pub pending: Vec<String>,
    /// Step → cumulative attempts (sum of this run and all prior resumed runs).
    pub retry_counters: HashMap<String, u32>,
}

/// Failure-category tags the engine attaches to failed nodes. These canonical
/// names mirror `openrt_checkpoint::classify::FailureCategory`'s serialized
/// form — runtime/checkpoint layers may parse them back into typed categories.
fn agent_failure_tag(e: &openrt_agents::AgentError) -> &'static str {
    match e {
        openrt_agents::AgentError::Model(_) => "model_failure",
        openrt_agents::AgentError::Tool(_) => "tool_failure",
        openrt_agents::AgentError::Cancelled => "cancellation",
        openrt_agents::AgentError::Internal(_) => "transient",
    }
}

/// Called by the engine after each node's result is durably recorded
/// (blackboard artifact written). Implementations must **fail closed**: any
/// error returned aborts the run before the next node is scheduled.
#[async_trait]
pub trait CheckpointHook: Send + Sync {
    async fn on_node_completed(&self, progress: &RunProgress) -> Result<(), String>;

    /// Called right after new steps are spawned, BEFORE any of them can
    /// join, so in-flight work is durably recorded as `Running`
    /// (Phase 10 crash safety: the window between spawn and first result
    /// must never look like "never started"). Default: no-op.
    async fn on_nodes_scheduled(&self, progress: &RunProgress) -> Result<(), String> {
        let _ = progress;
        Ok(())
    }
}

/// Outcome of one workflow run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StepReport {
    pub step: String,
    pub kind: SubAgentKind,
    pub status: WorkflowStatus,
    pub attempts: u32,
    pub artifact_key: Option<String>,
    pub error: Option<String>,
}

/// Outcome of a whole workflow run; persisted to `runs/<workflow_id>.json`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkflowRunReport {
    pub workflow_id: String,
    pub execution_id: ExecutionId,
    pub status: WorkflowStatus,
    pub steps: Vec<StepReport>,
}

/// Seed for resuming a workflow from a checkpoint: which steps already
/// completed (with their outputs), their cumulative attempt budgets, and any
/// absolute per-step attempt caps the recovery policy imposes.
#[derive(Debug, Clone, Default)]
pub struct ResumeSeed {
    /// step → recorded output text (re-fed into `only_if` evaluation).
    pub done: HashMap<String, String>,
    /// step → attempts consumed across all prior runs (retry budget).
    pub attempts_by_step: HashMap<String, u32>,
    /// step → absolute cumulative attempt cap (recovery `max_attempts`), when
    /// tighter than `step.max_retries + 1`.
    pub attempt_limits: HashMap<String, u32>,
}

/// The state a single wave-task needs; everything is cloned per task so tasks
/// are `'static` and safe to join on a [`tokio::task::JoinSet`].
#[derive(Clone)]
struct StepEnv {
    bus: Arc<EventBus>,
    store: Arc<dyn PersistenceStore>,
    blackboard: Arc<Blackboard>,
    cancel: CancellationToken,
    session_id: Option<SessionId>,
    task_id: Option<TaskId>,
    orchestrator: AgentId,
    /// Absolute cumulative per-step attempt caps (recovery policy); empty when
    /// the workflow's own `max_retries` governs.
    attempt_limits: HashMap<String, u32>,
    /// Cumulative attempts already consumed by this step in prior runs (resume).
    base_attempts: HashMap<String, u32>,
}

#[derive(Debug)]
enum StepOutcome {
    Skipped,
    Completed {
        result: openrt_agents::AgentResult,
        attempts: u32,
    },
    Failed {
        attempts: u32,
        error: String,
        category: String,
    },
}

/// Executes a [`Workflow`] over role sub-agents.
///
/// Scheduling is a *fine-grained continuous readiness loop*: every step whose
/// dependencies are satisfied is spawned onto a [`tokio::task::JoinSet`]
/// as soon as a slot is free (bounded by `max_concurrency`, and by an
/// optional resource-admission callback). A completion immediately frees the
/// slot and makes its dependents eligible — there is no wave barrier, so a
/// long-running branch never delays an already-ready sibling branch. Ordering
/// guarantees stay deterministic (deps still gate dependents). A step whose
/// `only_if` condition is false is skipped (counts as completed so its
/// dependents still run); a failed step with retries left is retried inside
/// its task; a step that exhausts its retries fails and any dependent is
/// never scheduled.
pub struct WorkflowEngine {
    agents: HashMap<SubAgentKind, Arc<dyn Agent>>,
    env: StepEnv,
    hook: Option<Arc<dyn CheckpointHook>>,
    /// Max steps in flight at once (0 = unlimited).
    max_concurrency: usize,
    /// Optional admission control checked before spawning each new step.
    /// `Err(reason)` with nothing in flight aborts the run (fail closed);
    /// with steps in flight the scheduler waits for a free slot and retries.
    admission: Option<Arc<dyn Fn() -> Result<(), String> + Send + Sync>>,
    /// Phase 8: validates every declared step sandbox profile before the
    /// first step runs (`None` = no admission attached).
    sandbox_admission: Option<Arc<dyn StepSandbox>>,
    /// Phase 11 cooperative pause: consulted at checkpoint boundaries
    /// (never mid-step) — when set, no new steps spawn, in-flight steps
    /// drain to their next checkpoint, and the run reports `Paused`
    /// instead of failing (`None` = no pause channel).
    pause: Option<Arc<std::sync::atomic::AtomicBool>>,
}

/// Borrowed view of live engine state handed to the checkpoint path — keeps
/// the hook signature compact and the snapshot consistent per node.
struct RunSnapshot<'a> {
    workflow: &'a Workflow,
    execution_id: ExecutionId,
    done: &'a HashMap<String, openrt_agents::AgentResult>,
    failed: &'a HashSet<String>,
    reports: &'a [StepReport],
    attempts_by_step: &'a HashMap<String, u32>,
    failed_categories: &'a HashMap<String, String>,
    /// Steps spawned but not yet joined (reported as `Running`).
    in_flight: &'a HashSet<String>,
    /// Attempt budgets carried over from prior (resumed) runs.
    base_attempts: &'a HashMap<String, u32>,
}
impl WorkflowEngine {
    pub fn new(
        agents: HashMap<SubAgentKind, Arc<dyn Agent>>,
        bus: Arc<EventBus>,
        store: Arc<dyn PersistenceStore>,
        blackboard: Arc<Blackboard>,
        cancel: CancellationToken,
        orchestrator: AgentId,
    ) -> Self {
        Self {
            agents,
            env: StepEnv {
                bus,
                store,
                blackboard,
                cancel,
                session_id: None,
                task_id: None,
                orchestrator,
                attempt_limits: HashMap::new(),
                base_attempts: HashMap::new(),
            },
            hook: None,
            max_concurrency: 0,
            admission: None,
            sandbox_admission: None,
            pause: None,
        }
    }

    pub fn bind(mut self, kind: SubAgentKind, agent: Arc<dyn Agent>) -> Self {
        self.agents.insert(kind, agent);
        self
    }

    /// Cap how many steps may run concurrently (0 = unlimited). Readiness
    /// still follows the DAG; this only bounds in-flight width.
    pub fn with_max_concurrency(mut self, max: usize) -> Self {
        self.max_concurrency = max;
        self
    }

    /// Install a resource-admission callback consulted before each new step
    /// spawn. `Err` while other steps are still running simply defers the
    /// spawn; `Err` with an idle engine fails the run closed.
    pub fn with_resource_admission(
        mut self,
        admit: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    ) -> Self {
        self.admission = Some(admit);
        self
    }

    /// Phase 11: attach the run-level cooperative pause flag. Consulted at
    /// checkpoint boundaries only — never aborts an in-flight step.
    pub fn with_pause_flag(mut self, flag: Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.pause = Some(flag);
        self
    }

    fn pause_requested(&self) -> bool {
        self.pause
            .as_ref()
            .map(|f| f.load(std::sync::atomic::Ordering::SeqCst))
            .unwrap_or(false)
    }

    /// Attach Phase 8 sandbox admission: every declared step sandbox profile
    /// is validated against the sandbox authority before the first step.
    pub fn with_sandbox_admission(mut self, admission: Arc<dyn StepSandbox>) -> Self {
        self.sandbox_admission = Some(admission);
        self
    }

    pub fn with_correlation(
        mut self,
        session_id: Option<SessionId>,
        task_id: Option<TaskId>,
    ) -> Self {
        self.env.session_id = session_id;
        self.env.task_id = task_id;
        self
    }

    /// Attach a checkpoint hook invoked after each node's result is durably
    /// recorded. Missing hooks are a no-op (plain determinism runs).
    pub fn with_checkpoint_hook(mut self, hook: Arc<dyn CheckpointHook>) -> Self {
        self.hook = Some(hook);
        self
    }

    /// Run the workflow to completion, cancellation, or structural failure.
    /// On completion the run report is persisted under `runs/`; a workflow
    /// whose steps collectively failed still returns a report (status
    /// `Failed`), so only engine-level failures surface as errors.
    pub async fn run(&mut self, workflow: &Workflow) -> Result<WorkflowRunReport, WorkflowError> {
        self.run_with_seed(workflow, ResumeSeed::default()).await
    }

    /// Resume a workflow from a checkpoint seed: already-completed steps are
    /// never re-executed; their outputs are re-fed for `only_if` evaluation.
    pub async fn run_resume(
        &mut self,
        workflow: &Workflow,
        seed: ResumeSeed,
    ) -> Result<WorkflowRunReport, WorkflowError> {
        self.run_with_seed(workflow, seed).await
    }

    /// Assemble the progress snapshot handed to checkpoint hooks: completed
    /// nodes from reports, **plus every in-flight step as `Running`** (Phase
    /// 10), with `pending` excluding anything currently executing.
    fn build_progress(&self, snap: &RunSnapshot<'_>) -> RunProgress {
        let mut nodes = snap
            .reports
            .iter()
            .map(|r| RunNodeProgress {
                step: r.step.clone(),
                state: if r.status == WorkflowStatus::Failed {
                    RunNodeState::Failed
                } else {
                    RunNodeState::Completed
                },
                attempts: r.attempts,
                artifact_key: r.artifact_key.clone(),
                error: r.error.clone(),
                failure_category: snap.failed_categories.get(&r.step).cloned(),
            })
            .collect::<Vec<_>>();
        // In-flight steps: outcome unknown, attempt consumed (base from prior
        // runs + the attempt now executing).
        let mut running: Vec<&String> = snap.in_flight.iter().collect();
        running.sort();
        for step_id in running {
            let completed_prior = snap.attempts_by_step.get(step_id).copied();
            let base = snap.base_attempts.get(step_id).copied();
            let attempts = completed_prior.or(base).map(|a| a + 1).unwrap_or(1);
            nodes.push(RunNodeProgress {
                step: step_id.clone(),
                state: RunNodeState::Running,
                attempts,
                artifact_key: None,
                error: None,
                failure_category: None,
            });
        }
        let pending = snap
            .workflow
            .steps
            .iter()
            .filter(|s| !snap.done.contains_key(&s.id) && !snap.failed.contains(&s.id))
            .filter(|s| !snap.in_flight.contains(&s.id))
            .filter(|s| s.depends_on.iter().all(|d| snap.done.contains_key(d)))
            .map(|s| s.id.clone())
            .collect::<Vec<_>>();
        RunProgress {
            workflow_id: snap.workflow.id.clone(),
            session_id: self.env.session_id,
            task_id: self.env.task_id,
            execution_id: snap.execution_id,
            nodes,
            pending,
            retry_counters: snap.attempts_by_step.clone(),
        }
    }

    /// Stream the current execution progress to the checkpoint hook (if any).
    async fn maybe_checkpoint(&self, snap: &RunSnapshot<'_>) -> Result<(), WorkflowError> {
        let Some(hook) = &self.hook else {
            return Ok(());
        };
        let progress = self.build_progress(snap);
        hook.on_node_completed(&progress)
            .await
            .map_err(WorkflowError::Checkpoint)
    }

    /// Durably record freshly spawned (in-flight) steps before awaiting any
    /// of them — fail-closed like the completion hook.
    async fn maybe_scheduled(&self, snap: &RunSnapshot<'_>) -> Result<(), WorkflowError> {
        let Some(hook) = &self.hook else {
            return Ok(());
        };
        let progress = self.build_progress(snap);
        hook.on_nodes_scheduled(&progress)
            .await
            .map_err(WorkflowError::Checkpoint)
    }

    async fn run_with_seed(
        &mut self,
        workflow: &Workflow,
        seed: ResumeSeed,
    ) -> Result<WorkflowRunReport, WorkflowError> {
        if workflow.steps.is_empty() {
            return Err(WorkflowError::EmptyWorkflow);
        }
        let order = workflow.topological_sort()?;
        for step in &workflow.steps {
            if !self.agents.contains_key(&step.kind) {
                return Err(WorkflowError::UnknownRole(step.kind.to_string()));
            }
        }
        let _ = order; // structural validation only

        // Phase 8: sandbox admission — every declared step sandbox profile
        // must validate against the sandbox authority BEFORE the first step
        // starts (workflow-level requirement; per-tool enforcement still
        // happens at the gate on every tool call inside the run).
        if let Some(sbx) = &self.sandbox_admission {
            for step in &workflow.steps {
                if let Some(profile) = &step.sandbox {
                    let reason = match sbx.admit_profile(profile).await {
                        Ok(()) => continue,
                        Err(reason) => reason,
                    };
                    return Err(WorkflowError::SandboxDenied {
                        step: step.id.clone(),
                        reason,
                    });
                }
            }
        }

        self.emit(
            EventKind::WorkflowStarted,
            &serde_json::json!({
                "workflow_id": workflow.id,
                "steps": workflow.steps.len(),
                "plan": workflow,
            }),
        )
        .await;
        let _ = self
            .env
            .store
            .put_record(
                DAG_NS,
                &workflow.id,
                &serde_json::to_value(workflow)
                    .map_err(openrt_persistence::PersistenceError::from)?,
            )
            .await;

        let mut done: HashMap<String, openrt_agents::AgentResult> = HashMap::new();
        let mut outputs: HashMap<String, String> = HashMap::new();
        let mut failed: HashSet<String> = HashSet::new();
        let mut reports: Vec<StepReport> = Vec::new();
        let mut attempts_by_step: HashMap<String, u32> = HashMap::new();
        let mut failed_categories: HashMap<String, String> = HashMap::new();
        let mut attempt_limits = seed.attempt_limits.clone();
        let base_attempts = seed.attempts_by_step.clone();
        // Wire cumulative prior-run attempt budgets into the environment so
        // retried nodes keep counting from where the previous run stopped.
        self.env.base_attempts = base_attempts.clone();
        let execution_id = ExecutionId::new();
        let mut status = WorkflowStatus::Completed;

        // Seed: steps already completed across prior runs are never rerun.
        let completion_agent = |text: String| openrt_agents::AgentResult {
            final_text: text,
            ended: openrt_agents::AgentEndReason::Completed,
            turns: 0,
            tool_calls: 0,
            retries: 0,
            messages: Vec::new(),
        };
        for (step_id, text) in &seed.done {
            if !workflow.steps.iter().any(|s| &s.id == step_id) {
                return Err(WorkflowError::Condition(format!(
                    "resume seed references unknown step `{step_id}`"
                )));
            }
            done.insert(step_id.clone(), completion_agent(text.clone()));
            outputs.insert(step_id.clone(), text.clone());
            let prior = seed
                .attempts_by_step
                .get(step_id)
                .copied()
                .unwrap_or(1)
                .max(1);
            attempts_by_step.insert(step_id.clone(), prior);
            let kind = workflow
                .steps
                .iter()
                .find(|s| &s.id == step_id)
                .map(|s| s.kind)
                .unwrap_or(SubAgentKind::Planner);
            reports.push(StepReport {
                step: step_id.clone(),
                kind,
                status: WorkflowStatus::Completed,
                attempts: prior,
                artifact_key: Some(format!("output.{step_id}")),
                error: None,
            });
        }
        // Seed: absolute per-step caps that override the workflow's own budget
        // (a node must always be allowed at least one fresh attempt).
        for (step_id, limit) in &seed.attempt_limits {
            if !workflow.steps.iter().any(|s| &s.id == step_id) {
                return Err(WorkflowError::Condition(format!(
                    "resume seed attempt limit references unknown step `{step_id}`"
                )));
            }
            let base = base_attempts.get(step_id).copied().unwrap_or(0);
            attempt_limits.insert(
                step_id.clone(),
                (*limit).max(base + 1), // at least one fresh attempt
            );
        }

        // ---- Fine-grained continuous readiness scheduler ----
        //
        // Spawn every ready step (deps satisfied, not done/failed/in-flight)
        // while slots remain; await *one* completion; free the slot; checkpoint
        // (fail closed); immediately re-evaluate readiness. No wave barrier:
        // a slow branch never holds back an already-ready sibling.
        let mut joinset = tokio::task::JoinSet::new();
        let mut in_flight: HashSet<String> = HashSet::new();
        let max_inflight = if self.max_concurrency == 0 {
            usize::MAX
        } else {
            self.max_concurrency
        };

        loop {
            if self.env.cancel.is_cancelled() {
                status = WorkflowStatus::Cancelled;
                break;
            }

            // Phase 11 cooperative pause: once requested, stop spawning new
            // steps; in-flight steps drain to their next checkpoint first
            // (pause is observed HERE, between checkpoints — never mid-step).
            if self.pause_requested() && in_flight.is_empty() {
                status = WorkflowStatus::Paused;
                break;
            }
            let paused_hold = self.pause_requested();

            // Spawn newly ready steps up to the concurrency / admission limits.
            let mut spawned_any = false;
            if !paused_hold {
                loop {
                    let ready: Vec<WorkflowStep> = workflow
                        .steps
                        .iter()
                        .filter(|s| {
                            !done.contains_key(&s.id)
                                && !failed.contains(&s.id)
                                && !in_flight.contains(&s.id)
                        })
                        .filter(|s| s.depends_on.iter().all(|d| done.contains_key(d)))
                        .cloned()
                        .collect();
                    if ready.is_empty() {
                        break;
                    }
                    let mut spawned = false;
                    for step in ready {
                        if in_flight.len() >= max_inflight {
                            break;
                        }
                        if let Some(admit) = &self.admission {
                            if let Err(reason) = admit() {
                                if in_flight.is_empty() {
                                    // Fail closed: nothing running, admission still denies.
                                    return Err(WorkflowError::Condition(format!(
                                        "resource admission denied: {reason}"
                                    )));
                                }
                                // Defer: wait for an in-flight step to free a slot.
                                break;
                            }
                        }
                        let env = make_env(&self.env, &self.agents, &step, attempt_limits.clone());
                        let wave_outputs = outputs.clone();
                        let step_id = step.id.clone();
                        joinset.spawn(async move { run_step(&env, &step, &wave_outputs).await });
                        in_flight.insert(step_id);
                        spawned = true;
                        spawned_any = true;
                    }
                    if !spawned {
                        break;
                    }
                }
            }

            // Phase 10: record the just-spawned batch as `Running` BEFORE
            // awaiting any completion — the spawn→first-result window must
            // be durable so a crash lands in UncertainPolicy, not in a
            // stale `Pending` that would silently re-run side effects.
            if spawned_any {
                let snap = RunSnapshot {
                    workflow,
                    execution_id,
                    done: &done,
                    failed: &failed,
                    reports: &reports,
                    attempts_by_step: &attempts_by_step,
                    failed_categories: &failed_categories,
                    in_flight: &in_flight,
                    base_attempts: &base_attempts,
                };
                self.maybe_scheduled(&snap).await?;
            }

            if in_flight.is_empty() {
                // Nothing running and nothing newly spawnable: either all work
                // is done, or remaining steps are gated behind failed deps.
                break;
            }

            // Await exactly one completion, then immediately re-evaluate readiness.
            let joined = joinset.join_next().await.ok_or_else(|| {
                WorkflowError::Condition("scheduler join set emptied unexpectedly".into())
            })?;
            let (step, outcome) = joined
                .map_err(|e| WorkflowError::Condition(format!("step task join failed: {e}")))??;
            in_flight.remove(&step.id);

            match outcome {
                StepOutcome::Skipped => {
                    let synthetic = openrt_agents::AgentResult {
                        final_text: "skipped".to_string(),
                        ended: openrt_agents::AgentEndReason::Completed,
                        turns: 0,
                        tool_calls: 0,
                        retries: 0,
                        messages: Vec::new(),
                    };
                    done.insert(step.id.clone(), synthetic);
                    outputs.insert(step.id.clone(), "skipped".to_string());
                    attempts_by_step.entry(step.id.clone()).or_insert(0);
                    reports.push(StepReport {
                        step: step.id.clone(),
                        kind: step.kind,
                        status: WorkflowStatus::Completed,
                        attempts: attempts_by_step[&step.id],
                        artifact_key: Some(format!("output.{}", step.id)),
                        error: None,
                    });
                }
                StepOutcome::Completed { result, attempts } => {
                    done.insert(step.id.clone(), result.clone());
                    outputs.insert(step.id.clone(), result.final_text.clone());
                    attempts_by_step.insert(step.id.clone(), attempts);
                    reports.push(StepReport {
                        step: step.id.clone(),
                        kind: step.kind,
                        status: WorkflowStatus::Completed,
                        attempts,
                        artifact_key: Some(format!("output.{}", step.id)),
                        error: None,
                    });
                    self.emit(
                        EventKind::WorkflowStepCompleted,
                        &serde_json::json!({
                            "step": step.id,
                            "kind": step.kind,
                            "artifact": format!("output.{}", step.id),
                            "attempts": attempts,
                            "turns": result.turns,
                        }),
                    )
                    .await;
                }
                StepOutcome::Failed {
                    attempts,
                    error,
                    category,
                } => {
                    failed.insert(step.id.clone());
                    attempts_by_step.insert(step.id.clone(), attempts);
                    failed_categories.insert(step.id.clone(), category);
                    reports.push(StepReport {
                        step: step.id.clone(),
                        kind: step.kind,
                        status: WorkflowStatus::Failed,
                        attempts,
                        artifact_key: None,
                        error: Some(error.clone()),
                    });
                    self.emit(
                        EventKind::WorkflowStepFailed,
                        &serde_json::json!({
                            "step": step.id,
                            "kind": step.kind,
                            "attempts": attempts,
                            "error": error,
                        }),
                    )
                    .await;
                }
            }
            // Fail closed: a checkpoint that cannot be durably written
            // aborts the run before the next node is scheduled.
            let snap = RunSnapshot {
                workflow,
                execution_id,
                done: &done,
                failed: &failed,
                reports: &reports,
                attempts_by_step: &attempts_by_step,
                failed_categories: &failed_categories,
                in_flight: &in_flight,
                base_attempts: &base_attempts,
            };
            self.maybe_checkpoint(&snap).await?;
        }

        let completed = reports
            .iter()
            .filter(|r| r.status == WorkflowStatus::Completed)
            .count();
        let step_failures = reports
            .iter()
            .filter(|r| r.status == WorkflowStatus::Failed)
            .count();
        if self.env.cancel.is_cancelled() {
            // A cancelled run reports Cancelled even if some steps recorded
            // failures on the way out (they were interrupted, not exhausted).
            status = WorkflowStatus::Cancelled;
        } else if status == WorkflowStatus::Completed && step_failures > 0 {
            status = WorkflowStatus::Failed;
        }
        let report = WorkflowRunReport {
            workflow_id: workflow.id.clone(),
            execution_id,
            status,
            steps: reports,
        };
        self.env
            .store
            .put_record(
                RUNS_NS,
                &workflow.id,
                &serde_json::to_value(&report)
                    .map_err(openrt_persistence::PersistenceError::from)?,
            )
            .await?;
        let kind = if status == WorkflowStatus::Paused {
            EventKind::WorkflowPaused
        } else {
            EventKind::WorkflowCompleted
        };
        self.emit(
            kind,
            &serde_json::json!({
                "workflow_id": workflow.id,
                "status": status,
                "steps_completed": completed,
                "steps_failed": step_failures,
            }),
        )
        .await;

        Ok(report)
    }

    async fn emit(&self, kind: EventKind, data: &serde_json::Value) {
        self.env
            .bus
            .emit_correlated(
                kind,
                self.env.session_id,
                self.env.task_id,
                Some(self.env.orchestrator),
                data.clone(),
            )
            .await;
    }
}

/// A `StepEnv` plus the piece that must own a step's role agent: the task runs
/// inside its own clone of the environment and the agent dispatch.
#[derive(Clone)]
struct TaskEnv {
    env: StepEnv,
    agent: Arc<dyn Agent>,
}

fn make_env(
    env: &StepEnv,
    agents: &HashMap<SubAgentKind, Arc<dyn Agent>>,
    step: &WorkflowStep,
    attempt_limits: HashMap<String, u32>,
) -> TaskEnv {
    let agent = agents
        .get(&step.kind)
        .cloned()
        .expect("missing agent for role (validated in run)");
    TaskEnv {
        env: StepEnv {
            bus: env.bus.clone(),
            store: env.store.clone(),
            blackboard: env.blackboard.clone(),
            cancel: env.cancel.clone(),
            session_id: env.session_id,
            task_id: env.task_id,
            orchestrator: env.orchestrator,
            attempt_limits,
            base_attempts: env.base_attempts.clone(),
        },
        agent,
    }
}

/// Execute a single step: evaluate `only_if`, run the role agent with retries,
/// persist the artifact to the blackboard, and emit step events.
///
/// `attempts` are counted **cumulatively** across prior resumed runs (resume
/// seeds seed `base_attempts`), so checkpointed attempt budgets stay accurate.
async fn run_step(
    env: &TaskEnv,
    step: &WorkflowStep,
    outputs: &HashMap<String, String>,
) -> Result<(WorkflowStep, StepOutcome), WorkflowError> {
    if let Some(condition) = &step.only_if {
        match condition.eval(outputs, &env.env.blackboard) {
            Ok(false) => {
                env.env
                    .blackboard
                    .put(
                        &format!("output.{}", step.id),
                        serde_json::json!({ "text": "skipped" }),
                    )
                    .await
                    .map_err(WorkflowError::from)?;
                return Ok((step.clone(), StepOutcome::Skipped));
            }
            Ok(true) => {}
            Err(e) => return Err(e),
        }
    }

    emit_step_event(
        env,
        EventKind::WorkflowStepStarted,
        &serde_json::json!({ "step": step.id, "kind": step.kind }),
    )
    .await;

    // Attempt budget (cumulative): recovery-policy cap when present, else the
    // workflow's own `max_retries` (which still allows the first attempt).
    let base = env.env.base_attempts.get(&step.id).copied().unwrap_or(0);
    // Saturating arithmetic: attempt budgets come from operator config and
    // resume seeds; overflow must clamp, never wrap around (H2).
    let total = env.env.attempt_limits.get(&step.id).copied().map_or(
        base.saturating_add(step.max_retries).saturating_add(1),
        |cap| cap.max(base.saturating_add(1)),
    );
    let mut attempts: u32 = base;
    let outcome = loop {
        attempts += 1;
        let ctx = openrt_agents::AgentContext {
            session_id: env.env.session_id.unwrap_or_default(),
            task_id: env.env.task_id.unwrap_or_default(),
            history: Vec::new(),
            cancel: env.env.cancel.child(),
            pause: None,
        };
        // Role identity is injected as a system-level framing prepended to the
        // step's task, so the engine works with any `Agent` implementation.
        let input = format!("{}\n\nTask:\n{}", step.kind.role_prompt(), step.prompt);
        // The agent run is raced against the run token so a workflow cancel
        // surfaces promptly even if the agent would otherwise block forever.
        let agent_result = tokio::select! {
            _ = ctx.cancel.future() => Err(openrt_agents::AgentError::Cancelled),
            result = env.agent.run(&ctx, &input) => result,
        };
        match agent_result {
            Ok(result) => {
                env.env
                    .blackboard
                    .put(
                        &format!("output.{}", step.id),
                        serde_json::json!({ "text": result.final_text }),
                    )
                    .await
                    .map_err(WorkflowError::from)?;
                break StepOutcome::Completed { result, attempts };
            }
            Err(e) => {
                if attempts >= total || matches!(e, openrt_agents::AgentError::Cancelled) {
                    let _ = env
                        .env
                        .blackboard
                        .remove(&format!("output.{}", step.id))
                        .await;
                    let exhausted = attempts >= total;
                    if exhausted {
                        emit_step_event(
                            env,
                            EventKind::RetryExhausted,
                            &serde_json::json!({
                                "node": step.id,
                                "attempts": attempts,
                            }),
                        )
                        .await;
                    }
                    break StepOutcome::Failed {
                        attempts,
                        error: e.to_string(),
                        category: agent_failure_tag(&e).to_string(),
                    };
                }
            }
        }
    };
    Ok((step.clone(), outcome))
}

async fn emit_step_event(env: &TaskEnv, kind: EventKind, data: &serde_json::Value) {
    env.env
        .bus
        .emit_correlated(
            kind,
            env.env.session_id,
            env.env.task_id,
            Some(env.env.orchestrator),
            data.clone(),
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_agents::{AgentContext, AgentEndReason};
    use openrt_persistence::PersistenceError;
    use openrt_telemetry::ChannelSink;

    /// Deterministic role agent for engine tests (no model dependency).
    struct EchoAgent {
        id: String,
        fail_times: std::sync::atomic::AtomicU32,
    }

    #[async_trait::async_trait]
    impl Agent for EchoAgent {
        fn id(&self) -> &str {
            &self.id
        }
        async fn run(
            &self,
            _ctx: &AgentContext,
            input: &str,
        ) -> Result<openrt_agents::AgentResult, openrt_agents::AgentError> {
            use std::sync::atomic::Ordering;
            if self.fail_times.fetch_sub(1, Ordering::SeqCst) > 0 {
                return Err(openrt_agents::AgentError::Internal("boom".into()));
            }
            Ok(openrt_agents::AgentResult {
                final_text: format!("{}-done", input),
                ended: AgentEndReason::Completed,
                turns: 1,
                tool_calls: 0,
                retries: 0,
                messages: Vec::new(),
            })
        }
    }

    fn echo(kind: SubAgentKind, fail_times: u32) -> (SubAgentKind, Arc<dyn Agent>) {
        (
            kind,
            Arc::new(EchoAgent {
                id: kind.to_string(),
                fail_times: std::sync::atomic::AtomicU32::new(fail_times),
            }),
        )
    }

    struct Harness {
        engine: WorkflowEngine,
        store: Arc<dyn PersistenceStore>,
        bb: Arc<Blackboard>,
        sink: Arc<ChannelSink>,
        bus: openrt_telemetry::EventBus,
    }

    async fn harness(agents: HashMap<SubAgentKind, Arc<dyn Agent>>) -> Harness {
        let sink = Arc::new(ChannelSink::default());
        let (bus, _handle) = openrt_telemetry::EventBusBuilder::new()
            .sink(sink.clone())
            .start();
        let dir = std::env::temp_dir().join(format!("openrt-orch-test-{}", uuid::Uuid::new_v4()));
        let store: Arc<dyn PersistenceStore> = Arc::new(
            openrt_persistence::LocalFileStore::open(&dir)
                .await
                .unwrap(),
        );
        let bb = Arc::new(Blackboard::new());
        let engine = WorkflowEngine::new(
            agents,
            Arc::new(bus.clone()),
            store.clone(),
            bb.clone(),
            CancellationToken::new(),
            AgentId::new(),
        );
        Harness {
            engine,
            store,
            bb,
            sink,
            bus,
        }
    }

    #[tokio::test]
    async fn runs_sequential_dag_and_writes_artifacts() {
        let mut agents = HashMap::new();
        let (k, a) = echo(SubAgentKind::Planner, 0);
        agents.insert(k, a);
        let (k, a) = echo(SubAgentKind::Coder, 0);
        agents.insert(k, a);
        let mut h = harness(agents).await;

        let wf = Workflow::new("wf-seq")
            .step(WorkflowStep::new(
                "plan",
                SubAgentKind::Planner,
                "make plan",
            ))
            .step(WorkflowStep::new("code", SubAgentKind::Coder, "implement").dep("plan"));
        let report = h.engine.run(&wf).await.unwrap();
        assert_eq!(report.status, WorkflowStatus::Completed);
        assert_eq!(report.steps.len(), 2);
        assert!(h.bb.get("output.code").unwrap().unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("implement"));
        assert!(h
            .store
            .get_record(RUNS_NS, "wf-seq")
            .await
            .unwrap()
            .is_some());
        assert!(h
            .store
            .get_record(DAG_NS, "wf-seq")
            .await
            .unwrap()
            .is_some());
        let kinds: Vec<EventKind> = h.sink.snapshot().iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&EventKind::WorkflowStarted));
        assert!(kinds.contains(&EventKind::WorkflowStepStarted));
        assert!(kinds.contains(&EventKind::WorkflowStepCompleted));
        assert!(kinds.contains(&EventKind::WorkflowCompleted));
    }

    #[tokio::test]
    async fn retries_then_recovers_and_failure_fails_workflow() {
        // recovery path
        let mut agents = HashMap::new();
        let (k, a) = echo(SubAgentKind::Researcher, 1);
        agents.insert(k, a);
        let mut h = harness(agents).await;
        let wf = Workflow::new("wf-retry")
            .step(WorkflowStep::new("r", SubAgentKind::Researcher, "research").retries(2));
        let report = h.engine.run(&wf).await.unwrap();
        assert_eq!(report.status, WorkflowStatus::Completed);
        assert_eq!(report.steps[0].attempts, 2);
        let _ = h;

        // exhausted retries → workflow Failed
        let mut agents = HashMap::new();
        let (k, a) = echo(SubAgentKind::Researcher, 100);
        agents.insert(k, a);
        let mut h = harness(agents).await;
        let wf = Workflow::new("wf-fail")
            .step(WorkflowStep::new("r", SubAgentKind::Researcher, "research").retries(1));
        let report = h.engine.run(&wf).await.unwrap();
        assert_eq!(report.status, WorkflowStatus::Failed);
        assert_eq!(report.steps[0].status, WorkflowStatus::Failed);
        assert_eq!(report.steps[0].attempts, 2);
    }

    #[tokio::test]
    async fn condition_skips_step_when_false_and_unknown_deps_rejected() {
        let mut agents = HashMap::new();
        let (k, a) = echo(SubAgentKind::Planner, 0);
        agents.insert(k, a);
        let (k, a) = echo(SubAgentKind::Coder, 0);
        agents.insert(k, a);
        let mut h = harness(agents).await;

        let wf = Workflow::new("wf-cond")
            .step(WorkflowStep::new(
                "plan",
                SubAgentKind::Planner,
                "make plan",
            ))
            .step(
                WorkflowStep::new("code", SubAgentKind::Coder, "implement")
                    .dep("plan")
                    .when(crate::Condition::BlackboardEquals {
                        key: "gate".into(),
                        value: serde_json::json!("never"),
                    }),
            );
        let report = h.engine.run(&wf).await.unwrap();
        assert_eq!(report.status, WorkflowStatus::Completed);
        assert_eq!(h.bb.get("output.code").unwrap().unwrap()["text"], "skipped");
    }

    #[tokio::test]
    async fn cycle_and_missing_agent_are_structural_errors() {
        let mut h = harness(HashMap::new()).await;
        let wf = Workflow::new("wf-cyc")
            .step(WorkflowStep::new("a", SubAgentKind::Planner, "a").dep("b"))
            .step(WorkflowStep::new("b", SubAgentKind::Planner, "b").dep("a"));
        assert!(h.engine.run(&wf).await.is_err());

        let wf2 = Workflow::new("wf-unknown").step(WorkflowStep::new(
            "x",
            SubAgentKind::Critic,
            "no agent bound",
        ));
        assert!(matches!(
            h.engine.run(&wf2).await,
            Err(WorkflowError::UnknownRole(_))
        ));
    }

    /// Records every progress snapshot it receives; optionally fails on a
    /// chosen call to exercise the fail-closed contract.
    struct FakeHook {
        calls: std::sync::Mutex<Vec<RunProgress>>,
        fail_at_call: std::sync::atomic::AtomicU32,
    }

    impl FakeHook {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                fail_at_call: std::sync::atomic::AtomicU32::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl CheckpointHook for FakeHook {
        async fn on_node_completed(&self, progress: &RunProgress) -> Result<(), String> {
            let n = {
                let mut calls = self.calls.lock().unwrap();
                calls.push(progress.clone());
                calls.len() as u32
            };
            if self.fail_at_call.load(std::sync::atomic::Ordering::SeqCst) == n {
                return Err(format!("injected checkpoint failure at call {n}"));
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn checkpoint_hook_receives_progress_after_every_node() {
        let mut agents = HashMap::new();
        let (k, a) = echo(SubAgentKind::Planner, 0);
        agents.insert(k, a);
        let (k, a) = echo(SubAgentKind::Coder, 0);
        agents.insert(k, a);
        let mut h = harness(agents).await;
        let hook = Arc::new(FakeHook::new());
        h.engine = h.engine.with_checkpoint_hook(hook.clone());

        let wf = Workflow::new("wf-hook")
            .step(WorkflowStep::new(
                "plan",
                SubAgentKind::Planner,
                "make plan",
            ))
            .step(WorkflowStep::new("code", SubAgentKind::Coder, "implement").dep("plan"));
        let report = h.engine.run(&wf).await.unwrap();
        assert_eq!(report.status, WorkflowStatus::Completed);

        let calls = hook.calls.lock().unwrap();
        // One hook call per completed node (2 nodes → 2 snapshots).
        assert_eq!(calls.len(), 2);
        let first = &calls[0];
        assert_eq!(first.nodes.len(), 1);
        assert_eq!(first.nodes[0].step, "plan");
        assert_eq!(first.nodes[0].attempts, 1);
        assert!(first.pending.contains(&"code".to_string()));

        let last = &calls[1];
        assert_eq!(last.nodes.len(), 2);
        assert_eq!(last.retry_counters["plan"], 1);
        assert_eq!(last.retry_counters["code"], 1);
        assert!(last.pending.is_empty());
    }

    #[tokio::test]
    async fn checkpoint_hook_failure_aborts_before_next_node() {
        let mut agents = HashMap::new();
        let (k, a) = echo(SubAgentKind::Planner, 0);
        agents.insert(k, a);
        let (k, a) = echo(SubAgentKind::Coder, 0);
        agents.insert(k, a);
        let mut h = harness(agents).await;
        let hook = Arc::new(FakeHook::new());
        hook.fail_at_call
            .store(1, std::sync::atomic::Ordering::SeqCst);
        h.engine = h.engine.with_checkpoint_hook(hook);

        let wf = Workflow::new("wf-hookfail")
            .step(WorkflowStep::new(
                "plan",
                SubAgentKind::Planner,
                "make plan",
            ))
            .step(WorkflowStep::new("code", SubAgentKind::Coder, "implement").dep("plan"));
        let err = h.engine.run(&wf).await.unwrap_err();
        assert!(matches!(err, WorkflowError::Checkpoint(_)));
        // The first node's artifact was written, but the second never ran.
        assert!(h.bb.get("output.plan").unwrap().is_some());
        assert!(h.bb.get("output.code").unwrap().is_none());
    }

    /// Records spawn-time snapshots alongside completion snapshots.
    struct SpawnRecordingHook {
        scheduled: std::sync::Mutex<Vec<RunProgress>>,
        completed: std::sync::Mutex<Vec<RunProgress>>,
    }

    #[async_trait::async_trait]
    impl CheckpointHook for SpawnRecordingHook {
        async fn on_node_completed(&self, progress: &RunProgress) -> Result<(), String> {
            self.completed.lock().unwrap().push(progress.clone());
            Ok(())
        }
        async fn on_nodes_scheduled(&self, progress: &RunProgress) -> Result<(), String> {
            self.scheduled.lock().unwrap().push(progress.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn checkpoint_hook_records_in_flight_nodes_as_running() {
        let mut agents = HashMap::new();
        let (k, a) = echo(SubAgentKind::Planner, 0);
        agents.insert(k, a);
        let (k, a) = echo(SubAgentKind::Coder, 0);
        agents.insert(k, a);
        let mut h = harness(agents).await;
        let hook = Arc::new(SpawnRecordingHook {
            scheduled: std::sync::Mutex::new(Vec::new()),
            completed: std::sync::Mutex::new(Vec::new()),
        });
        h.engine = h.engine.with_checkpoint_hook(hook.clone());

        let wf = Workflow::new("wf-spawn")
            .step(WorkflowStep::new(
                "plan",
                SubAgentKind::Planner,
                "make plan",
            ))
            .step(WorkflowStep::new("code", SubAgentKind::Coder, "implement").dep("plan"));
        let report = h.engine.run(&wf).await.unwrap();
        assert_eq!(report.status, WorkflowStatus::Completed);

        let scheduled = hook.scheduled.lock().unwrap();
        // One spawn wave per node: plan first, then code after plan finishes.
        assert_eq!(scheduled.len(), 2);
        // Wave 1: `plan` is in flight (Running, attempt consumed), `code`
        // still Pending (deps unsatisfied).
        let first = &scheduled[0];
        assert_eq!(first.nodes.len(), 1);
        assert_eq!(first.nodes[0].step, "plan");
        assert!(
            matches!(first.nodes[0].state, RunNodeState::Running),
            "spawn-time snapshot must mark in-flight work Running"
        );
        assert_eq!(first.nodes[0].attempts, 1);
        // `plan` is executing (Running, not Pending) and `code` is not yet
        // eligible (deps unsatisfied), so nothing is pending in this wave.
        assert!(first.pending.is_empty());
        // Wave 2: plan Completed, code in flight — `code` must not appear
        // as Pending while it is executing.
        let second = &scheduled[1];
        let code = second
            .nodes
            .iter()
            .find(|n| n.step == "code")
            .expect("in-flight code node recorded");
        assert!(matches!(code.state, RunNodeState::Running));
        assert_eq!(code.attempts, 1);
        let plan = second
            .nodes
            .iter()
            .find(|n| n.step == "plan")
            .expect("plan recorded");
        assert!(matches!(plan.state, RunNodeState::Completed));
        assert!(second.pending.is_empty());

        // Completion snapshots stay free of Running (they record outcomes).
        let completed = hook.completed.lock().unwrap();
        assert_eq!(completed.len(), 2);
        assert!(completed.iter().all(|p| p
            .nodes
            .iter()
            .all(|n| !matches!(n.state, RunNodeState::Running))));
    }

    #[tokio::test]
    async fn checkpoint_hook_scheduled_attempts_carry_prior_run_budget() {
        let mut agents = HashMap::new();
        let (k, a) = echo(SubAgentKind::Planner, 0);
        agents.insert(k, a);
        let mut h = harness(agents).await;
        let hook = Arc::new(SpawnRecordingHook {
            scheduled: std::sync::Mutex::new(Vec::new()),
            completed: std::sync::Mutex::new(Vec::new()),
        });
        h.engine = h.engine.with_checkpoint_hook(hook.clone());

        let wf = Workflow::new("wf-spawn-resume").step(WorkflowStep::new(
            "plan",
            SubAgentKind::Planner,
            "make plan",
        ));
        // Prior run consumed 2 attempts; the resumed in-flight attempt is #3.
        let mut seed = ResumeSeed::default();
        seed.attempts_by_step.insert("plan".to_string(), 2);
        let report = h.engine.run_resume(&wf, seed).await.unwrap();
        assert_eq!(report.status, WorkflowStatus::Completed);

        let scheduled = hook.scheduled.lock().unwrap();
        assert_eq!(scheduled.len(), 1);
        assert_eq!(scheduled[0].nodes[0].attempts, 3);
        assert!(matches!(scheduled[0].nodes[0].state, RunNodeState::Running));
    }

    /// Agent whose `run` never resolves on its own — cancellation must still
    /// surface promptly (the engine races the run against the token).
    struct HangAgent {
        id: String,
    }

    #[async_trait]
    impl Agent for HangAgent {
        fn id(&self) -> &str {
            &self.id
        }
        async fn run(
            &self,
            _ctx: &AgentContext,
            _input: &str,
        ) -> Result<openrt_agents::AgentResult, openrt_agents::AgentError> {
            let never = std::future::pending::<()>();
            never.await;
            unreachable!("hang agent must be cancelled before completing");
        }
    }

    #[tokio::test]
    async fn cancelling_the_engine_stops_promptly_and_reports_cancelled() {
        let sink = Arc::new(ChannelSink::default());
        let (bus, _handle) = openrt_telemetry::EventBusBuilder::new()
            .sink(sink.clone())
            .start();
        let dir = std::env::temp_dir().join(format!("openrt-orch-cancel-{}", uuid::Uuid::new_v4()));
        let store: Arc<dyn PersistenceStore> = Arc::new(
            openrt_persistence::LocalFileStore::open(&dir)
                .await
                .unwrap(),
        );
        let bb = Arc::new(Blackboard::new());
        let cancel = CancellationToken::new();

        let mut agents = HashMap::new();
        let (k, a) = echo(SubAgentKind::Planner, 0);
        agents.insert(k, a);
        let coder = Arc::new(HangAgent {
            id: "coder-hang".into(),
        });
        agents.insert(SubAgentKind::Coder, coder);

        let mut engine = WorkflowEngine::new(
            agents,
            Arc::new(bus),
            store.clone(),
            bb.clone(),
            cancel.clone(),
            AgentId::new(),
        );
        let wf = Workflow::new("wf-cancel")
            .step(WorkflowStep::new(
                "plan",
                SubAgentKind::Planner,
                "make plan",
            ))
            .step(WorkflowStep::new("code", SubAgentKind::Coder, "hang").dep("plan"));

        let start = std::time::Instant::now();
        let handle = tokio::spawn(async move { engine.run(&wf).await });
        // Let the second wave enter the hang, then cancel.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        cancel.cancel();
        let report = handle
            .await
            .unwrap()
            .expect("engine run must return, not deadlock");
        assert_eq!(report.status, WorkflowStatus::Cancelled);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "cancellation must stop the run promptly, took {:?}",
            start.elapsed()
        );
        // The run report is still persisted (as Cancelled).
        assert!(store
            .get_record(RUNS_NS, "wf-cancel")
            .await
            .unwrap()
            .is_some());
    }

    /// Store that fails run-report writes (namespace `runs`) while delegating
    /// everything else — proves a persistence failure is fail-closed at the
    /// run boundary instead of being silently swallowed.
    struct FailingRunsStore {
        inner: Arc<dyn PersistenceStore>,
    }

    #[async_trait]
    impl PersistenceStore for FailingRunsStore {
        async fn create_session(&self, s: &openrt_core::Session) -> Result<(), PersistenceError> {
            self.inner.create_session(s).await
        }
        async fn update_session(&self, s: &openrt_core::Session) -> Result<(), PersistenceError> {
            self.inner.update_session(s).await
        }
        async fn load_session(
            &self,
            id: openrt_core::SessionId,
        ) -> Result<openrt_core::Session, PersistenceError> {
            self.inner.load_session(id).await
        }
        async fn list_sessions(&self) -> Result<Vec<openrt_core::Session>, PersistenceError> {
            self.inner.list_sessions().await
        }
        async fn save_task(&self, t: &openrt_core::Task) -> Result<(), PersistenceError> {
            self.inner.save_task(t).await
        }
        async fn load_task(
            &self,
            id: openrt_core::TaskId,
        ) -> Result<openrt_core::Task, PersistenceError> {
            self.inner.load_task(id).await
        }
        async fn list_tasks(
            &self,
            session_id: Option<openrt_core::SessionId>,
        ) -> Result<Vec<openrt_core::Task>, PersistenceError> {
            self.inner.list_tasks(session_id).await
        }
        async fn save_execution(
            &self,
            r: &openrt_core::ExecutionRecord,
        ) -> Result<(), PersistenceError> {
            self.inner.save_execution(r).await
        }
        async fn load_execution(
            &self,
            id: openrt_core::ExecutionId,
        ) -> Result<openrt_core::ExecutionRecord, PersistenceError> {
            self.inner.load_execution(id).await
        }
        async fn append_event(&self, e: &openrt_core::DomainEvent) -> Result<(), PersistenceError> {
            self.inner.append_event(e).await
        }
        async fn list_events(
            &self,
            opts: openrt_persistence::ListOptions,
        ) -> Result<Vec<openrt_core::DomainEvent>, PersistenceError> {
            self.inner.list_events(opts).await
        }
        async fn put_record(
            &self,
            namespace: &str,
            key: &str,
            value: &serde_json::Value,
        ) -> Result<(), PersistenceError> {
            if namespace == RUNS_NS {
                return Err(PersistenceError::Unsupported(
                    "run report write fails".into(),
                ));
            }
            self.inner.put_record(namespace, key, value).await
        }
        async fn get_record(
            &self,
            namespace: &str,
            key: &str,
        ) -> Result<Option<serde_json::Value>, PersistenceError> {
            self.inner.get_record(namespace, key).await
        }
        async fn list_records(
            &self,
            namespace: &str,
        ) -> Result<Vec<(String, serde_json::Value)>, PersistenceError> {
            self.inner.list_records(namespace).await
        }
        async fn delete_record(&self, namespace: &str, key: &str) -> Result<(), PersistenceError> {
            self.inner.delete_record(namespace, key).await
        }
        async fn clear(&self) -> Result<(), PersistenceError> {
            self.inner.clear().await
        }
    }

    #[tokio::test]
    async fn run_report_persistence_failure_surfaces_not_swallowed() {
        let sink = Arc::new(ChannelSink::default());
        let (bus, _handle) = openrt_telemetry::EventBusBuilder::new()
            .sink(sink.clone())
            .start();
        let dir = std::env::temp_dir().join(format!("openrt-orch-rsnp-{}", uuid::Uuid::new_v4()));
        let inner: Arc<dyn PersistenceStore> = Arc::new(
            openrt_persistence::LocalFileStore::open(&dir)
                .await
                .unwrap(),
        );
        let store: Arc<dyn PersistenceStore> = Arc::new(FailingRunsStore { inner });
        let bb = Arc::new(Blackboard::new());

        let mut agents = HashMap::new();
        let (k, a) = echo(SubAgentKind::Coder, 0);
        agents.insert(k, a);
        let mut engine = WorkflowEngine::new(
            agents,
            Arc::new(bus),
            store.clone(),
            bb.clone(),
            CancellationToken::new(),
            AgentId::new(),
        );
        let wf =
            Workflow::new("wf-rsnp").step(WorkflowStep::new("code", SubAgentKind::Coder, "impl"));

        let err = engine.run(&wf).await.unwrap_err();
        assert!(
            matches!(err, WorkflowError::Persistence(_)),
            "a failed run-report write must surface as an error, got {err:?}"
        );
    }

    /// Shared concurrency probe: records peak in-flight `run` calls.
    struct SharedProbe {
        cur: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
        delay: std::time::Duration,
    }

    impl SharedProbe {
        fn new(delay_ms: u64) -> Self {
            Self {
                cur: std::sync::atomic::AtomicUsize::new(0),
                peak: std::sync::atomic::AtomicUsize::new(0),
                delay: std::time::Duration::from_millis(delay_ms),
            }
        }
    }

    struct ProbedAgent {
        id: String,
        probe: Arc<SharedProbe>,
    }

    #[async_trait]
    impl Agent for ProbedAgent {
        fn id(&self) -> &str {
            &self.id
        }
        async fn run(
            &self,
            _ctx: &AgentContext,
            input: &str,
        ) -> Result<openrt_agents::AgentResult, openrt_agents::AgentError> {
            use std::sync::atomic::Ordering;
            let now = self.probe.cur.fetch_add(1, Ordering::SeqCst) + 1;
            self.probe.peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(self.probe.delay).await;
            self.probe.cur.fetch_sub(1, Ordering::SeqCst);
            Ok(openrt_agents::AgentResult {
                final_text: format!("done:{input}"),
                ended: AgentEndReason::Completed,
                turns: 1,
                tool_calls: 0,
                retries: 0,
                messages: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn max_concurrency_bounds_in_flight_width() {
        let probe = Arc::new(SharedProbe::new(50));
        let mut agents = HashMap::new();
        for kind in [
            SubAgentKind::Planner,
            SubAgentKind::Researcher,
            SubAgentKind::Coder,
            SubAgentKind::Verifier,
            SubAgentKind::Critic,
        ] {
            agents.insert(
                kind,
                Arc::new(ProbedAgent {
                    id: kind.to_string(),
                    probe: probe.clone(),
                }) as Arc<dyn Agent>,
            );
        }
        let mut h = harness(agents).await;
        h.engine = h.engine.with_max_concurrency(2);

        // Five independent root steps → unlimited would peak near 5;
        // max_concurrency=2 must keep peak <= 2.
        let wf = Workflow::new("wf-mc")
            .step(WorkflowStep::new("a", SubAgentKind::Planner, "a"))
            .step(WorkflowStep::new("b", SubAgentKind::Researcher, "b"))
            .step(WorkflowStep::new("c", SubAgentKind::Coder, "c"))
            .step(WorkflowStep::new("d", SubAgentKind::Verifier, "d"))
            .step(WorkflowStep::new("e", SubAgentKind::Critic, "e"));
        let report = h.engine.run(&wf).await.unwrap();
        assert_eq!(report.status, WorkflowStatus::Completed);
        assert_eq!(report.steps.len(), 5);
        let peak = probe.peak.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            peak <= 2,
            "peak in-flight {peak} exceeded max_concurrency 2"
        );
        assert!(peak >= 1, "no steps ran");
    }

    #[tokio::test]
    async fn admission_denial_with_idle_engine_fails_closed() {
        let mut agents = HashMap::new();
        let (k, a) = echo(SubAgentKind::Coder, 0);
        agents.insert(k, a);
        let mut h = harness(agents).await;
        h.engine = h.engine.with_resource_admission(Arc::new(|| {
            Err("host memory usage 91% exceeds limit 50%".to_string())
        }));
        let wf =
            Workflow::new("wf-adm").step(WorkflowStep::new("code", SubAgentKind::Coder, "impl"));
        let err = h.engine.run(&wf).await.unwrap_err();
        match err {
            WorkflowError::Condition(msg) => {
                assert!(msg.contains("resource admission denied"), "got: {msg}");
                assert!(msg.contains("memory"), "got: {msg}");
            }
            other => panic!("expected Condition admission error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn admission_allow_runs_to_completion() {
        let mut agents = HashMap::new();
        let (k, a) = echo(SubAgentKind::Coder, 0);
        agents.insert(k, a);
        let mut h = harness(agents).await;
        h.engine = h.engine.with_resource_admission(Arc::new(|| Ok(())));
        let wf =
            Workflow::new("wf-adm-ok").step(WorkflowStep::new("code", SubAgentKind::Coder, "impl"));
        let report = h.engine.run(&wf).await.unwrap();
        assert_eq!(report.status, WorkflowStatus::Completed);
    }

    #[tokio::test]
    async fn fine_grained_scheduler_starts_dependent_immediately_after_completion() {
        // A completes, then B (dependent) runs — no wave barrier needed for
        // correctness; with max_concurrency=1 this proves readiness re-eval.
        let mut agents = HashMap::new();
        let (k, a) = echo(SubAgentKind::Planner, 0);
        agents.insert(k, a);
        let (k, a) = echo(SubAgentKind::Coder, 0);
        agents.insert(k, a);
        let mut h = harness(agents).await;
        h.engine = h.engine.with_max_concurrency(1);
        let wf = Workflow::new("wf-seq1")
            .step(WorkflowStep::new("plan", SubAgentKind::Planner, "p"))
            .step(WorkflowStep::new("code", SubAgentKind::Coder, "c").dep("plan"));
        let report = h.engine.run(&wf).await.unwrap();
        assert_eq!(report.status, WorkflowStatus::Completed);
        let keys: Vec<_> = report.steps.iter().map(|s| s.step.as_str()).collect();
        assert_eq!(keys, vec!["plan", "code"]);
    }

    /// Diamond: A → (B,C,D) → E. B/C/D must overlap in time; E waits for all.
    #[tokio::test]
    async fn diamond_branches_run_concurrently_and_join_waits_for_all() {
        use std::sync::Mutex;
        use std::time::Instant;

        #[derive(Default)]
        struct Timeline {
            starts: Mutex<std::collections::HashMap<String, Instant>>,
            ends: Mutex<std::collections::HashMap<String, Instant>>,
        }

        struct TimelineAgent {
            id: String,
            tl: Arc<Timeline>,
            delay: std::time::Duration,
        }

        #[async_trait]
        impl Agent for TimelineAgent {
            fn id(&self) -> &str {
                &self.id
            }
            async fn run(
                &self,
                _ctx: &AgentContext,
                input: &str,
            ) -> Result<openrt_agents::AgentResult, openrt_agents::AgentError> {
                self.tl
                    .starts
                    .lock()
                    .unwrap()
                    .insert(input.to_string(), Instant::now());
                tokio::time::sleep(self.delay).await;
                self.tl
                    .ends
                    .lock()
                    .unwrap()
                    .insert(input.to_string(), Instant::now());
                Ok(openrt_agents::AgentResult {
                    final_text: format!("ok:{input}"),
                    ended: AgentEndReason::Completed,
                    turns: 1,
                    tool_calls: 0,
                    retries: 0,
                    messages: Vec::new(),
                })
            }
        }

        // Input to the agent is the step prompt; we embed the step id in the prompt.
        let tl = Arc::new(Timeline::default());
        let mut agents = HashMap::new();
        for kind in [
            SubAgentKind::Planner,
            SubAgentKind::Researcher,
            SubAgentKind::Coder,
            SubAgentKind::Verifier,
            SubAgentKind::Critic,
        ] {
            agents.insert(
                kind,
                Arc::new(TimelineAgent {
                    id: kind.to_string(),
                    tl: tl.clone(),
                    delay: std::time::Duration::from_millis(80),
                }) as Arc<dyn Agent>,
            );
        }
        let mut h = harness(agents).await;
        h.engine = h.engine.with_max_concurrency(0); // unlimited

        let wf = Workflow::new("wf-diamond")
            .step(WorkflowStep::new("a", SubAgentKind::Planner, "a"))
            .step(WorkflowStep::new("b", SubAgentKind::Researcher, "b").dep("a"))
            .step(WorkflowStep::new("c", SubAgentKind::Coder, "c").dep("a"))
            .step(WorkflowStep::new("d", SubAgentKind::Verifier, "d").dep("a"))
            .step(
                WorkflowStep::new("e", SubAgentKind::Critic, "e")
                    .dep("b")
                    .dep("c")
                    .dep("d"),
            );
        let t0 = Instant::now();
        let report = h.engine.run(&wf).await.unwrap();
        assert_eq!(report.status, WorkflowStatus::Completed);
        assert_eq!(report.steps.len(), 5);

        let starts = tl.starts.lock().unwrap().clone();
        let ends = tl.ends.lock().unwrap().clone();
        // Agent input is `{role_prompt}\n\nTask:\n{prompt}`; key by the
        // trailing prompt (equals the step id).
        let key_for = |id: &str| {
            starts
                .keys()
                .find(|k| k.ends_with(&format!("Task:\n{id}")))
                .cloned()
                .unwrap_or_else(|| panic!("missing start for {id} (keys: {starts:?})"))
        };
        let keys: Vec<String> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|id| key_for(id))
            .collect();
        for k in &keys {
            assert!(ends.contains_key(k), "missing end for {k}");
        }
        // A finished before any branch started.
        let a_end = ends[&keys[0]];
        for id in ["b", "c", "d"] {
            let k = key_for(id);
            assert!(
                starts[&k] >= a_end,
                "{id} started before a finished ({:?} vs {:?})",
                starts[&k],
                a_end
            );
        }
        // b/c/d overlapped pairwise at least once.
        let branches: Vec<String> = ["b", "c", "d"].iter().map(|id| key_for(id)).collect();
        let mut overlapped = false;
        for i in 0..3 {
            for j in (i + 1)..3 {
                let (x, y) = (&branches[i], &branches[j]);
                if starts[x] < ends[y] && starts[y] < ends[x] {
                    overlapped = true;
                }
            }
        }
        assert!(
            overlapped,
            "b/c/d never overlapped in time: {starts:?} {ends:?}"
        );
        // e started only after every branch ended.
        let e_key = key_for("e");
        for id in ["b", "c", "d"] {
            let k = key_for(id);
            assert!(
                starts[&e_key] >= ends[&k],
                "e started before {id} finished ({:?} vs {:?})",
                starts[&e_key],
                ends[&k]
            );
        }
        // Events / history preserved.
        h.bus.flush().await;
        let kinds: Vec<EventKind> = h.sink.snapshot().iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&EventKind::WorkflowStarted));
        assert!(kinds.contains(&EventKind::WorkflowCompleted));
        assert!(h
            .store
            .get_record(RUNS_NS, "wf-diamond")
            .await
            .unwrap()
            .is_some());
        let _ = t0;
    }

    // ---- Phase 8 (M8.9): workflow sandbox admission ----

    struct CountingSandbox {
        calls: std::sync::atomic::AtomicUsize,
        accept: bool,
    }

    #[async_trait::async_trait]
    impl StepSandbox for CountingSandbox {
        async fn admit_profile(&self, profile: &str) -> Result<(), String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.accept {
                assert_eq!(profile, "strict");
                Ok(())
            } else {
                Err(format!("`{profile}` backend unavailable"))
            }
        }
    }

    #[tokio::test]
    async fn sandbox_admission_denies_before_any_step_runs() {
        let mut agents = HashMap::new();
        let (k, a) = echo(SubAgentKind::Planner, 0);
        agents.insert(k, a);
        let mut h = harness(agents).await;
        h.engine = std::mem::replace(&mut h.engine, harness(HashMap::new()).await.engine)
            .with_sandbox_admission(Arc::new(CountingSandbox {
                calls: std::sync::atomic::AtomicUsize::new(0),
                accept: false,
            }));

        let wf = Workflow::new("wf-sbx-deny").step(
            WorkflowStep::new("plan", SubAgentKind::Planner, "make plan").sandboxed("strict"),
        );
        match h.engine.run(&wf).await {
            Err(WorkflowError::SandboxDenied { step, reason }) => {
                assert_eq!(step, "plan");
                assert!(reason.contains("backend unavailable"), "{reason}");
            }
            other => panic!("expected SandboxDenied, got {other:?}"),
        }
        // Fail closed: the denied run executed NO step.
        assert!(h.bb.get("output.plan").unwrap().is_none());
        h.bus.flush().await;
        let kinds: Vec<EventKind> = h.sink.snapshot().iter().map(|e| e.kind).collect();
        assert!(!kinds.contains(&EventKind::WorkflowCompleted));
    }

    #[tokio::test]
    async fn sandbox_admission_runs_when_accepted_and_skips_when_undeclared() {
        // (a) declared + accepted → run completes.
        let mut agents = HashMap::new();
        let (k, a) = echo(SubAgentKind::Planner, 0);
        agents.insert(k, a);
        let mut h = harness(agents).await;
        h.engine = std::mem::replace(&mut h.engine, harness(HashMap::new()).await.engine)
            .with_sandbox_admission(Arc::new(CountingSandbox {
                calls: std::sync::atomic::AtomicUsize::new(0),
                accept: true,
            }));
        let wf = Workflow::new("wf-sbx-ok").step(
            WorkflowStep::new("plan", SubAgentKind::Planner, "make plan").sandboxed("strict"),
        );
        let report = h.engine.run(&wf).await.unwrap();
        assert_eq!(report.status, WorkflowStatus::Completed);

        // (b) no declared sandbox → admission is not consulted at all.
        let mut agents = HashMap::new();
        let (k, a) = echo(SubAgentKind::Planner, 0);
        agents.insert(k, a);
        let mut h = harness(agents).await;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct NeverCalled(Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait::async_trait]
        impl StepSandbox for NeverCalled {
            async fn admit_profile(&self, _profile: &str) -> Result<(), String> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err("should not be consulted".into())
            }
        }
        h.engine = std::mem::replace(&mut h.engine, harness(HashMap::new()).await.engine)
            .with_sandbox_admission(Arc::new(NeverCalled(calls.clone())));
        let wf = Workflow::new("wf-sbx-undeclared").step(WorkflowStep::new(
            "plan",
            SubAgentKind::Planner,
            "make plan",
        ));
        let report = h.engine.run(&wf).await.unwrap();
        assert_eq!(report.status, WorkflowStatus::Completed);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn workflow_step_sandbox_field_serde_default() {
        // Old persisted workflows (no `sandbox` key) deserialize unchanged.
        let json = r#"{"id":"a","kind":"planner","prompt":"p"}"#;
        let step: WorkflowStep = serde_json::from_str(json).unwrap();
        assert!(step.sandbox.is_none());
        let with = WorkflowStep::new("a", SubAgentKind::Planner, "p").sandboxed("development");
        let round: WorkflowStep =
            serde_json::from_str(&serde_json::to_string(&with).unwrap()).unwrap();
        assert_eq!(round.sandbox.as_deref(), Some("development"));
    }
}
