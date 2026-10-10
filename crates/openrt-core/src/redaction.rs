//! Secret redaction boundary (SEC-05).
//!
//! Events emitted on the bus carry rich payloads (prompts, tool arguments,
//! results). Before an event is persisted to the event log or rendered to
//! stderr it passes through a [`RedactionPolicy`] so secret-like values never
//! land in telemetry. This is a *boundary*: the in-memory event on the bus is
//! untouched (so consumers that need the full payload still see it); sinks
//! redact their own copy.
//!
//! The policy is field-name based and deliberately conservative: a JSON value
//! is replaced with [`REDACTED`] when its key names a well-known secret field
//! (API tokens, passwords, private keys, cookies, ...). Field names are
//! matched case-insensitively. This is not a general PII censoring layer; the
//! roadmap keeps telemetry content purposeful and 0600-protected at rest.

use serde_json::Value;

/// Placeholder written where a secret-like value was removed.
pub const REDACTED: &str = "[REDACTED]";

/// Configuration of which keys count as secret-like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactionPolicy {
    sensitive_names: Vec<String>,
    replacement: String,
}

/// The conservative default: well-known secret field names, lower-cased.
pub fn default_sensitive_names() -> &'static [&'static str] {
    &[
        "api_key",
        "apikey",
        "api-key",
        "access_token",
        "id_token",
        "refresh_token",
        "token",
        "secret",
        "secret_key",
        "secret-key",
        "client_secret",
        "password",
        "passwd",
        "passphrase",
        "authorization",
        "auth",
        "cookie",
        "set_cookie",
        "set-cookie",
        "private_key",
        "private-key",
        "authorization_code",
    ]
}

impl Default for RedactionPolicy {
    fn default() -> Self {
        Self {
            sensitive_names: default_sensitive_names()
                .iter()
                .map(|s| s.to_string())
                .collect(),
            replacement: REDACTED.to_string(),
        }
    }
}

