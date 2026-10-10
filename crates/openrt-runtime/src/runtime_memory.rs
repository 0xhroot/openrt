//! M6 — the real runtime consumer of capability-aware memory planning.
//!
//! M1–M5 delivered data (`openrt-resources`), contracts (`openrt-models`),
//! and the canonical adapter (`PlannerCapabilityView`). M6 is the **first
//! production-facing consumer**: one entry point on the existing [`Runtime`]
//! façade that wires registry metadata → capability view → [`MemoryPlan`] →
//! [`ResourceGate`] posture, without introducing a parallel planner, manager,
//! or gate.
//!
//! ```text
//! ModelRegistry (card + anatomy)      ← store metadata only, never the file
//!         +
//! ResidencySnapshot                   ← caller observation, or honest unknown
//!         +
//! ResourceGate (host snapshot + limits)  ← already available; not refreshed
//!         +
//! BackendCapabilities (provider)      ← M4 declared contract
//!         ↓
//! PlannerCapabilityView               ← the ONE M5 mapping path
//!         ↓
//! MemoryPlan::propose()               ← pure
//!         ↓
//! evaluate_against_gate(...)          ← probe only; never consumes overrides
//!         ↓
//! RuntimeMemoryPlan
//! ```
//!
//! # Invariants preserved
//!
//! * `MemoryPlan` proposes, `ResourceGate` authorizes, `Runtime` executes,
//!   `Backend` performs inference. M6 adds **no** authority.
//! * The planner result is **side-effect free**: no model loading, no provider
//!   inference, no memory allocation, no payload reads — only registry
//!   metadata and already-available observations are consumed (the M9 GPU
//!   observer is a read-only query; it never changes a decision, only reports
//!   what it measured).
//! * VRAM and process residency are **never invented**. Without a caller
//!   observation, `device_bytes` comes only from the runtime's M9 VRAM
//!   observer when that observer actually reported usage — never a guess,
//!   never a fabricated zero. File size is never treated as residency.
//! * Planning is deterministic: with identical registry state, capabilities,
//!   gate snapshot, and (optional) residency input it returns identical plans.

use openrt_models::BackendCapabilities;
use openrt_resources::{MemoryPlan, PlanInput, ResidencySnapshot, SystemInfo};

use crate::capability_adapter::PlannerCapabilityView;
use crate::runtime::Runtime;
use crate::Error;

/// Outcome of the runtime's one memory-planning entry point.
///
/// Everything in here is derived from already-available observations and
/// declared capabilities. Deterministic for identical inputs; serializes to
/// deterministic JSON for diagnostics.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RuntimeMemoryPlan {
    /// The pure planner verdict, with the gate's current posture folded in.
    pub plan: MemoryPlan,
    /// The active provider's declared execution capabilities (M4), as read
    /// from the provider contract — never inferred.
    pub backend: BackendCapabilities,
    /// True when the registry had recorded anatomy for this artifact (M2).
    /// Absent anatomy is preserved as an honest unknown, never fabricated.
    pub anatomy_available: bool,
    /// The registry-declared artifact file size consumed by the plan, `None`
    /// when the artifact is not installed (no declared size).
    pub artifact_bytes: Option<u64>,
    /// M8: resident working-set sum when every tensor length is known **but at
    /// least one length was derived from the versioned ggml block-size table**.
    /// Purely advisory — it is reported so an operator can see the number, but
    /// it never reaches the certified plan basis and can never elevate a plan
    /// to `Executable` on its own.
    #[serde(default)]
    pub advisory_resident_bytes: Option<u64>,
    /// M10: true when `nvme_bytes` was observed **directly from local
    /// storage** (a real `stat` of the installed artifact) rather than proxied
    /// from the registry's declared size. Declared size is provenance; a direct
    /// read is truth about the bytes on this disk.
    #[serde(default)]
    pub nvme_bytes_measured: bool,
}

