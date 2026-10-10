//! M4 regression tests: advisory memory hints never touch the resource
//! authority.
//!
//! The M4 invariant, kept at the composition layer where a provider
//! (`openrt-models`) and the authority (`openrt-resources`) actually meet:
//!
//! > **MemoryPlan proposes. ResourceGate authorizes. Runtime executes.
//! > Backend performs inference.**
//!
//! Hint submission is a provider-side, advisory-only channel. It must not
//! consume a `ResourceGate` override, must not grant resources, and must not
//! change a plan the gate already denied.

use async_trait::async_trait;
use openrt_models::{
    AnatomyAccess, BackendCapabilities, HintPriority, MemoryHint, MockProvider, ModelError,
    ModelProvider,
};
use openrt_resources::{
    BackendTierCapabilities, DiskUsage, MemInfo, MemoryPlan, PlanInput, PlanStatus,
    PreflightVerdict, ResidencySnapshot, ResourceGate, ResourceLimits, SystemInfo,
};

fn sys(total_kb: u64, avail_kb: u64) -> SystemInfo {
    SystemInfo {
        mem: MemInfo {
            total_kb,
            available_kb: avail_kb,
        },
        disk: DiskUsage {
            total_bytes: 1 << 40,
            avail_bytes: 1 << 30,
        },
        ..SystemInfo::default()
    }
}

fn residency() -> ResidencySnapshot {
    ResidencySnapshot::unknown()
        .with_device_bytes(Some(2 << 30))
        .with_nvme_bytes(Some(9 << 30))
}

fn input<'a>(
    r: &'a ResidencySnapshot,
    sys: &'a SystemInfo,
    limits: &'a ResourceLimits,
    tiers: &'a BackendTierCapabilities,
) -> PlanInput<'a> {
    PlanInput {
        artifact_id: "m",
        artifact_bytes: Some(9 << 30),
        estimated_resident_bytes: None,
        estimate_source: "",
        residency: r,
        system: sys,
        limits,
        vram_capacity_bytes: Some(12 << 30),
        backend: tiers,
        capability: &[],
    }
}

fn hint(model: &str) -> MemoryHint {
    MemoryHint::advisory(model, HintPriority::Medium, "advisory")
}

#[test]
fn runtime_provider_surface_stays_conservative_by_default() {
    // ``Unsupported'' is the default: a provider acquires no memory
    // capability through ordinary construction.
    let caps = MockProvider::echo("m").backend_capabilities();
    assert_eq!(caps, BackendCapabilities::default());
    assert_eq!(caps.access, AnatomyAccess::RemoteProvider);
    assert_eq!(caps.supported_count(), 0);
    assert!(!caps.claims_any());
    assert!(!caps.hint_supported());
}

#[tokio::test]
async fn hint_submission_does_not_consume_an_override_or_grant_resources() {
    let gate = ResourceGate::new(ResourceLimits::default(), sys(32 * 1024 * 1024, 512 * 1024));
    gate.grant_override("operator will free memory", "tui");
    assert!(gate.override_pending());

    let receipt = MockProvider::echo("m")
        .submit_memory_hint(hint("m"))
        .await
        .unwrap();
    assert!(
        receipt.is_noop(),
        "unsupported hint is a clean drop, not an error"
    );

    assert!(
        gate.override_pending(),
        "hint submission must not consume the one-shot override"
    );
    assert!(
        matches!(gate.preflight().verdict(), PreflightVerdict::Denied(_)),
        "hint submission must not grant resources: posture still denies"
    );
}

#[tokio::test]
async fn denied_plan_stays_denied_after_hint_submission() {
    let r = residency();
    let host = sys(32 * 1024 * 1024, 16 * 1024 * 1024);
    let limits = ResourceLimits::default();
    let tiers = BackendTierCapabilities {
        can_run_from_vram: true,
        ..BackendTierCapabilities::default()
    };
    let plan_input = input(&r, &host, &limits, &tiers);

    let plan = MemoryPlan::propose(&plan_input);
    let gate = ResourceGate::new(ResourceLimits::default(), sys(32 * 1024 * 1024, 512 * 1024));
    let denied = plan.evaluate_against_gate(&gate);
    assert_eq!(denied.status, PlanStatus::Denied);

    let _receipt = MockProvider::echo("m")
        .submit_memory_hint(hint("m"))
        .await
        .unwrap();

    assert_eq!(
        denied.status,
        PlanStatus::Denied,
        "a denied plan stays denied; hints are orthogonal to authority"
    );
    assert!(
        matches!(gate.preflight().verdict(), PreflightVerdict::Denied(_)),
        "the gate still denies after hint submission"
    );
}

#[tokio::test]
async fn hint_submission_does_not_alter_the_memory_plan() {
    let r = residency();
    let host = sys(32 * 1024 * 1024, 16 * 1024 * 1024);
    let limits = ResourceLimits::default();
    let tiers = BackendTierCapabilities {
        can_run_from_vram: true,
        ..BackendTierCapabilities::default()
    };
    let plan_input = input(&r, &host, &limits, &tiers);

    let before = MemoryPlan::propose(&plan_input);
    let _receipt = MockProvider::echo("m")
        .submit_memory_hint(hint("m"))
        .await
        .unwrap();
    let after = MemoryPlan::propose(&plan_input);

    assert_eq!(
        before, after,
        "proposing the same inputs before and after a hint yields identical plans"
    );
}

/// Stub that genuinely claims residency-hint support, proving that even an
/// accepted hint does not reach the gate (it only returns a receipt).
#[derive(Default)]
struct HintAwareProvider;

#[async_trait]
impl ModelProvider for HintAwareProvider {
    fn id(&self) -> &str {
        "hint-aware"
    }

    fn info(&self) -> openrt_models::ModelInfo {
        openrt_models::ModelInfo::new("hint-aware", openrt_models::ProviderKind::Mock, None)
    }

    async fn generate(
        &self,
        _req: openrt_models::ModelRequest,
    ) -> Result<openrt_models::ModelResponse, ModelError> {
        Ok(openrt_models::ModelResponse::with_text("hint-aware", "ok"))
    }

    fn capabilities(&self) -> openrt_models::ModelCapabilities {
        openrt_models::ModelCapabilities {
            supports_tools: false,
            supports_streaming: false,
            context_window: None,
        }
    }

    fn backend_capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            access: AnatomyAccess::LocalArtifact,
            supports_residency_hints: openrt_models::CapabilityState::Supported,
            ..BackendCapabilities::default()
        }
    }

    async fn submit_memory_hint(
        &self,
        _hint: MemoryHint,
    ) -> Result<openrt_models::HintReceipt, ModelError> {
        Ok(openrt_models::HintReceipt::Accepted)
    }
}

#[tokio::test]
async fn accepted_hint_still_never_bypasses_the_gate() {
    // Even a provider that accepts hints cannot turn a hint into an
    // admission: the gate's posture never changes and still denies.
    let provider = HintAwareProvider;
    assert!(provider.backend_capabilities().hint_supported());

    let gate = ResourceGate::new(ResourceLimits::default(), sys(32 * 1024 * 1024, 512 * 1024));
    let _receipt = provider.submit_memory_hint(hint("m")).await.unwrap();

    assert!(
        matches!(gate.preflight().verdict(), PreflightVerdict::Denied(_)),
        "an accepted hint grants nothing and changes no posture"
    );
    assert!(!gate.override_pending(), "nothing was armed or consumed");
}
