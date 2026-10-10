//! Memory Fabric M3/M5 — the tier vocabulary, the [`MemoryPlan`] proposal
//! layer, and capability-aware planning.
//!
//! This module is a **pure planner**. It answers one question:
//!
//! > "Given what we observe and what the deployment declares, which memory
//! > tier could a model artifact be resident in, how much of each tier would
//! > it request, and should the ResourceGate be asked to admit anything?"
//!
//! It never allocates model memory, never loads or mmaps an artifact, and
//! never asks a provider or backend to change how it executes. Proposal beats
//! authorisation: [`MemoryPlan`] proposes; [`crate::limits::ResourceGate`]
//! remains the **only** authority. M3/M5 ships no executor — an "executable"
//! plan means *a placement is certifiable and the gate allows it*, not that
//! OpenRT performs it.
//!
//! # Numbers mean what their tier says (the three-GiB distinction)
//!
//! A byte count is only meaningful when its tier is named, because the same
//! "9 GiB" means three very different claims:
//!
//! * **"The artifact is 9 GiB on NVMe"** — [`crate::residency::ResidencySnapshot::nvme_bytes`],
//!   the declared artifact size on local storage. This is *file size*, and file
//!   size is **not** resident memory. The [`Tier::Nvme`] budget carries it as
//!   observation only; the planner never sizes a residency proposal from it.
//! * **"9 GiB of VRAM is available"** — capacity minus currently-resident
//!   device bytes. The planner certifies a [`Tier::Vram`] fit **only** when
//!   both the declared capacity and an observed utilization are present. With
//!   usage unobserved (the M1 default `device_bytes: None`), even a 12 GiB
//!   declared capacity leaves the fit [`PlanTierStatus::Unknown`] — never a
//!   silent fit.
//! * **"9 GiB of process-owned RAM is available"** — host memory headroom,
//!   evaluated with the *same* policy arithmetic the ResourceGate uses
//!   ([`crate::preflight::ResourcePreflight::evaluate`]) against a host whose
//!   available memory has been reduced by the proposal. A known measurement is
//!   required; an unknown one stays [`PlanTierStatus::Unknown`].
//!
//! # Honesty rules
//!
//! * Nothing is fabricated. Every `Option` is `None` until a real observer or
//!   deployment declaration supplies a value, matching the residency snapshot.
//! * An unknown measurement can certify nothing. [`Tier::PageCache`] is
//!   reclaimable by the kernel and has no hard capacity, so its fit is always
//!   `Unknown`; it can never become a recommendation.
//! * Backend capability is an input a deployment declares; M3/M5 has no
//!   runtime probe. [`BackendTierCapabilities::default`] is the conservative
//!   CPU-only posture; the M5 adapter (runtime crate) produces the view from a
//!   provider's declared capabilities, keeping `Unknown` and `Unsupported`
//!   from ever becoming executable.
//! * The gate is consulted by probe only ([`ResourceGate::preflight`] and
//!   [`ResourceGate::admit_peek`]); [`MemoryPlan::evaluate_against_gate`]
//!   never consumes or arms an operator override. A plan the gate denies stays
//!   [`PlanStatus::Denied`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::limits::{ResourceGate, ResourceLimits};
use crate::preflight::{PreflightVerdict, ResourcePreflight, ResourceStatus};
use crate::residency::ResidencySnapshot;
use crate::system::{MemInfo, SystemInfo};

/// Version of the plan schema. Bump additively only.
pub const MEMORY_PLAN_SCHEMA_VERSION: u32 = 1;

/// The memory tiers the planner reasons about.
///
/// Serde order is stable (`snake_case`), and the `Ord` derive makes
/// [`MemoryBudget`] iteration deterministic, so two identical inputs always
/// produce identical plans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// GPU device memory.
    Vram,
    /// Process-owned anonymous RAM (`RssAnon`).
    RamOwned,
    /// Kernel page cache behind file-backed mapping pages (`RssFile`).
    /// Reclaimable; never certifiable as residency.
    PageCache,
    /// Local model-artifact storage (the artifact's origin). Backing, not
    /// residency.
    Nvme,
}

impl Tier {
    /// Stable label used by tuple surfaces and audit text.
    pub fn as_str(&self) -> &'static str {
        match self {
            Tier::Vram => "vram",
            Tier::RamOwned => "ram_owned",
            Tier::PageCache => "page_cache",
            Tier::Nvme => "nvme",
        }
    }

    /// Human label for TUI surfaces.
    pub fn label(&self) -> &'static str {
        match self {
            Tier::Vram => "VRAM",
            Tier::RamOwned => "Owned RAM",
            Tier::PageCache => "Page cache",
            Tier::Nvme => "NVMe",
        }
    }
}

/// How confident the plan is in the number it sizes placements with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResidentBasis {
    /// Sized from the measured artifact file size. A measured file, but the
    /// file size is an **upper bound**, not a resident footprint — such a plan
    /// is advisory.
    MeasuredFileSize,
    /// Sized from a caller-supplied estimate of resident bytes (e.g. a model
    /// resource profile). The sizing is an estimate by definition.
    ResidentEstimate,
    /// No sizing at all: neither a file size nor an estimate is known.
    Missing,
}

/// Whether a tier can certifyably hold the proposed bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanTierStatus {
    /// Measured headroom is sufficient.
    Fits,
    /// Fits, but the host would be left close to the gate's floor.
    Tight,
    /// Measured headroom is insufficient.
    DoesNotFit,
    /// Cannot certify: no observer, no capacity, no backend capability, or a
    /// tier that is not a residency target. Never treated as fit or unfit —
    /// only unknown.
    Unknown,
}

impl PlanTierStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            PlanTierStatus::Fits => "FITS",
            PlanTierStatus::Tight => "TIGHT",
            PlanTierStatus::DoesNotFit => "DOES_NOT_FIT",
            PlanTierStatus::Unknown => "UNKNOWN",
        }
    }
}

/// Overall posture of a [`MemoryPlan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    /// A recommended residency target certifiably fits with measured headroom,
    /// sized from a resident-footprint estimate, and (when a gate was
    /// consulted) the gate admits. "Executable" means the placement is
    /// certifiable — M3 performs no placement.
    Executable,
    /// Informative only: the sizing is an upper bound, a fit is unknown or
    /// absent, or the fit is tight. Never acted on without operator review.
    AdvisoryOnly,
    /// Nothing fits and nothing can be certified; there is no placement to
    /// propose.
    Unavailable,
    /// The ResourceGate's current posture denies admissions; the plan repeats
    /// the denial. A denied plan stays denied.
    Denied,
}

impl PlanStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            PlanStatus::Executable => "EXECUTABLE",
            PlanStatus::AdvisoryOnly => "ADVISORY_ONLY",
            PlanStatus::Unavailable => "UNAVAILABLE",
            PlanStatus::Denied => "DENIED",
        }
    }
}

/// Confidence in the plan's sizing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanConfidence {
    /// Sized from a measured artifact file size.
    Measured,
    /// Sized from a declared/derived estimate (resident estimate, declared
    /// VRAM capacity).
    Estimated,
    /// No sizing could be made.
    Unknown,
}

/// Tiers a backend can actually execute from.
///
/// A deployment declares this, or — M5 — the capability adapter in the runtime
/// crate derives it from a provider's declared capabilities
/// ([`PlannerCapabilityView`]). M3/M5 has no runtime probe. The default is
/// the conservative CPU-only posture: every provider runs from host RAM at
/// minimum, but no backend is assumed to use VRAM or to run entirely from page
/// cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BackendTierCapabilities {
    /// Whether the backend can place weights in VRAM and execute from them.
    pub can_run_from_vram: bool,
    /// Whether the backend can run with weights resident in process-owned RAM.
    pub can_run_from_ram: bool,
    /// Whether the backend can run entirely from file-backed page-cache memory.
    pub can_run_from_page_cache: bool,
}

impl Default for BackendTierCapabilities {
    fn default() -> Self {
        Self {
            can_run_from_vram: false,
            can_run_from_ram: true,
            can_run_from_page_cache: false,
        }
    }
}

/// A capability-derived explanation the planner may surface (M5).
///
/// Produced **only** by the capability adapter in the runtime crate (the one
/// canonical `BackendCapabilities → backend capabilities` mapping path), then
/// handed to [`MemoryPlan::propose`] verbatim. The planner does not recompute
/// capability meaning; it renders the applicable sentence into a
/// [`TierBudget`] so the plan's rationale states *why* a tier is excluded or a
/// candidate. This keeps the distinction the bool view cannot carry: an
/// `Unsupported` capability ("unavailable") versus an `Unknown` one ("status
/// unknown"), where neither ever becomes executable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityNote {
    /// The tier the explanation is about.
    pub tier: Tier,
    /// Shown when the tier is excluded because the backend lacks the required
    /// capability (`Unsupported` or `Unknown`). `None` = not applicable.
    pub excluded: Option<String>,
    /// Shown when the capability is present (`Supported`), qualifying the tier
    /// as a candidate. `None` = not applicable.
    pub candidate: Option<String>,
}

