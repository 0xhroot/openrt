use serde::{Deserialize, Serialize};

use crate::timestamp::Timestamp;

/// Role of a message in a conversation (ADR-007: conversation content is
/// `session.content`, never "context").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

/// A tool call the model asked to perform. Provider-agnostic: providers map
/// their native wire format to/from this type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingToolCall {
    /// Call reference id (echoed back by the model / provider).
    pub id: String,
    /// Identifier of the registered tool to invoke.
    pub tool_id: String,
    /// Structured arguments for the tool.
    pub input: serde_json::Value,
}

/// One message inside a conversation. `content` is the textual payload;
/// `tool_calls` may request tool invocations when role is `Assistant`;
/// `tool_call_name`/`tool_call_result` is only meaningful for role `Tool`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<PendingToolCall>,
    /// For `Tool` role: name of the tool whose result this message carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub at: Option<Timestamp>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self::new(MessageRole::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::new(MessageRole::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(MessageRole::Assistant, content)
    }

    pub fn tool(spec: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(spec.into()),
            at: None,
        }
    }

    pub fn new(role: MessageRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            at: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_round_trip() {
        let m = Message::assistant("hello");
        let json = serde_json::to_string(&m).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn tool_call_round_trip() {
        let m = Message {
            role: MessageRole::Assistant,
            content: String::new(),
            tool_calls: vec![PendingToolCall {
                id: "call_1".into(),
                tool_id: "echo".into(),
                input: serde_json::json!({"text": "hi"}),
            }],
            tool_call_id: None,
            at: None,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
        assert_eq!(back.tool_calls[0].tool_id, "echo");
    }
}
