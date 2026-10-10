use serde::{Deserialize, Serialize};

use crate::id::{AgentId, SessionId, TaskId, ToolId};
use crate::timestamp::{now, Timestamp};

/// Lifecycle state of a task. Explicit state machine — enum values, never
/// free-form strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskStatus {
    Created,
    Queued,
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
    /// Phase 10 (M10.4): discovered active at boot after the owning process
    /// died. Not terminal — recovery may transition it back to `Running`
    /// (`task_recover`) or an operator may terminate it. Never counted as
    /// successful work.
    Interrupted,
    /// Part C: a live task was paused cooperatively by the operator at a turn
    /// boundary. Not terminal and not actively running — the session
    /// transcript (tool rounds) is preserved and the task resumes from it via
    /// `task_recover`. Never counted as successful work.
    Paused,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Created | Self::Queued | Self::Running | Self::Waiting
        )
    }
}

/// What a recorded turn represents (foundation for execution history; distinct
/// from the Phase 2 audit log, ADR-002).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TurnKind {
    /// A model call was made.
    ModelCall,
    /// A registered tool was invoked.
    ToolCall(ToolId),
    /// The agent produced its final output.
    FinalOutput,
}

/// One recorded step of a task's execution (execution history substrate).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnRecord {
    /// Ordering within the execution (1-based).
    pub no: u32,
    pub kind: TurnKind,
    pub at: Timestamp,
    /// Optional structured detail (e.g. tool input / model output text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

/// Outcome of one execution segment (hardening-pass supervisor accounting).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentOutcome {
    /// The segment finished with a terminal answer; the task is done. Any
    /// later segments that exist came from earlier continuation rounds.
    Completed,
    /// The segment hit its per-segment budget (max_turns / max_tool_calls);
    /// the supervisor checkpointed and continued with a fresh segment.
    #[default]
    Continued,
    Paused,
    Cancelled,
    Failed,
}

/// Durable record of one supervisor execution segment. Segment numbering is
/// 1-based across the WHOLE task; the last record reflects the
/// currently-executing segment while the task is Running. `#[serde(default)]`
/// fields keep legacy tasks loadable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentRecord {
    pub no: u32,
    pub started_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<Timestamp>,
    #[serde(default)]
    pub turns: u32,
    #[serde(default)]
    pub tool_calls: u32,
    #[serde(default)]
    pub retries: u32,
    #[serde(default)]
    pub outcome: SegmentOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl SegmentRecord {
    pub fn new(no: u32) -> Self {
        Self {
            no,
            started_at: now(),
            ended_at: None,
            turns: 0,
            tool_calls: 0,
            retries: 0,
            outcome: SegmentOutcome::Continued,
            error: None,
        }
    }
}

/// A unit of work submitted to an agent within a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub session_id: SessionId,
    pub prompt: String,
    pub agent_id: Option<AgentId>,
    pub status: TaskStatus,
    pub created_at: Timestamp,
    pub started_at: Option<Timestamp>,
    pub completed_at: Option<Timestamp>,
    pub result: Option<String>,
    pub error: Option<String>,
    pub turns: Vec<TurnRecord>,
    /// Durable segment ledger written by the runtime supervisor. Empty for
    /// tasks created without the supervisor (never coalesced back into the
    /// per-segment limits they predate).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub segments: Vec<SegmentRecord>,
    /// Whether `prompt` has already been persisted in the session. The
    /// supervisor's fold rule reads this: a fresh task folds the FULL agent
    /// transcript into the session; a continuation folds `[1..]` so the user
    /// instruction is never duplicated into the durable transcript.
    #[serde(default)]
    pub prompt_persisted: bool,
}

impl Task {
    pub fn new(session_id: SessionId, prompt: impl Into<String>) -> Self {
        let t = now();
        Self {
            id: TaskId::new(),
            session_id,
            prompt: prompt.into(),
            agent_id: None,
            status: TaskStatus::Created,
            created_at: t,
            started_at: None,
            completed_at: None,
            result: None,
            error: None,
            turns: Vec::new(),
            segments: Vec::new(),
            prompt_persisted: false,
        }
    }

    /// Open a new supervisor segment (1-based numbering) and append it to the
    /// task's durable ledger.
    pub fn begin_segment(&mut self) -> u32 {
        let no = self.segments.len() as u32 + 1;
        self.segments.push(SegmentRecord::new(no));
        no
    }

