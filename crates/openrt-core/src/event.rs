use serde::{Deserialize, Serialize};

use crate::id::{AgentId, EventId, SessionId, TaskId};
use crate::timestamp::Timestamp;

/// The set of domain events that Phase 1 emits on behalf of the runtime.
///
/// These are the minimum, structured event surface. Later phases may add
/// events (additive, never rename or re-purpose existing kinds). Cancellation
/// of a task is surfaced through `TaskFailed` with an explicit `data.reason =
/// "cancelled"` (no separate event kind in Phase 1; see EVENT_MODEL.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventKind {
    RuntimeStarted,
    RuntimeStopping,
    SessionCreated,
    SessionLoaded,
    TaskCreated,
    TaskStarted,
    TaskCompleted,
    TaskFailed,
    /// Part C: a live task paused cooperatively at a turn boundary
    /// (data.reason, data.turns); the transcript is preserved and the task
    /// resumes from it via `task_recover` (same authority — no second store).
    TaskPaused,
    /// Part C: a paused task was resumed to `Running` in this process
    /// (data.attempt).
    TaskResumed,
    AgentStarted,
    AgentCompleted,
    ModelInvocationStarted,
    ModelInvocationCompleted,
    ToolInvocationStarted,
    ToolInvocationCompleted,
    PermissionRequested,
    PermissionGranted,
    PermissionDenied,
    /// An `Ask` decision was released by an approval resolver (P2.2).
    PermissionApproved,
    ResourceCheck,
    ErrorOccurred,
    /// Workflow orchestration lifecycle (P2.3): a workflow DAG started.
    WorkflowStarted,
    /// P2.3: a workflow step began executing its role agent.
    WorkflowStepStarted,
    /// P2.3: a workflow step completed (data.artifact key set).
    WorkflowStepCompleted,
    /// P2.3: a workflow step failed (or exhausted its retries).
    WorkflowStepFailed,
    /// P2.3: a workflow run ended (data.status = completed|failed|cancelled).
    WorkflowCompleted,
    /// Phase 11: a workflow run stopped cooperatively at an operator pause
    /// request (data.workflow_id, data.steps_completed) — partial history
    /// preserved, resumable by explicit operator action only.
    WorkflowPaused,
    /// P2.5: an ADR-008 HALT signal was emitted (data.scope, data.reason) or a
    /// computer-scope tool call was blocked because the kill switch is engaged
    /// (data.reaction = "denied").
    SafetyHalt,
    /// P2.6: a workflow checkpoint was durably written (data.checkpoint_id,
    /// data.workflow_id, data.status, data.nodes_total, data.nodes_completed).
    CheckpointCreated,
    /// P2.6: a checkpoint could not be loaded/validated (data.checkpoint_id,
    /// data.reason).
    CheckpointLoadFailed,
    /// P2.6: a checkpoint record was deleted by an operator (data.checkpoint_id).
    CheckpointDeleted,
    /// P2.6: recovery planning started (data.checkpoint_id, data.workflow_id,
    /// data.plan with resume/retry/skip/intervention node lists).
    RecoveryStarted,
    /// P2.6: a workflow was resumed from a checkpoint (data.checkpoint_id,
    /// data.workflow_id, data.resume_nodes, data.retry_nodes, data.skip_nodes).
    WorkflowResumed,
    /// P2.6: a failed node was scheduled for a bounded retry (data.node,
    /// data.attempt, data.max_attempts).
    RetryScheduled,
    /// P2.6: a node exhausted its permitted retries (data.node, data.attempts).
    RetryExhausted,
    /// P2.6: a rollback operation began (data.checkpoint_id, data.op).
    RollbackStarted,
    /// P2.6: a rollback operation completed successfully (data.checkpoint_id,
    /// data.op).
    RollbackCompleted,
    /// P2.6: a rollback operation is not supported for the given action
    /// (data.checkpoint_id, data.op, data.reason) — never faked.
    RollbackUnavailable,
    /// P2.6: recovery cannot proceed automatically and needs operator
    /// intervention (data.checkpoint_id, data.node, data.reason).
    RecoveryNeedsIntervention,
    // Phase 4 — multimodal ingest, knowledge, RAG, datasets, training, eval.
    // All payloads carry safe fields (ids, counts, sha256, state, method); doc
    // text, prompts, secrets, and full dataset content never appear in events.
    /// Doc intake submitted (data.doc_id, data.size_bytes, data.method).
    DocumentQueued,
    /// Parser completed successfully (data.doc_id, data.blocks, data.pages, data.format).
    DocumentExtracted,
    /// Persisted after normalisation (data.doc_id, data.version, data.content_sha256).
    DocumentStored,
    /// Content hash already present; returned existing document (data.doc_id, data.existing_version).
    DocumentDuplicate,
    /// Intake failed (data.doc_id, data.reason).
    DocumentFailed,
    /// Chunking started (data.doc_id, data.version, data.strategy).
    ChunkingStarted,
    /// Chunking completed (data.doc_id, data.version, data.chunk_count).
    ChunkingCompleted,
    /// Embedding requested via provider (data.doc_id, data.chunk_count).
    EmbeddingRequested,
    /// Vector index mutated (data.index_key, data.operation, data.count).
    IndexUpdated,
    /// Index rebuild started (data.index_key).
    IndexRebuildStarted,
    /// Index rebuild completed (data.index_key, data.count, data.hash).
    IndexRebuildCompleted,
    /// Stored index hash mismatch detected during reload (data.index_key, data.expected, data.actual).
    IndexIntegrityFailed,
    /// Retrieval executed (data.kb_id, data.top_k, data.result_count).
    RetrievalStarted,
    /// Retrieval returned results (data.kb_id, data.result_count).
    RetrievalCompleted,
    /// RAG generation finished (data.kb_id, data.citation_count, data.omitted).
    RagAnswerGenerated,
    /// Citation integrity check found a fabricated or deleted citation (data.kb_id, data.invalid_count).
    RagCitationInvalid,
    /// JSONL import completed (data.dataset_id, data.records, data.splits, data.schema).
    DatasetImported,
    /// Record-level validation summary (data.total, data.ok, data.skipped).
    DatasetValidated,
    /// Export produced at a safe path (data.dataset_id, data.bytes_written).
    DatasetExported,
    /// Training job started (data.run_id, data.method, data.backend).
    TrainJobStarted,
    /// Training completed (data.run_id, data.backend).
    TrainJobCompleted,
    /// Training failed (data.run_id, data.backend, data.reason).
    TrainJobFailed,
    /// Training cancelled (data.run_id, data.backend, data.requested_by).
    TrainJobCancelled,
    /// Checkpoint written (data.run_id, data.checkpoint_id).
    TrainCheckpointWritten,
    /// Resource preflight calculated (data.run_id, data.ram_gb, data.vram_gb, data.disk_gb, data.level).
    TrainResourcePreflight,
    /// Evaluation completed (data.eval_id, data.run_id).
    EvalCompleted,
    /// Derived artifact verified before registration (data.artifact_id, data.base_digest).
    ArtifactVerified,
    /// Derived artifact registered (data.artifact_id, data.class, data.base_digest).
    ArtifactRegistered,
    // Phase 5 — workbench observability: self-improvement loops, learning,
    // local metrics. Additive kinds only (never rename or re-purpose).
    /// Self-improvement proposal recorded (data.proposal_id, data.proposal_kind, data.target_kind).
    ImprovementProposed,
    /// Proposal evaluation completed (data.proposal_id, data.result = passed|regression).
    ImprovementEvaluated,
    /// Operator approved a proposal through the review gate (data.proposal_id, data.operator).
    ImprovementApproved,
    /// Operator rejected a proposal through the review gate (data.proposal_id, data.operator).
    ImprovementRejected,
    /// Approved proposal applied to its target (data.proposal_id, data.target_id, data.digest).
    ImprovementApplied,
    /// Self-debug report recorded (data.debug_id, data.run_id, data.failure_signature).
    DebugReported,
    /// Learning derived from local execution history (data.learning_id, data.learning_kind, data.run_id).
    LearningDerived,
    /// Local metric recorded (data.schema_version, data.name, data.value, data.unit).
    MetricRecorded,
}

