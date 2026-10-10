//! Memory Fabric M4 — backend capability negotiation + advisory memory hints.
//!
//! This module is the **contract only**: the smallest declarative vocabulary a
//! backend uses to *claim* what it supports for memory-aware execution, and
//! the smallest advisory interface OpenRT uses to *suggest* future residency
//! behaviour. Nothing here executes, allocates, mmaps, offloads, or changes
//! how a backend runs a model.
//!
//! The hard boundary M4 draws:
//!
//! * OpenRT **proposes, observes, and sends advisory hints**.
//! * The backend **owns** actual inference, model loading, mmap/offload/device
//!   placement, the KV cache, and execution semantics.
//! * OpenRT never fabricates remote model anatomy, and never assumes an
//!   HTTP/OpenAI-compatible server supports mmap, local file access, residency
//!   control, or prefetch. Only what the provider contract genuinely provides
//!   may be declared [`CapabilityState::Supported`].
//!
//! # Conservatism
//!
//! [`BackendCapabilities::default`] (and [`BackendCapabilities::remote_http`],
//! its explicit alias) is the conservative posture: a remote provider with
//! nothing claimed. A capability is `Supported` only when genuinely provided,
//! `Unsupported` by default, and `Unknown` only when the provider explicitly
//! states it cannot answer. Consumers must treat both `Unknown` and
//! `Unsupported` as "must not be relied on."
//!
//! # Advisory hints
//!
//! [`MemoryHint`] is always advisory (`advisory == true`): submitting it
//! allocates nothing in OpenRT, never consumes a [`crate`] authority such as a
//! `ResourceGate` override, never changes model routing or model outputs, and
//! is cleanly dropped by backends that do not support hints
//! ([`HintReceipt::Unsupported`]) instead of erroring.

use serde::{Deserialize, Serialize};

/// Tri-state capability claim from a backend/provider.
///
/// [`Supported`](CapabilityState::Supported) is claimed **only** when the
/// provider contract genuinely provides the capability.
/// [`Unsupported`](CapabilityState::Unsupported) is the default — nothing is
/// claimed or assumed. [`Unknown`](CapabilityState::Unknown) is used only when
/// a provider explicitly states it cannot answer; consumers still treat it as
/// "must not be relied on."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    /// The capability is genuinely provided by the provider contract.
    Supported,
    /// The capability is definitively absent, or not declared at all.
    Unsupported,
    /// The provider explicitly cannot answer whether the capability exists.
    /// Like `Unsupported`, never relied on.
    Unknown,
}

impl CapabilityState {
    /// Whether OpenRT may rely on the capability being available. Both
    /// `Unsupported` and `Unknown` answer no.
    pub fn is_supported(self) -> bool {
        matches!(self, Self::Supported)
    }

    /// Stable lowercase label for reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
            Self::Unknown => "unknown",
        }
    }
}

/// Whether OpenRT genuinely possesses the model artifact, or the model lives
/// behind a remote provider contract.
///
/// The LOCAL/REMOTE distinction is what M4 will not blur: a remote provider
/// does not expose tensor offsets, local filesystem layout, mmap state, or
/// residency to OpenRT, so OpenRT must never invent or infer them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnatomyAccess {
    /// OpenRT genuinely holds the artifact locally (registry/host): anatomy is
    /// real and observed, and the planner may reason from it.
    LocalArtifact,
    /// The model is behind a remote provider contract: OpenRT knows only what
    /// the provider declares. No fabricated tensor layout or residency.
    RemoteProvider,
}