    /// Close the currently-open segment, recording honest counters plus its
    /// outcome. No-op when the ledger is empty (a supervisor-less task).
    pub fn end_segment(&mut self, outcome: SegmentOutcome, error: Option<String>) {
        if let Some(last) = self.segments.last_mut() {
            last.ended_at = Some(now());
            last.outcome = outcome;
            last.error = error;
        }
    }

    /// Record the honest per-segment counters (turns / tool calls / retries)
    /// into the currently-open segment — written by the supervisor the moment
    /// a segment's agent run returns, so the durable ledger reflects real
    /// work, never estimates.
    pub fn push_segment_counts(&mut self, turns: u32, tool_calls: u32, retries: u32) {
        if let Some(last) = self.segments.last_mut() {
            last.turns = turns;
            last.tool_calls = tool_calls;
            last.retries = retries;
        }
    }

    /// Add an already-closed segment (used on checkpoint/resume replay).
    pub fn push_segment(&mut self, record: SegmentRecord) {
        self.segments.push(record);
    }

    /// 1-based ordinal of the last head-segment (0 = no segment), exposed for
    /// observability.
    pub fn current_segment_no(&self) -> u32 {
        self.segments.len() as u32
    }

    /// Cumulative counters across the whole task's segments.
    pub fn cumulative(&self) -> SegmentTotals {
        SegmentTotals {
            segments: self.segments.len() as u64,
            turns: self.segments.iter().map(|s| s.turns as u64).sum(),
            tool_calls: self.segments.iter().map(|s| s.tool_calls as u64).sum(),
            retries: self.segments.iter().map(|s| s.retries as u64).sum(),
        }
    }

    /// Whole-task wall-clock including all segments (created → now/completed).
    pub fn elapsed_ms(&self, at: Timestamp) -> u64 {
        let start = self.started_at.unwrap_or(self.created_at);
        (at - start).num_milliseconds().max(0) as u64
    }

    pub fn transition(&mut self, to: TaskStatus) -> Result<(), TaskError> {
        let from = self.status;
        if to == from {
            return Ok(());
        }
        if from.is_terminal() {
            return Err(TaskError::IllegalTransition { from, to });
        }
        // Allow any forward transition from active states; guard nonsense like
        // Completed -> Running by rejecting transitions from terminal states
        // (handled above).
        self.status = to;
        match to {
            TaskStatus::Running => {
                self.started_at.get_or_insert_with(now);
            }
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled => {
                self.completed_at.get_or_insert_with(now);
            }
            _ => {}
        }
        Ok(())
    }

    pub fn record_turn(&mut self, kind: TurnKind, detail: Option<serde_json::Value>) {
        let no = self.turns.len() as u32 + 1;
        self.turns.push(TurnRecord {
            no,
            kind,
            at: now(),
            detail,
        });
    }
}

/// Whole-task cumulative execution totals (supervisor budget accounting).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentTotals {
    pub segments: u64,
    pub turns: u64,
    pub tool_calls: u64,
    pub retries: u64,
}

/// Errors that occur within the task lifecycle.
#[derive(Debug, Clone, thiserror::Error)]
pub enum TaskError {
    #[error("illegal task transition: {from:?} -> {to:?}")]
    IllegalTransition { from: TaskStatus, to: TaskStatus },
    #[error("task {0} not found")]
    NotFound(TaskId),
    #[error("task was cancelled")]
    Cancelled,
    #[error("task execution timed out")]
    Timeout,
    /// Phase 10 live handles: a task with a live token in this process.
    #[error("task {0} is already running in this process")]
    AlreadyRunning(TaskId),
    /// Phase 10 live handles: active in the store but no live handle here —
    /// another process owns it, or it crashed and awaits boot reconciliation.
    #[error(
        "task {0} is active in the store but not running in this process (restart to reconcile)"
    )]
    StaleActive(TaskId),
}