impl RedactionPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add further names (case-insensitive, lower-case them yourself or not —
    /// matching lower-cases both sides).
    pub fn with_extra_names(mut self, names: &[&str]) -> Self {
        for n in names {
            self.sensitive_names.push(n.to_string());
        }
        self
    }

    /// Custom placeholder.
    pub fn with_replacement(mut self, replacement: impl Into<String>) -> Self {
        self.replacement = replacement.into();
        self
    }

    pub fn is_sensitive_name(&self, name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        self.sensitive_names.contains(&lower)
    }

    /// Conservative value-pattern match (SEC-05): well-known token/key shapes
    /// even when the field name is not in the sensitive list (e.g. free-form
    /// `notes`, `args`, nested strings). Deliberately prefix/substring-based
    /// and low false-positive: never redacts ordinary prose or file paths.
    pub fn is_sensitive_value(&self, s: &str) -> bool {
        let t = s.trim();
        if t.is_empty() || t.len() < 12 {
            return false;
        }
        let lower = t.to_ascii_lowercase();
        // PEM / private key blocks
        if t.starts_with("-----BEGIN") && t.contains("PRIVATE KEY") {
            return true;
        }
        // Authorization headers (prefix or embedded in a larger command string)
        if lower.starts_with("bearer ") || lower.contains("bearer ") || lower.starts_with("basic ")
        {
            return true;
        }
        // Common provider / cloud token shapes — match at start or after a
        // delimiter so `deploy with sk-...` is caught without flagging
        // ordinary words that merely contain those substrings.
        const PREFIXES: &[&str] = &[
            "sk-", // OpenAI / Anthropic-style
            "sk_live_",
            "sk_test_",
            "pk_live_",
            "ghp_", // GitHub personal
            "gho_",
            "ghu_",
            "ghs_",
            "github_pat_",
            "xoxb-", // Slack
            "xoxp-",
            "xoxa-",
            "xoxs-",
            "eyJ",    // JWT (base64 header)
            "akia",   // AWS key id (lower-cased compare)
            "glpat-", // GitLab
            "npm_",
            "AIza", // Google API
        ];
        PREFIXES.iter().any(|p| {
            if lower.starts_with(p) {
                return true;
            }
            // Embedded: each occurrence must start at a token boundary
            // (start of string, or right after a common delimiter).
            let mut from = 0usize;
            while let Some(rel) = lower[from..].find(p) {
                let idx = from + rel;
                let boundary_ok = idx == 0
                    || lower[..idx]
                        .chars()
                        .last()
                        .map(|c| matches!(c, ' ' | '"' | '\'' | '=' | ':' | '/' | '(' | ',' | ';'))
                        .unwrap_or(false);
                if boundary_ok {
                    return true;
                }
                from = idx + p.len();
                if from >= lower.len() {
                    break;
                }
            }
            false
        })
    }

    /// Walk a JSON value in place, redacting values under sensitive keys and
    /// sensitive-shaped string values under any key. Objects/arrays are
    /// traversed recursively; the value of a sensitive key is replaced
    /// wholesale (even if it is itself an object).
    pub fn redact_value(&self, value: &mut Value) {
        match value {
            Value::Object(map) => {
                let keys: Vec<String> = map.keys().cloned().collect();
                for k in keys {
                    if self.is_sensitive_name(&k) {
                        if let Some(v) = map.get_mut(&k) {
                            *v = Value::String(self.replacement.clone());
                        }
                    } else if let Some(v) = map.get_mut(&k) {
                        self.redact_value(v);
                    }
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.redact_value(item);
                }
            }
            Value::String(s) => {
                if self.is_sensitive_value(s) {
                    *s = self.replacement.clone();
                }
            }
            _ => {}
        }
    }

    /// Redact a rendered payload (JSON or free-form text) before it is
    /// persisted in a transcript or fed back to a model. JSON objects and
    /// arrays are walked by field name and value shape; anything else is
    /// checked line-by-line against the value patterns.
    pub fn redact_text(&self, s: &str) -> String {
        let trimmed = s.trim_start();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            if let Ok(mut v) = serde_json::from_str::<Value>(s) {
                self.redact_value(&mut v);
                if let Ok(out) = serde_json::to_string(&v) {
                    return out;
                }
            }
        }
        let mut lines: Vec<String> = s
            .lines()
            .map(|l| match self.redact_keyed_line(l) {
                Some(red) => red,
                None if self.is_sensitive_value(l) => self.replacement.clone(),
                None => l.to_string(),
            })
            .collect();
        if s.ends_with('\n') {
            lines.push(String::new());
        }
        lines.join("\n")
    }

    /// A `key: value` / `key = value` line whose key is sensitive: keep the
    /// key and the line structure (operators need to see that a password WAS
    /// present), replace the value. Plain-text tool output is full of these
    /// — the JSON field walk never sees them, and value-shape patterns only
    /// catch token-like strings, so `password: <free text>` used to reach
    /// the model and the durable transcript raw.
    fn redact_keyed_line(&self, line: &str) -> Option<String> {
        let idx = [':', '='].iter().find_map(|d| line.find(*d))?;
        let (key_raw, rest) = line.split_at(idx);
        let key = key_raw.trim();
        if key.is_empty() {
            return None;
        }
        // The sensitive name is either the whole key (`password:`) or the
        // last word of a short prose lead-in (`The password:`).
        let last = key.split_whitespace().last()?;
        if !self.is_sensitive_name(key) && !self.is_sensitive_name(last) {
            return None;
        }
        let value = rest[1..].trim_start();
        if value.is_empty() {
            return None;
        }
        // Keep the original key text and separator, swap only the value.
        Some(format!("{key_raw}{} {}", &rest[..1], self.replacement))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{DomainEvent, EventKind};

    #[test]
    fn redacts_secret_keys_case_insensitively() {
        let mut v = serde_json::json!({
            "Api_Key": "sk-1234",
            "body": { "password": "hunter2", "path": "/etc/known" },
            "plain": { "name": "openrt", "nested": { "token": "abc" } }
        });
        RedactionPolicy::default().redact_value(&mut v);
        assert_eq!(v["Api_Key"], REDACTED);
        assert_eq!(v["body"]["password"], REDACTED);
        assert_eq!(v["body"]["path"], "/etc/known");
        assert_eq!(v["plain"]["name"], "openrt");
        assert_eq!(v["plain"]["nested"]["token"], REDACTED);
    }

    #[test]
    fn array_elements_redacted() {
        let mut v = serde_json::json!([{ "secret": "x" }, { "ok": 1 }]);
        RedactionPolicy::default().redact_value(&mut v);
        assert_eq!(v[0]["secret"], REDACTED);
        assert_eq!(v[1]["ok"], 1);
    }

    #[test]
    fn scalar_values_untouched() {
        let mut v = serde_json::json!("just a string");
        RedactionPolicy::default().redact_value(&mut v);
        assert_eq!(v, "just a string");
    }

    #[test]
    fn value_pattern_tokens_redacted_under_innocent_keys() {
        let mut v = serde_json::json!({
            "notes": "deploy with sk-proj-abcdef1234567890",
            "args": { "cmd": "curl -H \"Authorization: Bearer abcdefghijklmnop\" https://x" },
            "body": { "password": "hunter2-not-a-pattern" },
            "msg": "hello world from openrt",
            "path": "/etc/known_hosts"
        });
        RedactionPolicy::default().redact_value(&mut v);
        // Nested secret *name* still redacted wholesale.
        assert_eq!(v["body"]["password"], REDACTED);
        // Free-form string containing a token-like value is redacted.
        assert_eq!(v["notes"], REDACTED);
        // Bearer header shape inside a string value.
        assert_eq!(v["args"]["cmd"], REDACTED);
        // Ordinary prose and paths untouched.
        assert_eq!(v["msg"], "hello world from openrt");
        assert_eq!(v["path"], "/etc/known_hosts");
    }

    /// CANARY, NOT A CREDENTIAL: the PEM body is a literal ellipsis, so this
    /// string can never be a usable key. Credential-shaped fixtures must exist
    /// for redaction to be testable at all — they belong here and nowhere else.
    #[test]
    fn pem_private_key_blocks_redacted() {
        let pem = "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkq...\n-----END PRIVATE KEY-----";
        let mut v = serde_json::json!({ "material": pem });
        RedactionPolicy::default().redact_value(&mut v);
        assert_eq!(v["material"], REDACTED);
    }

    /// CANARIES, NOT CREDENTIALS: `ghp_xxxx…` is all literal x's,
    /// `sk-proj-abcdef1234567890xyz` is a sequential placeholder, and
    /// `token-value-long-enough` is descriptive text. Each is synthetic by
    /// construction — none is a valid secret.
    #[test]
    fn short_and_ordinary_strings_not_over_redacted() {
        assert!(!RedactionPolicy::default().is_sensitive_value("sk-1"));
        assert!(!RedactionPolicy::default().is_sensitive_value("hello"));
        assert!(!RedactionPolicy::default().is_sensitive_value(""));
        assert!(!RedactionPolicy::default().is_sensitive_value("/var/log/app.log"));
        assert!(RedactionPolicy::default().is_sensitive_value("sk-proj-abcdef1234567890xyz"));
        assert!(
            RedactionPolicy::default().is_sensitive_value("ghp_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx")
        );
        assert!(RedactionPolicy::default().is_sensitive_value("Bearer token-value-long-enough"));
    }

    #[test]
    fn custom_policy_extendable() {
        let p = RedactionPolicy::default().with_extra_names(&["card_number"]);
        let mut v = serde_json::json!({ "card_number": "4111...", "secret": "s" });
        p.redact_value(&mut v);
        assert_eq!(v["card_number"], REDACTED);
        assert_eq!(v["secret"], REDACTED);
    }

    #[test]
    fn event_data_redacted_in_place() {
        let mut ev = DomainEvent::new(EventKind::TaskCreated)
            .with_data(serde_json::json!({ "prompt": "login", "args": { "password": "pw" } }));
        RedactionPolicy::default().redact_value(&mut ev.data);
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains(REDACTED));
        assert!(!s.contains("\"pw\""));
    }

    #[test]
    fn redact_text_json_by_field_and_value() {
        let p = RedactionPolicy::default();
        let out = p.redact_text(r#"{"token":"abcdef123456","note":"ok"}"#);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["token"], REDACTED);
        assert_eq!(v["note"], "ok");
    }

    #[test]
    fn redact_text_line_by_line() {
        let p = RedactionPolicy::default();
        let out = p.redact_text("export TOKEN\nAuthorization: Bearer abcdef123456\nall good");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[1], "Authorization: [REDACTED]");
        assert_eq!(lines[2], "all good");
        assert!(!out.contains("abcdef123456"));
    }

    #[test]
    fn redact_text_keyed_secret_lines_keep_the_key_redact_the_value() {
        let p = RedactionPolicy::default();
        let out = p.redact_text(
            "service: demo\n\
             username: alice\n\
             password: hunter2-plain-text\n\
             api_key = sk-live-12345\n\
             TOKEN: abc123def456\n\
             note: ordinary line that must stay readable\n",
        );
        assert!(!out.contains("hunter2-plain-text"), "{out}");
        assert!(!out.contains("sk-live-12345"), "{out}");
        assert!(!out.contains("abc123def456"), "{out}");
        // Keys and non-sensitive lines survive untouched.
        assert!(out.contains("password: [REDACTED]"), "{out}");
        assert!(out.contains("api_key = [REDACTED]"), "{out}");
        assert!(out.contains("TOKEN: [REDACTED]"), "{out}");
        assert!(out.contains("username: alice"), "{out}");
        assert!(
            out.contains("note: ordinary line that must stay readable"),
            "{out}"
        );
        // Prose lead-in: the key is still the sensitive word.
        let out = p.redact_text("The password: leaked-value-9876 was changed.");
        assert!(!out.contains("leaked-value-9876"), "{out}");
        assert!(out.contains("[REDACTED]"), "{out}");
    }

    #[test]
    fn redact_text_passthrough_ordinary_output() {
        let p = RedactionPolicy::default();
        let s = "hello world\n./src/main.rs:12\nstatus: ok";
        assert_eq!(p.redact_text(s), s);
    }
}

