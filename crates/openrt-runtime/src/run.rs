//! The execution pipeline: one task through provider → agent → gate → store.
//!
//! Hardening pass (Part A): [`Pipeline::run_supervised`] drives a task through
//! potentially MANY execution segments. A segment that hits its per-segment
//! budget no longer fails the task — the supervisor checkpoints the durable
//! state, compacts the transcript, and starts a fresh segment, until the task
//! completes, pauses, is cancelled, fails unrecoverably, or exhausts a GLOBAL
//! task budget ([`TaskExecutionBudget`]) — at which point it fails with the
//! explicit `global task execution budget exhausted` reason (never silently).

use std::sync::Arc;

use openrt_core::{
    now, AgentId, CancellationToken, DomainEvent, EventKind, ExecutionRecord, Message, MessageRole,
    PauseToken, RedactionPolicy, SegmentOutcome, Session, Task, TaskStatus, TurnKind,
};
use openrt_models::ModelProvider;
use openrt_persistence::PersistenceStore;
use openrt_telemetry::EventBus;

use crate::Error;

/// The legacy per-segment failure message — frozen; existing tests and older
/// callers assert it verbatim.
const LEGACY_LIMIT_MESSAGE: &str = "agent stopped at the configured iteration limit \
     (runtime.max_turns / runtime.max_tool_calls); \
     state is preserved — refine the task or raise the limits";

/// How a task run ended (input to downstream state machines).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineOutcome {
    Completed,
    Cancelled,
    Failed,
    /// Part C: ended cooperatively at a turn boundary on an operator pause
    /// request. The task persists as `Paused` (not terminal) with the tool
    /// rounds folded into the session, and resumes from there.
    Paused,
    /// The segment hit its per-segment budget. Legacy mode turns this into a
    /// `Failed` task; Supervised mode checkpoints and continues.
    LimitReached,
}

/// GLOBAL, task-lifetime execution ceilings enforced by the supervisor across
/// automatically-chained segments. `None` = no ceiling (an operator's explicit
/// relaxation; the default configuration ships finite values — config keys
/// `runtime.max_task_*`). Finite defaults mean the default configuration is
/// never an unbounded runaway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskExecutionBudget {
    pub max_task_turns: Option<u64>,
    pub max_task_tool_calls: Option<u64>,
    pub max_task_wall_time_ms: Option<u64>,
    pub max_task_iterations: Option<u64>,
}

impl TaskExecutionBudget {
    /// Finite, safe defaults — identical to the config defaults, so runtime
    /// code that never reads a config still has bounded behavior out of the
    /// box.
    pub fn default_finite() -> Self {
        Self {
            max_task_turns: Some(4096),
            max_task_tool_calls: Some(8192),
            max_task_wall_time_ms: Some(86_400_000),
            max_task_iterations: Some(128),
        }
    }

    /// No ceilings at all — only where an operator explicitly relaxed every
    /// `runtime.max_task_*` key.
    pub fn unbounded() -> Self {
        Self {
            max_task_turns: None,
            max_task_tool_calls: None,
            max_task_wall_time_ms: None,
            max_task_iterations: None,
        }
    }
}

impl From<&openrt_config::RuntimeConfig> for TaskExecutionBudget {
    fn from(cfg: &openrt_config::RuntimeConfig) -> Self {
        Self {
            max_task_turns: cfg.max_task_turns.map(u64::from),
            max_task_tool_calls: cfg.max_task_tool_calls.map(u64::from),
            max_task_wall_time_ms: cfg.max_task_wall_time,
            max_task_iterations: cfg.max_task_iterations,
        }
    }
}

/// Execution mode of [`Pipeline::run_inner`]: the frozen legacy single-segment
/// path, or the supervisor-driven multi-segment path.
#[derive(Clone)]
enum RunMode {
    Legacy,
    Supervised(TaskExecutionBudget),
}

/// Arguments needed to run an agent pipeline. `Clone` so a live
/// [`crate::TaskHandle`] can hand a snapshot to the worker while keeping
/// its own handle (Drop-controlled registry) intact.
#[derive(Clone)]
pub struct Pipeline {
    pub provider: Arc<dyn ModelProvider>,
    pub agent: Arc<dyn openrt_agents::Agent>,
    pub executor: Arc<dyn openrt_tools::ToolExecutor>,
    pub store: Arc<dyn PersistenceStore>,
    pub bus: EventBus,
    pub agent_id: AgentId,
}

impl Pipeline {
    /// Legacy single-segment execution. Behavior and message surface are
    /// frozen — tests and older callers depend on them: a segment hitting its
    /// per-segment budget FAILS the task with the preserved-state message.
    pub async fn run(
        &self,
        session: &Session,
        task: Task,
        prompt: &str,
        cancel: &CancellationToken,
        pause: Option<Arc<PauseToken>>,
    ) -> Result<Task, Error> {
        self.run_inner(session, task, prompt, cancel, pause, RunMode::Legacy)
            .await
    }

    /// Supervisor-driven multi-segment execution (hardening pass, Part A).
    ///
    /// A segment reaching its per-segment budget (`runtime.max_turns` /
    /// `runtime.max_tool_calls`) checkpoints the durable task + session,
    /// compacts the transcript, and CONTINUES with a fresh segment — the task
    /// keeps moving until it completes, is cancelled, pauses cooperatively at
    /// a boundary, fails unrecoverably, or exhausts a global
    /// [`TaskExecutionBudget`] (verified explicit failure, never silent). A
    /// resume of a durable `Paused` task flows through here the same way, so
    /// continuation never duplicates the user instruction in the transcript.
    pub async fn run_supervised(
        &self,
        session: &Session,
        task: Task,
        prompt: &str,
        cancel: &CancellationToken,
        pause: Option<Arc<PauseToken>>,
        budget: TaskExecutionBudget,
    ) -> Result<Task, Error> {
        self.run_inner(
            session,
            task,
            prompt,
            cancel,
            pause,
            RunMode::Supervised(budget),
        )
        .await
    }