impl Runtime {
    /// M6: propose a capability-aware memory plan for one artifact **through
    /// the real runtime**.
    ///
    /// This is observation/planning only. It never executes a memory
    /// transition, never loads a model, and never touches the provider's
    /// inference path. Specifically it does **not** call
    /// [`ResourceGate::grant_override`], consume an override, allocate model
    /// bytes, or refresh the gate's snapshot — it reads the cached host
    /// snapshot and consults the gate by probe (`preflight`), so a denial is
    /// reported and never spends an authorization.
    ///
    /// # Inputs
    ///
    /// * `artifact_id` — registry artifact id; registry `status()`, `anatomy()`
    ///   and `manifest()` metadata are read (never the payload file).
    /// * `vram_capacity_bytes` — declared deployment VRAM capacity (a
    ///   configuration value, not a measurement). `None` = not declared.
    /// * `residency` — optional caller-supplied residency observation (e.g. a
    ///   real device observer). `None` degrades every residency field to the
    ///   honest unknown, with the registry-declared artifact size anchored as
    ///   the NVMe observation and, when the runtime's M9 VRAM observer reports
    ///   usage, that measured value anchored as `device_bytes`. A supplied
    ///   observation is honored verbatim and is never overwritten except that a
    ///   missing NVMe field is filled from the registry-declared size.
    ///
    /// # Determinism
    ///
    /// With no `residency` input the plan's observation timestamp is anchored
    /// to the artifact's manifest `created_at` (an existing observation), so
    /// repeated calls with unchanged registry state produce identical plans. An
    /// injected M9 observer (e.g. the deterministic test fixture) contributes a
    /// fixed device reading; a live `nvidia-smi` observer makes `device_bytes`
    /// a real point-in-time measurement, never a guess.
    pub async fn propose_memory_plan(
        &self,
        artifact_id: &str,
        vram_capacity_bytes: Option<u64>,
        residency: Option<ResidencySnapshot>,
    ) -> Result<RuntimeMemoryPlan, Error> {
        // 1. Registry metadata — store records only, never the payload file.
        let card = self.model_registry.status(artifact_id).await?;
        let anatomy = self.model_registry.anatomy(artifact_id).await?;
        let manifest = self.model_registry.manifest(artifact_id).await?;
        // M7: a sharded model's storage footprint is the sum of its shard
        // sizes from anatomy (the manifest/card knows only the index member).
        // A single-file model keeps the M6 card-based size. Neither is ever
        // treated as residency by the planner.
        let artifact_bytes = if anatomy
            .as_ref()
            .is_some_and(openrt_models::ModelAnatomy::is_multi_shard)
        {
            anatomy
                .as_ref()
                .and_then(openrt_models::ModelAnatomy::shard_bytes)
                .filter(|b| *b > 0)
        } else {
            card.artifact_bytes
                .or(Some(card.installed_bytes))
                .filter(|b| *b > 0)
        };

        // 2. Residency — use the caller's observation when one exists;
        //    otherwise the honest unknown anchored to an existing observation,
        //    back-filled from the runtime's M9 VRAM observer when that observer
        //    actually reported usage (device bytes are real measurements here,
        //    never a guess from system RAM).
        let mut residency = match residency {
            Some(snapshot) => snapshot,
            None => {
                let mut unknown = ResidencySnapshot::unknown();
                if let Some(manifest) = &manifest {
                    unknown.timestamp = manifest.created_at.clone();
                }
                let obs = self.observe_gpu().await;
                if let Some(device) = obs.used_bytes() {
                    unknown = unknown.with_device_bytes(Some(device));
                    unknown
                        .notes
                        .push(format!("device bytes observed via {:?}", obs.source));
                }
                unknown
            }
        };
        // M10: an NVMe observation can come from the local filesystem itself —
        // a direct `stat` of the installed artifact (all-or-nothing across
        // shards). Only when no direct read is possible does the registry's
        // declared size serve as the (labelled) proxy. A caller-supplied
        // residency always wins.
        let nvme_bytes_measured = if residency.nvme_bytes.is_some() {
            false
        } else if let Some((measured, note)) = observe_installed_storage(&manifest, &anatomy) {
            residency = residency.with_nvme_bytes(Some(measured));
            if !note.is_empty() {
                residency.notes.push(note);
            }
            true
        } else if let Some(declared) = artifact_bytes {
            residency = residency.with_nvme_bytes(Some(declared));
            false
        } else {
            false
        };

        // 3. Backend capability contract (M4) → planner view (M5, one path).
        let backend = self.provider.backend_capabilities();
        let view = PlannerCapabilityView::from_backend(&backend);

        // 4. Resident-footprint estimate from M2 anatomy: only when *every*
        //    tensor length is known. Partial or absent lengths stay Unknown —
        //    never a made-up working set, never a silent estimate. A sum that
        //    relies on M8 table-derived lengths is reported separately and
        //    stays advisory (it never certifies residency).
        let estimate = resident_estimate(&anatomy);
        let (estimated_resident_bytes, estimate_source) = match &estimate {
            ResidentEstimate::Certified(bytes) => {
                (Some(*bytes), "registry anatomy tensor-payload lengths")
            }
            ResidentEstimate::TableDerived(_) | ResidentEstimate::None => (None, ""),
        };
        let advisory_resident_bytes = match estimate {
            ResidentEstimate::TableDerived(bytes) => Some(bytes),
            _ => None,
        };

        // 5. Host + policy from the gate's own cached state.
        let system: SystemInfo = self.resource_gate.current();
        let limits = self.resource_gate.limits();

        // 6. Hybrid it all into the pure planner, then consult the gate by
        //    probe (denial wins; an override is never armed or consumed).
        let input = PlanInput {
            artifact_id,
            artifact_bytes,
            estimated_resident_bytes,
            estimate_source,
            residency: &residency,
            system: &system,
            limits,
            vram_capacity_bytes,
            backend: &view.tiers,
            capability: &view.notes,
        };
        let plan = MemoryPlan::propose(&input).evaluate_against_gate(&self.resource_gate);

        Ok(RuntimeMemoryPlan {
            plan,
            backend,
            anatomy_available: anatomy.is_some(),
            artifact_bytes,
            advisory_resident_bytes,
            nvme_bytes_measured,
        })
    }
}

/// M10: observe the installed artifact's bytes directly on local storage.
///
/// Returns `Some((bytes, note))` only when every installed file actually
/// statted to a real size — all-or-nothing, never a partial or guessed sum.
///
/// * Single-file artifacts: the manifest's external blob path is statted.
/// * Sharded artifacts: every anatomy shard file is statted *next to the
///   external index file* (the operator's local directory); any missing shard
///   aborts the whole direct read (nothing is partially measured).
///
/// A stored blob (registry-owned copy) or a manifest with no external path has
/// no directly-observable local file here → `None`, and the caller falls back
/// to the registry-declared size (clearly labelled as a proxy).
fn observe_installed_storage(
    manifest: &Option<openrt_registry::ManifestV1>,
    anatomy: &Option<openrt_models::ModelAnatomy>,
) -> Option<(u64, String)> {
    let manifest = manifest.as_ref()?;
    // Only externally-stored artifacts have an operator-visible local path.
    let index_path = manifest
        .blobs
        .iter()
        .find_map(|blob| blob.location.path())?;
    let index = std::path::Path::new(index_path);

    let fields = anatomy
        .as_ref()
        .filter(|a| a.is_multi_shard())
        .map(|a| &a.shard_files);
    let (measured, note) = match fields {
        Some(shards) if !shards.is_empty() => {
            // Store-owned shards are not statted via the index path; only
            // external shard names physically beside the index file are.
            let dir = index.parent()?;
            let mut total = 0u64;
            for shard in shards {
                let file = dir.join(&shard.file);
                let bytes = openrt_resources::stat_size_bytes(&file)?;
                total = total.checked_add(bytes)?;
            }
            (
                total,
                format!(
                    "nvme bytes measured directly from {} shard files beside {}",
                    shards.len(),
                    index.display()
                ),
            )
        }
        _ => (openrt_resources::stat_size_bytes(index)?, String::new()),
    };
    Some((measured, note))
}

