use serde::{Deserialize, Serialize};

use crate::id::{ExecutionId, SessionId, TaskId};
use crate::task::TaskStatus;
use crate::timestamp::Timestamp;

/// A durable record of one task's execution (the execution-history substrate).
///
/// This is deliberately distinct from the Phase 2 audit log (ADR-002): it is
/// runtime history for replay/debugging, not tamper-evident proof. It is
/// additive and versioned; consumers replay it, they never reorder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionRecord {
    pub execution_id: ExecutionId,
    pub task_id: TaskId,
    pub session_id: SessionId,
    pub agent_id: Option<crate::id::AgentId>,
    pub status: TaskStatus,
    pub started_at: Option<Timestamp>,
    pub completed_at: Option<Timestamp>,
    /// Tool ids invoked, in order.
    pub tool_calls: Vec<String>,
    /// Number of recorded turns.
    pub turn_count: u32,
    pub result: Option<String>,
    pub error: Option<String>,
}

impl ExecutionRecord {
    pub fn new(
        session_id: SessionId,
        task_id: TaskId,
        agent_id: Option<crate::id::AgentId>,
    ) -> Self {
        Self {
            execution_id: ExecutionId::new(),
            task_id,
            session_id,
            agent_id,
            status: TaskStatus::Created,
            started_at: None,
            completed_at: None,
            tool_calls: Vec::new(),
            turn_count: 0,
            result: None,
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_record_round_trip() {
        let r = ExecutionRecord::new(SessionId::new(), TaskId::new(), None);
        let json = serde_json::to_string(&r).unwrap();
        let back: ExecutionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }
}