/// One tier's budget: its limit, what is already committed to it, what remains,
/// and what this plan would propose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierBudget {
    pub tier: Tier,
    /// Capacity/limit of the tier. `None` = not bounded or not declared.
    pub capacity: Option<u64>,
    /// Bytes currently committed to the tier. `None` = no observer.
    pub reserved: Option<u64>,
    /// Bytes currently observed resident in the tier (device usage, owned
    /// anonymous pages, file-backed pages, declared artifact bytes).
    pub currently_observed: Option<u64>,
    /// What remains for new placements: `capacity - reserved` where both are
    /// known, else mirroring the gate's headroom field. `None` when either
    /// side is unknown.
    pub headroom: Option<u64>,
    /// Bytes this plan would request of the tier. `None` when the tier is not
    /// a candidate (backend excludes it, or it is not a residency target).
    pub proposed: Option<u64>,
    pub status: PlanTierStatus,
    /// Why the tier is [`PlanTierStatus::Unknown`]. Empty otherwise.
    pub unknown_reason: String,
    /// Free-text qualification (e.g. "sized from file size, not residency").
    pub note: String,
}

impl TierBudget {
    /// One-line summary for compact surfaces.
    pub fn summary(&self) -> String {
        match self.status {
            PlanTierStatus::Unknown => format!("{} — {}", self.tier.label(), self.unknown_reason),
            _ => format!(
                "{} — {} ({} bytes proposed: {})",
                self.tier.label(),
                self.status.as_str(),
                self.proposed.unwrap_or(0),
                self.note
            ),
        }
    }
}

/// The set of per-tier budgets plus the total the plan would ask for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryBudget {
    /// Bytes the plan would ask its tiers to hold in total. `None` = no sizing
    /// could be made.
    pub requested_bytes: Option<u64>,
    /// How `requested_bytes` was obtained.
    pub basis: ResidentBasis,
    /// Per-tier budgets, deterministically ordered (`Vram`, `RamOwned`,
    /// `PageCache`, `Nvme`).
    pub by_tier: BTreeMap<Tier, TierBudget>,
}

/// What this plan would present to the ResourceGate's own dimensions.
///
/// The gate stays the only authority: nothing in M3 acts on this. It exists so
/// a future executor has exactly the two numbers the gate's policy reasons
/// about, produced **through** the gate's own arithmetic, never around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateProposal {
    /// Bytes this plan proposes be placed in process-owned RAM.
    pub proposed_owned_ram_bytes: Option<u64>,
    /// Bytes this plan proposes be placed in VRAM.
    pub proposed_vram_bytes: Option<u64>,
}

/// Inputs to [`MemoryPlan::propose`]. All references, all pure — nothing is
/// read from the environment or a backend.
#[derive(Debug, Clone)]
pub struct PlanInput<'a> {
    /// Registry/manifest artifact identifier.
    pub artifact_id: &'a str,
    /// Exact artifact file size (`ModelAnatomy.total_bytes` style). Measured.
    /// Not a resident footprint.
    pub artifact_bytes: Option<u64>,
    /// Optional estimate of resident bytes when executing (e.g. a model
    /// resource profile's working set). Takes precedence over `artifact_bytes`
    /// for sizing.
    pub estimated_resident_bytes: Option<u64>,
    /// Where the estimate came from, for the rationale. Empty when `None`.
    pub estimate_source: &'a str,
    /// Process residency observation (owned RAM, page cache, VRAM, NVMe).
    pub residency: &'a ResidencySnapshot,
    /// Host snapshot used for the host-RAM tier and the NVMe backing budget.
    pub system: &'a SystemInfo,
    /// The resource policy the gate enforces. The host-RAM fit is computed
    /// with the very same arithmetic as [`ResourcePreflight::evaluate`].
    pub limits: &'a ResourceLimits,
    /// Declared VRAM capacity (deployment configuration, not a measurement).
    /// `None` = the deployment has not declared one.
    pub vram_capacity_bytes: Option<u64>,
    /// Tiers the backend can execute from.
    pub backend: &'a BackendTierCapabilities,
    /// M5 capability→planner explanation notes, produced by the capability
    /// adapter. `&[]` = the planner was not given capability notes and uses
    /// its historical generic texts. Notes never add a tier: backing
    /// `can_run_*` bools still decide executability, and no note can turn an
    /// `Unsupported`/`Unknown` capability into an executable tier.
    pub capability: &'a [CapabilityNote],
}

/// A placement proposal for one model artifact.
///
/// Pure and deterministic: given identical inputs it produces identical plans
/// (including the observation-anchored timestamp, so equality is exact).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryPlan {
    pub artifact_id: String,
    pub schema_version: u32,
    /// Timestamp of the residency observation the plan is anchored to. Not a
    /// wall-clock creation time, so planning stays deterministic.
    pub observed_at: String,
    pub budget: MemoryBudget,
    /// The tier a residency placement would be proposed for, if any. `None`
    /// when nothing certifiably fits.
    pub recommended_tier: Option<Tier>,
    pub rationale: Vec<String>,
    pub confidence: PlanConfidence,
    pub status: PlanStatus,
    /// The gate's current posture when [`MemoryPlan::evaluate_against_gate`]
    /// was consulted. `None` = no gate has been consulted.
    pub gate_posture: Option<PreflightVerdict>,
}

impl MemoryPlan {
    /// Propose a plan **without** consulting a gate.
    ///
    /// Pure arithmetic over the inputs: no I/O, no allocation of model bytes,
    /// no backend invocation, no gate state touched.
    pub fn propose(input: &PlanInput) -> MemoryPlan {
        let (requested, basis) = match (input.estimated_resident_bytes, input.artifact_bytes) {
            (Some(e), _) => (Some(e), ResidentBasis::ResidentEstimate),
            (None, Some(a)) => (Some(a), ResidentBasis::MeasuredFileSize),
            (None, None) => (None, ResidentBasis::Missing),
        };

        let by_tier = build_tiers(input, requested, basis);
        let confidence = match basis {
            ResidentBasis::MeasuredFileSize => PlanConfidence::Measured,
            ResidentBasis::ResidentEstimate => PlanConfidence::Estimated,
            ResidentBasis::Missing => PlanConfidence::Unknown,
        };

        let recommended = recommend(&by_tier);
        let mut rationale = rationale_for(input, basis, requested, &by_tier, recommended);
        let status = if requested.is_none() {
            PlanStatus::Unavailable
        } else {
            let executable = basis == ResidentBasis::ResidentEstimate
                && recommended
                    .map(|t| by_tier[&t].status == PlanTierStatus::Fits)
                    .unwrap_or(false);
            if executable {
                PlanStatus::Executable
            } else {
                PlanStatus::AdvisoryOnly
            }
        };
        if matches!(status, PlanStatus::Unavailable) {
            rationale.push(
                "no resident sizing available (neither file size nor estimate); \
                 nothing can be proposed"
                    .to_string(),
            );
        }

        MemoryPlan {
            artifact_id: input.artifact_id.to_string(),
            schema_version: MEMORY_PLAN_SCHEMA_VERSION,
            observed_at: input.residency.timestamp.clone(),
            budget: MemoryBudget {
                requested_bytes: requested,
                basis,
                by_tier,
            },
            recommended_tier: recommended,
            rationale,
            confidence,
            status,
            gate_posture: None,
        }
    }

    /// Consult the existing gate — the only authority — and fold its **current**
    /// posture into the plan.
    ///
    /// This is a probe: it uses [`ResourceGate::preflight`] (pure arithmetic
    /// over the cached snapshot) and never consumes or arms an operator
    /// override. A plan the gate currently denies is demoted to
    /// [`PlanStatus::Denied`]; the recommendation is kept so the operator sees
    /// *why* it is denied, but the denial always wins.
    pub fn evaluate_against_gate(mut self, gate: &ResourceGate) -> MemoryPlan {
        let verdict = gate.preflight().verdict();
        match &verdict {
            PreflightVerdict::Denied(_) => {
                if self.status != PlanStatus::Unavailable {
                    self.status = PlanStatus::Denied;
                }
                self.rationale.push(
                    "the ResourceGate's current posture is a denial; this plan is denied".into(),
                );
            }
            PreflightVerdict::Admitted | PreflightVerdict::Constrained(_) => {
                self.rationale.push(
                    "the ResourceGate's current posture admits; authorisation for any actual \
                     placement remains with the gate"
                        .to_string(),
                );
            }
        }
        self.gate_posture = Some(verdict);
        self
    }

    /// The two numbers a future executor would present to the gate — produced
    /// from this plan, decided by the gate, and never acted on by M3.
    pub fn gate_proposal(&self) -> GateProposal {
        GateProposal {
            proposed_owned_ram_bytes: self
                .budget
                .by_tier
                .get(&Tier::RamOwned)
                .and_then(|b| b.proposed),
            proposed_vram_bytes: self
                .budget
                .by_tier
                .get(&Tier::Vram)
                .and_then(|b| b.proposed),
        }
    }