/// A single event envelope. Every event gets a unique id and timestamp and
/// carries optional correlation ids so consumers (logs, persistence, replay)
/// can reconstruct ordering across a session/task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DomainEvent {
    pub event_id: EventId,
    pub at: Timestamp,
    pub kind: EventKind,
    pub session_id: Option<SessionId>,
    pub task_id: Option<TaskId>,
    pub agent_id: Option<AgentId>,
    pub data: serde_json::Value,
}

impl DomainEvent {
    pub fn new(kind: EventKind) -> Self {
        Self {
            event_id: EventId::new(),
            at: crate::timestamp::now(),
            kind,
            session_id: None,
            task_id: None,
            agent_id: None,
            data: serde_json::Value::Null,
        }
    }

    pub fn with_session(mut self, id: SessionId) -> Self {
        self.session_id = Some(id);
        self
    }

    pub fn with_task(mut self, id: TaskId) -> Self {
        self.task_id = Some(id);
        self
    }

    pub fn with_agent(mut self, id: AgentId) -> Self {
        self.agent_id = Some(id);
        self
    }

    pub fn with_data(mut self, data: impl Into<serde_json::Value>) -> Self {
        self.data = data.into();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::SessionId;

    #[test]
    fn event_envelope_round_trip() {
        let ev = DomainEvent::new(EventKind::TaskStarted)
            .with_session(SessionId::new())
            .with_data(serde_json::json!({"turn": 1}));
        let json = serde_json::to_string(&ev).unwrap();
        let back: DomainEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
        assert_ne!(back.event_id, EventId::new());
    }

    #[test]
    fn all_kinds_serialize() {
        for kind in [
            EventKind::RuntimeStarted,
            EventKind::RuntimeStopping,
            EventKind::SessionCreated,
            EventKind::SessionLoaded,
            EventKind::TaskCreated,
            EventKind::TaskStarted,
            EventKind::TaskCompleted,
            EventKind::TaskFailed,
            EventKind::AgentStarted,
            EventKind::AgentCompleted,
            EventKind::ModelInvocationStarted,
            EventKind::ModelInvocationCompleted,
            EventKind::ToolInvocationStarted,
            EventKind::ToolInvocationCompleted,
            EventKind::PermissionRequested,
            EventKind::PermissionGranted,
            EventKind::PermissionDenied,
            EventKind::PermissionApproved,
            EventKind::ResourceCheck,
            EventKind::ErrorOccurred,
            EventKind::WorkflowStarted,
            EventKind::WorkflowStepStarted,
            EventKind::WorkflowStepCompleted,
            EventKind::WorkflowStepFailed,
            EventKind::WorkflowCompleted,
            EventKind::SafetyHalt,
            EventKind::CheckpointCreated,
            EventKind::CheckpointLoadFailed,
            EventKind::CheckpointDeleted,
            EventKind::RecoveryStarted,
            EventKind::WorkflowResumed,
            EventKind::RetryScheduled,
            EventKind::RetryExhausted,
            EventKind::RollbackStarted,
            EventKind::RollbackCompleted,
            EventKind::RollbackUnavailable,
            EventKind::RecoveryNeedsIntervention,
            EventKind::ImprovementProposed,
            EventKind::ImprovementEvaluated,
            EventKind::ImprovementApproved,
            EventKind::ImprovementRejected,
            EventKind::ImprovementApplied,
            EventKind::DebugReported,
            EventKind::LearningDerived,
            EventKind::MetricRecorded,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: EventKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back);
        }
    }
}