/// Resident-footprint estimate derived from recorded anatomy.
///
/// The only estimate the runtime will certify is one that is *complete* and
/// *explicit*: every tensor payload length recorded and every one known from an
/// explicit header size. A GGUF with quantized tensors (or any partial anatomy)
/// yields [`ResidentEstimate::None`] — the plan then sizes from the measured
/// file size as an upper bound (advisory), never as a certified residency.
///
/// M8: when every length is known but at least one came from the versioned
/// ggml block-size table, the sum is [`ResidentEstimate::TableDerived`] — a
/// real number an operator can see, but one that must not certify a fit by
/// itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResidentEstimate {
    Certified(u64),
    TableDerived(u64),
    None,
}

fn resident_estimate(anatomy: &Option<openrt_models::ModelAnatomy>) -> ResidentEstimate {
    match anatomy {
        Some(an) if !an.tensors.is_empty() && an.tensors.iter().all(|t| t.length.is_some()) => {
            let bytes = an.tensors.iter().filter_map(|t| t.length).sum::<u64>();
            let table_derived = an
                .tensors
                .iter()
                .any(|t| t.length_basis() == Some(openrt_models::TensorLengthBasis::QuantTableV1));
            if table_derived {
                ResidentEstimate::TableDerived(bytes)
            } else {
                ResidentEstimate::Certified(bytes)
            }
        }
        _ => ResidentEstimate::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use openrt_config::Config;
    use openrt_models::mock::MockMode;
    use openrt_models::{
        AnatomyFormat, BackendCapabilities, CapabilityState, MockProvider, ModelCapabilities,
        ModelInfo, ModelProvider, ModelRequest, ModelResponse,
    };
    use openrt_persistence::MemoryStore;
    use openrt_registry::{
        BlobLocation, BlobRef, LifecycleState, ManifestV1, RegistryCard, RegistryError,
        SourceLabel, MANIFESTS_NS, MANIFEST_SCHEMA_VERSION, META_NS,
    };
    use openrt_resources::{PlanStatus, PlanTierStatus, Tier};

    const GIB: u64 = 1 << 30;

    /// A provider that forwards everything to [`MockProvider`] but declares a
    /// specific execution-capability surface (M4). Lets M6 tests exercise the
    /// local-capability planning path without touching the network.
    struct CapabilityProvider {
        inner: MockProvider,
        caps: BackendCapabilities,
    }

    impl CapabilityProvider {
        fn from(caps: BackendCapabilities) -> Self {
            Self {
                inner: MockProvider::new("caps", MockMode::Echo),
                caps,
            }
        }

        fn call_count(&self) -> u64 {
            self.inner.call_count()
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for CapabilityProvider {
        fn id(&self) -> &str {
            self.inner.id()
        }
        fn info(&self) -> ModelInfo {
            self.inner.info()
        }
        async fn generate(
            &self,
            req: ModelRequest,
        ) -> Result<ModelResponse, openrt_models::ModelError> {
            self.inner.generate(req).await
        }
        fn capabilities(&self) -> ModelCapabilities {
            self.inner.capabilities()
        }
        fn backend_capabilities(&self) -> BackendCapabilities {
            self.caps.clone()
        }
    }

    async fn rt_with(provider: Arc<dyn ModelProvider>, headroom: u64) -> Arc<Runtime> {
        let mut config = Config::default();
        if headroom > 0 {
            config.resources.min_memory_headroom_percent = Some(headroom);
        }
        crate::RuntimeBuilder::new()
            .config(config)
            .store(Arc::new(MemoryStore::new()))
            .with_provider(provider)
            .bus_capacity(16)
            .build()
            .await
            .unwrap()
    }

    /// Seed the registry metadata a plan consumes — the card (`registry.meta`)
    /// and the immutable manifest (`registry.manifests`) — **directly through
    /// the persistence store**, with zero bytes on disk. The declared size is
    /// manifest metadata; the planner reads metadata, never the payload.
    async fn seed_registry(rt: &Runtime, id: &str, declared_size: u64) {
        let key = openrt_core::encode_safe_key(id);
        let card = RegistryCard {
            artifact_id: id.to_string(),
            display_name: "fixture".to_string(),
            family: "demo".to_string(),
            tags: vec![],
            installed_bytes: 0,
            artifact_bytes: Some(declared_size),
            blob_location: Some("external".to_string()),
            state: LifecycleState::Active,
            source: SourceLabel::LocalImport,
            digest_prefix: None,
        };
        let manifest = ManifestV1 {
            schema_version: MANIFEST_SCHEMA_VERSION,
            artifact_id: id.to_string(),
            kind: openrt_registry::RegistryArtifactKind::Binary,
            blobs: vec![BlobRef {
                file_key: "artifact.bin".to_string(),
                digest: "fixture (payload never materialized)".to_string(),
                size_bytes: declared_size,
                location: BlobLocation::External {
                    path: "fixture-path-never-read".to_string(),
                },
            }],
            base_digest: None,
            // Fixed so planning is anchored to a stable observation time.
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };
        rt.store
            .put_record(META_NS, &key, &serde_json::to_value(card).unwrap())
            .await
            .unwrap();
        rt.store
            .put_record(MANIFESTS_NS, &key, &serde_json::to_value(manifest).unwrap())
            .await
            .unwrap();
    }

    fn tensor(name: &str, length: u64) -> openrt_models::TensorAnatomy {
        openrt_models::TensorAnatomy {
            name: name.to_string(),
            shard: 0,
            offset: Some(1024),
            length: Some(length),
            length_basis: Some(openrt_models::TensorLengthBasis::Header),
            alignment: Some(1024),
            page_aligned: true,
            dtype: "F16".to_string(),
        }
    }

    /// Overwrite the registry anatomy with a curated one (tensor lengths known).
    async fn seed_anatomy(
        rt: &Runtime,
        id: &str,
        total_bytes: u64,
        tensors: Vec<openrt_models::TensorAnatomy>,
    ) {
        let anatomy = openrt_models::ModelAnatomy {
            schema_version: openrt_models::ANATOMY_SCHEMA_VERSION,
            format: AnatomyFormat::Safetensors,
            total_bytes,
            shards: 1,
            tensors,
            shard_files: Vec::new(),
        };
        rt.model_registry
            .store_anatomy(id, "testsig", total_bytes, "/nope/never-read", &anatomy)
            .await
            .unwrap();
    }

    /// Seed a multi-shard (M7) anatomy directly through the store: three
    /// physical shards whose sizes **sum to `shard_total`**, with per-shard
    /// tensor records reflecting a `resident_sum` working set.
    async fn seed_sharded_anatomy(rt: &Runtime, id: &str, shard_total: u64, resident_sum: u64) {
        let shard_size = shard_total / 3;
        let shards = (0..3u32)
            .map(|i| openrt_models::ShardAnatomy {
                shard: i,
                name: format!("model-{i}.safetensors"),
                file: format!("model-{i}.safetensors"),
                size_bytes: shard_size,
                checksum: None,
            })
            .collect::<Vec<_>>();
        let third = resident_sum / 3;
        let tensors = (0..3u32)
            .map(|i| openrt_models::TensorAnatomy {
                shard: i,
                ..tensor(&format!("shard{i}.w"), third)
            })
            .collect::<Vec<_>>();
        let anatomy = openrt_models::ModelAnatomy {
            schema_version: openrt_models::ANATOMY_SCHEMA_VERSION,
            format: AnatomyFormat::Safetensors,
            total_bytes: shard_total,
            shards: 3,
            tensors,
            shard_files: shards,
        };
        rt.model_registry
            .store_anatomy(id, "testsig", shard_size, "/nope/never-read", &anatomy)
            .await
            .unwrap();
    }

    fn remote_caps() -> BackendCapabilities {
        BackendCapabilities::remote_http()
    }

    fn local_gpu_caps() -> BackendCapabilities {
        BackendCapabilities {
            access: openrt_models::AnatomyAccess::LocalArtifact,
            supports_gpu_offload: CapabilityState::Supported,
            ..BackendCapabilities::default()
        }
    }

    // ---- runtime → adapter/planner wiring --------------------------------

    #[tokio::test]
    async fn proposes_through_the_real_runtime_and_gate() {
        let rt = rt_with(Arc::new(CapabilityProvider::from(local_gpu_caps())), 0).await;
        seed_registry(&rt, "local/qwen3.5-9b", 9 * GIB).await;
        seed_anatomy(&rt, "local/qwen3.5-9b", 9 * GIB, vec![tensor("w", 8 * GIB)]).await;

        let out = rt
            .propose_memory_plan("local/qwen3.5-9b", Some(12 * GIB), None)
            .await
            .unwrap();
        assert!(out.anatomy_available);
        assert_eq!(out.artifact_bytes, Some(9 * GIB));
        assert_eq!(out.backend.supports_gpu_offload, CapabilityState::Supported);
        assert_eq!(out.plan.artifact_id, "local/qwen3.5-9b");
        // The pure planner's verdict + the gate's posture are both present.
        assert!(out.plan.gate_posture.is_some());
    }

    #[test]
    fn resident_estimate_requires_complete_tensor_lengths() {
        let complete = openrt_models::ModelAnatomy {
            schema_version: 1,
            format: AnatomyFormat::Safetensors,
            total_bytes: 10,
            shards: 1,
            tensors: vec![tensor("a", 4), tensor("b", 6)],
            shard_files: Vec::new(),
        };
        assert_eq!(
            resident_estimate(&Some(complete)),
            ResidentEstimate::Certified(10)
        );

        let empty = openrt_models::ModelAnatomy {
            schema_version: 1,
            format: AnatomyFormat::Unknown,
            total_bytes: 10,
            shards: 1,
            tensors: vec![],
            shard_files: Vec::new(),
        };
        assert_eq!(resident_estimate(&Some(empty)), ResidentEstimate::None);

        // A quantized GGUF records no lengths: never a partial estimate.
        let partial = openrt_models::ModelAnatomy {
            schema_version: 1,
            format: AnatomyFormat::Gguf,
            total_bytes: 10,
            shards: 1,
            tensors: vec![openrt_models::TensorAnatomy {
                length: None,
                ..tensor("q", 4)
            }],
            shard_files: Vec::new(),
        };
        assert_eq!(resident_estimate(&Some(partial)), ResidentEstimate::None);
        assert_eq!(resident_estimate(&None), ResidentEstimate::None);
    }

    // ---- 12 GiB / 9 GiB scenarios ----------------------------------------

    #[tokio::test]
    async fn a_gpu_unsupported_never_recommends_executable_vram() {
        let rt = rt_with(Arc::new(CapabilityProvider::from(remote_caps())), 0).await;
        seed_registry(&rt, "local/qwen3.5-9b", 9 * GIB).await;

        let out = rt
            .propose_memory_plan("local/qwen3.5-9b", Some(12 * GIB), None)
            .await
            .unwrap();
        let vram = &out.plan.budget.by_tier[&Tier::Vram];
        assert_eq!(vram.status, PlanTierStatus::Unknown);
        assert!(
            vram.unknown_reason
                .contains("VRAM unavailable: backend does not support GPU offload."),
            "the adapter's verbatim note must reach the runtime plan"
        );
        assert!(
            out.plan.recommended_tier != Some(Tier::Vram),
            "an unsupported GPU must never yield an executable VRAM recommendation"
        );
        assert!(!out.plan.is_executable());
    }

    #[tokio::test]
    async fn b_gpu_supported_without_device_observation_is_not_certified() {
        let rt = rt_with(Arc::new(CapabilityProvider::from(local_gpu_caps())), 0).await;
        seed_registry(&rt, "local/qwen3.5-9b", 9 * GIB).await;

        // No residency input: device_bytes is unknown — M6 never invents VRAM.
        let out = rt
            .propose_memory_plan("local/qwen3.5-9b", Some(12 * GIB), None)
            .await
            .unwrap();
        let vram = &out.plan.budget.by_tier[&Tier::Vram];
        assert_eq!(vram.status, PlanTierStatus::Unknown);
        assert!(vram.unknown_reason.contains("unobserved"));
        assert_eq!(vram.currently_observed, None);
        assert!(out.plan.recommended_tier != Some(Tier::Vram));
    }

    #[tokio::test]
    async fn c_gpu_supported_with_measured_headroom_certifies_vram() {
        let rt = rt_with(Arc::new(CapabilityProvider::from(local_gpu_caps())), 0).await;
        seed_registry(&rt, "local/qwen3.5-9b", 9 * GIB).await;
        seed_anatomy(&rt, "local/qwen3.5-9b", 9 * GIB, vec![tensor("w", 8 * GIB)]).await;

        // A caller with a real device observer supplies measured residency.
        let observed =
            openrt_resources::ResidencySnapshot::unknown().with_device_bytes(Some(2 * GIB));
        let out = rt
            .propose_memory_plan("local/qwen3.5-9b", Some(12 * GIB), Some(observed))
            .await
            .unwrap();
        let vram = &out.plan.budget.by_tier[&Tier::Vram];
        assert_eq!(vram.status, PlanTierStatus::Fits);
        assert_eq!(out.plan.recommended_tier, Some(Tier::Vram));
        assert_eq!(out.plan.status, PlanStatus::Executable);
        // Sizing came from the registry anatomy, not the file.
        assert_eq!(
            out.plan.budget.basis,
            openrt_resources::ResidentBasis::ResidentEstimate
        );
        assert_eq!(out.plan.budget.requested_bytes, Some(8 * GIB));
    }

    #[tokio::test]
    async fn d_gate_denial_keeps_the_plan_denied_and_never_consumes_override() {
        // Headroom of 99% is unachievable on any live host → the gate is a
        // standing denial.
        let rt = rt_with(Arc::new(CapabilityProvider::from(local_gpu_caps())), 99).await;
        seed_registry(&rt, "local/qwen3.5-9b", 9 * GIB).await;
        seed_anatomy(&rt, "local/qwen3.5-9b", 9 * GIB, vec![tensor("w", 8 * GIB)]).await;

        // Arm an operator override first: planning must not spend it.
        assert!(rt
            .resource_gate
            .grant_override("test surface", "test operator"));
        assert!(rt.resource_gate.override_pending());

        let out = rt
            .propose_memory_plan("local/qwen3.5-9b", Some(12 * GIB), None)
            .await
            .unwrap();
        assert_eq!(
            out.plan.status,
            PlanStatus::Denied,
            "a denied plan stays denied"
        );
        assert!(
            rt.resource_gate.override_pending(),
            "planning must not consume the one-shot override"
        );
    }

    #[tokio::test]
    async fn e_remote_provider_fabricates_nothing() {
        let rt = rt_with(Arc::new(MockProvider::echo("remote")), 0).await;
        seed_registry(&rt, "local/qwen3.5-9b", 9 * GIB).await;

        let out = rt
            .propose_memory_plan("local/qwen3.5-9b", Some(12 * GIB), None)
            .await
            .unwrap();
        assert!(out.plan.recommended_tier.is_none());
        assert!(!out.plan.is_executable());
        // No residency was fabricated on any tier.
        for (tier, budget) in &out.plan.budget.by_tier {
            if *tier != Tier::Nvme {
                assert_eq!(
                    budget.currently_observed, None,
                    "tier {tier:?} must not inherit a fabricated residency"
                );
            }
        }
        assert_eq!(
            out.plan.budget.by_tier[&Tier::Vram].status,
            PlanTierStatus::Unknown
        );
    }

    // ---- conservatism + determinism + no-execution ------------------------

    #[tokio::test]
    async fn missing_anatomy_preserves_unknown_and_plans_safely() {
        let rt = rt_with(Arc::new(CapabilityProvider::from(local_gpu_caps())), 0).await;
        // Metadata only: no anatomy record, no payload — planning must still
        // return safely and honestly (missing anatomy stays missing).
        seed_registry(&rt, "meta/only", 9 * GIB).await;

        let out = rt
            .propose_memory_plan("meta/only", Some(12 * GIB), None)
            .await
            .unwrap();
        assert!(!out.anatomy_available, "absent anatomy stays absent");
        assert_eq!(
            out.plan.budget.basis,
            openrt_resources::ResidentBasis::MeasuredFileSize
        );
        assert_eq!(out.plan.budget.requested_bytes, Some(9 * GIB));
        assert_eq!(
            out.plan.budget.by_tier[&Tier::Nvme].currently_observed,
            Some(9 * GIB),
            "registry-declared artifact size is the NVMe observation"
        );
        // Sized from file size only → advisory, never executable.
        assert_eq!(out.plan.status, PlanStatus::AdvisoryOnly);
    }

    #[tokio::test]
    async fn unknown_artifact_id_fails_closed() {
        let rt = rt_with(Arc::new(CapabilityProvider::from(remote_caps())), 0).await;
        let err = rt
            .propose_memory_plan("does/not-exist", Some(12 * GIB), None)
            .await
            .unwrap_err();
        match err {
            Error::Registry(RegistryError::NotFound(id)) => assert_eq!(id, "does/not-exist"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn repeated_planning_is_deterministic() {
        let rt = rt_with(Arc::new(CapabilityProvider::from(local_gpu_caps())), 0).await;
        seed_registry(&rt, "local/qwen3.5-9b", 9 * GIB).await;
        seed_anatomy(&rt, "local/qwen3.5-9b", 9 * GIB, vec![tensor("w", 8 * GIB)]).await;

        let a = rt
            .propose_memory_plan("local/qwen3.5-9b", Some(12 * GIB), None)
            .await
            .unwrap();
        let b = rt
            .propose_memory_plan("local/qwen3.5-9b", Some(12 * GIB), None)
            .await
            .unwrap();
        assert_eq!(a, b, "identical inputs must produce identical plans");
    }

    #[tokio::test]
    async fn planning_triggers_no_execution_or_payload_reads() {
        let provider = Arc::new(CapabilityProvider::from(local_gpu_caps()));
        let rt = rt_with(provider.clone(), 0).await;
        seed_registry(&rt, "local/qwen3.5-9b", 9 * GIB).await;

        let before = provider.call_count();
        let _ = rt
            .propose_memory_plan("local/qwen3.5-9b", Some(12 * GIB), None)
            .await
            .unwrap();
        assert_eq!(
            provider.call_count(),
            before,
            "planning must never reach the provider's inference path"
        );
    }

    #[tokio::test]
    async fn runtime_memory_plan_serializes_to_stable_json() {
        let rt = rt_with(Arc::new(CapabilityProvider::from(local_gpu_caps())), 0).await;
        seed_registry(&rt, "local/qwen3.5-9b", 9 * GIB).await;

        let out = rt
            .propose_memory_plan("local/qwen3.5-9b", Some(12 * GIB), None)
            .await
            .unwrap();
        let json_a = serde_json::to_string_pretty(&out).unwrap();
        let json_b = serde_json::to_string_pretty(&out).unwrap();
        assert_eq!(json_a, json_b);
        let round: RuntimeMemoryPlan = serde_json::from_str(&json_a).unwrap();
        assert_eq!(round, out);
    }

    // ---- M7: multi-shard anatomy drives the runtime plan ------------------

    #[tokio::test]
    async fn m7_multi_shard_uses_aggregate_storage_not_index_member_size() {
        // 9 GiB model split as 3 x 3 GiB shards. The card/manifest only ever
        // knew the index member's size (say 4 KiB); the plan must size storage
        // from the anatomy's shard sum, not the registered member.
        let rt = rt_with(Arc::new(CapabilityProvider::from(local_gpu_caps())), 0).await;
        seed_registry(&rt, "local/qwen3.5-9b", 1 << 12).await;
        seed_sharded_anatomy(&rt, "local/qwen3.5-9b", 9 * GIB, 6 * GIB).await;

        let out = rt
            .propose_memory_plan("local/qwen3.5-9b", Some(12 * GIB), None)
            .await
            .unwrap();
        assert!(out.anatomy_available);
        assert_eq!(out.artifact_bytes, Some(9 * GIB));
        assert_eq!(
            out.plan.budget.by_tier[&Tier::Nvme].currently_observed,
            Some(9 * GIB),
            "NVMe observation is the aggregate storage size"
        );
        // Storage (9 GiB) is not residency (6 GiB): the certified working set
        // comes from tensor-payload lengths, independent of the shard sum.
        assert_eq!(
            out.plan.budget.basis,
            openrt_resources::ResidentBasis::ResidentEstimate
        );
        assert_eq!(out.plan.budget.requested_bytes, Some(6 * GIB));
    }

    #[tokio::test]
    async fn m7_multi_shard_planning_is_deterministic() {
        let rt = rt_with(Arc::new(CapabilityProvider::from(local_gpu_caps())), 0).await;
        seed_registry(&rt, "local/qwen3.5-9b", 1 << 12).await;
        seed_sharded_anatomy(&rt, "local/qwen3.5-9b", 9 * GIB, 6 * GIB).await;

        let a = rt
            .propose_memory_plan("local/qwen3.5-9b", Some(12 * GIB), None)
            .await
            .unwrap();
        let b = rt
            .propose_memory_plan("local/qwen3.5-9b", Some(12 * GIB), None)
            .await
            .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn m7_resident_estimate_ignores_shard_files() {
        // The residency estimate sums tensor-payload lengths only. A sharded
        // model with all lengths known certifies a working set even though the
        // shard sum far exceeds it (storage and residency are separate axes).
        let shard_total = 9u64;
        let shards = (0..3u32)
            .map(|i| openrt_models::ShardAnatomy {
                shard: i,
                name: format!("s{i}.safetensors"),
                file: format!("s{i}.safetensors"),
                size_bytes: 3,
                checksum: None,
            })
            .collect::<Vec<_>>();
        let anatomy = openrt_models::ModelAnatomy {
            schema_version: 1,
            format: AnatomyFormat::Safetensors,
            total_bytes: shard_total,
            shards: 3,
            tensors: vec![tensor("a", 4), tensor("b", 6)],
            shard_files: shards,
        };
        assert_eq!(
            resident_estimate(&Some(anatomy)),
            ResidentEstimate::Certified(10),
            "shard sizes never leak into the resident estimate"
        );
    }

    #[test]
    fn m7_single_file_v1_anatomy_keeps_card_sized_planning() {
        // A pre-M7 record (no shard_files, shards == 1) is single-file: the M6
        // card-size path must be preserved bit-for-bit.
        let v1 = openrt_models::ModelAnatomy {
            schema_version: 1,
            format: AnatomyFormat::Safetensors,
            total_bytes: 9 * GIB,
            shards: 1,
            tensors: vec![tensor("w", 8 * GIB)],
            shard_files: Vec::new(),
        };
        assert!(!v1.is_multi_shard());
        assert_eq!(v1.shard_count(), 1);
        assert_eq!(
            v1.shard_bytes(),
            None,
            "no shard records, no fabricated sum"
        );
    }

    // ---- M8: table-derived lengths stay advisory, never certifying --------

    #[test]
    fn m8_mixed_table_derived_estimate_is_advisory() {
        let a = tensor("a", 4);
        let mut q = tensor("q", 6);
        q.length_basis = Some(openrt_models::TensorLengthBasis::QuantTableV1);
        let anatomy = openrt_models::ModelAnatomy {
            schema_version: 1,
            format: AnatomyFormat::Safetensors,
            total_bytes: 10,
            shards: 1,
            tensors: vec![a, q],
            shard_files: Vec::new(),
        };
        assert_eq!(
            resident_estimate(&Some(anatomy)),
            ResidentEstimate::TableDerived(10),
            "one table-derived length demotes the whole sum to advisory"
        );
    }

    #[tokio::test]
    async fn m8_table_derived_estimate_is_advisory_never_certified() {
        let rt = rt_with(Arc::new(CapabilityProvider::from(local_gpu_caps())), 0).await;
        seed_registry(&rt, "local/qwen3.5-9b", 9 * GIB).await;
        let mut w = tensor("w", 8 * GIB);
        w.length_basis = Some(openrt_models::TensorLengthBasis::QuantTableV1);
        seed_anatomy(&rt, "local/qwen3.5-9b", 9 * GIB, vec![w]).await;

        let out = rt
            .propose_memory_plan("local/qwen3.5-9b", Some(12 * GIB), None)
            .await
            .unwrap();
        // The number is reported so it can be seen…
        assert_eq!(out.advisory_resident_bytes, Some(8 * GIB));
        // …but it never becomes the plan's basis and never certifies a fit.
        assert_ne!(
            out.plan.budget.basis,
            openrt_resources::ResidentBasis::ResidentEstimate
        );
        assert_ne!(out.plan.status, openrt_resources::PlanStatus::Executable);
    }

    #[tokio::test]
    async fn m8_advisory_field_round_trips_in_plan_json() {
        let rt = rt_with(Arc::new(CapabilityProvider::from(local_gpu_caps())), 0).await;
        seed_registry(&rt, "local/qwen3.5-9b", 9 * GIB).await;
        let mut w = tensor("w", 8 * GIB);
        w.length_basis = Some(openrt_models::TensorLengthBasis::QuantTableV1);
        seed_anatomy(&rt, "local/qwen3.5-9b", 9 * GIB, vec![w]).await;

        let out = rt
            .propose_memory_plan("local/qwen3.5-9b", Some(12 * GIB), None)
            .await
            .unwrap();
        let json = serde_json::to_string(&out).unwrap();
        let round: RuntimeMemoryPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(round, out);
        assert_eq!(round.advisory_resident_bytes, Some(8 * GIB));
    }

    // ---- M9: the runtime's VRAM observer feeds real device_bytes ----------

    /// Build a runtime with an injected (deterministic) VRAM observer.
    async fn rt_with_gpu(
        provider: Arc<dyn ModelProvider>,
        observer: Arc<dyn openrt_resources::GpuObserver>,
    ) -> Arc<Runtime> {
        crate::RuntimeBuilder::new()
            .config(Config::default())
            .store(Arc::new(MemoryStore::new()))
            .with_provider(provider)
            .gpu_observer(observer)
            .bus_capacity(16)
            .build()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn m9_observer_backfills_device_bytes_without_caller_residency() {
        // A MockGpuObserver reports fixed values: used 2 GiB on a 12 GiB GPU.
        let observer = Arc::new(openrt_resources::MockGpuObserver {
            rows: vec![("RTX 4070".into(), 12_288, 2_048)],
        });
        let rt = rt_with_gpu(
            Arc::new(CapabilityProvider::from(local_gpu_caps())),
            observer,
        )
        .await;
        seed_registry(&rt, "local/qwen3.5-9b", 9 * GIB).await;
        seed_anatomy(&rt, "local/qwen3.5-9b", 9 * GIB, vec![tensor("w", 8 * GIB)]).await;

        // No caller residency: the observer's measurement must flow into the
        // plan's VRAM tier (a real measurement, never a guess).
        let out = rt
            .propose_memory_plan("local/qwen3.5-9b", Some(12 * GIB), None)
            .await
            .unwrap();
        let vram = &out.plan.budget.by_tier[&Tier::Vram];
        assert_eq!(vram.currently_observed, Some(2_048 * 1024 * 1024));
        assert_eq!(vram.status, PlanTierStatus::Fits);
        assert_eq!(out.plan.recommended_tier, Some(Tier::Vram));
        assert!(out.plan.is_executable());
    }

    #[tokio::test]
    async fn m9_no_observer_keeps_device_honestly_unknown() {
        // Explicit default (no-op) observer: nothing measured, nothing invented.
        let observer = Arc::new(openrt_resources::NoGpuObserver);
        let rt = rt_with_gpu(
            Arc::new(CapabilityProvider::from(local_gpu_caps())),
            observer,
        )
        .await;
        seed_registry(&rt, "local/qwen3.5-9b", 9 * GIB).await;
        seed_anatomy(&rt, "local/qwen3.5-9b", 9 * GIB, vec![tensor("w", 8 * GIB)]).await;

        let out = rt
            .propose_memory_plan("local/qwen3.5-9b", Some(12 * GIB), None)
            .await
            .unwrap();
        let vram = &out.plan.budget.by_tier[&Tier::Vram];
        assert_eq!(vram.currently_observed, None);
        assert_eq!(vram.status, PlanTierStatus::Unknown);
        assert_eq!(
            rt.observe_gpu().await.source,
            openrt_resources::GpuObservationSource::None
        );
    }

    #[tokio::test]
    async fn m9_caller_residency_wins_over_the_observer() {
        let observer = Arc::new(openrt_resources::MockGpuObserver {
            rows: vec![("RTX 4070".into(), 12_288, 2_048)],
        });
        let rt = rt_with_gpu(
            Arc::new(CapabilityProvider::from(local_gpu_caps())),
            observer,
        )
        .await;
        seed_registry(&rt, "local/qwen3.5-9b", 9 * GIB).await;
        seed_anatomy(&rt, "local/qwen3.5-9b", 9 * GIB, vec![tensor("w", 8 * GIB)]).await;

        // A caller-supplied measurement is honored verbatim and is never
        // overwritten by the observer.
        let caller_obs = openrt_resources::ResidencySnapshot::unknown()
            .with_device_bytes(Some(4 * GIB))
            .with_nvme_bytes(Some(8 * GIB));
        let out = rt
            .propose_memory_plan("local/qwen3.5-9b", Some(12 * GIB), Some(caller_obs))
            .await
            .unwrap();
        let vram = &out.plan.budget.by_tier[&Tier::Vram];
        assert_eq!(
            vram.currently_observed,
            Some(4 * GIB),
            "caller data is never overwritten by the observer"
        );
        assert_eq!(
            out.plan.budget.by_tier[&Tier::Nvme].currently_observed,
            Some(8 * GIB)
        );
    }

    #[tokio::test]
    async fn m9_deterministic_observer_keeps_planning_deterministic() {
        let observer = Arc::new(openrt_resources::MockGpuObserver {
            rows: vec![("RTX 4070".into(), 12_288, 2_048)],
        });
        let rt = rt_with_gpu(
            Arc::new(CapabilityProvider::from(local_gpu_caps())),
            observer,
        )
        .await;
        seed_registry(&rt, "local/qwen3.5-9b", 9 * GIB).await;
        seed_anatomy(&rt, "local/qwen3.5-9b", 9 * GIB, vec![tensor("w", 8 * GIB)]).await;

        let a = rt
            .propose_memory_plan("local/qwen3.5-9b", Some(12 * GIB), None)
            .await
            .unwrap();
        let b = rt
            .propose_memory_plan("local/qwen3.5-9b", Some(12 * GIB), None)
            .await
            .unwrap();
        assert_eq!(a, b, "a fixed observer yields identical plans");
    }

    // ---- M10: nvme_bytes measured directly from local storage -------------

    /// Seed a registry entry whose manifest points at a real local file.
    async fn seed_registry_external(
        rt: &Runtime,
        id: &str,
        declared_size: u64,
        path: &std::path::Path,
    ) {
        let key = openrt_core::encode_safe_key(id);
        let card = RegistryCard {
            artifact_id: id.to_string(),
            display_name: "fixture".to_string(),
            family: "demo".to_string(),
            tags: vec![],
            installed_bytes: 0,
            artifact_bytes: Some(declared_size),
            blob_location: Some(path.to_string_lossy().into_owned()),
            state: LifecycleState::Active,
            source: SourceLabel::LocalImport,
            digest_prefix: None,
        };
        let manifest = ManifestV1 {
            schema_version: MANIFEST_SCHEMA_VERSION,
            artifact_id: id.to_string(),
            kind: openrt_registry::RegistryArtifactKind::Binary,
            blobs: vec![BlobRef {
                file_key: "model.bin".to_string(),
                digest: "fixture (payload exists on disk)".to_string(),
                size_bytes: declared_size,
                location: BlobLocation::External {
                    path: path.to_string_lossy().into_owned(),
                },
            }],
            base_digest: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };
        rt.store
            .put_record(META_NS, &key, &serde_json::to_value(card).unwrap())
            .await
            .unwrap();
        rt.store
            .put_record(MANIFESTS_NS, &key, &serde_json::to_value(manifest).unwrap())
            .await
            .unwrap();
    }

    fn temp_dir(tag: &str, id: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "openrt-m10-{}-{}-{tag}",
            std::process::id(),
            id.replace('/', "-")
        ))
    }

    #[tokio::test]
    async fn m10_direct_stat_beats_the_declared_proxy() {
        let dir = temp_dir("single", "m10a");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("model.bin");
        std::fs::write(&path, vec![0u8; 1234]).unwrap();

        let rt = rt_with(Arc::new(CapabilityProvider::from(local_gpu_caps())), 0).await;
        // Declared size says 9 GiB; the file on disk is really 1234 bytes.
        seed_registry_external(&rt, "local/m10a", 9 * GIB, &path).await;

        let out = rt
            .propose_memory_plan("local/m10a", Some(12 * GIB), None)
            .await
            .unwrap();
        assert_eq!(
            out.plan.budget.by_tier[&Tier::Nvme].currently_observed,
            Some(1234),
            "a real stat must replace the declared proxy"
        );
        assert!(out.nvme_bytes_measured, "must be labelled as measured");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn m10_missing_file_falls_back_to_the_declared_proxy() {
        // Register an external path that does not exist: no direct read is
        // possible, so the declared size is used and honestly labelled.
        let rt = rt_with(Arc::new(CapabilityProvider::from(local_gpu_caps())), 0).await;
        seed_registry(&rt, "local/m10b", 9 * GIB).await; // "fixture-path-never-read"

        let out = rt
            .propose_memory_plan("local/m10b", Some(12 * GIB), None)
            .await
            .unwrap();
        assert_eq!(
            out.plan.budget.by_tier[&Tier::Nvme].currently_observed,
            Some(9 * GIB),
            "declared proxy still fills the unmeasurable case"
        );
        assert!(!out.nvme_bytes_measured, "proxy is NOT a measurement");
    }

    #[tokio::test]
    async fn m10_multi_shard_direct_read_is_all_or_nothing() {
        let dir = temp_dir("shards", "m10c");
        std::fs::create_dir_all(&dir).unwrap();
        // Shard files on disk sum to 300 bytes — deliberately NOT the
        // registered 600, so "measured" and "declared" are distinguishable.
        let file_bytes = 300u64;
        let thirds = [298u64, 1, 1];
        for (i, n) in thirds.iter().enumerate() {
            std::fs::write(
                dir.join(format!("model-{i}.safetensors")),
                vec![0u8; *n as usize],
            )
            .unwrap();
        }
        let index = dir.join("model.safetensors");
        std::fs::write(&index, vec![0u8; 32]).unwrap();

        let rt = rt_with(Arc::new(CapabilityProvider::from(local_gpu_caps())), 0).await;
        seed_registry_external(&rt, "local/m10c", 9 * GIB, &index).await;
        seed_sharded_anatomy(&rt, "local/m10c", 600, 200).await;

        let out = rt
            .propose_memory_plan("local/m10c", Some(12 * GIB), None)
            .await
            .unwrap();
        assert_eq!(
            out.plan.budget.by_tier[&Tier::Nvme].currently_observed,
            Some(file_bytes),
            "sum of the statted shards, not the declared proxy"
        );
        assert!(out.nvme_bytes_measured);
        // resident_sum 200 -> three tensors of 66 each (= 198) in the helper.
        assert_eq!(out.plan.budget.requested_bytes, Some(198));
        assert_eq!(
            out.plan.budget.basis,
            openrt_resources::ResidentBasis::ResidentEstimate
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn m10_missing_shard_aborts_the_whole_direct_read() {
        let dir = temp_dir("shard-missing", "m10d");
        std::fs::create_dir_all(&dir).unwrap();
        // Only the first shard exists; model-1/model-2 are absent.
        std::fs::write(dir.join("model-0.safetensors"), vec![0u8; 4]).unwrap();
        let index = dir.join("model.safetensors");
        std::fs::write(&index, vec![0u8; 32]).unwrap();

        let rt = rt_with(Arc::new(CapabilityProvider::from(local_gpu_caps())), 0).await;
        seed_registry_external(&rt, "local/m10d", 9 * GIB, &index).await;
        // Registered shard sum = 600 (the proxy that would be used).
        seed_sharded_anatomy(&rt, "local/m10d", 600, 200).await;

        let out = rt
            .propose_memory_plan("local/m10d", Some(12 * GIB), None)
            .await
            .unwrap();
        assert_eq!(
            out.plan.budget.by_tier[&Tier::Nvme].currently_observed,
            Some(600),
            "partial shards must NOT be summed — fall back to the proxy"
        );
        assert!(!out.nvme_bytes_measured);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
