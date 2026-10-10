//! Model provider abstraction.

use async_trait::async_trait;

use crate::error::ModelError;
use crate::memory::{BackendCapabilities, HintReceipt, MemoryHint};
use crate::types::{ModelInfo, ModelRequest, ModelResponse, StreamEvent};

/// M11: load counters a provider *adapter* can genuinely observe, feeding the
/// reserved `model_io.*` telemetry surface.
///
/// OpenRT itself never loads weights, so only the adapter that owns execution
/// can tell a cold load from a warm one. Every field is `None` until that
/// adapter actually observes it — a placeholder zero is fabrication. The
/// default [`ProviderLoadTelemetry::unobserved`] is the honest state for
/// adapters that cannot answer (e.g. a pure HTTP proxy with no residency
/// control).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderLoadTelemetry {
    /// Generations that required a cold (re)load — the model was not resident.
    pub cold_load_count: Option<u64>,
    /// Generations served from an already-resident model.
    pub warm_load_count: Option<u64>,
    /// Residency-tier hits observed (a resident read found the data).
    pub tier_hit_count: Option<u64>,
    /// Residency-tier misses observed (a resident read had to go to disk).
    pub tier_miss_count: Option<u64>,
    /// Which adapter observed these (e.g. `mock:<id>`), for attribution.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub observed_by: String,
}

impl Default for ProviderLoadTelemetry {
    fn default() -> Self {
        Self::unobserved()
    }
}

impl ProviderLoadTelemetry {
    /// An all-Unknown snapshot: until an adapter measures, nothing is claimed.
    pub fn unobserved() -> Self {
        Self {
            cold_load_count: None,
            warm_load_count: None,
            tier_hit_count: None,
            tier_miss_count: None,
            observed_by: String::new(),
        }
    }

    /// True when no counter has a value.
    pub fn is_unobserved(&self) -> bool {
        self.cold_load_count.is_none()
            && self.warm_load_count.is_none()
            && self.tier_hit_count.is_none()
            && self.tier_miss_count.is_none()
    }
}

/// Capabilities a provider/model may advertise. Consumers branch on these
/// rather than on concrete providers, keeping Phase 3 backends (llama.cpp,
/// vLLM) behind the same contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ModelCapabilities {
    pub supports_tools: bool,
    pub supports_streaming: bool,
    pub context_window: Option<usize>,
}

/// Context semantics of a provider/model — the distinction the hardening pass
/// needs so a *default* context value is never mistaken for a hard maximum.
///
/// A remote backend (Ollama) that accepts a per-request context override
/// (`options.num_ctx`) may truthfully report a small `default_context` (what
/// runs without an override) while declining to certify any `maximum_context`.
/// Clamping a requested 65536 down to that default — just because the default
/// is 8192 — silently rewrites the operator's working configuration; that is
/// exactly what this surface removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextCapability {
    /// Provider/model default context — what runs when NO per-request value
    /// is sent. A default is NOT a maximum.
    pub default_context: Option<usize>,
    /// Certified hard maximum the backend declares it will refuse to exceed.
    /// `None` = no maximum declared.
    pub maximum_context: Option<usize>,
    /// Whether the declared `maximum_context` is a *known* hard ceiling.
    /// `false` = the provider cannot certify a maximum; a per-request value
    /// larger than the default is not refused on that basis alone.
    pub maximum_known: bool,
    /// Whether the backend accepts a per-request context override (Ollama
    /// `options.num_ctx`). A non-configurable backend sizes its request guard
    /// from the default context — it cannot be told to use a larger window.
    pub configurable: bool,
}

impl Default for ContextCapability {
    /// Conservative posture for a provider that declares nothing extra: the
    /// declared `context_window` is a default only; no maximum is certified;
    /// no per-request knob exists.
    fn default() -> Self {
        Self {
            default_context: None,
            maximum_context: None,
            maximum_known: false,
            configurable: false,
        }
    }
}

impl ContextCapability {
    /// Derive the honest capability view from a provider's plain `ModelCapabilities`
    /// declaration: the declared window is a *default*, never certified as a
    /// hard maximum, and no per-request knob is assumed.
    pub fn from_declared(window: Option<usize>) -> Self {
        Self {
            default_context: window,
            ..Self::default()
        }
    }