    /// The shared core both paths use. `Legacy` reproduces the original
    /// single-segment behavior byte-for-byte; `Supervised` adds the segment
    /// ledger, the between-segment checkpoint/continuation loop, the durable
    /// fold rule (prompt never duplicated), and the global budget failure.
    async fn run_inner(
        &self,
        session: &Session,
        mut task: Task,
        prompt: &str,
        cancel: &CancellationToken,
        pause: Option<Arc<PauseToken>>,
        mode: RunMode,
    ) -> Result<Task, Error> {
        let sid = session.id;
        let tid = task.id;
        let aid = self.agent_id;

        // Already-cancelled before we started: terminal immediately.
        if cancel.is_cancelled() {
            task.agent_id = Some(aid);
            task.transition(TaskStatus::Cancelled)?;
            self.store.save_task(&task).await?;
            return Ok(task);
        }

        let emit = |kind: EventKind, data: serde_json::Value| {
            let ev = DomainEvent::new(kind)
                .with_session(sid)
                .with_task(tid)
                .with_data(data);
            self.bus.emit(ev)
        };

        // Durable + event: TaskCreated.
        self.store.save_task(&task).await?;
        emit(
            EventKind::TaskCreated,
            serde_json::json!({ "prompt": prompt }),
        )
        .await;

        task.agent_id = Some(aid);
        task.transition(TaskStatus::Running)?;
        self.store.save_task(&task).await?;
        emit(
            EventKind::TaskStarted,
            serde_json::json!({ "agent_id": aid.to_string() }),
        )
        .await;

        let mut exec = ExecutionRecord::new(sid, tid, Some(aid));
        exec.status = TaskStatus::Running;
        exec.started_at = task.started_at;
        self.store.save_execution(&exec).await?;

        let supervised = matches!(&mode, RunMode::Supervised(_));
        let mut session = session.clone();
        // Legacy-durable resume compatibility: an OLD durable `Paused` task
        // (pre-hardening — no `prompt_persisted` flag) already carries its
        // prompt as the session head; mark it persisted so no code path ever
        // folds the user instruction a second time.
        if !task.prompt_persisted
            && session
                .content
                .first()
                .map(|m| m.content == prompt)
                .unwrap_or(false)
        {
            task.prompt_persisted = true;
        }

        // Per-segment working state.
        let mut segment_prompt = prompt.to_string();
        let mut transcript: Vec<Message> = Vec::new();
        let mut result_turns: u32 = 0;
        let mut result_tool_calls: u32 = 0;
        let mut result_retries: u32 = 0;

        loop {
            if cancel.is_cancelled() {
                task.agent_id.get_or_insert(aid);
                task.transition(TaskStatus::Cancelled)?;
                self.store.save_task(&task).await?;
                return Ok(task);
            }

            // Open this segment's durable ledger record (supervised only).
            if supervised {
                task.begin_segment();
            }

            emit(
                EventKind::AgentStarted,
                serde_json::json!({ "agent_id": aid.to_string() }),
            )
            .await;

            let agent_ctx = openrt_agents::AgentContext::new(sid, tid)
                .with_history(session.content.clone())
                .with_cancel(cancel.clone());
            let agent_ctx = match pause.as_ref() {
                Some(p) => agent_ctx.with_pause(p.clone()),
                None => agent_ctx,
            };

            // Race agent work against cancellation so a non-cooperative
            // provider still yields a clean Cancelled task. `biased` makes an
            // already-cancelled token win deterministically.
            let outcome = tokio::select! {
                biased;
                _ = cancel.future() => PipelineOutcome::Cancelled,
                result = self.agent.run(&agent_ctx, &segment_prompt) => match result {
                    Ok(res) => {
                        transcript = res.messages;
                        result_turns = res.turns;
                        result_tool_calls = res.tool_calls;
                        result_retries = res.retries;
                        match res.ended {
                            openrt_agents::AgentEndReason::Cancelled => PipelineOutcome::Cancelled,
                            openrt_agents::AgentEndReason::Paused => PipelineOutcome::Paused,
                            openrt_agents::AgentEndReason::LimitReached => {
                                match &mode {
                                    RunMode::Legacy => {
                                        task.error = Some(LEGACY_LIMIT_MESSAGE.to_string());
                                        PipelineOutcome::Failed
                                    }
                                    RunMode::Supervised(_) => PipelineOutcome::LimitReached,
                                }
                            }
                            openrt_agents::AgentEndReason::Completed => {
                                task.record_turn(
                                    TurnKind::FinalOutput,
                                    Some(serde_json::json!({
                                        "turns": res.turns,
                                        "final_text": res.final_text,
                                    })),
                                );
                                task.result = Some(res.final_text);
                                PipelineOutcome::Completed
                            }
                        }
                    }
                    Err(e) if matches!(&e, openrt_agents::AgentError::Cancelled) => {
                        PipelineOutcome::Cancelled
                    }
                    Err(e) => {
                        // Security (PART 28): the error crosses the persistence
                        // boundary — run it through the SAME redaction policy
                        // as content so a provider error body can never store
                        // a secret.
                        task.error = Some(RedactionPolicy::default().redact_text(&e.to_string()));
                        PipelineOutcome::Failed
                    }
                }
            };

            emit(
                EventKind::AgentCompleted,
                serde_json::json!({ "agent_id": aid.to_string(), "outcome": format!("{outcome:?}") }),
            )
            .await;

            // Close this segment's ledger record from the honest per-segment
            // counters BEFORE any fold/transition decision reads `cumulative()`.
            if supervised {
                task.push_segment_counts(result_turns, result_tool_calls, result_retries);
            }

            match outcome {
                PipelineOutcome::Completed => {
                    if supervised {
                        // A segment that finishes yet pushes the WHOLE task
                        // past a global ceiling is still a ceiling violation:
                        // fail explicitly, and never claim a result.
                        if let Some(reason) = global_budget_reason(&task, &mode) {
                            task.error = Some(reason);
                            task.result = None;
                            task.turns.retain(|t| t.kind != TurnKind::FinalOutput);
                            task.end_segment(SegmentOutcome::Failed, task.error.clone());
                            task.transition(TaskStatus::Failed)?;
                            emit(
                                EventKind::TaskFailed,
                                serde_json::json!({ "reason": "global_budget", "error": task.error }),
                            )
                            .await;
                            self.fold_transcript(
                                &mut task,
                                &mut session,
                                prompt,
                                false,
                                &transcript,
                                true,
                            );
                            break;
                        }
                        task.end_segment(SegmentOutcome::Completed, None);
                    }
                    task.transition(TaskStatus::Completed)?;
                    emit(
                        EventKind::TaskCompleted,
                        serde_json::json!({
                            "result": task.result.clone(),
                            "turns": task.turns.len(),
                        }),
                    )
                    .await;
                    self.fold_transcript(
                        &mut task,
                        &mut session,
                        prompt,
                        outcome == PipelineOutcome::Paused,
                        &transcript,
                        true,
                    );
                    break;
                }
                PipelineOutcome::Cancelled => {
                    if supervised {
                        task.end_segment(SegmentOutcome::Cancelled, task.error.clone());
                    }
                    task.transition(TaskStatus::Cancelled)?;
                    emit(
                        EventKind::TaskFailed,
                        serde_json::json!({ "reason": "cancelled", "error": task.error }),
                    )
                    .await;
                    self.fold_transcript(
                        &mut task,
                        &mut session,
                        prompt,
                        outcome == PipelineOutcome::Paused,
                        &transcript,
                        true,
                    );
                    break;
                }
                PipelineOutcome::Failed => {
                    if supervised {
                        task.end_segment(SegmentOutcome::Failed, task.error.clone());
                    }
                    task.transition(TaskStatus::Failed)?;
                    emit(
                        EventKind::TaskFailed,
                        serde_json::json!({ "reason": "error", "error": task.error }),
                    )
                    .await;
                    self.fold_transcript(
                        &mut task,
                        &mut session,
                        prompt,
                        outcome == PipelineOutcome::Paused,
                        &transcript,
                        true,
                    );
                    break;
                }
                // Part C: cooperative pause at a turn boundary. The task
                // persists as `Paused` (not terminal, no completion
                // timestamp) and resumes via `task_recover`/`run_supervised`.
                PipelineOutcome::Paused => {
                    if supervised {
                        task.end_segment(SegmentOutcome::Paused, None);
                    }
                    task.transition(TaskStatus::Paused)?;
                    emit(
                        EventKind::TaskPaused,
                        serde_json::json!({
                            "reason": "operator_pause",
                            "turns": task.turns.len(),
                            "error": task.error,
                        }),
                    )
                    .await;
                    self.fold_transcript(
                        &mut task,
                        &mut session,
                        prompt,
                        outcome == PipelineOutcome::Paused,
                        &transcript,
                        true,
                    );
                    break;
                }
                PipelineOutcome::LimitReached => {
                    let RunMode::Supervised(_) = &mode else {
                        unreachable!("Legacy maps LimitReached to Failed earlier");
                    };
                    task.end_segment(SegmentOutcome::Continued, None);
                    // Checkpoint this segment before continuing: the durable
                    // task + session reflect the work up to this boundary.
                    self.fold_transcript(
                        &mut task,
                        &mut session,
                        prompt,
                        false,
                        &transcript,
                        false,
                    );
                    self.store.update_session(&session).await?;
                    self.store.save_task(&task).await?;
                    if let Some(reason) = global_budget_reason(&task, &mode) {
                        task.error = Some(reason);
                        task.transition(TaskStatus::Failed)?;
                        emit(
                            EventKind::TaskFailed,
                            serde_json::json!({ "reason": "global_budget", "error": task.error }),
                        )
                        .await;
                        // Failure marker + final persistence are handled by
                        // the loop-exit closeout below.
                        break;
                    }
                    // Continue with a fresh segment over the evolved session.
                    segment_prompt = format!(
                        "Continue the task from the transcript above. The previous segment \
                         reached its per-segment execution budget after {result_turns} turns and \
                         {result_tool_calls} tool calls (retries: {result_retries}). Drive this \
                         entire task to a real final answer now — do not restart or duplicate work \
                         already present in the transcript."
                    );
                    continue;
                }
            }
        }

        // Terminal closeout for every non-continuation path: failure marker
        // (Failed/Cancelled), exec closeout, persist.
        self.closeout(&mut task, &mut session, &mut exec).await?;
        Ok(task)
    }

