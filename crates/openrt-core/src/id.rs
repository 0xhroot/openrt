use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A typed, opaque identifier.
///
/// IDs are UUID v4 values wrapped in a domain-specific newtype so mismatched
/// identifiers cannot be silently swapped between domains at compile time.
pub struct Id<T> {
    inner: Uuid,
    _marker: std::marker::PhantomData<fn() -> T>,
}

impl<T> Id<T> {
    /// Create a fresh random identifier.
    pub fn new() -> Self {
        Self {
            inner: Uuid::new_v4(),
            _marker: std::marker::PhantomData,
        }
    }

    /// Create an identifier from a `Uuid`.
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self {
            inner: uuid,
            _marker: std::marker::PhantomData,
        }
    }

    /// Access the underlying `Uuid`.
    pub fn as_uuid(&self) -> Uuid {
        self.inner
    }
}

impl<T> Default for Id<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Clone for Id<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Id<T> {}

impl<T> PartialEq for Id<T> {
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

impl<T> Eq for Id<T> {}

impl<T> std::hash::Hash for Id<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.inner.hash(state);
    }
}

impl<T> fmt::Debug for Id<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Id({})", self.inner)
    }
}

impl<T> fmt::Display for Id<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.inner)
    }
}

impl<T> FromStr for Id<T> {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::from_uuid(Uuid::parse_str(s)?))
    }
}

impl<T> Serialize for Id<T> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.inner.serialize(s)
    }
}

impl<'de, T> Deserialize<'de> for Id<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Uuid::deserialize(d).map(Self::from_uuid)
    }
}

// Distinct zero-sized tag types keep `Id<T>` nominal per domain without
// embedding the identifier type recursively.
pub struct SessionTag;
pub struct TaskTag;
pub struct AgentTag;
pub struct ToolTag;
pub struct ModelTag;
pub struct EventTag;
pub struct ExecutionTag;
pub struct CheckpointTag;

/// Identifier of a conversation session.
pub type SessionId = Id<SessionTag>;
/// Identifier of an individual task within a session.
pub type TaskId = Id<TaskTag>;
/// Identifier of an agent.
pub type AgentId = Id<AgentTag>;
/// Identifier of a registered tool.
pub type ToolId = Id<ToolTag>;
/// Identifier of a model / provider port.
pub type ModelId = Id<ModelTag>;
/// Identifier of a single emitted domain event.
pub type EventId = Id<EventTag>;
/// Identifier of an execution record (history, distinct from audit records).
pub type ExecutionId = Id<ExecutionTag>;
/// Identifier of a workflow checkpoint (P2.6 execution snapshot).
pub type CheckpointId = Id<CheckpointTag>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_round_trip() {
        let a = SessionId::new();
        let b = SessionId::new();
        assert_ne!(a, b);
        let s = a.to_string();
        let c: SessionId = s.parse().unwrap();
        assert_eq!(a, c);
    }

    #[test]
    fn id_serde_round_trip() {
        let t = TaskId::new();
        let json = serde_json::to_string(&t).unwrap();
        let back: TaskId = serde_json::from_str(&json).unwrap();
        assert_eq!(t, back);
    }
}
