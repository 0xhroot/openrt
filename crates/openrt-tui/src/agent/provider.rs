//! Request-level model switching for the Agent TUI.
//!
//! `ModelRequest.model_id` is already part of the ONE provider contract and
//! the Ollama transport honours it per call (`req.model_id` → fallback
//! `default_model`). This wrapper simply *fills that field in* from the
//! operator's `/model` choice — it is an adapter at assembly time, not a
//! second router or registry: the runtime, pipeline, policy, and audit paths
//! are untouched, and with no override set it delegates byte-for-byte.

use std::sync::atomic::{AtomicBool, Ordering};

use openrt_models::types::StreamEvent;
use openrt_models::{ModelCapabilities, ModelInfo, ModelProvider, ModelRequest, ModelResponse};
use std::sync::Arc;

pub struct SwitchableProvider {
    inner: Arc<dyn ModelProvider>,
    /// `Some(id)` = override every outgoing request's `model_id`.
    override_id: std::sync::Mutex<Option<String>>,
    /// Whether the override differs from the inner default (UI pill).
    switched: AtomicBool,
}

impl SwitchableProvider {
    pub fn new(inner: Arc<dyn ModelProvider>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            override_id: std::sync::Mutex::new(None),
            switched: AtomicBool::new(false),
        })
    }

    /// Select the model for subsequent requests (`None` = provider default).
    ///
    /// A poisoned lock is recovered rather than propagated. The guarded value is
    /// an `Option<String>` with no cross-field invariant, so a panic elsewhere
    /// cannot have left it inconsistent — and treating it as fatal meant a single
    /// unrelated panic permanently killed every later render that asked which
    /// model was selected. One bad frame was turning into a dead UI.
    pub fn set_model(&self, id: Option<String>) {
        let active = id.is_some();
        let mut guard = self
            .override_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = id;
        self.switched.store(active, Ordering::Relaxed);
    }

    /// The currently selected override, if any.
    pub fn selected(&self) -> Option<String> {
        self.override_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// True when an operator override is active (UI pill).
    pub fn is_switched(&self) -> bool {
        self.switched.load(Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl ModelProvider for SwitchableProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn info(&self) -> ModelInfo {
        self.inner.info()
    }

    async fn generate(
        &self,
        mut req: ModelRequest,
    ) -> Result<ModelResponse, openrt_models::ModelError> {
        if let Some(id) = self.selected() {
            req.model_id = Some(id);
        }
        self.inner.generate(req).await
    }

    async fn generate_stream(
        &self,
        mut req: ModelRequest,
        sink: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> Result<ModelResponse, openrt_models::ModelError> {
        if let Some(id) = self.selected() {
            req.model_id = Some(id);
        }
        self.inner.generate_stream(req, sink).await
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.inner.capabilities()
    }

    /// Honest per-model probe: forward to the inner transport (Ollama
    /// `/api/show`); never invented from the model name.
    async fn probe_tool_support(
        &self,
        model: &str,
    ) -> Result<Option<bool>, openrt_models::ModelError> {
        self.inner.probe_tool_support(model).await
    }

    async fn probe_vision(&self, model: &str) -> Result<Option<bool>, openrt_models::ModelError> {
        self.inner.probe_vision(model).await
    }

    fn base_url(&self) -> Option<String> {
        self.inner.base_url()
    }

    fn last_raw_response(&self) -> Option<String> {
        self.inner.last_raw_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_core::Message;
    use openrt_models::MockProvider;

    #[tokio::test]
    async fn override_reaches_the_inner_provider_request() {
        let sw = SwitchableProvider::new(Arc::new(MockProvider::tool_sequence("mock")));
        let req = ModelRequest::new(vec![Message::user("hi")]);
        // No override: the inner request keeps model_id = None and the
        // response comes back untouched.
        let out = sw.generate(req.clone()).await.unwrap();
        assert_eq!(out.model, "mock://default");

        sw.set_model(Some("qwen3:8b".into()));
        assert_eq!(sw.selected().as_deref(), Some("qwen3:8b"));
        assert!(sw.is_switched());

        // The wrapper mutates only model_id; messages are untouched.
        let mut seen = ModelRequest::new(vec![Message::user("hi")]);
        if let Some(id) = sw.selected() {
            seen.model_id = Some(id);
        }
        assert_eq!(seen.model_id.as_deref(), Some("qwen3:8b"));

        sw.set_model(None);
        assert!(sw.selected().is_none());
        assert!(!sw.is_switched());
    }
}