    /// Whether the plan is certifiably ready to hand to a (future) executor:
    /// recommendable, executable, and not demoted by the gate.
    pub fn is_executable(&self) -> bool {
        self.status == PlanStatus::Executable
    }

    /// One-line summary for the transcript.
    pub fn summary(&self) -> String {
        format!(
            "MEMORY PLAN {}: recommended {} — {} (requested {:?})",
            self.artifact_id,
            self.recommended_tier.map(|t| t.as_str()).unwrap_or("none"),
            self.status.as_str(),
            self.budget.requested_bytes,
        )
    }
}

fn capability_note<'a>(input: &'a PlanInput<'a>, tier: Tier) -> Option<&'a CapabilityNote> {
    input.capability.iter().find(|n| n.tier == tier)
}

/// The verbatim explanation used when a tier is excluded because the backend
/// lacks the capability. `None` = no capability note covers the tier, and the
/// planner falls back to its generic exclusion text.
fn note_excluded<'a>(input: &'a PlanInput<'a>, tier: Tier) -> Option<&'a str> {
    capability_note(input, tier)
        .and_then(|n| n.excluded.as_deref())
        .filter(|s| !s.is_empty())
}

fn note_candidate<'a>(input: &'a PlanInput<'a>, tier: Tier) -> Option<&'a str> {
    capability_note(input, tier)
        .and_then(|n| n.candidate.as_deref())
        .filter(|s| !s.is_empty())
}

fn append_candidate(note: &mut String, candidate: Option<&str>) {
    if let Some(c) = candidate {
        if note.is_empty() {
            *note = c.to_string();
        } else {
            note.push(' ');
            note.push_str(c);
        }
    }
}

fn build_tiers(
    input: &PlanInput,
    requested: Option<u64>,
    basis: ResidentBasis,
) -> BTreeMap<Tier, TierBudget> {
    let mut tiers = BTreeMap::new();
    tiers.insert(Tier::Vram, vram_budget(input, requested, basis));
    tiers.insert(Tier::RamOwned, ram_owned_budget(input, requested, basis));
    tiers.insert(Tier::PageCache, page_cache_budget(input, requested, basis));
    tiers.insert(Tier::Nvme, nvme_budget(input));
    tiers
}

fn vram_budget(input: &PlanInput, requested: Option<u64>, basis: ResidentBasis) -> TierBudget {
    let capacity = input.vram_capacity_bytes;
    let observed = input.residency.device_bytes;
    let reserved = observed;
    let headroom = match (capacity, reserved) {
        (Some(c), Some(r)) => Some(c.saturating_sub(r)),
        _ => None,
    };

    let proposable = input.backend.can_run_from_vram && basis != ResidentBasis::Missing;
    let proposed = if proposable { requested } else { None };

    let (status, unknown_reason, mut note) =
        match vram_state(input, basis, capacity, observed, headroom, proposed) {
            VramState::Fits(remaining, pct) => (
                PlanTierStatus::Fits,
                String::new(),
                format!("fits; leaves {} B free ({}% of capacity)", remaining, pct),
            ),
            VramState::DoesNotFit => (
                PlanTierStatus::DoesNotFit,
                String::new(),
                "proposed bytes exceed measured VRAM headroom".into(),
            ),
            VramState::Unknown(reason) => (PlanTierStatus::Unknown, reason, String::new()),
        };
    if proposable {
        append_candidate(&mut note, note_candidate(input, Tier::Vram));
    }

    TierBudget {
        tier: Tier::Vram,
        capacity,
        reserved,
        currently_observed: observed,
        headroom,
        proposed,
        status,
        unknown_reason,
        note,
    }
}

enum VramState {
    Fits(u64, u64),
    DoesNotFit,
    Unknown(String),
}

fn vram_state(
    input: &PlanInput,
    basis: ResidentBasis,
    capacity: Option<u64>,
    observed: Option<u64>,
    headroom: Option<u64>,
    proposed: Option<u64>,
) -> VramState {
    if !input.backend.can_run_from_vram {
        return VramState::Unknown(
            note_excluded(input, Tier::Vram)
                .unwrap_or("backend has no VRAM residency capability; tier excluded")
                .to_string(),
        );
    }
    if basis == ResidentBasis::Missing {
        return VramState::Unknown("no resident sizing available".into());
    }
    let Some(proposed) = proposed else {
        return VramState::Unknown("no resident sizing available".into());
    };
    let Some(headroom) = headroom else {
        let reason = match (capacity, observed) {
            (None, _) => "no VRAM capacity declared by the deployment".into(),
            (Some(_), None) => "VRAM utilization is unobserved; a fit cannot be certified".into(),
            _ => "VRAM headroom is unmeasured".into(),
        };
        return VramState::Unknown(reason);
    };
    if proposed > headroom {
        return VramState::DoesNotFit;
    }
    match capacity {
        Some(c) if c > 0 => {
            let remaining = c
                .saturating_sub(observed.unwrap_or(0))
                .saturating_sub(proposed);
            VramState::Fits(remaining, (remaining as f64 / c as f64 * 100.0) as u64)
        }
        _ => VramState::Fits(headroom.saturating_sub(proposed), 0),
    }
}

fn ram_owned_budget(input: &PlanInput, requested: Option<u64>, basis: ResidentBasis) -> TierBudget {
    let total_bytes = input.system.mem.total_kb.saturating_mul(1024);
    let avail_bytes = input.system.mem.available_kb.saturating_mul(1024);
    let proposable = input.backend.can_run_from_ram && basis != ResidentBasis::Missing;
    let proposed = if proposable { requested } else { None };

    // Fit is computed through the gate's own policy: evaluate the host posture
    // against the *hypothetical* availability with the proposal removed.
    let after = avail_bytes.saturating_sub(proposed.unwrap_or(0));
    let hypothetical = SystemInfo {
        mem: MemInfo {
            total_kb: input.system.mem.total_kb,
            available_kb: after / 1024,
        },
        ..input.system.clone()
    };
    let reading = ResourcePreflight::evaluate(&hypothetical, input.limits).memory;

    // A backend that cannot run from owned RAM excludes the tier: the tier is
    // Unknown (with the capability note verbatim when present), never "fits".
    let (status, unknown_reason, note) = if !input.backend.can_run_from_ram {
        (
            PlanTierStatus::Unknown,
            note_excluded(input, Tier::RamOwned)
                .unwrap_or("backend cannot run from process-owned RAM; tier excluded")
                .to_string(),
            String::new(),
        )
    } else {
        let (status, unknown_reason) = match reading.status {
            ResourceStatus::Ok => (PlanTierStatus::Fits, String::new()),
            ResourceStatus::Tight => (PlanTierStatus::Tight, String::new()),
            ResourceStatus::Blocked => (PlanTierStatus::DoesNotFit, String::new()),
            ResourceStatus::Unknown => (PlanTierStatus::Unknown, reading.unknown_reason.clone()),
        };
        let mut note =
            "fit evaluated through ResourcePreflight with availability reduced by the proposal"
                .to_string();
        append_candidate(&mut note, note_candidate(input, Tier::RamOwned));
        (status, unknown_reason, note)
    };

    TierBudget {
        tier: Tier::RamOwned,
        capacity: (total_bytes > 0).then_some(total_bytes),
        reserved: (total_bytes > 0).then(|| total_bytes.saturating_sub(avail_bytes)),
        currently_observed: input.residency.owned_ram_bytes,
        headroom: reading.headroom,
        proposed,
        status,
        unknown_reason,
        note,
    }
}

fn page_cache_budget(
    input: &PlanInput,
    requested: Option<u64>,
    basis: ResidentBasis,
) -> TierBudget {
    let proposable = input.backend.can_run_from_page_cache && basis != ResidentBasis::Missing;
    let mut note = "never a residency recommendation: page cache is reclaimable".to_string();
    if input.backend.can_run_from_page_cache {
        append_candidate(&mut note, note_candidate(input, Tier::PageCache));
    }
    TierBudget {
        tier: Tier::PageCache,
        capacity: None,
        reserved: None,
        currently_observed: input.residency.file_backed_bytes,
        headroom: None,
        proposed: if proposable { requested } else { None },
        status: PlanTierStatus::Unknown,
        unknown_reason: if input.backend.can_run_from_page_cache {
            "page-cache residency is kernel-reclaimable and has no hard capacity; \
             it cannot be certified"
                .to_string()
        } else {
            note_excluded(input, Tier::PageCache)
                .unwrap_or("backend has no page-cache residency capability; tier excluded")
                .to_string()
        },
        note,
    }
}

fn nvme_budget(input: &PlanInput) -> TierBudget {
    TierBudget {
        tier: Tier::Nvme,
        capacity: (input.system.disk.total_bytes > 0).then_some(input.system.disk.total_bytes),
        reserved: None,
        currently_observed: input.residency.nvme_bytes,
        headroom: (input.system.disk.avail_bytes > 0).then_some(input.system.disk.avail_bytes),
        proposed: None,
        status: PlanTierStatus::Unknown,
        unknown_reason: "NVMe is artifact storage only; no executable NVMe tier is claimed".into(),
        note: {
            match input.residency.nvme_bytes {
                Some(b) => format!(
                    "artifact declared as {} B on local storage — file size, NOT resident memory",
                    b
                ),
                None => "no local artifact bytes observed".into(),
            }
        },
    }
}

