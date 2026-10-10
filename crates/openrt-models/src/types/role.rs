use serde::{Deserialize, Serialize};

/// Role of a message as sent to a model provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelRole {
    System,
    User,
    Assistant,
    Tool,
    /// A message carrying one or more tool calls requested by the assistant.
    #[serde(rename = "tool")]
    ToolCall,
}