    /// Whether a requested per-request context may be forwarded unchanged:
    /// the backend is configurable AND the request does not exceed a certified
    /// hard maximum (when one is known). A call that answers `false` must be
    /// refused explicitly upstream — never silently downgraded.
    pub fn accepts_override(&self, requested: usize) -> bool {
        if !self.configurable {
            return false;
        }
        match (self.maximum_known, self.maximum_context) {
            (true, Some(max)) => requested <= max,
            // No certified maximum: a per-request value is not refused on a
            // hunch — forward it and let the backend enforce its real limit.
            _ => true,
        }
    }

    /// Whether `requested` breaches a *certified* hard maximum — the one,
    /// and only, basis on which an agent may refuse a context request.
    /// Honest unknowns (`maximum_known: false`) never refuse.
    pub fn breaches_certified_maximum(&self, requested: usize) -> bool {
        match (self.maximum_known, self.maximum_context) {
            (true, Some(max)) => requested > max,
            _ => false,
        }
    }

    /// The window the request-assembly guard should size against: a passable
    /// per-request override, or the provider default. `None` when nothing is
    /// declared (no fabricated window). Callers must check
    /// [`Self::accepts_override`] first and refuse a hard-max breach
    /// explicitly.
    pub fn effective_window(&self, requested: Option<u32>) -> Option<usize> {
        match requested {
            Some(r) if self.configurable => Some(r as usize),
            _ => self.default_context,
        }
    }
}

#[cfg(test)]
mod context_capability_tests {
    use super::*;

    #[test]
    fn default_derives_from_declared_window() {
        let cap = ContextCapability::from_declared(Some(8192));
        assert_eq!(cap.default_context, Some(8192));
        assert_eq!(cap.maximum_context, None);
        assert!(!cap.maximum_known);
        assert!(!cap.configurable);
        // A non-configurable backend cannot be told to widen its window; the
        // "effective" window for a per-request override is still the default.
        assert!(!cap.accepts_override(65536));
        assert_eq!(cap.effective_window(Some(65536)), Some(8192));
    }

    #[test]
    fn configurable_backend_passes_the_requested_window_through() {
        let cap = ContextCapability {
            default_context: Some(8192),
            maximum_context: None,
            maximum_known: false,
            configurable: true,
        };
        assert!(cap.accepts_override(65536), "no certified max => forward");
        assert_eq!((cap.effective_window(Some(65536))), Some(65536));
        assert_eq!(cap.effective_window(None), Some(8192));
    }

    /// A provider with a CERTIFIED hard maximum refuses only above that max —
    /// never silently downgrades.
    #[test]
    fn certified_hard_max_refuses_explicitly_above_it_only() {
        let cap = ContextCapability {
            default_context: Some(8192),
            maximum_context: Some(32768),
            maximum_known: true,
            configurable: true,
        };
        assert!(cap.accepts_override(32768));
        assert!(!cap.accepts_override(65536));
        assert_eq!(cap.effective_window(Some(65536)), Some(65536));
    }
}

