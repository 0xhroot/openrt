//! M5 — the canonical capability↔planner adapter.
//!
//! This is the **one mapping path** between the M4 execution-capability
//! contract and the M3 planner view. It is the only place that reads the
//! tri-state meaning of a provider's declared capabilities and turns it into
//! what the planner may execute from.
//!
//! ```text
//! BackendCapabilities (execution claims, openrt-models)
//!         ↓  PlannerCapabilityView::from_backend   (only here)
//! BackendTierCapabilities (bool executable view)  +  CapabilityNote rationale
//!         ↓  PlanInput { backend, capability }
//! MemoryPlan (capability-aware reference)
//! ```
//!
//! # Rules that never bend here
//!
//! * `Supported` → the tier may be an executable candidate.
//! * `Unsupported` and `Unknown` → the tier is excluded from executability;
//!   the produced [`CapabilityNote`] keeps the two distinguishable in the
//!   plan's rationale ("unavailable" vs "status unknown"). **`Unknown` never
//!   becomes `Supported`**, and a note never flips a `can_run` bool.
//! * `BackendCapabilities::default()` / `remote_http()` — a remote provider
//!   claims nothing — therefore maps to **no** executable tier and notes that
//!   say so. No fabricated anatomy, no fabricated residency, no assumed
//!   filesystem.
//! * Mapping is pure and deterministic: identical capabilities produce
//!   identical `tiers` and `notes`.

use openrt_models::{BackendCapabilities, CapabilityState};
use openrt_resources::{BackendTierCapabilities, CapabilityNote, Tier};

/// The planner-facing result of reading a provider's capability contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannerCapabilityView {
    /// Bool executable view the planner gates `proposed`/`recommended` on.
    pub tiers: BackendTierCapabilities,
    /// Capability rationale the planner renders verbatim into the plan.
    /// Deterministic order: `Vram`, `RamOwned`, `PageCache`.
    pub notes: Vec<CapabilityNote>,
}

impl PlannerCapabilityView {
    /// Canonical mapping from a provider's declared execution capabilities to
    /// the planner view. Conservative by construction: only `Supported`
    /// enables a tier.
    pub fn from_backend(backend: &BackendCapabilities) -> Self {
        let tiers = BackendTierCapabilities {
            can_run_from_vram: backend.supports_gpu_offload.is_supported(),
            can_run_from_ram: backend.supports_cpu_offload.is_supported(),
            can_run_from_page_cache: backend.supports_mmap.is_supported()
                && backend.supports_file_backed_models.is_supported(),
        };
        let notes = vec![
            offload_note(Tier::Vram, backend.supports_gpu_offload),
            offload_note(Tier::RamOwned, backend.supports_cpu_offload),
            page_cache_note(backend),
        ];
        Self { tiers, notes }
    }
}

fn offload_note(tier: Tier, state: CapabilityState) -> CapabilityNote {
    let (label, unsupported, candidate) = match tier {
        Tier::Vram => (
            "VRAM",
            "VRAM unavailable: backend does not support GPU offload.",
            "VRAM candidate: backend supports GPU offload.",
        ),
        Tier::RamOwned => (
            "RAM",
            "RAM unavailable: backend does not support CPU execution.",
            "RAM candidate: backend supports CPU execution.",
        ),
        Tier::PageCache | Tier::Nvme => unreachable!("offload_note covers residency tiers only"),
    };
    let unknown = format!("{label} status unknown: backend capability is unknown.");
    note_for(tier, state, unsupported, &unknown, candidate)
}

fn note_for(
    tier: Tier,
    state: CapabilityState,
    unsupported: &str,
    unknown: &str,
    candidate: &str,
) -> CapabilityNote {
    let (excluded, candidate) = match state {
        CapabilityState::Supported => (None, Some(candidate.to_string())),
        CapabilityState::Unsupported => (Some(unsupported.to_string()), None),
        CapabilityState::Unknown => (Some(unknown.to_string()), None),
    };
    CapabilityNote {
        tier,
        excluded,
        candidate,
    }
}