    /// Fold the agent transcript into the durable session. `paused_fold`
    /// mirrors the Part C rule (a paused run must not double the instruction —
    /// resume hands the same prompt). Otherwise the durable rule applies: the
    /// FULL transcript folds exactly once (when the prompt was not yet
    /// persisted), and every later segment folds `[1..]` so the user
    /// instruction is never duplicated. `terminal_fold` drives the final
    /// closeout (assistant result appended); `false` is the between-segment
    /// checkpoint path (no markers, no result).
    fn fold_transcript(
        &self,
        task: &mut Task,
        session: &mut Session,
        prompt: &str,
        paused_fold: bool,
        transcript: &[Message],
        terminal_fold: bool,
    ) {
        let fold: &[Message] = if paused_fold {
            transcript.get(1..).unwrap_or(&[])
        } else if !task.prompt_persisted {
            task.prompt_persisted = true;
            &transcript[..]
        } else {
            transcript.get(1..).unwrap_or(&[])
        };
        if fold.is_empty() && transcript.is_empty() && !task.prompt_persisted {
            // Bare-input fallback for agents with no transcript (only when the
            // input is genuinely not yet in the session).
            session.push(Message::user(prompt));
        } else if !fold.is_empty() {
            session.content.extend_from_slice(fold);
        }
        if terminal_fold {
            if let Some(result) = task.result.as_deref() {
                if !result.is_empty() {
                    session.push(Message::assistant(result));
                }
            }
        }
    }

