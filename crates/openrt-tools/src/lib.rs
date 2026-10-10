//! Tool framework: trait, spec, registry, and the gate-side executor.
//!
//! Tools declare a [`ToolSpec`] (name, description, JSON-schema parameters,
//! declared capabilities) and implement [`Tool::run`]. Agents must never call
//! a tool directly — they go through the [`ToolExecutor`] supplied by the
//! runtime, which interposes permission and resource gates.

mod error;
mod executor;
mod registry;

/// Module wiring (public for doc links).
pub mod builtin;

pub use error::ToolError;
#[cfg(any(test, feature = "demo"))]
pub use executor::DirectExecutor;
pub use executor::{ToolExecution, ToolExecutionContext, ToolExecutor};
pub use registry::{RegistryError, ToolRegistry};

use std::fmt;

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::str::FromStr;

use openrt_permissions::Capability;

/// Declared capabilities a tool consumes (annotation for introspection and
/// future policy rules). Phase 1 permission decisions key on the tool id;
/// these strings document intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub version: String,
    /// Declared capabilities (e.g. `["filesystem.read.all"]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_caps: Vec<Capability>,
    /// JSON-Schema (subset) of the invocation arguments.
    pub parameters_schema: serde_json::Value,
}

impl ToolSpec {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            version: "0.1.0".to_string(),
            required_caps: Vec::new(),
            parameters_schema: serde_json::json!({ "type": "object" }),
        }
    }

    pub fn with_version(mut self, version: &str) -> Self {
        self.version = version.to_string();
        self
    }

    pub fn with_caps(mut self, caps: &[&str]) -> Self {
        self.required_caps = caps
            .iter()
            .filter_map(|c| Capability::from_str(c).ok())
            .collect();
        self
    }

    pub fn with_schema(mut self, schema: serde_json::Value) -> Self {
        self.parameters_schema = schema;
        self
    }

    /// The JSON tool definition handed to model providers for function calling.
    pub fn to_provider_tool(&self) -> serde_json::Value {
        serde_json::json!({
            "name": self.name,
            "description": self.description,
            "parameters": self.parameters_schema,
        })
    }
}

/// What a tool returns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "lowercase")]
pub enum ToolOutput {
    Text(String),
    Json(serde_json::Value),
}

impl ToolOutput {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text(s.into())
    }
    pub fn json(v: serde_json::Value) -> Self {
        Self::Json(v)
    }

    /// Lossy textual rendering for messages/terminal display.
    pub fn render(&self) -> String {
        match self {
            Self::Text(t) => t.clone(),
            Self::Json(v) => match serde_json::to_string_pretty(v) {
                Ok(s) => s,
                Err(_) => v.to_string(),
            },
        }
    }

    /// Deserialize as JSON if `Json`, else parse text as JSON. Used by the
    /// mock provider's self-check path.
    pub fn try_typed<T: DeserializeOwned>(&self) -> Option<T> {
        let s = self.render();
        serde_json::from_str(&s).ok()
    }
}

impl fmt::Display for ToolOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

/// Contract implemented by every tool.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;

    /// Execute with validated arguments. Errors are already wrapped by the
    /// runtime; tools should return their own [`ToolError`] on failure.
    async fn run(&self, args: serde_json::Value) -> Result<ToolOutput, ToolError>;
}