/// A single non-streaming model invocation.
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Stable identifier of this provider port (matches `ProviderConfig.id`).
    fn id(&self) -> &str;

    /// Static metadata about the provider and its default model.
    fn info(&self) -> ModelInfo;

    /// Run one generation. Implementations must be cancellation-friendly at
    /// the driver level (the runtime races this future against cancellation
    /// and drops it, so long-running payloads should check cancellation or
    /// use bounded timeouts internally).
    async fn generate(&self, req: ModelRequest) -> Result<ModelResponse, ModelError>;

    /// Stream one generation over an SSE-capable transport (Part B).
    ///
    /// The provider pushes real token deltas into `sink` ([`StreamEvent::Token`])
    /// as they arrive and completes by returning the full [`ModelResponse`]
    /// once the backend's `done` frame is seen. Deltas are never synthesized —
    /// a gap with no events is idle, not completion; the stream must not end
    /// until the provider actually finishes. The default is an honest
    /// [`ModelError::StreamingUnsupported`] for non-streaming providers;
    /// providers that advertise `supports_streaming` must override this.
    async fn generate_stream(
        &self,
        _req: ModelRequest,
        _sink: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> Result<ModelResponse, ModelError> {
        Err(ModelError::StreamingUnsupported(
            "this provider has no streaming transport".into(),
        ))
    }

    /// Advertised capabilities of the default model.
    fn capabilities(&self) -> ModelCapabilities;

    /// Context semantics of this provider (hardening pass, Part B).
    ///
    /// The default treats the declared `context_window` as a *default* only —
    /// never a certified hard maximum, never a fabricble override surface
    /// ([`ContextCapability::from_declared`]). Backends that genuinely accept
    /// a per-request context knob (Ollama `options.num_ctx`) MUST override
    /// this with `configurable: true` so an operator's larger window reaches
    /// the backend instead of being silently clamped.
    fn context_capability(&self) -> ContextCapability {
        ContextCapability::from_declared(self.capabilities().context_window)
    }

    /// M4: memory-aware execution capabilities this backend **claims**.
    ///
    /// Conservative contract: nothing is claimed unless the provider genuinely
    /// provides it. The default is the ordinary remote HTTP/OpenAI-compatible
    /// posture — no mmap, no local file access, no residency control, no
    /// prefetch ([`BackendCapabilities::remote_http`]). Backends use the
    /// explicit `Unknown` state only when they cannot answer; consumers must
    /// treat `Unknown` like `Unsupported`.
    fn backend_capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::default()
    }

    /// M4: submit one *advisory* memory hint.
    ///
    /// Purely informational: it allocates nothing in OpenRT, never consumes a
    /// ResourceGate override, never changes model routing or model outputs,
    /// and is safely ignored by unsupported backends. The default drops the
    /// hint with an [`HintReceipt::Unsupported`] receipt — hint submission
    /// must never produce an error that breaks inference. A genuine transport
    /// failure may still yield a [`ModelError`]; callers should treat any hint
    /// failure as ignorable, since a hint is never required for inference.
    async fn submit_memory_hint(&self, _hint: MemoryHint) -> Result<HintReceipt, ModelError> {
        Ok(HintReceipt::Unsupported)
    }

    /// Honest, per-model probe for native tool calling when the backend can
    /// answer (e.g. Ollama `/api/show` `capabilities`). `Ok(None)` is the
    /// honest default: unknown — consumers must not infer support from a
    /// model name.
    async fn probe_tool_support(&self, _model: &str) -> Result<Option<bool>, ModelError> {
        Ok(None)
    }

    /// Honest, per-model probe for vision (image input) support. Same
    /// honesty contract as [`Self::probe_tool_support`]: `Ok(None)` = the
    /// transport cannot tell — never inferred from the model name.
    async fn probe_vision(&self, _model: &str) -> Result<Option<bool>, ModelError> {
        Ok(None)
    }

    /// Absolute base URL of a networked transport (e.g.
    /// `http://127.0.0.1:11434`), or `None` for in-process / deterministic
    /// providers. The runtime's ONE egress guard consults this before any
    /// `generate` may reach a socket (ADR-010: network entry points are
    /// refused before the client call under Sovereign Mode).
    fn base_url(&self) -> Option<String> {
        None
    }

    /// PART 21: the raw body of the most recent successful `generate`
    /// response, kept for operator diagnosis (`/debug model-response`).
    /// `None` = this transport does not capture one. Callers must redact
    /// before display; the body is capped by the provider.
    fn last_raw_response(&self) -> Option<String> {
        None
    }

    /// M11: tier hit/miss (and cold/warm load) counters observed by THIS
    /// adapter. `None` = honestly unobserved; an adapter must only count what
    /// it can actually observe — e.g. a generation arriving while the backend
    /// is idle is a cold load / tier miss, while one arriving to a resident
    /// model is a warm load / tier hit. Default: nothing observed.
    fn load_telemetry(&self) -> ProviderLoadTelemetry {
        ProviderLoadTelemetry::unobserved()
    }
}