    /// Append the durable failure marker for Failed/Cancelled terminal folds.
    fn fold_on_failure(&self, task: &Task, session: &mut Session) {
        session.push(Message {
            role: MessageRole::Assistant,
            content: format!(
                "[task {:?}: {}]",
                task.status,
                task.error.as_deref().unwrap_or("")
            ),
            tool_calls: Vec::new(),
            tool_call_id: None,
            at: None,
        });
    }

    /// Persist the final session + execution record + task for a finished run.
    async fn closeout(
        &self,
        task: &mut Task,
        session: &mut Session,
        exec: &mut ExecutionRecord,
    ) -> Result<(), Error> {
        if task.status == TaskStatus::Failed || task.status == TaskStatus::Cancelled {
            self.fold_on_failure(task, session);
        }
        self.store.update_session(session).await?;
        exec.status = task.status;
        exec.completed_at = task.completed_at;
        exec.result = task.result.clone();
        exec.error = task.error.clone();
        exec.turn_count = task.turns.len() as u32;
        self.store.save_execution(exec).await?;
        self.store.save_task(task).await?;
        Ok(())
    }
}

/// Returns the explicit global-budget exhaustion message when any configured
/// ceiling is met (`Option::None` otherwise). Evaluated from the task's
/// durable cumulative ledger, so it never fabricates a limit.
fn global_budget_reason(task: &Task, mode: &RunMode) -> Option<String> {
    let RunMode::Supervised(budget) = mode else {
        return None;
    };
    let cum = task.cumulative();
    if let Some(limit) = budget.max_task_turns {
        if cum.turns >= limit {
            return Some(format!(
                "global task execution budget exhausted: cumulative {cum_turns} turns across \
                 {segments} task segment(s), global turn ceiling {limit}; state is preserved",
                cum_turns = cum.turns,
                segments = cum.segments
            ));
        }
    }
    if let Some(limit) = budget.max_task_tool_calls {
        if cum.tool_calls >= limit {
            return Some(format!(
                "global task execution budget exhausted: cumulative {calls} tool calls across \
                 {segments} task segment(s), global tool-call ceiling {limit}; state is preserved",
                calls = cum.tool_calls,
                segments = cum.segments
            ));
        }
    }
    if let Some(limit) = budget.max_task_iterations {
        if cum.segments >= limit {
            return Some(format!(
                "global task execution budget exhausted: {segments} task segment(s) reached, \
                 automatic-continuation ceiling {limit}; state is preserved",
                segments = cum.segments
            ));
        }
    }
    if let Some(limit) = budget.max_task_wall_time_ms {
        let elapsed = task.elapsed_ms(now());
        if elapsed >= limit {
            return Some(format!(
                "global task execution budget exhausted: task ran {elapsed} ms, global \
                 wall-clock ceiling {limit} ms; state is preserved"
            ));
        }
    }
    None
}

