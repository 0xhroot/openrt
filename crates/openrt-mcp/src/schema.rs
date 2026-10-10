//! Client-side JSON-Schema **subset** validator for imported MCP tools.
//!
//! Runs *before* every `tools/call` (argument validation) and at import time
//! (schema sanity). Supported keywords — exactly what the mission allows:
//! `type` (string or list), `required`, `properties`, `items`, `enum`,
//! `minimum`/`maximum`, `minLength`/`maxLength`. Anything else is ignored
//! (never trusted as an enforcement mechanism: policy gates still apply).

use serde_json::Value;

/// Validate `args` against `schema`; returns `Err(why)` with a path-precise
/// message on the first violation.
pub fn validate(schema: &Value, args: &Value) -> Result<(), String> {
    check(schema, args, "$")
}

/// Quick sanity check used at import time: reject schemas that are not
/// objects or whose declared `type` values are not JSON type names we can
/// ever enforce (a server sending `"type": "banana"` gets that tool refused,
/// surfaced through `mcp.discover` — never silently accepted).
pub fn schema_is_sane(schema: &Value) -> Result<(), String> {
    let obj = schema
        .as_object()
        .ok_or_else(|| "schema must be a JSON object".to_string())?;
    let known = [
        "object", "array", "string", "number", "integer", "boolean", "null",
    ];
    match obj.get("type") {
        None => Ok(()),
        Some(Value::String(t)) => {
            if known.contains(&t.as_str()) {
                Ok(())
            } else {
                Err(format!("unknown type `{t}`"))
            }
        }
        Some(Value::Array(list)) => {
            for t in list {
                let t = t.as_str().ok_or("type list must contain strings")?;
                if !known.contains(&t) {
                    return Err(format!("unknown type `{t}`"));
                }
            }
            Ok(())
        }
        Some(_) => Err("`type` must be a string or list of strings".into()),
    }
}

fn type_matches(expected: &str, v: &Value) -> bool {
    match expected {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        "integer" => match v {
            Value::Number(n) => {
                n.is_i64() || n.is_u64() || n.as_f64().map_or(false, |f| f.fract() == 0.0)
            }
            _ => false,
        },
        "number" => v.is_number(),
        _ => true, // unknown keyword value — schema_is_sane already refused it
    }
}

fn check(schema: &Value, value: &Value, path: &str) -> Result<(), String> {
    let obj = match schema.as_object() {
        Some(o) => o,
        None => return Ok(()), // not an object schema: nothing to enforce
    };

    if let Some(Value::Array(types)) = obj.get("type") {
        let ok = types
            .iter()
            .filter_map(Value::as_str)
            .any(|t| type_matches(t, value));
        if !ok {
            let want: Vec<&str> = types.iter().filter_map(Value::as_str).collect();
            return Err(format!(
                "{path}: expected type {:?}, got {}",
                want,
                kind(value)
            ));
        }
    } else if let Some(Value::String(t)) = obj.get("type") {
        if !type_matches(t, value) {
            return Err(format!("{path}: expected type `{t}`, got {}", kind(value)));
        }
    }

    if let Some(Value::Array(opts)) = obj.get("enum") {
        if !opts.contains(value) {
            return Err(format!(
                "{path}: value {} not in enum {:?}",
                serde_json::to_string(value).unwrap_or_default(),
                opts
            ));
        }
    }

    if value.is_object() {
        if let Some(Value::Array(required)) = obj.get("required") {
            for r in required.iter().filter_map(Value::as_str) {
                if !value.get(r).is_some() {
                    return Err(format!("{path}: missing required property `{r}`"));
                }
            }
        }
        if let Some(Value::Object(props)) = obj.get("properties") {
            for (k, sub) in props {
                if let Some(v) = value.get(k) {
                    check(sub, v, &format!("{path}.{k}"))?;
                }
            }
        }
    }

    if value.is_array() {
        if let Some(items) = obj.get("items") {
            if let Value::Array(list) = value {
                for (i, item) in list.iter().enumerate() {
                    check(items, item, &format!("{path}[{i}]"))?;
                }
            }
        }
    }

    if let Some(n) = value.as_f64() {
        if let Some(min) = obj.get("minimum").and_then(Value::as_f64) {
            if n < min {
                return Err(format!("{path}: {n} < minimum {min}"));
            }
        }
        if let Some(max) = obj.get("maximum").and_then(Value::as_f64) {
            if n > max {
                return Err(format!("{path}: {n} > maximum {max}"));
            }
        }
    }

    if let Some(s) = value.as_str() {
        if let Some(min) = obj.get("minLength").and_then(Value::as_u64) {
            if (s.chars().count() as u64) < min {
                return Err(format!("{path}: shorter than minLength {min}"));
            }
        }
        if let Some(max) = obj.get("maxLength").and_then(Value::as_u64) {
            if (s.chars().count() as u64) > max {
                return Err(format!("{path}: longer than maxLength {max}"));
            }
        }
    }

    Ok(())
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn type_required_enum_items() {
        let schema = json!({
            "type": "object",
            "required": ["q"],
            "properties": {
                "q": { "type": "string", "minLength": 1, "maxLength": 8 },
                "limit": { "type": "integer", "minimum": 1, "maximum": 10 },
                "mode": { "enum": ["fast", "slow"] },
                "tags": { "type": "array", "items": { "type": "string" } }
            }
        });
        assert!(validate(
            &schema,
            &json!({"q": "hi", "limit": 3, "mode": "fast", "tags": ["a"]})
        )
        .is_ok());
        assert!(validate(&schema, &json!({})).unwrap_err().contains("`q`"));
        assert!(validate(&schema, &json!({"q": ""}))
            .unwrap_err()
            .contains("minLength"));
        assert!(validate(&schema, &json!({"q": "toolongstring"}))
            .unwrap_err()
            .contains("maxLength"));
        assert!(validate(&schema, &json!({"q": "x", "limit": 0}))
            .unwrap_err()
            .contains("minimum"));
        assert!(validate(&schema, &json!({"q": "x", "limit": 1.5}))
            .unwrap_err()
            .contains("integer"));
        assert!(validate(&schema, &json!({"q": "x", "mode": "medium"}))
            .unwrap_err()
            .contains("enum"));
        assert!(validate(&schema, &json!({"q": "x", "tags": [1]}))
            .unwrap_err()
            .contains("$"));
    }

    #[test]
    fn path_precision() {
        let schema = json!({
            "type": "object",
            "properties": { "a": { "type": "object", "properties": { "b": { "type": "boolean" } } } }
        });
        let err = validate(&schema, &json!({"a": {"b": "yes"}})).unwrap_err();
        assert!(err.contains("$.a.b"), "{err}");
    }

    #[test]
    fn sanity_rejects_unknown_types() {
        assert!(schema_is_sane(&json!({"type": "object"})).is_ok());
        assert!(schema_is_sane(&json!({"type": "banana"})).is_err());
        assert!(schema_is_sane(&json!({"type": ["string", "null"]})).is_ok());
        assert!(schema_is_sane(&json!(42)).is_err());
        // No type: nothing to enforce, accepted.
        assert!(schema_is_sane(&json!({"required": ["x"]})).is_ok());
    }

    #[test]
    fn unknown_keywords_are_ignored_not_trusted() {
        // `format`/`pattern` are not enforced — args that a strict schema
        // would reject still pass, because policy gates (not client schemas)
        // are the enforcement authority. Documented behavior.
        let schema = json!({"type": "string", "format": "email"});
        assert!(validate(&schema, &json!("not-an-email")).is_ok());
    }
}