fn recommend(by_tier: &BTreeMap<Tier, TierBudget>) -> Option<Tier> {
    // Prefer Fits over Tight; among equals, tier order decides (Vram before
    // RamOwned), keeping the planner deterministic.
    let mut best: Option<Tier> = None;
    let mut best_rank = 2usize;
    for tier in [Tier::Vram, Tier::RamOwned] {
        let status = &by_tier[&tier].status;
        let rank = match status {
            PlanTierStatus::Fits => 0usize,
            PlanTierStatus::Tight => 1,
            PlanTierStatus::DoesNotFit | PlanTierStatus::Unknown => 2,
        };
        if rank < best_rank {
            best_rank = rank;
            best = Some(tier);
        }
    }
    best
}

fn rationale_for(
    input: &PlanInput,
    basis: ResidentBasis,
    requested: Option<u64>,
    by_tier: &BTreeMap<Tier, TierBudget>,
    recommended: Option<Tier>,
) -> Vec<String> {
    let mut r = Vec::new();
    match (basis, requested) {
        (ResidentBasis::ResidentEstimate, Some(b)) => r.push(format!(
            "sized from estimated resident footprint {} B (source: {}); \
             distinct from the {} B file on disk",
            b,
            if input.estimate_source.is_empty() {
                "declared estimate"
            } else {
                input.estimate_source
            },
            input.artifact_bytes.unwrap_or(0),
        )),
        (ResidentBasis::MeasuredFileSize, Some(b)) => r.push(format!(
            "sized from measured artifact size {} B — an upper bound, NOT a claimed \
             resident footprint (file size != resident memory)",
            b
        )),
        (ResidentBasis::Missing, None) => r.push(
            "no artifact file size and no resident estimate supplied; sizing impossible".into(),
        ),
        _ => {}
    }
    for tier in [Tier::Vram, Tier::RamOwned, Tier::PageCache, Tier::Nvme] {
        let b = &by_tier[&tier];
        match b.status {
            PlanTierStatus::Unknown => r.push(format!("{}: {}", tier.label(), b.unknown_reason)),
            PlanTierStatus::Fits | PlanTierStatus::Tight | PlanTierStatus::DoesNotFit => {
                r.push(format!("{}: {}", tier.label(), b.summary()));
            }
        }
    }
    match recommended {
        Some(t) => r.push(format!(
            "recommend {} (least contention: {} before {})",
            by_tier[&t].tier.label(),
            Tier::Vram.as_str(),
            Tier::RamOwned.as_str(),
        )),
        None => r.push("no residency target can be certified; recommend none".to_string()),
    }
    r.push(format!(
        "requested bytes by tier: {}",
        by_tier
            .values()
            .filter(|b| b.proposed.is_some())
            .map(|b| format!("{}={}", b.tier.as_str(), b.proposed.unwrap_or(0)))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::ResourceLimits;
    use crate::preflight::{PreflightVerdict, ResourceKind};
    use crate::residency::{ResidencySnapshot, ResidencySource, RESIDENCY_SCHEMA_VERSION};
    use crate::system::DiskUsage;

    const GIB: u64 = 1 << 30;
    // The 9 GiB model every placement fixture reasons about.
    const MODEL_BYTES: u64 = 9 * GIB;
    const MODEL_EST: u64 = 8 * GIB;

    fn snap(total_kb: u64, avail_kb: u64, avail_disk: u64) -> SystemInfo {
        SystemInfo {
            os: "linux".into(),
            kernel: "test".into(),
            arch: "x86_64".into(),
            cpu: Default::default(),
            uptime_secs: 1,
            mem: MemInfo {
                total_kb,
                available_kb: avail_kb,
            },
            disk: DiskUsage {
                total_bytes: 1 << 40,
                avail_bytes: avail_disk,
            },
        }
    }

    fn residency(
        owned: Option<u64>,
        file_backed: Option<u64>,
        device: Option<u64>,
        nvme: Option<u64>,
    ) -> ResidencySnapshot {
        ResidencySnapshot {
            schema_version: RESIDENCY_SCHEMA_VERSION,
            timestamp: "2026-01-01T00:00:00Z".into(),
            owned_ram_bytes: owned,
            file_backed_bytes: file_backed,
            device_bytes: device,
            nvme_bytes: nvme,
            source: ResidencySource::Fixture,
            notes: Vec::new(),
        }
    }

    fn default_limits() -> &'static ResourceLimits {
        static L: std::sync::OnceLock<ResourceLimits> = std::sync::OnceLock::new();
        L.get_or_init(ResourceLimits::default)
    }

    fn input<'a>(
        artifact_bytes: Option<u64>,
        estimate: Option<u64>,
        residency: &'a ResidencySnapshot,
        system: &'a SystemInfo,
        vram_capacity: Option<u64>,
        backend: &'a BackendTierCapabilities,
    ) -> PlanInput<'a> {
        PlanInput {
            artifact_id: "qwen3-9b",
            artifact_bytes,
            estimated_resident_bytes: estimate,
            estimate_source: "model resource profile",
            residency,
            system,
            limits: default_limits(),
            vram_capacity_bytes: vram_capacity,
            backend,
            capability: &[],
        }
    }

    #[test]
    fn nine_gib_model_fits_twelve_gib_vram_with_observer() {
        // 12 GiB declared capacity, 2 GiB observed in use -> 10 GiB headroom.
        let r = residency(Some(2 * GIB), None, Some(2 * GIB), Some(MODEL_BYTES));
        let sys = snap(32 * 1024 * 1024, 16 * 1024 * 1024, 1 << 30);
        let backend = BackendTierCapabilities {
            can_run_from_vram: true,
            ..BackendTierCapabilities::default()
        };
        let plan = MemoryPlan::propose(&input(
            Some(MODEL_BYTES),
            None,
            &r,
            &sys,
            Some(12 * GIB),
            &backend,
        ));

        let vram = &plan.budget.by_tier[&Tier::Vram];
        assert_eq!(vram.capacity, Some(12 * GIB));
        assert_eq!(vram.headroom, Some(10 * GIB));
        assert_eq!(vram.status, PlanTierStatus::Fits);
        assert_eq!(plan.recommended_tier, Some(Tier::Vram));
        // Sizing from file size -> advisory, and the plan says so out loud,
        // because file size is not resident memory.
        assert_eq!(plan.status, PlanStatus::AdvisoryOnly);
        assert!(plan
            .rationale
            .iter()
            .any(|l| l.contains("NOT a claimed resident footprint")));
        assert_eq!(plan.confidence, PlanConfidence::Measured);
    }

    #[test]
    fn unknown_vram_without_observer_never_certifies_fit() {
        // 12 GiB declared capacity, but no device observer: even a comfortably
        // large VRAM must not silently become "fits".
        let r = residency(Some(2 * GIB), None, None, Some(MODEL_BYTES));
        let sys = snap(32 * 1024 * 1024, 16 * 1024 * 1024, 1 << 30);
        let backend = BackendTierCapabilities {
            can_run_from_vram: true,
            ..BackendTierCapabilities::default()
        };
        let plan = MemoryPlan::propose(&input(
            Some(MODEL_BYTES),
            None,
            &r,
            &sys,
            Some(12 * GIB),
            &backend,
        ));

        let vram = &plan.budget.by_tier[&Tier::Vram];
        assert_eq!(vram.status, PlanTierStatus::Unknown);
        assert!(
            vram.unknown_reason.contains("utilization is unobserved"),
            "{}",
            vram.unknown_reason
        );
        assert_ne!(plan.status, PlanStatus::Executable);
        assert_eq!(plan.recommended_tier, Some(Tier::RamOwned));
    }

    #[test]
    fn unknown_ram_is_never_silently_ok() {
        // No /proc/meminfo -> RamOwned must be Unknown, not "fits".
        let r = residency(Some(GIB), None, Some(2 * GIB), None);
        let sys = snap(0, 0, 1 << 30);
        let backend = BackendTierCapabilities {
            can_run_from_vram: true,
            ..BackendTierCapabilities::default()
        };
        let plan = MemoryPlan::propose(&input(
            Some(MODEL_BYTES),
            None,
            &r,
            &sys,
            Some(12 * GIB),
            &backend,
        ));

        let ram = &plan.budget.by_tier[&Tier::RamOwned];
        assert_eq!(ram.status, PlanTierStatus::Unknown);
        assert!(
            ram.unknown_reason.contains("MemTotal"),
            "{}",
            ram.unknown_reason
        );
        assert_eq!(
            plan.budget.by_tier[&Tier::Vram].status,
            PlanTierStatus::Fits
        );
        assert_eq!(plan.recommended_tier, Some(Tier::Vram));
    }

    #[test]
    fn nvme_presence_is_file_size_not_residency() {
        // The artifact is on NVMe (9 GiB declared) but nothing is resident.
        // The plan must NOT count NVMe bytes as available residency or size a
        // proposal from them.
        let r = residency(None, None, None, Some(MODEL_BYTES));
        let sys = snap(32 * 1024 * 1024, 24 * 1024 * 1024, 1 << 30);
        let backend = BackendTierCapabilities {
            can_run_from_vram: true,
            ..BackendTierCapabilities::default()
        };
        let plan = MemoryPlan::propose(&input(
            Some(MODEL_BYTES),
            None,
            &r,
            &sys,
            Some(12 * GIB),
            &backend,
        ));

        let nvme = &plan.budget.by_tier[&Tier::Nvme];
        assert_eq!(nvme.currently_observed, Some(MODEL_BYTES));
        assert_eq!(
            nvme.proposed, None,
            "NVMe is backing, never a placement target"
        );
        assert_eq!(nvme.status, PlanTierStatus::Unknown);
        assert!(nvme.note.contains("NOT resident memory"), "{}", nvme.note);
        // Vram/RamOwned are untouched by the NVMe presence: without a device
        // observer the VRAM fit stays honestly Unobserved.
        assert_eq!(plan.budget.by_tier[&Tier::Vram].headroom, None);
        assert_eq!(
            plan.budget.by_tier[&Tier::Vram].status,
            PlanTierStatus::Unknown
        );
        assert_eq!(
            plan.budget.by_tier[&Tier::RamOwned].status,
            PlanTierStatus::Fits
        );
    }

    #[test]
    fn fits_vram_is_executable_with_resident_estimate() {
        let r = residency(Some(2 * GIB), None, Some(GIB), Some(MODEL_BYTES));
        let sys = snap(32 * 1024 * 1024, 16 * 1024 * 1024, 1 << 30);
        let backend = BackendTierCapabilities {
            can_run_from_vram: true,
            ..BackendTierCapabilities::default()
        };
        let plan = MemoryPlan::propose(&input(
            Some(MODEL_BYTES),
            Some(MODEL_EST),
            &r,
            &sys,
            Some(12 * GIB),
            &backend,
        ));

        assert_eq!(plan.budget.requested_bytes, Some(MODEL_EST));
        assert_eq!(plan.budget.basis, ResidentBasis::ResidentEstimate);
        assert_eq!(
            plan.budget.by_tier[&Tier::Vram].status,
            PlanTierStatus::Fits
        );
        assert_eq!(plan.recommended_tier, Some(Tier::Vram));
        assert_eq!(plan.status, PlanStatus::Executable);
        assert_eq!(plan.confidence, PlanConfidence::Estimated);
        assert!(plan.is_executable());
    }

    #[test]
    fn does_not_fit_vram_but_fits_ram() {
        // 8 GiB VRAM with 1 GiB in use -> 7 GiB headroom < 8 GiB estimate.
        // Host RAM: 48 GiB total, 32 GiB available, 15% floor = 7.2 GiB;
        // after placing 8 GiB -> 24 GiB available, comfortable.
        let r = residency(Some(2 * GIB), None, Some(GIB), Some(MODEL_BYTES));
        let sys = snap(48 * 1024 * 1024, 32 * 1024 * 1024, 1 << 30);
        let backend = BackendTierCapabilities {
            can_run_from_vram: true,
            ..BackendTierCapabilities::default()
        };
        let plan = MemoryPlan::propose(&input(
            Some(MODEL_BYTES),
            Some(MODEL_EST),
            &r,
            &sys,
            Some(8 * GIB),
            &backend,
        ));

        assert_eq!(
            plan.budget.by_tier[&Tier::Vram].status,
            PlanTierStatus::DoesNotFit
        );
        assert_eq!(
            plan.budget.by_tier[&Tier::RamOwned].status,
            PlanTierStatus::Fits
        );
        assert_eq!(plan.recommended_tier, Some(Tier::RamOwned));
        assert_eq!(plan.status, PlanStatus::Executable);
    }

    #[test]
    fn insufficient_headroom_blocks_placement() {
        // Host RAM: 16 GiB, 10 GiB available, 15% floor = 2.4 GiB; after
        // placing 9 GiB -> 1 GiB < floor -> does not fit.
        let r = residency(Some(2 * GIB), None, None, Some(MODEL_BYTES));
        let sys = snap(16 * 1024 * 1024, 10 * 1024 * 1024, 1 << 30);
        let plan = MemoryPlan::propose(&input(
            Some(MODEL_BYTES),
            Some(MODEL_EST),
            &r,
            &sys,
            None,
            &BackendTierCapabilities::default(),
        ));

        assert_eq!(
            plan.budget.by_tier[&Tier::RamOwned].status,
            PlanTierStatus::DoesNotFit
        );
        assert_eq!(
            plan.budget.by_tier[&Tier::Vram].status,
            PlanTierStatus::Unknown
        );
        assert_eq!(plan.recommended_tier, None);
        assert_eq!(plan.status, PlanStatus::AdvisoryOnly);
    }

    #[test]
    fn no_tier_available_yields_no_recommendation() {
        // No VRAM capacity, no observer, and host RAM cannot hold the model.
        let r = residency(Some(2 * GIB), None, None, Some(MODEL_BYTES));
        let sys = snap(16 * 1024 * 1024, 4 * 1024 * 1024, 1 << 30);
        let plan = MemoryPlan::propose(&input(
            Some(MODEL_BYTES),
            Some(MODEL_EST),
            &r,
            &sys,
            None,
            &BackendTierCapabilities::default(),
        ));

        assert_eq!(plan.recommended_tier, None);
        assert_eq!(plan.status, PlanStatus::AdvisoryOnly);
        assert!(plan
            .rationale
            .iter()
            .any(|l| l.contains("no residency target")));
    }

    #[test]
    fn gate_denial_keeps_the_plan_denied() {
        let r = residency(Some(2 * GIB), None, Some(GIB), Some(MODEL_BYTES));
        let sys = snap(32 * 1024 * 1024, 16 * 1024 * 1024, 1 << 30);
        let backend = BackendTierCapabilities {
            can_run_from_vram: true,
            ..BackendTierCapabilities::default()
        };
        let plan = MemoryPlan::propose(&input(
            Some(MODEL_BYTES),
            Some(MODEL_EST),
            &r,
            &sys,
            Some(12 * GIB),
            &backend,
        ));
        assert_eq!(plan.status, PlanStatus::Executable);

        // The gate's CURRENT host posture is a denial (memory below floor),
        // even though the plan's numbers look fine.
        let gate_sys = snap(32 * 1024 * 1024, 512 * 1024, 1 << 30);
        let gate = ResourceGate::new(ResourceLimits::default(), gate_sys);
        let plan = plan.evaluate_against_gate(&gate);

        assert!(matches!(
            plan.gate_posture,
            Some(PreflightVerdict::Denied(_))
        ));
        assert_eq!(plan.status, PlanStatus::Denied);
        assert!(!plan.is_executable());
        // The recommendation survives so the operator can see what was denied.
        assert_eq!(plan.recommended_tier, Some(Tier::Vram));
    }

    #[test]
    fn gate_approval_promotes_an_executable_plan() {
        let r = residency(Some(2 * GIB), None, Some(GIB), Some(MODEL_BYTES));
        let sys = snap(32 * 1024 * 1024, 16 * 1024 * 1024, 1 << 30);
        let backend = BackendTierCapabilities {
            can_run_from_vram: true,
            ..BackendTierCapabilities::default()
        };
        let plan = MemoryPlan::propose(&input(
            Some(MODEL_BYTES),
            Some(MODEL_EST),
            &r,
            &sys,
            Some(12 * GIB),
            &backend,
        ));
        let gate = ResourceGate::new(
            ResourceLimits::default(),
            snap(32 * 1024 * 1024, 16 * 1024 * 1024, 1 << 30),
        );
        let plan = plan.evaluate_against_gate(&gate);

        assert_eq!(plan.gate_posture, Some(PreflightVerdict::Admitted));
        assert_eq!(plan.status, PlanStatus::Executable);
        assert!(plan.is_executable());
        assert_eq!(plan.gate_proposal().proposed_vram_bytes, Some(MODEL_EST));
    }

    #[test]
    fn backend_capability_filters_the_recommendation() {
        // The model would fit VRAM numerically, but the backend cannot use
        // VRAM at all: the tier must be excluded, not recommended.
        let r = residency(Some(2 * GIB), None, Some(GIB), Some(MODEL_BYTES));
        let sys = snap(48 * 1024 * 1024, 32 * 1024 * 1024, 1 << 30);
        // Default backend: can_run_from_ram = true, vram = false.
        let plan = MemoryPlan::propose(&input(
            Some(MODEL_BYTES),
            Some(MODEL_EST),
            &r,
            &sys,
            Some(12 * GIB),
            &BackendTierCapabilities::default(),
        ));

        let vram = &plan.budget.by_tier[&Tier::Vram];
        assert_eq!(vram.status, PlanTierStatus::Unknown);
        assert!(
            vram.unknown_reason.contains("backend"),
            "{}",
            vram.unknown_reason
        );
        assert_eq!(plan.recommended_tier, Some(Tier::RamOwned));
    }

    #[test]
    fn planner_is_deterministic_with_identical_inputs() {
        let r = residency(Some(2 * GIB), None, Some(GIB), Some(MODEL_BYTES));
        let sys = snap(32 * 1024 * 1024, 16 * 1024 * 1024, 1 << 30);
        let backend = BackendTierCapabilities {
            can_run_from_vram: true,
            ..BackendTierCapabilities::default()
        };
        let a = plan_full(&r, &sys, &backend);
        let b = PlanInput {
            artifact_id: "qwen3-9b",
            artifact_bytes: Some(MODEL_BYTES),
            estimated_resident_bytes: Some(MODEL_EST),
            estimate_source: "model resource profile",
            residency: &r,
            system: &sys,
            limits: default_limits(),
            vram_capacity_bytes: Some(12 * GIB),
            backend: &backend,
            capability: &[],
        };
        assert_eq!(a, MemoryPlan::propose(&b));
        assert_eq!(a, MemoryPlan::propose(&a_input(&r, &sys, &backend)));
    }

    fn plan_full(
        r: &ResidencySnapshot,
        sys: &SystemInfo,
        backend: &BackendTierCapabilities,
    ) -> MemoryPlan {
        MemoryPlan::propose(&a_input(r, sys, backend))
    }

    fn a_input<'a>(
        r: &'a ResidencySnapshot,
        sys: &'a SystemInfo,
        backend: &'a BackendTierCapabilities,
    ) -> PlanInput<'a> {
        PlanInput {
            artifact_id: "qwen3-9b",
            artifact_bytes: Some(MODEL_BYTES),
            estimated_resident_bytes: Some(MODEL_EST),
            estimate_source: "model resource profile",
            residency: r,
            system: sys,
            limits: default_limits(),
            vram_capacity_bytes: Some(12 * GIB),
            backend,
            capability: &[],
        }
    }

    #[test]
    fn identical_inputs_produce_identical_plans() {
        let r = residency(Some(2 * GIB), None, Some(GIB), Some(MODEL_BYTES));
        let sys = snap(32 * 1024 * 1024, 16 * 1024 * 1024, 1 << 30);
        let backend = BackendTierCapabilities {
            can_run_from_vram: true,
            ..BackendTierCapabilities::default()
        };
        let p1 = MemoryPlan::propose(&a_input(&r, &sys, &backend));
        let mut r2 = r.clone();
        let sys2 = sys.clone();
        r2.notes = Vec::new();
        let p2 = MemoryPlan::propose(&PlanInput {
            artifact_id: "qwen3-9b",
            artifact_bytes: Some(MODEL_BYTES),
            estimated_resident_bytes: Some(MODEL_EST),
            estimate_source: "model resource profile",
            residency: &r2,
            system: &sys2,
            limits: default_limits(),
            vram_capacity_bytes: Some(12 * GIB),
            backend: &backend,
            capability: &[],
        });
        assert_eq!(p1, p2);
    }

    #[test]
    fn nothing_is_fabricated_when_everything_is_unknown() {
        // Every observer and declaration is absent, including the host disk —
        // nothing at all may be invented.
        let r = residency(None, None, None, None);
        let sys = SystemInfo {
            os: "linux".into(),
            kernel: "".into(),
            arch: "x86_64".into(),
            cpu: Default::default(),
            uptime_secs: 0,
            mem: MemInfo {
                total_kb: 0,
                available_kb: 0,
            },
            disk: DiskUsage {
                total_bytes: 0,
                avail_bytes: 0,
            },
        };
        let plan = MemoryPlan::propose(&input(
            None,
            None,
            &r,
            &sys,
            None,
            &BackendTierCapabilities::default(),
        ));

        assert_eq!(plan.budget.requested_bytes, None);
        assert_eq!(plan.budget.basis, ResidentBasis::Missing);
        assert!(plan.recommended_tier.is_none());
        assert_eq!(plan.status, PlanStatus::Unavailable);
        assert_eq!(plan.confidence, PlanConfidence::Unknown);
        for tier in [Tier::Vram, Tier::RamOwned, Tier::PageCache, Tier::Nvme] {
            let b = &plan.budget.by_tier[&tier];
            assert_eq!(b.status, PlanTierStatus::Unknown);
            assert_eq!(b.proposed, None);
            // No hidden side effects from an unknown: budget may carry known
            // capacity only if declared, and none is here.
            assert_eq!(b.capacity, None);
            assert!(!b.unknown_reason.is_empty());
        }
        assert!(plan
            .rationale
            .iter()
            .any(|l| l.contains("no resident sizing")));
    }

    #[test]
    fn ram_fit_matches_the_gates_own_semantics() {
        // Regression: the plan's RamOwned status must be exactly what the
        // existing gate policy says for the reduced-availability host.
        let r = residency(Some(2 * GIB), None, None, None);
        let sys = snap(32 * 1024 * 1024, 12 * 1024 * 1024, 1 << 30);
        let plan = MemoryPlan::propose(&input(
            Some(MODEL_BYTES),
            None,
            &r,
            &sys,
            None,
            &BackendTierCapabilities::default(),
        ));

        let after_kb = (12 * 1024 * 1024 * 1024 - MODEL_BYTES) / 1024;
        let hypot = SystemInfo {
            mem: MemInfo {
                total_kb: sys.mem.total_kb,
                available_kb: after_kb,
            },
            ..sys
        };
        let gate_memory = ResourcePreflight::evaluate(&hypot, &ResourceLimits::default()).memory;
        let expected = match gate_memory.status {
            ResourceStatus::Ok => PlanTierStatus::Fits,
            ResourceStatus::Tight => PlanTierStatus::Tight,
            ResourceStatus::Blocked => PlanTierStatus::DoesNotFit,
            ResourceStatus::Unknown => PlanTierStatus::Unknown,
        };
        assert_eq!(plan.budget.by_tier[&Tier::RamOwned].status, expected);
    }

    #[test]
    fn gate_evaluation_does_not_consume_an_override() {
        // Probe semantics: evaluating a plan must not spend the operator's
        // one-shot grant the way an actual admit would.
        let r = residency(Some(2 * GIB), None, Some(GIB), Some(MODEL_BYTES));
        let sys = snap(32 * 1024 * 1024, 16 * 1024 * 1024, 1 << 30);
        let backend = BackendTierCapabilities {
            can_run_from_vram: true,
            ..BackendTierCapabilities::default()
        };
        let plan = MemoryPlan::propose(&input(
            Some(MODEL_BYTES),
            Some(MODEL_EST),
            &r,
            &sys,
            Some(12 * GIB),
            &backend,
        ));

        let gate = ResourceGate::new(
            ResourceLimits::default(),
            snap(32 * 1024 * 1024, 512 * 1024, 1 << 30),
        );
        gate.grant_override("operator will free memory", "tui");
        let plan = plan.evaluate_against_gate(&gate);

        assert_eq!(plan.status, PlanStatus::Denied, "raw posture still denies");
        assert!(
            gate.override_pending(),
            "a plan probe must not consume the one-shot override"
        );
    }

    #[test]
    fn page_cache_and_nvme_are_never_recommendations() {
        let r = residency(Some(2 * GIB), Some(20 * GIB), Some(GIB), Some(MODEL_BYTES));
        let sys = snap(32 * 1024 * 1024, 16 * 1024 * 1024, 1 << 30);
        let backend = BackendTierCapabilities {
            can_run_from_vram: true,
            can_run_from_page_cache: true,
            ..BackendTierCapabilities::default()
        };
        let plan = MemoryPlan::propose(&input(
            Some(MODEL_BYTES),
            Some(MODEL_EST),
            &r,
            &sys,
            Some(12 * GIB),
            &backend,
        ));

        assert_eq!(
            plan.budget.by_tier[&Tier::PageCache].status,
            PlanTierStatus::Unknown
        );
        assert_eq!(
            plan.budget.by_tier[&Tier::Nvme].status,
            PlanTierStatus::Unknown
        );
        assert_eq!(plan.recommended_tier, Some(Tier::Vram));
        assert!(!plan
            .rationale
            .iter()
            .any(|l| l.contains("recommend page_cache")));
    }

    #[test]
    fn vram_headroom_reflects_the_remaining_headroom() {
        // A thin fit still reports the actual remaining headroom and Fits.
        let r = residency(Some(2 * GIB), None, Some(11 * GIB), Some(MODEL_BYTES));
        let sys = snap(32 * 1024 * 1024, 16 * 1024 * 1024, 1 << 30);
        let backend = BackendTierCapabilities {
            can_run_from_vram: true,
            ..BackendTierCapabilities::default()
        };
        let plan = MemoryPlan::propose(&input(
            Some(MODEL_BYTES),
            Some(MODEL_EST),
            &r,
            &sys,
            Some(12 * GIB),
            &backend,
        ));
        let vram = &plan.budget.by_tier[&Tier::Vram];
        assert_eq!(vram.headroom, Some(GIB));
        // 8 GiB estimate does not fit into 1 GiB of headroom.
        assert_eq!(vram.status, PlanTierStatus::DoesNotFit);
        assert_eq!(plan.recommended_tier, Some(Tier::RamOwned));
    }

    #[test]
    fn tier_round_trips_through_serde() {
        let json = serde_json::to_string(&Tier::RamOwned).unwrap();
        assert_eq!(json, "\"ram_owned\"");
        let back: Tier = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Tier::RamOwned);
    }

    #[test]
    fn gate_proposal_reports_owned_ram_and_vram() {
        let r = residency(Some(2 * GIB), None, Some(GIB), Some(MODEL_BYTES));
        let sys = snap(32 * 1024 * 1024, 16 * 1024 * 1024, 1 << 30);
        let backend = BackendTierCapabilities {
            can_run_from_vram: true,
            ..BackendTierCapabilities::default()
        };
        let plan = MemoryPlan::propose(&input(
            Some(MODEL_BYTES),
            Some(MODEL_EST),
            &r,
            &sys,
            Some(12 * GIB),
            &backend,
        ));
        let proposal = plan.gate_proposal();
        assert_eq!(proposal.proposed_vram_bytes, Some(MODEL_EST));
        // The backend also runs from RAM, so the same footprint is proposed
        // there for the gate's host-memory dimension.
        assert_eq!(proposal.proposed_owned_ram_bytes, Some(MODEL_EST));
    }

    #[test]
    fn constrained_gate_posture_is_reported_not_denied() {
        // A Tight host (above floor, below the warning band) must stay
        // advisory-positively executable-capable, not be demoted to Denied.
        let r = residency(Some(2 * GIB), None, Some(GIB), Some(MODEL_BYTES));
        let sys = snap(32 * 1024 * 1024, 16 * 1024 * 1024, 1 << 30);
        let backend = BackendTierCapabilities {
            can_run_from_vram: true,
            ..BackendTierCapabilities::default()
        };
        let plan = MemoryPlan::propose(&input(
            Some(MODEL_BYTES),
            Some(MODEL_EST),
            &r,
            &sys,
            Some(12 * GIB),
            &backend,
        ));
        // Host: 16 GiB total, 3 GiB available -> 18.75% (Tight band).
        let gate = ResourceGate::new(
            ResourceLimits::default(),
            snap(16 * 1024 * 1024, 3 * 1024 * 1024, 1 << 30),
        );
        let plan = plan.evaluate_against_gate(&gate);
        assert!(matches!(
            plan.gate_posture,
            Some(PreflightVerdict::Constrained(v)) if v.contains(&ResourceKind::Memory)
        ));
        assert_eq!(plan.status, PlanStatus::Executable);
    }

    // ---- M5: capability-aware planning -------------------------------------

    fn cap(tier: Tier, excluded: Option<&str>, candidate: Option<&str>) -> CapabilityNote {
        CapabilityNote {
            tier,
            excluded: excluded.map(str::to_string),
            candidate: candidate.map(str::to_string),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn cap_input<'a>(
        artifact_bytes: Option<u64>,
        estimate: Option<u64>,
        residency: &'a ResidencySnapshot,
        system: &'a SystemInfo,
        vram_capacity: Option<u64>,
        backend: &'a BackendTierCapabilities,
        capability: &'a [CapabilityNote],
    ) -> PlanInput<'a> {
        PlanInput {
            artifact_id: "qwen3-9b",
            artifact_bytes,
            estimated_resident_bytes: estimate,
            estimate_source: "model resource profile",
            residency,
            system,
            limits: default_limits(),
            vram_capacity_bytes: vram_capacity,
            backend,
            capability,
        }
    }

    #[test]
    fn capability_note_round_trips_through_serde() {
        let n = cap(
            Tier::Vram,
            Some("VRAM unavailable: backend does not support GPU offload."),
            None,
        );
        let json = serde_json::to_string(&n).unwrap();
        let back: CapabilityNote = serde_json::from_str(&json).unwrap();
        assert_eq!(back, n);
    }

    #[test]
    fn unsupported_gpu_capability_blocks_executable_vram() {
        // 12 GiB GPU / 9 GiB model, Case A: the capability is Unsupported.
        // Even perfect VRAM numbers must not produce an executable VRAM
        // placement; the note explains it by capability.
        let r = residency(Some(2 * GIB), None, Some(2 * GIB), Some(MODEL_BYTES));
        let sys = snap(32 * 1024 * 1024, 16 * 1024 * 1024, 1 << 30);
        let backend = BackendTierCapabilities {
            can_run_from_vram: false,
            can_run_from_ram: true,
            can_run_from_page_cache: false,
        };
        let notes = [cap(
            Tier::Vram,
            Some("VRAM unavailable: backend does not support GPU offload."),
            None,
        )];
        let plan = MemoryPlan::propose(&cap_input(
            Some(MODEL_BYTES),
            Some(MODEL_EST),
            &r,
            &sys,
            Some(12 * GIB),
            &backend,
            &notes,
        ));

        let vram = &plan.budget.by_tier[&Tier::Vram];
        assert_eq!(vram.status, PlanTierStatus::Unknown, "blocked, never fits");
        assert_eq!(
            vram.unknown_reason,
            "VRAM unavailable: backend does not support GPU offload."
        );
        assert_eq!(vram.proposed, None);
        assert_ne!(
            plan.recommended_tier,
            Some(Tier::Vram),
            "no VRAM recommendation"
        );
        assert!(
            plan.recommended_tier != Some(Tier::Vram) || plan.status != PlanStatus::Executable,
            "VRAM must never be the certified placement"
        );
        assert!(plan
            .rationale
            .iter()
            .any(|l| l.contains("backend does not support GPU offload")));
    }

    #[test]
    fn unknown_gpu_capability_stays_advisory_never_certified() {
        // Case B: GPU capability Unknown. VRAM may remain advisory/unknown — a
        // fit is never certified.
        let r = residency(Some(2 * GIB), None, Some(2 * GIB), Some(MODEL_BYTES));
        let sys = snap(32 * 1024 * 1024, 16 * 1024 * 1024, 1 << 30);
        let backend = BackendTierCapabilities {
            can_run_from_vram: false,
            can_run_from_ram: true,
            can_run_from_page_cache: false,
        };
        let notes = [cap(
            Tier::Vram,
            Some("VRAM status unknown: backend capability is unknown."),
            None,
        )];
        let plan = MemoryPlan::propose(&cap_input(
            Some(MODEL_BYTES),
            Some(MODEL_EST),
            &r,
            &sys,
            Some(12 * GIB),
            &backend,
            &notes,
        ));

        let vram = &plan.budget.by_tier[&Tier::Vram];
        assert_eq!(vram.status, PlanTierStatus::Unknown);
        assert_eq!(
            vram.unknown_reason,
            "VRAM status unknown: backend capability is unknown."
        );
        assert!(
            plan.status != PlanStatus::Executable || plan.recommended_tier != Some(Tier::Vram),
            "an Unknown capability must never become an executable VRAM placement"
        );
    }

    #[test]
    fn supported_gpu_capability_with_observation_certifies_vram() {
        // Case C: capability Supported + real measured device availability +
        // sufficient headroom -> VRAM is a valid plan candidate.
        let r = residency(Some(2 * GIB), None, Some(2 * GIB), Some(MODEL_BYTES));
        let sys = snap(32 * 1024 * 1024, 16 * 1024 * 1024, 1 << 30);
        let backend = BackendTierCapabilities {
            can_run_from_vram: true,
            ..BackendTierCapabilities::default()
        };
        let notes = [cap(
            Tier::Vram,
            None,
            Some("VRAM candidate: backend supports GPU offload."),
        )];
        let plan = MemoryPlan::propose(&cap_input(
            Some(MODEL_BYTES),
            Some(MODEL_EST),
            &r,
            &sys,
            Some(12 * GIB),
            &backend,
            &notes,
        ));

        let vram = &plan.budget.by_tier[&Tier::Vram];
        assert_eq!(vram.status, PlanTierStatus::Fits);
        assert!(
            vram.note
                .contains("VRAM candidate: backend supports GPU offload."),
            "{}",
            vram.note
        );
        assert_eq!(plan.recommended_tier, Some(Tier::Vram));
        assert_eq!(plan.status, PlanStatus::Executable);
    }

    #[test]
    fn supported_gpu_without_device_observation_is_not_certified() {
        // Case D: capability Supported, but device residency is unobserved. The
        // anti-fabrication rule wins: no certified fit.
        let r = residency(Some(2 * GIB), None, None, Some(MODEL_BYTES));
        let sys = snap(32 * 1024 * 1024, 16 * 1024 * 1024, 1 << 30);
        let backend = BackendTierCapabilities {
            can_run_from_vram: true,
            ..BackendTierCapabilities::default()
        };
        let plan = MemoryPlan::propose(&cap_input(
            Some(MODEL_BYTES),
            Some(MODEL_EST),
            &r,
            &sys,
            Some(12 * GIB),
            &backend,
            &[],
        ));

        let vram = &plan.budget.by_tier[&Tier::Vram];
        assert_eq!(vram.status, PlanTierStatus::Unknown);
        assert!(
            vram.unknown_reason.contains("utilization is unobserved"),
            "{}",
            vram.unknown_reason
        );
        assert!(!plan.is_executable() || plan.recommended_tier != Some(Tier::Vram));
    }

    #[test]
    fn unsupported_mmap_blocks_file_backed_execution_claims() {
        // The model fits RAM comfortably, but file-backed execution is not
        // available (mmap Unsupported): the page-cache tier must not claim it
        // can execute and must never be recommended.
        let r = residency(Some(2 * GIB), Some(20 * GIB), Some(GIB), Some(MODEL_BYTES));
        let sys = snap(32 * 1024 * 1024, 16 * 1024 * 1024, 1 << 30);
        let backend = BackendTierCapabilities {
            can_run_from_ram: true,
            can_run_from_page_cache: false,
            can_run_from_vram: false,
        };
        let notes = [cap(
            Tier::PageCache,
            Some("page cache unavailable: backend does not support mmap."),
            None,
        )];
        let plan = MemoryPlan::propose(&cap_input(
            Some(MODEL_BYTES),
            Some(MODEL_EST),
            &r,
            &sys,
            None,
            &backend,
            &notes,
        ));

        let pc = &plan.budget.by_tier[&Tier::PageCache];
        assert_eq!(pc.status, PlanTierStatus::Unknown);
        assert_eq!(
            pc.unknown_reason,
            "page cache unavailable: backend does not support mmap."
        );
        assert_eq!(pc.proposed, None);
        assert_ne!(plan.recommended_tier, Some(Tier::PageCache));
        assert!(!pc.note.contains("can execute"));
    }

    #[test]
    fn unsupported_capability_cannot_produce_an_executable_tier() {
        // A remote-sized backend claims nothing: VRAM and RAM are excluded, so
        // nothing is executable even though the numbers would fit.
        let r = residency(Some(2 * GIB), None, Some(2 * GIB), Some(MODEL_BYTES));
        let sys = snap(32 * 1024 * 1024, 24 * 1024 * 1024, 1 << 30);
        let backend = BackendTierCapabilities {
            can_run_from_vram: false,
            can_run_from_ram: false,
            can_run_from_page_cache: false,
        };
        let notes = [
            cap(
                Tier::Vram,
                Some("VRAM unavailable: backend does not support GPU offload."),
                None,
            ),
            cap(
                Tier::RamOwned,
                Some("RAM unavailable: backend does not support CPU execution."),
                None,
            ),
        ];
        let plan = MemoryPlan::propose(&cap_input(
            Some(MODEL_BYTES),
            Some(MODEL_EST),
            &r,
            &sys,
            Some(12 * GIB),
            &backend,
            &notes,
        ));

        assert!(plan.recommended_tier.is_none());
        assert!(!plan.is_executable());
        assert_eq!(plan.status, PlanStatus::AdvisoryOnly);
        for tier in [Tier::Vram, Tier::RamOwned] {
            assert_eq!(plan.budget.by_tier[&tier].status, PlanTierStatus::Unknown);
            assert_eq!(plan.budget.by_tier[&tier].proposed, None);
        }
    }

    #[test]
    fn nvme_is_artifact_storage_only_no_executable_tier() {
        let r = residency(None, None, None, Some(MODEL_BYTES));
        let sys = snap(32 * 1024 * 1024, 24 * 1024 * 1024, 1 << 30);
        let plan = MemoryPlan::propose(&input(
            Some(MODEL_BYTES),
            None,
            &r,
            &sys,
            None,
            &BackendTierCapabilities::default(),
        ));
        let nvme = &plan.budget.by_tier[&Tier::Nvme];
        assert_eq!(nvme.status, PlanTierStatus::Unknown);
        assert_eq!(
            nvme.unknown_reason,
            "NVMe is artifact storage only; no executable NVMe tier is claimed"
        );
    }

    #[test]
    fn gate_denial_still_wins_with_capability_notes() {
        // A supported capability + a fitting plan: the gate's denial still
        // wins. Capability information never bypasses admission.
        let r = residency(Some(2 * GIB), None, Some(GIB), Some(MODEL_BYTES));
        let sys = snap(32 * 1024 * 1024, 16 * 1024 * 1024, 1 << 30);
        let backend = BackendTierCapabilities {
            can_run_from_vram: true,
            ..BackendTierCapabilities::default()
        };
        let notes = [cap(
            Tier::Vram,
            None,
            Some("VRAM candidate: backend supports GPU offload."),
        )];
        let plan = MemoryPlan::propose(&cap_input(
            Some(MODEL_BYTES),
            Some(MODEL_EST),
            &r,
            &sys,
            Some(12 * GIB),
            &backend,
            &notes,
        ));
        assert_eq!(plan.status, PlanStatus::Executable);

        let gate = ResourceGate::new(
            ResourceLimits::default(),
            snap(32 * 1024 * 1024, 512 * 1024, 1 << 30),
        );
        let plan = plan.evaluate_against_gate(&gate);
        assert_eq!(plan.status, PlanStatus::Denied);
        assert!(!plan.is_executable());
    }

    #[test]
    fn capability_filtering_does_not_consume_an_override() {
        let r = residency(Some(2 * GIB), None, Some(GIB), Some(MODEL_BYTES));
        let sys = snap(32 * 1024 * 1024, 16 * 1024 * 1024, 1 << 30);
        let backend = BackendTierCapabilities {
            can_run_from_vram: true,
            ..BackendTierCapabilities::default()
        };
        let notes = [
            cap(
                Tier::Vram,
                None,
                Some("VRAM candidate: backend supports GPU offload."),
            ),
            cap(
                Tier::RamOwned,
                None,
                Some("RAM candidate: backend supports CPU execution."),
            ),
        ];
        let plan = MemoryPlan::propose(&cap_input(
            Some(MODEL_BYTES),
            Some(MODEL_EST),
            &r,
            &sys,
            Some(12 * GIB),
            &backend,
            &notes,
        ));

        let gate = ResourceGate::new(
            ResourceLimits::default(),
            snap(32 * 1024 * 1024, 512 * 1024, 1 << 30),
        );
        gate.grant_override("operator will free memory", "tui");
        let plan = plan.evaluate_against_gate(&gate);

        assert_eq!(plan.status, PlanStatus::Denied);
        assert!(
            gate.override_pending(),
            "capability filtering must not consume the one-shot override"
        );
    }

    #[test]
    fn capability_aware_planning_is_deterministic() {
        let r = residency(Some(2 * GIB), None, Some(2 * GIB), Some(MODEL_BYTES));
        let sys = snap(32 * 1024 * 1024, 16 * 1024 * 1024, 1 << 30);
        let backend = BackendTierCapabilities {
            can_run_from_vram: false,
            can_run_from_ram: true,
            can_run_from_page_cache: false,
        };
        let notes = [cap(
            Tier::Vram,
            Some("VRAM unavailable: backend does not support GPU offload."),
            None,
        )];
        let a = MemoryPlan::propose(&cap_input(
            Some(MODEL_BYTES),
            Some(MODEL_EST),
            &r,
            &sys,
            Some(12 * GIB),
            &backend,
            &notes,
        ));
        let b = MemoryPlan::propose(&cap_input(
            Some(MODEL_BYTES),
            Some(MODEL_EST),
            &r,
            &sys,
            Some(12 * GIB),
            &backend,
            &notes,
        ));
        assert_eq!(a, b, "identical capability inputs produce identical plans");
        assert_eq!(
            a.budget.by_tier[&Tier::Vram].unknown_reason,
            b.budget.by_tier[&Tier::Vram].unknown_reason
        );
    }

    #[test]
    fn capability_notes_do_not_fabricate_residency() {
        // Capability notes change nothing about observation: an unobserved
        // device stays None; a plan never invents residency.
        let r = residency(Some(2 * GIB), None, None, Some(MODEL_BYTES));
        let sys = snap(32 * 1024 * 1024, 16 * 1024 * 1024, 1 << 30);
        let backend = BackendTierCapabilities {
            can_run_from_vram: true,
            can_run_from_ram: true,
            can_run_from_page_cache: false,
        };
        let notes = [cap(
            Tier::Vram,
            None,
            Some("VRAM candidate: backend supports GPU offload."),
        )];
        let plan = MemoryPlan::propose(&cap_input(
            Some(MODEL_BYTES),
            Some(MODEL_EST),
            &r,
            &sys,
            Some(12 * GIB),
            &backend,
            &notes,
        ));
        assert_eq!(plan.budget.by_tier[&Tier::Vram].currently_observed, None);
        assert_eq!(
            plan.budget.by_tier[&Tier::Vram].status,
            PlanTierStatus::Unknown,
            "no fabricated VRAM residency"
        );
    }
}