fn page_cache_note(backend: &BackendCapabilities) -> CapabilityNote {
    let file_backed =
        backend.supports_mmap.is_supported() && backend.supports_file_backed_models.is_supported();
    let (excluded, candidate) = if file_backed {
        (
            None,
            Some("page cache candidate: backend supports file-backed execution.".to_string()),
        )
    } else {
        let excluded = if backend.supports_mmap == CapabilityState::Unsupported {
            "page cache unavailable: backend does not support mmap."
        } else if backend.supports_file_backed_models == CapabilityState::Unsupported {
            "page cache unavailable: backend does not support file-backed models."
        } else {
            "page cache status unknown: backend capability is unknown."
        };
        (Some(excluded.to_string()), None)
    };
    CapabilityNote {
        tier: Tier::PageCache,
        excluded,
        candidate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_models::{AnatomyAccess, MockProvider, ModelProvider};
    use openrt_resources::{
        DiskUsage, MemInfo, MemoryPlan, PlanInput, PlanStatus, PlanTierStatus, ResidencySnapshot,
        ResourceGate, ResourceLimits, SystemInfo, MEMORY_PLAN_SCHEMA_VERSION,
    };

    const GIB: u64 = 1 << 30;

    fn remote() -> BackendCapabilities {
        BackendCapabilities::remote_http()
    }

    #[test]
    fn remote_provider_maps_to_no_executable_tier() {
        let view = PlannerCapabilityView::from_backend(&remote());
        assert_eq!(
            view.tiers,
            BackendTierCapabilities {
                can_run_from_vram: false,
                can_run_from_ram: false,
                can_run_from_page_cache: false,
            }
        );
        assert_eq!(view.notes.len(), 3);
        assert_eq!(
            view.notes[0].excluded.as_deref(),
            Some("VRAM unavailable: backend does not support GPU offload.")
        );
        assert_eq!(view.notes[0].candidate, None);
        assert_eq!(
            view.notes[1].excluded.as_deref(),
            Some("RAM unavailable: backend does not support CPU execution.")
        );
        assert_eq!(
            view.notes[2].excluded.as_deref(),
            Some("page cache unavailable: backend does not support mmap.")
        );
    }

    #[test]
    fn unknown_capability_never_becomes_supported_in_the_view() {
        let caps = BackendCapabilities {
            access: AnatomyAccess::LocalArtifact,
            supports_gpu_offload: CapabilityState::Unknown,
            supports_mmap: CapabilityState::Unknown,
            supports_file_backed_models: CapabilityState::Supported,
            ..BackendCapabilities::default()
        };
        let view = PlannerCapabilityView::from_backend(&caps);
        assert!(
            !view.tiers.can_run_from_vram,
            "Unknown GPU never enables VRAM"
        );
        assert!(
            !view.tiers.can_run_from_page_cache,
            "a single Unknown blocking key keeps file-backed non-executable"
        );
        assert_eq!(
            view.notes[0].excluded.as_deref(),
            Some("VRAM status unknown: backend capability is unknown.")
        );
        assert_eq!(
            view.notes[2].excluded.as_deref(),
            Some("page cache status unknown: backend capability is unknown.")
        );
    }

    #[test]
    fn local_provider_declared_capabilities_map() {
        let caps = BackendCapabilities {
            access: AnatomyAccess::LocalArtifact,
            supports_mmap: CapabilityState::Supported,
            supports_file_backed_models: CapabilityState::Supported,
            supports_cpu_offload: CapabilityState::Supported,
            ..BackendCapabilities::default()
        };
        let view = PlannerCapabilityView::from_backend(&caps);
        assert!(view.tiers.can_run_from_ram);
        assert!(view.tiers.can_run_from_page_cache);
        assert!(!view.tiers.can_run_from_vram);
        assert_eq!(
            view.notes[1].candidate.as_deref(),
            Some("RAM candidate: backend supports CPU execution.")
        );
        assert_eq!(view.notes[1].excluded, None);
        assert_eq!(
            view.notes[2].candidate.as_deref(),
            Some("page cache candidate: backend supports file-backed execution.")
        );
        assert_eq!(
            view.notes[0].excluded.as_deref(),
            Some("VRAM unavailable: backend does not support GPU offload.")
        );
    }

    #[test]
    fn unsupported_mmap_blocks_file_backed_even_when_models_are_supported() {
        let caps = BackendCapabilities {
            access: AnatomyAccess::LocalArtifact,
            supports_mmap: CapabilityState::Unsupported,
            supports_file_backed_models: CapabilityState::Supported,
            ..BackendCapabilities::default()
        };
        let view = PlannerCapabilityView::from_backend(&caps);
        assert!(!view.tiers.can_run_from_page_cache);
        assert_eq!(
            view.notes[2].excluded.as_deref(),
            Some("page cache unavailable: backend does not support mmap.")
        );
    }

    #[test]
    fn mapping_is_deterministic_for_identical_capabilities() {
        let a = PlannerCapabilityView::from_backend(&remote());
        let b = PlannerCapabilityView::from_backend(&remote());
        assert_eq!(a, b);
        assert_eq!(a.notes, b.notes);
    }

    // ---- End-to-end: provider capability → MemoryPlan ----------------------

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

    fn residency(device: Option<u64>) -> ResidencySnapshot {
        let mut base = ResidencySnapshot::unknown();
        base.device_bytes = device;
        base.nvme_bytes = Some(9 * GIB);
        base
    }

    fn plan_from(view: &PlannerCapabilityView) -> MemoryPlan {
        plan_from_obs(view, Some(2 * GIB))
    }

    fn plan_from_obs(view: &PlannerCapabilityView, device: Option<u64>) -> MemoryPlan {
        let r = residency(device);
        let host = sys(32 * 1024 * 1024, 24 * 1024 * 1024);
        let input = PlanInput {
            artifact_id: "qwen3-9b",
            artifact_bytes: Some(9 * GIB),
            estimated_resident_bytes: Some(8 * GIB),
            estimate_source: "model resource profile",
            residency: &r,
            system: &host,
            limits: &ResourceLimits::default(),
            vram_capacity_bytes: Some(12 * GIB),
            backend: &view.tiers,
            capability: &view.notes,
        };
        MemoryPlan::propose(&input)
    }

    #[test]
    fn remote_provider_stays_conservative_end_to_end() {
        let view = PlannerCapabilityView::from_backend(&remote());
        // No device observer at all: nothing may be invented.
        let plan = plan_from_obs(&view, None);
        assert!(
            plan.recommended_tier.is_none(),
            "no executable tier for a claims-nothing provider"
        );
        assert!(!plan.is_executable());
        assert_eq!(
            plan.budget.by_tier[&Tier::Vram].status,
            PlanTierStatus::Unknown
        );
        assert_eq!(
            plan.budget.by_tier[&Tier::RamOwned].status,
            PlanTierStatus::Unknown
        );
        assert_eq!(
            plan.budget.by_tier[&Tier::Vram].currently_observed,
            None,
            "no fabricated residency"
        );
        assert_eq!(
            plan.budget.by_tier[&Tier::RamOwned].currently_observed,
            None,
            "no fabricated owned-RAM residency"
        );
    }

    #[test]
    fn provider_remote_http_surface_is_conservative() {
        // The concrete remote providers (llama.cpp, vLLM) declare exactly this
        // surface; through the adapter it yields the conservative plan above.
        let provider = MockProvider::echo("m");
        let caps = provider.backend_capabilities();
        assert_eq!(caps, remote());
        let view = PlannerCapabilityView::from_backend(&caps);
        let plan = plan_from(&view);
        assert!(plan.recommended_tier.is_none());
        assert!(!plan.is_executable());
    }

    #[test]
    fn supported_gpu_offload_enables_vram_end_to_end() {
        let caps = BackendCapabilities {
            access: AnatomyAccess::LocalArtifact,
            supports_gpu_offload: CapabilityState::Supported,
            ..BackendCapabilities::default()
        };
        let view = PlannerCapabilityView::from_backend(&caps);
        let plan = plan_from(&view);
        assert_eq!(
            plan.budget.by_tier[&Tier::Vram].status,
            PlanTierStatus::Fits
        );
        assert_eq!(plan.recommended_tier, Some(Tier::Vram));
        assert_eq!(plan.status, PlanStatus::Executable);
        assert!(plan.budget.by_tier[&Tier::Vram]
            .note
            .contains("VRAM candidate: backend supports GPU offload."));
    }

    #[test]
    fn prefetch_unsupported_keeps_planning_working_without_claiming_execution() {
        // A local CPU-executable backend that does not support prefetch: the
        // plan still proposes RAM and never claims prefetch can execute.
        let caps = BackendCapabilities {
            access: AnatomyAccess::LocalArtifact,
            supports_cpu_offload: CapabilityState::Supported,
            supports_prefetch_hints: CapabilityState::Unsupported,
            ..BackendCapabilities::default()
        };
        let view = PlannerCapabilityView::from_backend(&caps);
        let plan = plan_from(&view);
        assert_eq!(plan.recommended_tier, Some(Tier::RamOwned));
        assert_eq!(plan.status, PlanStatus::Executable);
        assert_eq!(plan.budget.by_tier[&Tier::RamOwned].proposed, Some(8 * GIB));
        let all_text = plan
            .rationale
            .iter()
            .chain(plan.budget.by_tier.values().map(|b| &b.note))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !all_text.to_lowercase().contains("prefetch"),
            "plan must not claim prefetch can execute"
        );
    }

    #[test]
    fn adapter_never_touches_gate_or_overrides() {
        let caps = BackendCapabilities {
            access: AnatomyAccess::LocalArtifact,
            supports_gpu_offload: CapabilityState::Supported,
            ..BackendCapabilities::default()
        };
        let view = PlannerCapabilityView::from_backend(&caps);
        let plan = plan_from(&view);
        assert_eq!(plan.status, PlanStatus::Executable);

        let gate = ResourceGate::new(ResourceLimits::default(), sys(32 * 1024 * 1024, 512 * 1024));
        gate.grant_override("operator will free memory", "tui");
        let plan = plan.evaluate_against_gate(&gate);

        assert_eq!(plan.status, PlanStatus::Denied, "gate denial still wins");
        assert!(
            gate.override_pending(),
            "capability filtering must not consume the one-shot override"
        );
    }

    #[test]
    fn plan_with_view_serializes_and_round_trips() {
        let caps = BackendCapabilities {
            access: AnatomyAccess::LocalArtifact,
            supports_gpu_offload: CapabilityState::Supported,
            ..BackendCapabilities::default()
        };
        let view = PlannerCapabilityView::from_backend(&caps);
        let plan = plan_from(&view);
        let json = serde_json::to_string(&plan).unwrap();
        let back: MemoryPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(back, plan);
        assert_eq!(back.schema_version, MEMORY_PLAN_SCHEMA_VERSION);
    }
}