/// Memory-aware execution capabilities a backend **claims**.
///
/// Conservative by construction ([`Default`] claims nothing — see
/// [`Self::remote_http`]). Each field is tri-state so a provider can
/// explicitly say a claim is definitively absent (`Unsupported`) versus merely
/// unknown (`Unknown`); both refuse reliance. Partial serde configuration must
/// never unlock a capability: omitted fields deserialize as `Unsupported`.
///
/// This is the *execution* capability contract. The planner's declarative,
/// per-tier input lives separately in `openrt_resources::BackendTierCapabilities`;
/// M5's `PlannerCapabilityView` (openrt-runtime) is the **one** canonical
/// adapter between them — this module ships the contract, the adapter ships
/// the wiring.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct BackendCapabilities {
    /// Whether OpenRT holds the artifact locally or it is remote-only.
    pub access: AnatomyAccess,
    /// Backend can memory-map a locally-present artifact.
    pub supports_mmap: CapabilityState,
    /// Backend can serve a model directly from file-backed pages.
    pub supports_file_backed_models: CapabilityState,
    /// Backend can offload execution/work to the CPU.
    pub supports_cpu_offload: CapabilityState,
    /// Backend can offload execution/work to a GPU.
    pub supports_gpu_offload: CapabilityState,
    /// Backend accepts residency hints (keep-this-region-resident).
    pub supports_residency_hints: CapabilityState,
    /// Backend accepts read-ahead/prefetch hints.
    pub supports_prefetch_hints: CapabilityState,
    /// Backend accepts an explicit memory budget.
    pub supports_explicit_memory_budget: CapabilityState,
}

impl Default for BackendCapabilities {
    /// Conservative posture: a remote provider with nothing claimed.
    fn default() -> Self {
        Self {
            access: AnatomyAccess::RemoteProvider,
            supports_mmap: CapabilityState::Unsupported,
            supports_file_backed_models: CapabilityState::Unsupported,
            supports_cpu_offload: CapabilityState::Unsupported,
            supports_gpu_offload: CapabilityState::Unsupported,
            supports_residency_hints: CapabilityState::Unsupported,
            supports_prefetch_hints: CapabilityState::Unsupported,
            supports_explicit_memory_budget: CapabilityState::Unsupported,
        }
    }
}

impl BackendCapabilities {
    /// Conservative posture for an ordinary remote HTTP/OpenAI-compatible
    /// provider: nothing local is claimed and no hint/prefetch/budget support
    /// is assumed. Identical to [`Default`]; the concrete remote providers
    /// (`LlamacppProvider`, `VllmProvider`) return this explicitly so their
    /// conservatism is a declaration, not an accident of a default.
    pub fn remote_http() -> Self {
        Self::default()
    }

    /// Whether OpenRT genuinely possesses the artifact, so its anatomy is real
    /// and the planner may reason from it. Remote providers are never "local".
    pub fn has_local_anatomy(&self) -> bool {
        self.access == AnatomyAccess::LocalArtifact
    }

    /// Whether the provider claims *any* memory-aware execution capability.
    pub fn claims_any(&self) -> bool {
        self.supported_count() > 0
    }

    /// Number of capabilities declared `Supported`.
    pub fn supported_count(&self) -> usize {
        [
            self.supports_mmap,
            self.supports_file_backed_models,
            self.supports_cpu_offload,
            self.supports_gpu_offload,
            self.supports_residency_hints,
            self.supports_prefetch_hints,
            self.supports_explicit_memory_budget,
        ]
        .iter()
        .filter(|c| c.is_supported())
        .count()
    }

    /// Whether the provider declares it accepts advisory hints (residency,
    /// prefetch, or an explicit budget). `Unknown`/`Unsupported` both answer
    /// no — a hint must never be sent to an unconfirmed channel.
    pub fn hint_supported(&self) -> bool {
        self.supports_residency_hints.is_supported()
            || self.supports_prefetch_hints.is_supported()
            || self.supports_explicit_memory_budget.is_supported()
    }
}

/// A concrete byte range of the model artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MemoryRange {
    pub start: u64,
    pub length: u64,
}

/// Target memory tier/region family a hint concerns.
///
/// Provider-facing, advisory vocabulary only; a future runtime adapter maps it
/// to the planner's tier set. It never carries authority on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HintTier {
    /// GPU device memory.
    Vram,
    /// Process-owned anonymous RAM.
    RamOwned,
    /// Kernel page cache behind file-backed pages.
    PageCache,
}

/// Relative importance of a hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HintPriority {
    High,
    Medium,
    Low,
}