/// Wrapper kept API-stable for the runtime module (avoids re-export churn).
pub async fn run_pipeline(
    pipeline: &Pipeline,
    session: &Session,
    task: Task,
    prompt: &str,
    cancel: &CancellationToken,
) -> Result<Task, Error> {
    pipeline.run(session, task, prompt, cancel, None).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_models::MockProvider;
    use openrt_persistence::MemoryStore;
    use openrt_tools::{DirectExecutor, ToolRegistry};

    async fn pipeline() -> (Pipeline, Session) {
        let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
        let registry = ToolRegistry::builtins();
        let tools = registry.provider_tools();
        let executor: Arc<dyn openrt_tools::ToolExecutor> = Arc::new(DirectExecutor::new(registry));
        let provider: Arc<dyn ModelProvider> = Arc::new(MockProvider::tool_sequence("mock"));
        let agent: Arc<dyn openrt_agents::Agent> =
            Arc::new(openrt_agents::MinimalAgent::with_default_limits(
                "minimal",
                provider.clone(),
                executor.clone(),
                tools,
            ));
        let (bus, _handle) = openrt_telemetry::EventBusBuilder::new().start();
        let session = Session::new("test");
        store.create_session(&session).await.unwrap();
        (
            Pipeline {
                provider,
                agent,
                executor,
                store,
                bus,
                agent_id: AgentId::new(),
            },
            session,
        )
    }

    #[tokio::test]
    async fn completes_task_and_persists() {
        let (p, session) = pipeline().await;
        let task = Task::new(session.id, "echo hello world");
        let cancel = CancellationToken::new();
        let done = p
            .run(&session, task, "echo hello world", &cancel, None)
            .await
            .unwrap();
        assert_eq!(done.status, TaskStatus::Completed);
        assert!(done.result.as_deref().unwrap_or_default().contains("hello"));
        assert!(done
            .turns
            .iter()
            .any(|t| t.kind == openrt_core::TurnKind::FinalOutput));

        // Persisted: task flowed through transitions; session transcript updated.
        let reloaded = p.store.load_task(done.id).await.unwrap();
        assert_eq!(reloaded.status, TaskStatus::Completed);
        let updated = p.store.load_session(session.id).await.unwrap();
        // user + assistant tool-call turn + tool result + final answer
        assert_eq!(updated.content.len(), 4);

        // Execution record present and closed out.
        let execs = p.store.list_tasks(Some(session.id)).await.unwrap();
        assert_eq!(execs.len(), 1);
    }

    #[tokio::test]
    async fn cancellation_produces_cancelled_task() {
        // A provider that sleeps 200ms per generation so the cancel lands mid-run.
        use openrt_models::mock::{MockMode, MockProvider};
        use openrt_tools::ToolRegistry;
        let registry = ToolRegistry::builtins();
        let tools = registry.provider_tools();
        let executor: Arc<dyn openrt_tools::ToolExecutor> = Arc::new(DirectExecutor::new(registry));
        let provider: Arc<dyn ModelProvider> = Arc::new(MockProvider::new("mock", MockMode::Slow));
        let agent: Arc<dyn openrt_agents::Agent> =
            Arc::new(openrt_agents::MinimalAgent::with_default_limits(
                "minimal",
                provider.clone(),
                executor.clone(),
                tools,
            ));
        let (bus, _handle) = openrt_telemetry::EventBusBuilder::new().start();
        let p = super::Pipeline {
            provider,
            agent,
            executor,
            store: Arc::new(MemoryStore::new()),
            bus,
            agent_id: AgentId::new(),
        };
        let session = Session::new("cancel");
        p.store.create_session(&session).await.unwrap();
        let task = Task::new(session.id, "slow work");
        let cancel = CancellationToken::new();
        let handle = tokio::spawn({
            let p = p;
            let session = session.clone();
            let cancel = cancel.clone();
            async move {
                p.run(&session, task, "slow work", &cancel, None)
                    .await
                    .unwrap()
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cancel.cancel();
        let done = handle.await.unwrap();
        assert_eq!(done.status, TaskStatus::Cancelled);
    }

    /// PART 26: a provider/model failure leaves coherent state — the task is
    /// FAILED (never Completed with a fabricated result), the session
    /// transcript records the failure honestly, and the session stays usable
    /// for the next task.
    #[tokio::test]
    async fn provider_failure_fails_task_and_session_stays_usable() {
        use openrt_models::mock::{MockMode, MockProvider};
        use openrt_tools::ToolRegistry;
        let registry = ToolRegistry::builtins();
        let tools = registry.provider_tools();
        let executor: Arc<dyn openrt_tools::ToolExecutor> = Arc::new(DirectExecutor::new(registry));
        let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
        let (bus, _handle) = openrt_telemetry::EventBusBuilder::new().start();

        // Failing provider: the agent surfaces a structured model error.
        let provider: Arc<dyn ModelProvider> = Arc::new(MockProvider::new("mock", MockMode::Fail));
        let agent: Arc<dyn openrt_agents::Agent> =
            Arc::new(openrt_agents::MinimalAgent::with_default_limits(
                "minimal",
                provider.clone(),
                executor.clone(),
                tools.clone(),
            ));
        let p = super::Pipeline {
            provider,
            agent,
            executor: executor.clone(),
            store: store.clone(),
            bus: bus.clone(),
            agent_id: AgentId::new(),
        };
        let session = Session::new("coherence");
        store.create_session(&session).await.unwrap();

        let task = Task::new(session.id, "will fail");
        let cancel = CancellationToken::new();
        let done = p
            .run(&session, task, "will fail", &cancel, None)
            .await
            .unwrap();

        // 1. FAILED, with the structured error — never a fake completion.
        assert_eq!(done.status, TaskStatus::Failed, "{done:?}");
        assert!(done.result.is_none(), "no fabricated result: {done:?}");
        let err = done.error.as_deref().unwrap_or_default();
        assert!(err.contains("model provider error"), "error: {err}");

        // 2. Persisted: the reload shows the same failure (no divergence).
        let reloaded = store.load_task(done.id).await.unwrap();
        assert_eq!(reloaded.status, TaskStatus::Failed);
        assert_eq!(reloaded.error, done.error);

        // 3. Session transcript records the failure marker and persists.
        let updated = store.load_session(session.id).await.unwrap();
        let marker = updated
            .content
            .iter()
            .any(|m| m.content.contains("[task Failed:"));
        assert!(marker, "failure marker missing: {updated:?}");

        // 4. The session stays usable: a working provider on the SAME
        //    session completes the next task.
        let good: Arc<dyn ModelProvider> = Arc::new(MockProvider::tool_sequence("mock"));
        let agent2: Arc<dyn openrt_agents::Agent> =
            Arc::new(openrt_agents::MinimalAgent::with_default_limits(
                "minimal",
                good.clone(),
                executor.clone(),
                tools,
            ));
        let p2 = super::Pipeline {
            provider: good,
            agent: agent2,
            executor,
            store: store.clone(),
            bus,
            agent_id: AgentId::new(),
        };
        let session2 = store.load_session(session.id).await.unwrap();
        let task2 = Task::new(session.id, "next");
        let done2 = p2
            .run(&session2, task2, "next", &cancel, None)
            .await
            .unwrap();
        assert_eq!(done2.status, TaskStatus::Completed, "{done2:?}");
    }

    /// PART 26+: empty provider response produces a structured failure and
    /// the session remains usable (no fabricated completion).
    #[tokio::test]
    async fn empty_response_failure_fails_task_and_session_stays_usable() {
        use openrt_models::mock::{MockMode, MockProvider};
        use openrt_tools::ToolRegistry;
        let registry = ToolRegistry::builtins();
        let tools = registry.provider_tools();
        let executor: Arc<dyn openrt_tools::ToolExecutor> = Arc::new(DirectExecutor::new(registry));
        let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
        let (bus, _handle) = openrt_telemetry::EventBusBuilder::new().start();

        let provider: Arc<dyn ModelProvider> = Arc::new(MockProvider::new("mock", MockMode::Empty));
        let agent: Arc<dyn openrt_agents::Agent> =
            Arc::new(openrt_agents::MinimalAgent::with_default_limits(
                "minimal",
                provider.clone(),
                executor.clone(),
                tools.clone(),
            ));
        let p = super::Pipeline {
            provider,
            agent,
            executor: executor.clone(),
            store: store.clone(),
            bus: bus.clone(),
            agent_id: AgentId::new(),
        };
        let session = Session::new("empty");
        store.create_session(&session).await.unwrap();

        let task = Task::new(session.id, "will fail");
        let cancel = CancellationToken::new();
        let done = p
            .run(&session, task, "will fail", &cancel, None)
            .await
            .unwrap();

        assert_eq!(done.status, TaskStatus::Failed, "{done:?}");
        assert!(done.result.is_none(), "no fabricated result: {done:?}");
        let err = done.error.as_deref().unwrap_or_default();
        assert!(
            err.contains("MODEL_TOOL_CALL_PARSE_ERROR") || err.contains("model provider error"),
            "error: {err}"
        );

        let reloaded = store.load_task(done.id).await.unwrap();
        assert_eq!(reloaded.status, TaskStatus::Failed);
        assert_eq!(reloaded.error, done.error);

        let updated = store.load_session(session.id).await.unwrap();
        let marker = updated
            .content
            .iter()
            .any(|m| m.content.contains("[task Failed:"));
        assert!(marker, "failure marker missing: {updated:?}");

        // Session still usable
        let good: Arc<dyn ModelProvider> = Arc::new(MockProvider::tool_sequence("mock"));
        let agent2: Arc<dyn openrt_agents::Agent> =
            Arc::new(openrt_agents::MinimalAgent::with_default_limits(
                "minimal",
                good.clone(),
                executor.clone(),
                tools,
            ));
        let p2 = super::Pipeline {
            provider: good,
            agent: agent2,
            executor,
            store: store.clone(),
            bus,
            agent_id: AgentId::new(),
        };
        let session2 = store.load_session(session.id).await.unwrap();
        let task2 = Task::new(session.id, "next");
        let done2 = p2
            .run(&session2, task2, "next", &cancel, None)
            .await
            .unwrap();
        assert_eq!(done2.status, TaskStatus::Completed, "{done2:?}");
    }

    /// PART 28: a provider error that embeds a secret-shaped value is
    /// REDACTED before it crosses the persistence boundary (task.error →
    /// store + execution record + transcript marker).
    #[tokio::test]
    async fn provider_error_is_redacted_before_persistence() {
        use openrt_models::{ModelError, ModelRequest, ModelResponse};
        use openrt_tools::ToolRegistry;

        struct SecretProvider;
        #[async_trait::async_trait]
        impl ModelProvider for SecretProvider {
            fn id(&self) -> &str {
                "secret"
            }
            fn info(&self) -> openrt_models::ModelInfo {
                openrt_models::ModelInfo::new("secret", openrt_models::ProviderKind::Mock, None)
            }
            fn capabilities(&self) -> openrt_models::ModelCapabilities {
                openrt_models::ModelCapabilities {
                    supports_tools: false,
                    supports_streaming: false,
                    context_window: Some(4096),
                }
            }
            async fn generate(&self, _req: ModelRequest) -> Result<ModelResponse, ModelError> {
                Err(ModelError::Http {
                    provider: "secret".into(),
                    status: 500,
                    body: "auth failed: Authorization: Bearer supersecretvalue123".into(),
                })
            }
        }

        let registry = ToolRegistry::builtins();
        let tools = registry.provider_tools();
        let executor: Arc<dyn openrt_tools::ToolExecutor> = Arc::new(DirectExecutor::new(registry));
        let provider: Arc<dyn ModelProvider> = Arc::new(SecretProvider);
        let agent: Arc<dyn openrt_agents::Agent> =
            Arc::new(openrt_agents::MinimalAgent::with_default_limits(
                "minimal",
                provider.clone(),
                executor.clone(),
                tools,
            ));
        let (bus, _handle) = openrt_telemetry::EventBusBuilder::new().start();
        let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
        let p = super::Pipeline {
            provider,
            agent,
            executor,
            store: store.clone(),
            bus,
            agent_id: AgentId::new(),
        };
        let session = Session::new("sec");
        store.create_session(&session).await.unwrap();
        let task = Task::new(session.id, "fail");
        let done = p
            .run(&session, task, "fail", &CancellationToken::new(), None)
            .await
            .unwrap();
        assert_eq!(done.status, TaskStatus::Failed);
        let err = done.error.as_deref().unwrap_or_default();
        assert!(
            !err.contains("supersecretvalue123"),
            "secret leaked into task.error: {err}"
        );
        assert!(err.contains("[REDACTED]"), "redaction marker: {err}");

        // And it stays redacted after the round-trip through the store.
        let reloaded = store.load_task(done.id).await.unwrap();
        let re_err = reloaded.error.as_deref().unwrap_or_default();
        assert!(!re_err.contains("supersecretvalue123"), "{re_err}");
        let updated = store.load_session(session.id).await.unwrap();
        for m in &updated.content {
            assert!(
                !m.content.contains("supersecretvalue123"),
                "secret in transcript: {}",
                m.content
            );
        }
    }

    // ---- hardening pass (Part C): per-segment lifecycle events ----------

    /// Every supervised segment emits exactly one `AgentStarted`/`AgentCompleted`
    /// pair, correlated to the task + session, strictly interleaved in segment
    /// order (start1 < complete1 < start2 < complete2 < …). This is the event
    /// contract the TUI renders (task details pane + history).
    #[tokio::test]
    async fn segment_events_emit_started_completed_per_segment() {
        use openrt_telemetry::ChannelSink;
        let sink = Arc::new(ChannelSink::new());
        let store: Arc<MemoryStore> = Arc::new(MemoryStore::new());
        let registry = ToolRegistry::builtins();
        let tools = registry.provider_tools();
        let executor: Arc<dyn openrt_tools::ToolExecutor> = Arc::new(DirectExecutor::new(registry));
        let provider: Arc<dyn ModelProvider> = Arc::new(
            openrt_models::mock::MockProvider::tool_sequence("mock").with_tool_script(vec![
                ("echo", serde_json::json!({ "text": "step-1" })),
                ("echo", serde_json::json!({ "text": "step-2" })),
            ]),
        );
        let agent: Arc<dyn openrt_agents::Agent> = Arc::new(openrt_agents::MinimalAgent::new(
            "minimal",
            provider.clone(),
            executor.clone(),
            openrt_agents::AgentTurnLimits {
                max_turns: 2,
                max_tool_calls: 2,
                ..openrt_agents::AgentTurnLimits::default()
            },
            tools,
        ));
        let (bus, _handle) = openrt_telemetry::EventBusBuilder::new()
            .sink(sink.clone())
            .start();
        let session = Session::new("ev");
        store.create_session(&session).await.unwrap();
        let p = super::Pipeline {
            provider,
            agent,
            executor,
            store: store.clone(),
            bus,
            agent_id: AgentId::new(),
        };
        let task = Task::new(session.id, "eventful");
        let done = p
            .run_supervised(
                &session,
                task,
                "eventful",
                &CancellationToken::new(),
                None,
                super::TaskExecutionBudget::default_finite(),
            )
            .await
            .unwrap();
        assert_eq!(done.status, TaskStatus::Completed, "{done:?}");
        // The bus dispatcher writes to the sink asynchronously; drain a short
        // bounded poll instead of flushing the moved bus.
        use std::time::Duration;
        for _ in 0..50 {
            let n = sink
                .snapshot()
                .iter()
                .filter(|e| e.kind == openrt_core::EventKind::AgentCompleted)
                .count();
            if n >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let snaps = sink.snapshot();
        use openrt_core::EventKind;
        let started: Vec<_> = snaps
            .iter()
            .filter(|e| e.kind == EventKind::AgentStarted)
            .collect();
        let completed: Vec<_> = snaps
            .iter()
            .filter(|e| e.kind == EventKind::AgentCompleted)
            .collect();
        assert_eq!(started.len(), 2, "{started:?}");
        assert_eq!(completed.len(), 2, "{completed:?}");

        // Every AgentStarted/AgentCompleted is correlated to THIS task+session.
        for e in started.iter().chain(completed.iter()) {
            assert_eq!(e.session_id, Some(session.id), "{e:?}");
            assert_eq!(e.task_id, Some(done.id), "{e:?}");
        }

        // Strict per-segment interleaving: s1 < c1 < s2 < c2.
        let all: Vec<_> = snaps
            .iter()
            .filter(|e| e.kind == EventKind::AgentStarted || e.kind == EventKind::AgentCompleted)
            .collect();
        let kind_of = |e: &&openrt_core::DomainEvent| {
            if e.kind == EventKind::AgentStarted {
                'S'
            } else {
                'C'
            }
        };
        let pattern: String = all.iter().map(kind_of).collect();
        assert_eq!(
            pattern, "SCSC",
            "expected per-segment start/completed pairs, got {pattern}"
        );

        // And exactly one TaskCompleted at the end, same correlation.
        let finished: Vec<_> = snaps
            .iter()
            .filter(|e| e.kind == EventKind::TaskCompleted)
            .collect();
        assert_eq!(finished.len(), 1, "{finished:?}");
        assert_eq!(finished[0].task_id, Some(done.id));
    }

    //
    // A pool of scripted-tool providers that run out of script entries after a
    // few tool calls, so a constrained segment budget forces the supervisor to
    // continue — the ONLY way these tests observe chained segments.

    /// Scripted provider helper: `script` tool-call turns then a final text
    /// answer. Returns (pipeline pieces, store) wired to a constrained agent
    /// (max_turns = 2, max_tool_calls = 2 per segment).
    async fn supervised_pipeline(
        script: Vec<(&str, serde_json::Value)>,
    ) -> (super::Pipeline, Session, Arc<MemoryStore>) {
        use openrt_models::mock::MockProvider;
        let store: Arc<MemoryStore> = Arc::new(MemoryStore::new());
        let registry = ToolRegistry::builtins();
        let tools = registry.provider_tools();
        let executor: Arc<dyn openrt_tools::ToolExecutor> = Arc::new(DirectExecutor::new(registry));
        let provider: Arc<dyn ModelProvider> = Arc::new(
            MockProvider::tool_sequence("mock").with_tool_script(
                script
                    .into_iter()
                    .map(|(n, a)| (n.to_string(), a))
                    .collect::<Vec<_>>(),
            ),
        );
        let agent: Arc<dyn openrt_agents::Agent> = Arc::new(openrt_agents::MinimalAgent::new(
            "minimal",
            provider.clone(),
            executor.clone(),
            openrt_agents::AgentTurnLimits {
                max_turns: 2,
                max_tool_calls: 2,
                ..openrt_agents::AgentTurnLimits::default()
            },
            tools,
        ));
        let (bus, _handle) = openrt_telemetry::EventBusBuilder::new().start();
        let session = Session::new("supervised");
        store.create_session(&session).await.unwrap();
        (
            super::Pipeline {
                provider,
                agent,
                executor,
                store: store.clone(),
                bus,
                agent_id: AgentId::new(),
            },
            session,
            store,
        )
    }

    fn two_echo_script() -> Vec<(&'static str, serde_json::Value)> {
        vec![
            ("echo", serde_json::json!({ "text": "step-1" })),
            ("echo", serde_json::json!({ "text": "step-2" })),
        ]
    }

    /// A segment reaching its per-segment budget checkpoints and CONTINUES;
    /// the next segment drives the task to completion. The durable session
    /// never duplicates the user instruction, and the segment ledger records
    /// honest counters in order.
    #[tokio::test]
    async fn supervisor_continues_multisegment_and_completes() {
        let (p, session, store) = supervised_pipeline(two_echo_script()).await;
        let task = Task::new(session.id, "multi-segment work");
        let done = p
            .run_supervised(
                &session,
                task,
                "multi-segment work",
                &CancellationToken::new(),
                None,
                super::TaskExecutionBudget::default_finite(),
            )
            .await
            .unwrap();

        // Completed through TWO segments, with the final answer persisted.
        assert_eq!(done.status, TaskStatus::Completed, "{done:?}");
        assert!(done
            .result
            .as_deref()
            .unwrap_or_default()
            .contains("mock final"));

        // Durable segment ledger: segment 1 Continued (2 turns / 2 tool
        // calls), segment 2 Completed.
        assert_eq!(done.segments.len(), 2, "{:?}", done.segments);
        assert_eq!(done.segments[0].no, 1);
        assert_eq!(
            done.segments[0].outcome,
            openrt_core::SegmentOutcome::Continued
        );
        assert_eq!(done.segments[0].turns, 2);
        assert_eq!(done.segments[0].tool_calls, 2);
        assert_eq!(
            done.segments[1].outcome,
            openrt_core::SegmentOutcome::Completed
        );
        assert_eq!(done.segments[1].turns, 1);
        assert_eq!(done.segments[1].tool_calls, 0);

        // Cumulative whole-task totals are honest.
        let cum = done.cumulative();
        assert_eq!(cum.segments, 2);
        assert_eq!(cum.turns, 3);
        assert_eq!(cum.tool_calls, 2);

        // The ledger survives the durable round-trip.
        let reloaded = store.load_task(done.id).await.unwrap();
        assert_eq!(reloaded.segments.len(), 2);

        // Test-15 rule: the user instruction is persisted EXACTLY ONCE — the
        // supervisor's fold rule (full transcript on segment 1, `[1..]` on
        // every continuation) never duplicates it, even across segments.
        let updated = store.load_session(session.id).await.unwrap();
        let prompt_occurrences = updated
            .content
            .iter()
            .filter(|m| m.role == MessageRole::User && m.content.contains("multi-segment work"))
            .count();
        assert_eq!(
            prompt_occurrences, 1,
            "instruction duplicated:\n{updated:?}"
        );
        assert!(reloaded.prompt_persisted);
    }

    /// When the global automatic-continuation ceiling is reached, the task
    /// FAILS with the explicit `global task execution budget exhausted`
    /// reason (never silent, never spun forever).
    #[tokio::test]
    async fn supervisor_stops_at_max_task_iterations_with_explicit_error() {
        // Six scripted tool calls: at two per segment this forces three full
        // continuation segments, so the ceiling is what actually stops it.
        let script: Vec<_> = (0..6)
            .map(|i| ("echo", serde_json::json!({ "text": format!("step-{i}") })))
            .collect();
        let (p, session, store) = supervised_pipeline(script).await;
        let budget = super::TaskExecutionBudget {
            max_task_iterations: Some(3),
            ..super::TaskExecutionBudget::default_finite()
        };
        let task = Task::new(session.id, "runaway?");
        let done = p
            .run_supervised(
                &session,
                task,
                "runaway?",
                &CancellationToken::new(),
                None,
                budget,
            )
            .await
            .unwrap();
        assert_eq!(done.status, TaskStatus::Failed, "{done:?}");
        let err = done.error.as_deref().unwrap_or_default();
        assert!(
            err.contains("global task execution budget exhausted"),
            "error: {err}"
        );
        assert!(
            err.contains("automatic-continuation ceiling"),
            "error: {err}"
        );
        assert_eq!(done.segments.len(), 3);
        for s in &done.segments {
            assert_eq!(s.outcome, openrt_core::SegmentOutcome::Continued);
        }
        // A global-budget failure is never counted as successful work.
        assert!(done.result.is_none());

        // The failure marker is durable and the session stays usable.
        let updated = store.load_session(session.id).await.unwrap();
        assert!(
            updated
                .content
                .iter()
                .any(|m| m.content.contains("[task Failed:")),
            "failure marker missing: {updated:?}"
        );
    }

    /// Exceeding the GLOBAL turn ceiling across segments fails explicitly with
    /// the turn detail (even though each individual segment stayed within its
    /// own per-segment budget).
    #[tokio::test]
    async fn supervisor_fails_on_global_turn_ceiling() {
        let (p, session, store) = supervised_pipeline(two_echo_script()).await;
        let budget = super::TaskExecutionBudget {
            max_task_turns: Some(3),
            ..super::TaskExecutionBudget::default_finite()
        };
        let task = Task::new(session.id, "hot loop");
        let done = p
            .run_supervised(
                &session,
                task,
                "hot loop",
                &CancellationToken::new(),
                None,
                budget,
            )
            .await
            .unwrap();
        assert_eq!(done.status, TaskStatus::Failed, "{done:?}");
        let err = done.error.as_deref().unwrap_or_default();
        assert!(err.contains("global turn ceiling 3"), "error: {err}");
        assert_eq!(done.cumulative().turns, 3);
        // Segment 1 Continued (2 turns), segment 2 finished under the line but
        // pushed cumulative past the ceiling → Failed with an explicit reason.
        assert_eq!(done.segments.len(), 2);
        assert_eq!(
            done.segments[1].outcome,
            openrt_core::SegmentOutcome::Failed
        );
        assert!(done.result.is_none(), "no result on a ceiling failure");
        let _ = store;
    }

    /// A fresh single-segment task that COMPLETES within its global budgets is
    /// a Completed task — the supervisor never touches tasks that do not need
    /// continuation, and its ledger records one Completed segment.
    #[tokio::test]
    async fn supervisor_single_segment_completes_untouched() {
        let (p, session, store) = supervised_pipeline(vec![]).await;
        let task = Task::new(session.id, "simple");
        let done = p
            .run_supervised(
                &session,
                task,
                "simple",
                &CancellationToken::new(),
                None,
                super::TaskExecutionBudget::default_finite(),
            )
            .await
            .unwrap();
        assert_eq!(done.status, TaskStatus::Completed, "{done:?}");
        assert_eq!(done.segments.len(), 1);
        assert_eq!(
            done.segments[0].outcome,
            openrt_core::SegmentOutcome::Completed
        );
        let reloaded = store.load_task(done.id).await.unwrap();
        assert_eq!(reloaded.segments.len(), 1);
    }
}
