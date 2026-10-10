use serde::{Deserialize, Serialize};

use crate::id::SessionId;
use crate::message::Message;
use crate::timestamp::{now, Timestamp};

/// Lifecycle state of a session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SessionStatus {
    #[default]
    Active,
    Archived,
}

/// A conversation session: the durable container of conversation content
/// (`session.content` per ADR-007). Sessions are long-lived and hold tasks.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub title: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub status: SessionStatus,
    pub content: Vec<Message>,
    /// M10.8: the genome applied to this session — id + version + digest
    /// recorded at apply time (a running session is never silently
    /// re-genomed; each apply is an explicit, recorded transition).
    #[serde(default)]
    pub genome: Option<SessionGenome>,
}

/// M10.8: the durable reference recorded on a session when a genome is
/// applied (id + version + digest — provenance that survives everything).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGenome {
    pub id: String,
    pub version: u32,
    pub digest: String,
}

impl Session {
    pub fn new(title: impl Into<String>) -> Self {
        let t = now();
        Self {
            id: SessionId::new(),
            title: title.into(),
            created_at: t,
            updated_at: t,
            status: SessionStatus::Active,
            content: Vec::new(),
            genome: None,
        }
    }

    pub fn touch(&mut self) {
        self.updated_at = now();
    }

    /// Append a message to the conversation content and bump `updated_at`.
    pub fn push(&mut self, msg: Message) {
        self.content.push(msg);
        self.touch();
    }

    pub fn archive(&mut self) {
        self.status = SessionStatus::Archived;
        self.touch();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_lifecycle() {
        let mut s = Session::new("t");
        assert_eq!(s.status, SessionStatus::Active);
        assert!(s.content.is_empty());
        s.push(Message::user("ping"));
        assert_eq!(s.content.len(), 1);
        s.archive();
        assert_eq!(s.status, SessionStatus::Archived);
    }
}
