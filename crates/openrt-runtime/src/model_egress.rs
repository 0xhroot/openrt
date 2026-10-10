//! Phase 13 (Part E): the ONE model-egress guard.
//!
//! ADR-010 requires that in Sovereign Mode *every* network entry point —
//! explicitly including provider calls — is refused **before any client call
//! is made**. Model transports are network entry points, so the runtime
//! wraps its single provider and its vision grounding in guards that answer
//! through the existing URL egress authority
//! ([`openrt_fabric::net_guard::ensure_url_allowed`]): loopback and local
//! schemes stay reachable in Sovereign (ADR-003 keeps loopback), everything
//! else must pass mode + `netpolicy.grants`. Every refusal is a clear error
//! *and* a chained audit row (`model.egress`) — no second policy, no silent
//! fallback, no hidden transport.

use std::sync::Arc;

use async_trait::async_trait;
use openrt_computer::VisionGrounding;
use openrt_fabric::net_guard::{ensure_url_allowed, SCOPE_MODEL};
use openrt_models::{
    BackendCapabilities, ContextCapability, HintReceipt, MemoryHint, ModelCapabilities, ModelError,
    ModelInfo, ModelProvider, ModelRequest, ModelResponse, ProviderLoadTelemetry,
};

/// Wraps the runtime's single [`ModelProvider`] so no networked `generate`
/// can reach a socket without passing the egress authority first. Static
/// metadata (`id`/`info`/`capabilities`/`base_url`) forwards unchanged; only
/// the network-touching call is gated.
pub(crate) struct EgressGuardedProvider {
    inner: Arc<dyn ModelProvider>,
    modes: Arc<openrt_modes::ModeManager>,
    netpol: Arc<openrt_netpol::NetworkPolicy>,
    audit: Arc<openrt_audit::AuditLog>,
}

impl EgressGuardedProvider {
    pub(crate) fn new(
        inner: Arc<dyn ModelProvider>,
        modes: Arc<openrt_modes::ModeManager>,
        netpol: Arc<openrt_netpol::NetworkPolicy>,
        audit: Arc<openrt_audit::AuditLog>,
    ) -> Self {
        Self {
            inner,
            modes,
            netpol,
            audit,
        }
    }

    /// Allow the transport or refuse with a clear reason + audit row.
    async fn allow_transport(&self) -> Result<(), ModelError> {
        let Some(url) = self.inner.base_url() else {
            // In-process / deterministic provider: nothing to gate.
            return Ok(());
        };
        if let Err(e) = ensure_url_allowed(&url, SCOPE_MODEL, &self.modes, &self.netpol).await {
            let reason = format!("{e}");
            let mut msg = format!(
                "model provider `{}` may not reach `{url}` from this mode: {reason}",
                self.inner.id()
            );
            let appended = self
                .audit
                .append(
                    "runtime",
                    "model.egress",
                    self.inner.id(),
                    "denied",
                    serde_json::json!({ "destination": url.as_str(), "reason": reason }),
                )
                .await;
            if let Err(ae) = appended {
                msg.push_str(&format!(" (audit append failed: {ae})"));
            }
            return Err(ModelError::EgressRefused(msg));
        }
        Ok(())
    }
}