/// Repository hygiene: credential-shaped literals are legitimate *only* inside
/// test code, and only as synthetic canaries.
///
/// A redaction implementation cannot be tested without strings that look like
/// secrets. The risk is that such a literal escapes into a fixture, a doc, or a
/// snapshot and is later mistaken for — or mistaken to be — a real credential.
/// This test draws that line mechanically: any credential-shaped literal outside
/// a `#[cfg(test)]` region is a failure.
///
/// Patterns deliberately narrow: provider prefixes plus a run of token
/// characters, and PEM headers. Prose that merely contains "sk-" (`task-level`)
/// does not match.
#[test]
fn credential_shaped_literals_stay_inside_test_code() {
    const PATTERNS: &[&str] = &[
        "sk-",
        "ghp_",
        "gho_",
        "ghs_",
        "github_pat_",
        "AKIA",
        "xoxb-",
        "xoxp-",
        "BEGIN RSA PRIVATE KEY",
        "BEGIN OPENSSH PRIVATE KEY",
    ];
    // Long enough that an ordinary identifier cannot match.
    const MIN_RUN: usize = 16;

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut offenders: Vec<String> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if p.extension().is_none_or(|x| x != "rs") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            let Some(test_start) = text.find("#[cfg(test)]") else {
                continue;
            };
            for (n, line) in text[..test_start].lines().enumerate() {
                for pat in PATTERNS {
                    let Some(at) = line.find(pat) else { continue };
                    let run = line[at + pat.len()..]
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                        .count();
                    if run >= MIN_RUN {
                        offenders.push(format!(
                            "{}:{}: {pat}…",
                            p.strip_prefix(root).unwrap_or(&p).display(),
                            n + 1
                        ));
                    }
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "credential-shaped literal outside test code — move it into a #[cfg(test)] \
         region or use an obviously synthetic placeholder:\n{}",
        offenders.join("\n")
    );
}
