//! `echo` — returns its argument. The canonical smoke-test tool.

use serde_json::Value;

use crate::{Tool, ToolError, ToolOutput, ToolSpec};

pub struct Echo {
    pub spec: ToolSpec,
}

impl Echo {
    pub fn new() -> Self {
        let spec = ToolSpec::new("echo", "Echoes the given text back unchanged.").with_schema(
            serde_json::json!({
                "type": "object",
                "properties": { "text": { "type": "string", "description": "text to echo" } },
                "required": ["text"]
            }),
        );
        Self { spec }
    }
}

impl Default for Echo {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for Echo {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let text = args.get("text").and_then(Value::as_str).ok_or_else(|| {
            ToolError::InvalidArguments("echo".into(), "missing `text` string".into())
        })?;
        Ok(ToolOutput::text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn echoes() {
        let t = Echo::new();
        let out = t.run(serde_json::json!({ "text": "hello" })).await.unwrap();
        assert_eq!(out.render(), "hello");
    }

    #[tokio::test]
    async fn rejects_missing_text() {
        let t = Echo::new();
        assert!(matches!(
            t.run(serde_json::json!({})).await,
            Err(ToolError::InvalidArguments(_, _))
        ));
    }
}