/// One advisory memory hint.
///
/// Always advisory. Sending a [`MemoryHint`] allocates nothing in OpenRT,
/// never consumes a `ResourceGate` override, never changes model routing or
/// model outputs, and is safely ignored by backends that do not support hints.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MemoryHint {
    /// Model identifier as the provider knows it.
    pub model: String,
    /// Optional target tier/region family.
    pub target: Option<HintTier>,
    /// Optional concrete artifact byte range, when known.
    pub range: Option<MemoryRange>,
    /// Optional logical region label, when the byte range is unknown
    /// (e.g. `"embedding-layer"`).
    pub region: Option<String>,
    pub priority: HintPriority,
    /// Free-text reason (e.g. "likely needed soon", "keep resident").
    pub reason: String,
    /// Hints are always advisory. The flag is carried on the wire (and in
    /// serialized records) so every consumer can prove the hint allocates
    /// nothing, consumes no override, and changes no routing or output.
    pub advisory: bool,
}

impl MemoryHint {
    /// Build an advisory hint. `advisory` is always `true` — this type has no
    /// non-advisory constructor.
    pub fn advisory(
        model: impl Into<String>,
        priority: HintPriority,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            model: model.into(),
            target: None,
            range: None,
            region: None,
            priority,
            reason: reason.into(),
            advisory: true,
        }
    }

    pub fn with_target(mut self, target: HintTier) -> Self {
        self.target = Some(target);
        self
    }

    pub fn with_range(mut self, range: MemoryRange) -> Self {
        self.range = Some(range);
        self
    }

    pub fn with_region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }
}

/// Result of submitting one advisory hint.
///
/// Hint submission is never an inference-breaking error: an unsupported
/// backend answers [`Unsupported`](HintReceipt::Unsupported) and drops the
/// hint. A genuine transport failure may still yield a `ModelError`, which a
/// caller must treat as ignorable (a hint is never required for inference).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HintReceipt {
    /// The provider received the hint and will treat it as advisory input.
    Accepted,
    /// The provider accepted the submission but had nothing to do with it.
    NoOp,
    /// The backend does not support hints; the hint was cleanly dropped.
    Unsupported,
}