#[async_trait]
impl ModelProvider for EgressGuardedProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn info(&self) -> ModelInfo {
        self.inner.info()
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.inner.capabilities()
    }

    fn context_capability(&self) -> ContextCapability {
        self.inner.context_capability()
    }

    fn backend_capabilities(&self) -> BackendCapabilities {
        self.inner.backend_capabilities()
    }

    fn base_url(&self) -> Option<String> {
        self.inner.base_url()
    }

    /// M11: load counters are passive diagnostics and forward unchanged — the
    /// guard adds no telemetry and hides none.
    fn load_telemetry(&self) -> ProviderLoadTelemetry {
        self.inner.load_telemetry()
    }

    fn last_raw_response(&self) -> Option<String> {
        self.inner.last_raw_response()
    }

    async fn generate(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        self.allow_transport().await?;
        self.inner.generate(req).await
    }

    /// Streaming is the same transport authority as `generate` (Part B):
    /// SSE bytes are egress too — one guard, one rule set.
    async fn generate_stream(
        &self,
        req: ModelRequest,
        sink: tokio::sync::mpsc::Sender<openrt_models::types::StreamEvent>,
    ) -> Result<ModelResponse, ModelError> {
        self.allow_transport().await?;
        self.inner.generate_stream(req, sink).await
    }

    /// M4 hint submission is advisory and network-free today, but when a
    /// backend transports hints it is an egress: same ONE authority as
    /// `generate`/`probe_tool_support` (Sovereign refuses a non-loopback
    /// transport before any hint reaches a socket). An unsupported hint still
    /// drops cleanly; only a refused/failed transport errors.
    async fn submit_memory_hint(&self, hint: MemoryHint) -> Result<HintReceipt, ModelError> {
        self.allow_transport().await?;
        self.inner.submit_memory_hint(hint).await
    }

    /// Capability probes are network calls too — same ONE egress authority
    /// as `generate` (Sovereign refuses a non-loopback transport before any
    /// socket opens).
    async fn probe_tool_support(&self, model: &str) -> Result<Option<bool>, ModelError> {
        self.allow_transport().await?;
        self.inner.probe_tool_support(model).await
    }
}

/// Wraps vision grounding (the single `openrt-vlm` pipeline) with the same
/// egress authority: under Sovereign a non-loopback vision endpoint is
/// refused with a reason instead of opening a socket.
pub(crate) struct EgressGuardedVision {
    endpoint: String,
    inner: Arc<dyn VisionGrounding>,
    modes: Arc<openrt_modes::ModeManager>,
    netpol: Arc<openrt_netpol::NetworkPolicy>,
    audit: Arc<openrt_audit::AuditLog>,
}

impl EgressGuardedVision {
    pub(crate) fn new(
        endpoint: impl Into<String>,
        inner: Arc<dyn VisionGrounding>,
        modes: Arc<openrt_modes::ModeManager>,
        netpol: Arc<openrt_netpol::NetworkPolicy>,
        audit: Arc<openrt_audit::AuditLog>,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            inner,
            modes,
            netpol,
            audit,
        }
    }
}

