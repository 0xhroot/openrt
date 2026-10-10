//! Backend abstraction: what the `browser.*` tools talk to. Implementations:
//! [`crate::firefox::FirefoxBiDi`] (real) and [`crate::mock::MockBackend`]
//! (deterministic, tests/CI). There is no third path — no fake success.

use async_trait::async_trait;
use serde_json::Value;

use openrt_fabric::FabricError;

#[async_trait]
pub trait BrowserBackend: Send + Sync {
    /// Backend identity (`firefox-bidi` / `mock`).
    fn name(&self) -> &'static str;

    /// Navigate the current tab; returns final URL + document title.
    async fn navigate(&self, url: &str) -> Result<Value, FabricError>;

    /// Evaluate a JavaScript expression and return its JSON value.
    /// Contract: expressions used by tools are `JSON.stringify(...)`-wrapped
    /// and resolve to a string containing JSON — [`unwrap_stringified`]
    /// turns any backend's raw result into that JSON.
    async fn evaluate(&self, expr: &str) -> Result<Value, FabricError>;

    /// Capture the viewport as PNG bytes.
    async fn screenshot(&self) -> Result<Vec<u8>, FabricError>;

    /// End the session (kill a spawned browser process; drop mock state).
    async fn close(&self) -> Result<(), FabricError>;
}

/// Normalize any backend's raw evaluation result into the tool-level JSON
/// value: parses string payloads (the `JSON.stringify` contract), unwraps
/// WebDriver-BiDi `RemoteValue` objects, and maps `undefined`/null to null.
pub fn unwrap_stringified(raw: &Value) -> Value {
    match raw {
        Value::String(s) => {
            let t = s.trim();
            if t == "undefined" || t.is_empty() {
                Value::Null
            } else {
                serde_json::from_str(t).unwrap_or(Value::String(s.clone()))
            }
        }
        Value::Null => Value::Null,
        Value::Object(map) => match map.get("type").and_then(Value::as_str) {
            Some("string") => match map.get("value") {
                Some(v) => unwrap_stringified(v),
                None => Value::Null,
            },
            Some("number") => map.get("value").cloned().unwrap_or(Value::Null),
            Some("boolean") => map.get("value").cloned().unwrap_or(Value::Null),
            Some("null") | Some("undefined") => Value::Null,
            Some("object") => match map.get("value") {
                Some(Value::Object(inner)) => Value::Object(
                    inner
                        .iter()
                        .map(|(k, v)| (k.clone(), unwrap_stringified(v)))
                        .collect(),
                ),
                _ => Value::Null,
            },
            // Structured values from the mock backend (or already-parsed JSON).
            _ => raw.clone(),
        },
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unwraps_all_shapes() {
        assert_eq!(unwrap_stringified(&json!("null")), Value::Null);
        assert_eq!(unwrap_stringified(&json!("undefined")), Value::Null);
        assert_eq!(unwrap_stringified(&json!(r#"{"a":1}"#)), json!({ "a": 1 }));
        assert_eq!(
            unwrap_stringified(&json!({"type": "string", "value": "{\"b\":2}"})),
            json!({ "b": 2 })
        );
        assert_eq!(
            unwrap_stringified(&json!({"type": "number", "value": 3.5})),
            json!(3.5)
        );
        assert_eq!(
            unwrap_stringified(
                &json!({"type": "object", "value": {"k": {"type": "string", "value": "v"}}})
            ),
            json!({"k": "v"})
        );
        let mock = json!({"mock": true, "expr_chars": 5});
        assert_eq!(unwrap_stringified(&mock), mock);
    }
}
