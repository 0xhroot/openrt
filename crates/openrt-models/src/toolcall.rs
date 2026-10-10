//! Canonical tool-call normalization at the provider boundary (PART 2-4).
//!
//! Every networked provider funnels its raw `tool_calls` entries through
//! [`canonical_tool_call_id`] and [`normalize_arguments`] before a
//! [`crate::types::ProviderToolCall`] crosses into the agent loop:
//!
//! * **IDs** — a provider-supplied string id is preserved exactly; a
//!   numeric id becomes its decimal string; missing/null ids get a
//!   process-unique OpenRT-generated `openrt-call-<ts>-<n>` so no parse
//!   ever fails (or fabricates a *different* id for a real one).
//! * **Arguments** — stringified JSON objects/arrays are parsed back to
//!   structured values; missing/null becomes `{}`; raw prose stays a
//!   string so the agent loop can reject it as a structured failure
//!   instead of executing text.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

/// Monotonic per-process sequence for generated call ids (combined with a
/// wall-clock component so ids never collide across restarts either).
static CALL_ID_SEQ: AtomicU64 = AtomicU64::new(0);

/// Canonicalize a tool-call id from a provider response (PART 2).
///
/// * `string` (non-empty) → preserved exactly (round-trip stability, PART 7).
/// * `number` → decimal string form.
/// * missing / `null` / empty / other → freshly generated
///   `openrt-call-<unix-ms>-<seq>`.
pub fn canonical_tool_call_id(raw: Option<&Value>) -> String {
    match raw {
        Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => {
            let seq = CALL_ID_SEQ.fetch_add(1, Ordering::Relaxed);
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            format!("openrt-call-{ms}-{seq}")
        }
    }
}

/// Normalize raw `function.arguments` (PART 4).
///
/// * missing / `null` → `{}` (a call with no arguments is a valid object).
/// * object / array → kept (the tool's schema validates arrays).
/// * string → parsed when it holds a JSON object/array; otherwise left as
///   the raw string — the agent loop rejects non-structured input with a
///   structured tool failure, so prose is never executed.
/// * other scalars → left as-is (rejected by the agent loop the same way).
pub fn normalize_arguments(raw: Option<&Value>) -> Value {
    match raw {
        None | Some(Value::Null) => Value::Object(Default::default()),
        Some(v @ Value::Object(_)) | Some(v @ Value::Array(_)) => v.clone(),
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(v @ Value::Object(_)) | Ok(v @ Value::Array(_)) => v,
            // JSON scalar or not JSON at all: keep the raw text so the
            // agent loop can reject it without ever executing it.
            Ok(_) | Err(_) => Value::String(s.clone()),
        },
        Some(other) => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn string_id_is_preserved_exactly() {
        assert_eq!(
            canonical_tool_call_id(Some(&json!("call_abc-1"))),
            "call_abc-1"
        );
    }

    #[test]
    fn numeric_id_becomes_string() {
        assert_eq!(canonical_tool_call_id(Some(&json!(42))), "42");
        assert_eq!(canonical_tool_call_id(Some(&json!(7.5))), "7.5");
    }

    #[test]
    fn missing_null_empty_ids_are_generated() {
        let a = canonical_tool_call_id(None);
        let b = canonical_tool_call_id(Some(&Value::Null));
        let c = canonical_tool_call_id(Some(&json!("")));
        let d = canonical_tool_call_id(Some(&json!("   ")));
        for id in [&a, &b, &c, &d] {
            assert!(id.starts_with("openrt-call-"), "{id}");
            assert!(id.len() > "openrt-call-".len(), "{id}");
        }
        assert_ne!(a, b, "generated ids must be unique");
        assert_ne!(a, c);
        assert_ne!(b, d);
    }

    #[test]
    fn bool_id_is_generated_not_stringified() {
        // Booleans are not meaningful ids — treat like missing rather than
        // inventing "true" as a correlation key.
        assert!(canonical_tool_call_id(Some(&json!(true))).starts_with("openrt-call-"));
    }

    #[test]
    fn arguments_object_passes_through() {
        let v = json!({"path": "/tmp/x"});
        assert_eq!(normalize_arguments(Some(&v)), v);
    }

    #[test]
    fn stringified_object_is_parsed() {
        assert_eq!(
            normalize_arguments(Some(&json!("{\"text\":\"hi\"}"))),
            json!({"text": "hi"})
        );
    }

    #[test]
    fn stringified_array_is_parsed() {
        assert_eq!(
            normalize_arguments(Some(&json!("[1,2,3]"))),
            json!([1, 2, 3])
        );
    }

    #[test]
    fn native_array_passes_through() {
        let v = json!([1, "two"]);
        assert_eq!(normalize_arguments(Some(&v)), v);
    }

    #[test]
    fn missing_and_null_become_empty_object() {
        assert_eq!(normalize_arguments(None), json!({}));
        assert_eq!(normalize_arguments(Some(&Value::Null)), json!({}));
    }

    #[test]
    fn prose_stays_raw_string_for_agent_rejection() {
        assert_eq!(
            normalize_arguments(Some(&json!("please run the thing"))),
            json!("please run the thing")
        );
        // JSON scalar (stringified string/number) is not structured input —
        // the raw text is kept so the agent loop rejects it.
        assert_eq!(
            normalize_arguments(Some(&json!("\"just a string\""))),
            json!("\"just a string\"")
        );
        assert_eq!(normalize_arguments(Some(&json!("123"))), json!("123"));
    }

    #[test]
    fn scalar_arguments_stay_rejectable() {
        assert_eq!(normalize_arguments(Some(&json!(5))), json!(5));
        assert_eq!(normalize_arguments(Some(&json!(true))), json!(true));
    }
}