#[async_trait]
impl VisionGrounding for EgressGuardedVision {
    async fn ground(&self, image: &[u8], target: &str) -> Result<String, String> {
        if let Err(e) =
            ensure_url_allowed(&self.endpoint, SCOPE_MODEL, &self.modes, &self.netpol).await
        {
            let reason = format!("{e}");
            let mut msg = format!(
                "vision endpoint `{}` refused before any request: {reason}",
                self.endpoint
            );
            let appended = self
                .audit
                .append(
                    "runtime",
                    "model.egress",
                    "vision.vlm",
                    "denied",
                    serde_json::json!({
                        "destination": self.endpoint.as_str(),
                        "reason": reason
                    }),
                )
                .await;
            if let Err(ae) = appended {
                msg.push_str(&format!(" (audit append failed: {ae})"));
            }
            return Err(msg);
        }
        self.inner.ground(image, target).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_audit::{AuditLog, AuditQuery};
    use openrt_core::Message;
    use openrt_modes::{ExecutionMode, ModeManager};
    use openrt_netpol::NetworkPolicy;
    use openrt_persistence::MemoryStore;

    struct StubProvider {
        id: String,
        base_url: Option<String>,
        calls: std::sync::atomic::AtomicUsize,
        probes: std::sync::atomic::AtomicUsize,
        hints: std::sync::atomic::AtomicUsize,
    }

    impl StubProvider {
        fn new(id: &str, base_url: Option<&str>) -> Arc<Self> {
            Arc::new(Self {
                id: id.to_string(),
                base_url: base_url.map(str::to_string),
                calls: std::sync::atomic::AtomicUsize::new(0),
                probes: std::sync::atomic::AtomicUsize::new(0),
                hints: std::sync::atomic::AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl ModelProvider for StubProvider {
        fn id(&self) -> &str {
            &self.id
        }

        fn info(&self) -> ModelInfo {
            ModelInfo::new(&self.id, openrt_models::ProviderKind::Mock, None)
        }

        async fn generate(&self, _req: ModelRequest) -> Result<ModelResponse, ModelError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ModelResponse::with_text(self.id.clone(), "ok"))
        }

        async fn probe_tool_support(&self, _model: &str) -> Result<Option<bool>, ModelError> {
            self.probes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Some(true))
        }

        async fn submit_memory_hint(&self, _hint: MemoryHint) -> Result<HintReceipt, ModelError> {
            self.hints.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(HintReceipt::Unsupported)
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                supports_tools: true,
                supports_streaming: false,
                context_window: None,
            }
        }

        fn base_url(&self) -> Option<String> {
            self.base_url.clone()
        }
    }

    struct StubVision {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl StubVision {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl VisionGrounding for StubVision {
        async fn ground(&self, _image: &[u8], _target: &str) -> Result<String, String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok("1,2".into())
        }
    }

    struct Ctx {
        modes: Arc<ModeManager>,
        netpol: Arc<NetworkPolicy>,
        audit: Arc<AuditLog>,
    }

    fn ctx() -> Ctx {
        let store: Arc<dyn openrt_persistence::PersistenceStore> = Arc::new(MemoryStore::new());
        Ctx {
            modes: Arc::new(ModeManager::new(store.clone())),
            netpol: Arc::new(NetworkPolicy::new(store.clone())),
            audit: Arc::new(AuditLog::new(store)),
        }
    }

    fn request() -> ModelRequest {
        ModelRequest::new(vec![Message::user("hi")])
    }

    #[tokio::test]
    async fn sovereign_refuses_external_provider_before_any_client_call() {
        let c = ctx();
        let stub = StubProvider::new("ext", Some("https://models.example.com"));
        let g = EgressGuardedProvider::new(
            stub.clone(),
            c.modes.clone(),
            c.netpol.clone(),
            c.audit.clone(),
        );
        let err = g.generate(request()).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("egress"), "clear reason expected: {msg}");
        assert!(msg.contains("models.example.com"), "{msg}");
        // Refused BEFORE any client call: the inner provider never ran.
        assert_eq!(
            stub.calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no client call may happen on refusal"
        );
        // Refusal is audited on the chained log.
        let rows = c
            .audit
            .query(&AuditQuery::new().with_action_prefix("model.egress"))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].entry.result, "denied");
        assert_eq!(rows[0].entry.subject, "ext");
        c.audit.verify().await.unwrap();
    }

    #[tokio::test]
    async fn loopback_provider_stays_reachable_under_sovereign() {
        let c = ctx();
        let stub = StubProvider::new("local", Some("http://127.0.0.1:11434"));
        let g = EgressGuardedProvider::new(
            stub.clone(),
            c.modes.clone(),
            c.netpol.clone(),
            c.audit.clone(),
        );
        let resp = g.generate(request()).await.unwrap();
        assert_eq!(resp.text, "ok");
        assert_eq!(
            stub.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "loopback keeps local models reachable (ADR-003)"
        );
    }

    #[tokio::test]
    async fn deterministic_provider_is_never_gated() {
        let c = ctx();
        let stub = StubProvider::new("mock", None);
        let g = EgressGuardedProvider::new(
            stub.clone(),
            c.modes.clone(),
            c.netpol.clone(),
            c.audit.clone(),
        );
        g.generate(request()).await.unwrap();
        assert_eq!(stub.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn probe_tool_support_is_forwarded_through_the_guard() {
        let c = ctx();
        let stub = StubProvider::new("local", Some("http://127.0.0.1:11434"));
        let g = EgressGuardedProvider::new(
            stub.clone(),
            c.modes.clone(),
            c.netpol.clone(),
            c.audit.clone(),
        );
        let supported = g.probe_tool_support("llama3.1:8b").await.unwrap();
        assert_eq!(supported, Some(true));
        assert_eq!(stub.probes.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn sovereign_refuses_a_probe_before_any_request() {
        let c = ctx();
        let stub = StubProvider::new("ext", Some("https://models.example.com"));
        let g = EgressGuardedProvider::new(
            stub.clone(),
            c.modes.clone(),
            c.netpol.clone(),
            c.audit.clone(),
        );
        let err = g.probe_tool_support("ext-model").await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("egress"), "{msg}");
        assert_eq!(
            stub.probes.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "probe is a network call — same pre-socket refusal as generate"
        );
        let rows = c
            .audit
            .query(&AuditQuery::new().with_action_prefix("model.egress"))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].entry.result, "denied");
    }

    #[tokio::test]
    async fn provisioning_with_grant_opens_model_scope() {
        let c = ctx();
        c.modes
            .transition("op", ExecutionMode::Provisioning, "test")
            .await
            .unwrap();
        c.netpol
            .grant(
                "op",
                SCOPE_MODEL,
                vec!["models.example.com".into(), "*.example.com:443".into()],
                "test grant",
            )
            .await
            .unwrap();
        let stub = StubProvider::new("ext", Some("https://models.example.com"));
        let g = EgressGuardedProvider::new(
            stub.clone(),
            c.modes.clone(),
            c.netpol.clone(),
            c.audit.clone(),
        );
        g.generate(request()).await.unwrap();
        assert_eq!(stub.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    fn hint() -> MemoryHint {
        openrt_models::MemoryHint::advisory("m", openrt_models::HintPriority::Low, "advisory")
    }

    #[tokio::test]
    async fn hint_submission_is_forwarded_through_the_guard() {
        let c = ctx();
        let stub = StubProvider::new("local", Some("http://127.0.0.1:11434"));
        let g = EgressGuardedProvider::new(
            stub.clone(),
            c.modes.clone(),
            c.netpol.clone(),
            c.audit.clone(),
        );
        let receipt = g.submit_memory_hint(hint()).await.unwrap();
        assert_eq!(receipt, HintReceipt::Unsupported);
        assert!(receipt.is_noop());
        assert_eq!(stub.hints.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Capabilities forward unchanged as static metadata.
        assert_eq!(g.backend_capabilities(), BackendCapabilities::default());
    }

    #[tokio::test]
    async fn sovereign_refuses_a_hint_before_any_request() {
        let c = ctx();
        let stub = StubProvider::new("ext", Some("https://models.example.com"));
        let g = EgressGuardedProvider::new(
            stub.clone(),
            c.modes.clone(),
            c.netpol.clone(),
            c.audit.clone(),
        );
        let err = g.submit_memory_hint(hint()).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("egress"), "{msg}");
        assert_eq!(
            stub.hints.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a hint is an egress too — refused before any client call"
        );
        let rows = c
            .audit
            .query(&AuditQuery::new().with_action_prefix("model.egress"))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].entry.result, "denied");
    }

    #[tokio::test]
    async fn vision_grounding_gated_by_the_same_authority() {
        let c = ctx();
        let stub = StubVision::new();
        let inner: Arc<dyn VisionGrounding> = stub.clone();
        let refused = EgressGuardedVision::new(
            "https://vision.example.com",
            inner.clone(),
            c.modes.clone(),
            c.netpol.clone(),
            c.audit.clone(),
        );
        let err = refused.ground(&[0u8], "ok").await.unwrap_err();
        assert!(err.contains("vision.example.com"), "{err}");

        let allowed = EgressGuardedVision::new(
            "http://localhost:11434",
            inner.clone(),
            c.modes.clone(),
            c.netpol.clone(),
            c.audit.clone(),
        );
        assert_eq!(allowed.ground(&[0u8], "ok").await.unwrap(), "1,2");
        assert_eq!(stub.calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        let rows = c
            .audit
            .query(&AuditQuery::new().with_action_prefix("model.egress"))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].entry.subject, "vision.vlm");
        c.audit.verify().await.unwrap();
    }
}