impl PartialEq for TaskError {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::IllegalTransition { from: a, to: b }, Self::IllegalTransition { from: c, to: d })
                if a == c && b == d
        ) || matches!((self, other), (Self::NotFound(a), Self::NotFound(b)) if a == b)
            || matches!((self, other), (Self::AlreadyRunning(a), Self::AlreadyRunning(b)) if a == b)
            || matches!((self, other), (Self::StaleActive(a), Self::StaleActive(b)) if a == b)
            || matches!(
                (self, other),
                (Self::Cancelled, Self::Cancelled) | (Self::Timeout, Self::Timeout)
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::SessionId;

    fn t(s: &str) -> Task {
        Task::new(SessionId::new(), s)
    }

    #[test]
    fn task_state_machine() {
        let mut task = t("do it");
        task.transition(TaskStatus::Queued).unwrap();
        task.transition(TaskStatus::Running).unwrap();
        assert!(task.started_at.is_some());
        task.transition(TaskStatus::Completed).unwrap();
        assert!(task.completed_at.is_some());
        assert!(task.status.is_terminal());
        // Terminal -> anything else is an error.
        let err = task.transition(TaskStatus::Running).unwrap_err();
        assert!(matches!(err, TaskError::IllegalTransition { .. }));
    }

    #[test]
    fn task_interrupted_is_recoverable_not_terminal() {
        let mut task = t("interrupted");
        task.transition(TaskStatus::Running).unwrap();
        task.transition(TaskStatus::Interrupted).unwrap();
        // Discovered work is never claimed successful: no completion stamp.
        assert_eq!(task.status, TaskStatus::Interrupted);
        assert!(task.completed_at.is_none());
        assert!(!task.status.is_terminal());
        assert!(!task.status.is_active());
        // Recovery may re-run it; a real completion stamps completed_at.
        task.transition(TaskStatus::Running).unwrap();
        task.transition(TaskStatus::Completed).unwrap();
        assert!(task.completed_at.is_some());
        assert!(task.status.is_terminal());
        // Terminal stays terminal.
        assert!(task.transition(TaskStatus::Interrupted).is_err());
    }

    #[test]
    fn task_interrupted_serde_round_trip() {
        let mut task = t("serde interrupted");
        task.transition(TaskStatus::Running).unwrap();
        task.transition(TaskStatus::Interrupted).unwrap();
        let json = serde_json::to_string(&task).unwrap();
        assert!(json.contains("\"Interrupted\""));
        let back: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(task, back);
    }

    #[test]
    fn task_records_turns() {
        let mut task = t("x");
        task.record_turn(TurnKind::ModelCall, None);
        task.record_turn(TurnKind::ToolCall(ToolId::new()), None);
        assert_eq!(task.turns.len(), 2);
        assert_eq!(task.turns[1].no, 2);
    }

    #[test]
    fn segment_ledger_round_trips_and_totals() {
        let mut task = t("multi");
        let _no = task.begin_segment();
        // Simulate the supervisor closing a Continued segment with honest
        // counters, then a pair of Completed segments on a second task.
        task.push_segment(SegmentRecord {
            no: 1,
            started_at: now(),
            ended_at: Some(now()),
            turns: 6,
            tool_calls: 9,
            retries: 1,
            outcome: SegmentOutcome::Continued,
            error: None,
        });
        task.begin_segment();
        task.end_segment(SegmentOutcome::Completed, None);

        let totals = task.cumulative();
        assert_eq!(totals.segments, 3);
        assert_eq!(totals.turns, 6);
        assert_eq!(totals.tool_calls, 9);
        assert_eq!(totals.retries, 1);
        assert_eq!(task.current_segment_no(), 3);

        let json = serde_json::to_string(&task).unwrap();
        assert!(json.contains("\"prompt_persisted\":false"));
        let back: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(task, back);
        assert_eq!(back.segments.len(), 3);
    }

    /// A legacy task record with NO segment fields loads fine (prompt not yet
    /// persisted; no segments) — old durable stores never break on the new
    /// supervisor bookkeeping.
    #[test]
    fn legacy_task_without_segment_fields_loads() {
        let json = r#"{
            "id": "00000000-0000-0000-0000-000000000001",
            "session_id": "00000000-0000-0000-0000-000000000002",
            "prompt": "legacy",
            "agent_id": null,
            "status": "Paused",
            "created_at": "2026-01-01T00:00:00Z",
            "started_at": null,
            "completed_at": null,
            "result": null,
            "error": null,
            "turns": []
        }"#;
        let task: Task = serde_json::from_str(json).unwrap();
        assert!(task.segments.is_empty());
        assert!(!task.prompt_persisted);
    }

    #[test]
    fn task_serde_round_trip() {
        let mut task = t("x");
        task.transition(TaskStatus::Running).unwrap();
        let json = serde_json::to_string(&task).unwrap();
        let back: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(task, back);
    }

    #[test]
    fn task_error_partial_eq() {
        let a = TaskError::Cancelled;
        let b = TaskError::Timeout;
        assert_ne!(a, b);
        assert_eq!(a, TaskError::Cancelled);
    }
}