impl HintReceipt {
    /// Whether the submission changed nothing (safe to ignore): both
    /// [`NoOp`](HintReceipt::NoOp) and
    /// [`Unsupported`](HintReceipt::Unsupported) mean the hint is gone with
    /// zero effect.
    pub fn is_noop(self) -> bool {
        matches!(self, Self::NoOp | Self::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ModelError;
    use crate::mock::MockProvider;
    use crate::provider::{ModelCapabilities, ModelProvider};
    use crate::types::{FinishReason, ModelInfo, ModelRequest, ModelResponse};
    use openrt_core::Message;

    #[test]
    fn conservative_default_capabilities() {
        let c = BackendCapabilities::default();
        assert_eq!(c.access, AnatomyAccess::RemoteProvider);
        assert_eq!(c.supported_count(), 0);
        assert!(!c.claims_any());
        assert!(!c.hint_supported());
        assert!(!c.has_local_anatomy());
        assert_eq!(c.supports_mmap, CapabilityState::Unsupported);
        assert_eq!(c.supports_file_backed_models, CapabilityState::Unsupported);
        assert_eq!(c.supports_cpu_offload, CapabilityState::Unsupported);
        assert_eq!(c.supports_gpu_offload, CapabilityState::Unsupported);
        assert_eq!(c.supports_residency_hints, CapabilityState::Unsupported);
        assert_eq!(c.supports_prefetch_hints, CapabilityState::Unsupported);
        assert_eq!(
            c.supports_explicit_memory_budget,
            CapabilityState::Unsupported
        );
    }

    #[test]
    fn remote_http_is_conservative_and_identical_to_default() {
        let c = BackendCapabilities::remote_http();
        assert_eq!(c, BackendCapabilities::default());
        assert_eq!(c.access, AnatomyAccess::RemoteProvider);
        assert_eq!(c.supported_count(), 0);
        assert!(!c.claims_any());
        assert!(!c.hint_supported());
    }

    #[test]
    fn local_backend_declares_only_verified_capabilities() {
        // A hypothetical local backend that genuinely verifies mmap + residency
        // hints declares exactly those; every other capability stays at the
        // conservative default.
        let c = BackendCapabilities {
            access: AnatomyAccess::LocalArtifact,
            supports_mmap: CapabilityState::Supported,
            supports_residency_hints: CapabilityState::Supported,
            ..BackendCapabilities::default()
        };
        assert!(c.has_local_anatomy());
        assert_eq!(c.supported_count(), 2);
        assert!(c.claims_any());
        assert!(c.hint_supported());
        assert_eq!(c.supports_mmap, CapabilityState::Supported);
        assert!(!c.supports_gpu_offload.is_supported());
        assert!(!c.supports_prefetch_hints.is_supported());
        assert!(!c.supports_file_backed_models.is_supported());
        assert!(!c.supports_explicit_memory_budget.is_supported());
    }

    #[test]
    fn unknown_capability_is_never_relied_on() {
        let c = BackendCapabilities {
            access: AnatomyAccess::LocalArtifact,
            supports_mmap: CapabilityState::Unknown,
            supports_prefetch_hints: CapabilityState::Unknown,
            ..BackendCapabilities::default()
        };
        assert!(!c.supports_mmap.is_supported(), "Unknown is not reliable");
        assert!(!c.hint_supported(), "Unknown hint support is not reliable");
        assert_eq!(c.supported_count(), 0);
        assert!(!c.claims_any());
    }

    #[test]
    fn partial_configuration_stays_conservative() {
        // A config that names `access` but omits every capability must not
        // unlock anything (serde default). "Unsupported cannot become
        // supported through configuration accidentally."
        let json = r#"{"access":"remote_provider"}"#;
        let caps: BackendCapabilities = serde_json::from_str(json).unwrap();
        assert_eq!(caps, BackendCapabilities::default());
        assert_eq!(caps.supported_count(), 0);
        assert_eq!(caps.access, AnatomyAccess::RemoteProvider);
    }

    #[test]
    fn no_fabricated_remote_anatomy() {
        // A remote posture is fixed: RemoteProvider, nothing local claimed.
        let remote = BackendCapabilities::remote_http();
        assert_eq!(remote.access, AnatomyAccess::RemoteProvider);
        assert!(!remote.has_local_anatomy());
        assert_eq!(remote.supports_mmap, CapabilityState::Unsupported);
        assert_eq!(
            remote.supports_file_backed_models,
            CapabilityState::Unsupported
        );
        assert!(!remote.supports_residency_hints.is_supported());
        assert!(!remote.supports_prefetch_hints.is_supported());
        // Even a hypothetical remote provider that claims gpu_offload does not
        // thereby gain local anatomy.
        let offload = BackendCapabilities {
            supports_gpu_offload: CapabilityState::Supported,
            ..BackendCapabilities::default()
        };
        assert!(!offload.has_local_anatomy());
    }

    #[test]
    fn local_vs_remote_distinction() {
        let local = BackendCapabilities {
            access: AnatomyAccess::LocalArtifact,
            ..BackendCapabilities::default()
        };
        let remote = BackendCapabilities::remote_http();
        assert_ne!(local.access, remote.access);
        assert!(local.has_local_anatomy());
        assert!(!remote.has_local_anatomy());
    }

    #[test]
    fn hint_constructor_is_always_advisory() {
        let h = MemoryHint::advisory("qwen3:8b", HintPriority::Medium, "likely needed soon")
            .with_target(HintTier::Vram)
            .with_range(MemoryRange {
                start: 0,
                length: 4096,
            });
        assert!(h.advisory);
        assert_eq!(h.model, "qwen3:8b");
        assert_eq!(h.target, Some(HintTier::Vram));
        assert_eq!(
            h.range,
            Some(MemoryRange {
                start: 0,
                length: 4096
            })
        );
        assert_eq!(h.reason, "likely needed soon");
    }

    #[test]
    fn capability_round_trip_and_deterministic_serialization() {
        let caps = BackendCapabilities {
            access: AnatomyAccess::LocalArtifact,
            supports_mmap: CapabilityState::Supported,
            supports_residency_hints: CapabilityState::Unknown,
            ..BackendCapabilities::default()
        };
        let json = serde_json::to_string(&caps).unwrap();
        let back: BackendCapabilities = serde_json::from_str(&json).unwrap();
        assert_eq!(back, caps);
        // Deterministic: the same struct always serializes to the same bytes.
        assert_eq!(serde_json::to_string(&caps).unwrap(), json);
    }

    #[test]
    fn hint_round_trip_and_deterministic_serialization() {
        let hint = MemoryHint::advisory("qwen3:8b", HintPriority::High, "keep resident")
            .with_target(HintTier::RamOwned)
            .with_region("decoder");
        let json = serde_json::to_string(&hint).unwrap();
        let back: MemoryHint = serde_json::from_str(&json).unwrap();
        assert_eq!(back, hint);
        assert_eq!(serde_json::to_string(&hint).unwrap(), json);
    }

    #[tokio::test]
    async fn unsupported_hint_is_a_clean_noop() {
        let p = MockProvider::echo("m");
        assert_eq!(p.backend_capabilities(), BackendCapabilities::default());
        let receipt = p
            .submit_memory_hint(MemoryHint::advisory("m", HintPriority::Low, "x"))
            .await
            .unwrap();
        assert_eq!(receipt, HintReceipt::Unsupported);
        assert!(receipt.is_noop());
    }

    #[tokio::test]
    async fn supported_hint_is_accepted() {
        struct HintAware;
        #[async_trait::async_trait]
        impl ModelProvider for HintAware {
            fn id(&self) -> &str {
                "hint-aware"
            }
            fn info(&self) -> ModelInfo {
                ModelInfo::new("hint-aware", crate::types::ProviderKind::Mock, None)
            }
            async fn generate(&self, _req: ModelRequest) -> Result<ModelResponse, ModelError> {
                Ok(ModelResponse {
                    model: "hint-aware".into(),
                    text: "ok".into(),
                    tool_calls: vec![],
                    finish_reason: FinishReason::Stop,
                    usage: Default::default(),
                })
            }
            fn capabilities(&self) -> ModelCapabilities {
                ModelCapabilities {
                    supports_tools: false,
                    supports_streaming: false,
                    context_window: None,
                }
            }
            fn backend_capabilities(&self) -> BackendCapabilities {
                BackendCapabilities {
                    access: AnatomyAccess::LocalArtifact,
                    supports_residency_hints: CapabilityState::Supported,
                    ..BackendCapabilities::default()
                }
            }
            async fn submit_memory_hint(
                &self,
                hint: MemoryHint,
            ) -> Result<HintReceipt, ModelError> {
                assert!(hint.advisory, "hints are always advisory");
                Ok(HintReceipt::Accepted)
            }
        }

        let p = HintAware;
        assert!(p.backend_capabilities().hint_supported());
        let receipt = p
            .submit_memory_hint(MemoryHint::advisory(
                "m",
                HintPriority::High,
                "keep resident",
            ))
            .await
            .unwrap();
        assert_eq!(receipt, HintReceipt::Accepted);
    }

    #[tokio::test]
    async fn hints_do_not_alter_model_output_or_routing() {
        let p = MockProvider::echo("m");
        let before = p
            .generate(ModelRequest::new(vec![Message::user("ping")]))
            .await
            .unwrap();
        let _receipt = p
            .submit_memory_hint(MemoryHint::advisory("m", HintPriority::Medium, "r"))
            .await
            .unwrap();
        let after = p
            .generate(ModelRequest::new(vec![Message::user("ping")]))
            .await
            .unwrap();
        // Identical outputs; routing untouched; the hint added no generation.
        assert_eq!(before.text, after.text);
        assert_eq!(before, after);
        assert_eq!(p.call_count(), 2);
    }
}
