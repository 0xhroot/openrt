//! Deterministic in-memory backend (tests/CI, `fabric.browser.backend: mock`).
//!
//! Records every call so integration tests can assert the exact operation
//! sequence, and emulates the `JSON.stringify` result contract the tools use.

use std::sync::Mutex;

use serde_json::{json, Value};

use openrt_fabric::FabricError;

use crate::backend::{unwrap_stringified, BrowserBackend};

/// 1×1 transparent PNG.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x62, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

struct State {
    current_url: Option<String>,
    title: String,
    calls: Vec<String>,
    closed: bool,
}

pub struct MockBackend {
    state: Mutex<State>,
}

impl MockBackend {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                current_url: None,
                title: String::from("Mock Page"),
                calls: Vec::new(),
                closed: false,
            }),
        }
    }

    /// Recorded operation names, in order (for sequence assertions).
    pub fn calls(&self) -> Vec<String> {
        self.state.lock().expect("mock poisoned").calls.clone()
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl BrowserBackend for MockBackend {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn navigate(&self, url: &str) -> Result<Value, FabricError> {
        let mut st = self.state.lock().expect("mock poisoned");
        st.calls.push(format!("navigate:{url}"));
        st.current_url = Some(url.to_string());
        Ok(json!({ "url": url, "title": st.title.clone() }))
    }

    async fn evaluate(&self, expr: &str) -> Result<Value, FabricError> {
        let mut st = self.state.lock().expect("mock poisoned");
        if st.closed {
            return Err(FabricError::backend_failure(
                "browser backend",
                "mock backend is closed",
                "browser.*",
                "start a new session (call a tool other than browser.close)",
            ));
        }
        st.calls.push(format!("evaluate:{}", expr.len()));
        // Emulate the tools' `JSON.stringify(...)` contract without running JS:
        // recognize the marker each tool's expression returns and answer with
        // the same result shape. Downloads deliberately fail (the mock never
        // fakes remote bytes) and everything unmarked stays generic.
        let raw = if expr.contains("fetch(") {
            json!({ "ok": false, "error": "mock backend does not fetch remote data" })
        } else if expr.contains("uploaded:true") {
            json!({ "uploaded": true })
        } else if expr.contains("typed:true") {
            json!({ "typed": true, "submitted": false })
        } else if expr.contains("clicked:true") {
            json!({ "clicked": true })
        } else if expr.contains("selected:") {
            json!({ "selected": true, "value": "" })
        } else if expr.contains("count:els.length") {
            json!({ "count": 0, "texts": [] })
        } else if expr.contains("submitted:true") {
            json!({ "submitted": true })
        } else {
            json!({ "mock": true, "expr_chars": expr.len() })
        };
        Ok(unwrap_stringified(&raw))
    }

    async fn screenshot(&self) -> Result<Vec<u8>, FabricError> {
        let mut st = self.state.lock().expect("mock poisoned");
        st.calls.push("screenshot".into());
        Ok(TINY_PNG.to_vec())
    }

    async fn close(&self) -> Result<(), FabricError> {
        let mut st = self.state.lock().expect("mock poisoned");
        st.closed = true;
        st.calls.push("close".into());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn records_call_sequence() {
        let m = MockBackend::new();
        m.navigate("about:blank").await.unwrap();
        m.evaluate("JSON.stringify({})").await.unwrap();
        m.screenshot().await.unwrap();
        m.close().await.unwrap();
        assert_eq!(
            m.calls(),
            vec![
                "navigate:about:blank".to_string(),
                "evaluate:18".to_string(),
                "screenshot".to_string(),
                "close".to_string(),
            ]
        );
        assert!(m.evaluate("x").await.is_err());
    }

    #[test]
    fn tiny_png_has_signature() {
        assert!(TINY_PNG.starts_with(&[0x89, 0x50, 0x4E, 0x47]));
    }
}
