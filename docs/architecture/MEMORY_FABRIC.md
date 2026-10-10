# Memory Fabric (M1–M13)

Status: **Milestones 1 through 12 landed.** Memory 13 is deliberately NOT
implemented: it is an execution-semantics boundary requiring a separate
approved plan (documented in "M13 — the execution boundary" below).

Headline distinction, stated once and enforced everywhere:

> **Anatomy/metadata describing a model file is NOT OpenRT executing that model
> through tiered memory.**

Everything landed so far is *descriptive and additive*. OpenRT remains an
orchestrator: actual weight loading and execution happen in provider backends
(`remote openai` / arbitrary planning-approved execution). OpenRT never
allocates weights in-process, never writes its own allocator, never memory-maps
or tiers layers itself, and never gestures at "offload" inside an HTTP client.
Tiered execution that OpenRT itself performs is future work (M13+) and would
require a separate, plan-approved and execution-semantics-changing effort.

A second distinction is enforced by the M3 layer:

> **MemoryPlan proposes; ResourceGate authorizes.**

`MemoryPlan` is a pure planner. It never allocates, loads, mmaps or demotes, and
never asks a provider to change how it executes. `ResourceGate` (`limits.rs`)
remains the **only** admission authority. A plan can be *executable* (certifiably
fits, gate admits) and still be nothing but a proposal — nothing in M3 hands it
to an executor.

## M1 — Residency + model-I/O telemetry (observation only)

Source of truth: `crates/openrt-resources/src/residency.rs` and
`crates/openrt-resources/src/observations.rs`.

- `ResidencySnapshot` — canonical, always-safe process residency view:
  - `owned_ram_bytes` — anonymous pages the process owns (`RssAnon`).
  - `file_backed_bytes` — file-backed (page-cache) pages (`RssFile`).
  - `device_bytes` — VRAM. **Always `None` in M1–M5**: no VRAM observer is
    wired, and OpenRT refuses to guess. The planner therefore cannot certify a
    VRAM fit unless a caller supplies a measured value via
    `ResidencySnapshot::with_device_bytes`.
  - `nvme_bytes` — declared model-artifact bytes, a registry proxy
    (`artifact_bytes_declared`), not a device class measurement.
  - `source` — which real observer (or none) produced the values.
  - `notes` — explicit "field X is Unknown" when a partial view was observed.
  - `timestamp`, `schema_version`.
- Linux discovery reads `/proc/self/status` (`RssAnon`/`RssFile`) with a
  `/proc/self/smaps_rollup` (`Anonymous`/`File`) fallback. `VmRSS` is
  deliberately ignored: it counts reclaimable file-backed pages, which would
  mislabel file pages as owned memory. Non-Linux yields `unknown` cleanly.
- Failure contract: an unreadable/missing/renamed `/proc` field is `None`
  (Unknown), **never a panic, never an error, never a fabricated zero**.
- `ModelLoadTelemetry` (`observations.rs`) mirrors Colibrì's cold/warm/dedupe
  counters as *observation fields*: all start `None` and are only ever filled
  by the component that actually observed them (today: nothing fills them,
  because OpenRT does not load weights; the registry fills nothing but byte
  counts). `is_unobserved()` → no gauges emitted.
- Integration: `Runtime::record_residency_metrics()` (`phase5.rs`) samples,
  records gauges (`residency.*`) on the existing `MetricsRegistry`, and emits
  them on the existing Phase-1 bus. It is additive read-only telemetry; it
  allocates nothing weight-like, demotes/evicts nothing, and never touches
  admission.

## M2 — Model anatomy (optional, bounded, backward compatible)

Source of truth: `crates/openrt-models/src/anatomy.rs`, persisted by the
registry in `registry.anatomy` (`AnatomyRecord`, keyed by artifact id).

- `ModelAnatomy` — `format`, `total_bytes`, `shards`, `tensors`. Each
  `TensorAnatomy` carries `name`, `shard`, `offset` (absolute payload offset),
  `length`, `alignment`/`page_aligned`, and a best-effort `dtype` label.
- GGUF: magic + version, then kv-metadata skipped via the existing bounded
  `HeaderReader` (`GGUF_MAX_HEADER_BYTES` ceiling, 1 MiB string cap), then the
  tensor table. **Tensor payload bytes are never read** — they are jumped with
  seeks.
- Safetensors: 8-byte little-endian header length, capped at 16 MiB (matching
  the scanner's `SafetensorsPolicy::default().max_header_bytes`), then JSON with
  per-tensor `data_offsets` bounds-checked against the exact data block size.
- Length derivation rule: `length` is recorded **only** when it can be
  guaranteed (F32/F16 — the two dtypes with stable byte sizes across every
  GGUF ggml-type enum). Quantized/unknown dtypes keep `length = None`. No
  quantization block-size tables exist in this milestone; a wrong number is
  worse than an honest `None`. When a prefix length is unknown, later payload
  offsets become `None` too (they mathematically cannot be derived from a
  contiguous tail and a file size alone).
- Bounds: `ANATOMY_MAX_TENSORS = 1<<20`, dims per tensor ≤ 64, header sizes
  capped as above. A malformed file errors the *parsing*; it can never
  allocate unboundedly or panic.
- Persistence: `Registry::store_anatomy` / `Registry::anatomy`, same store, new
  namespace — **not** a parallel registry, **no** silent rewrite of manifests
  (manifests stay immutable/write-once). Where it naturally belongs: captured
  in `register_local` after the streamed SHA-256 pass, best-effort, using the
  already-computed digest. Registration outcome is byte-for-byte unchanged when
  anatomy is absent, unparseable, or the file is not a model.
- Reading profile: `register_local` still performs exactly one full-file read
  (the streaming hash); anatomy adds only bounded header/prefix reads + seeks.
  Memory is the hash buffer + a ≤64 MiB header window, never the artifact.

## M3 — Tier vocabulary + MemoryPlan proposal layer (pure planning)

Source of truth: `crates/openrt-resources/src/plan.rs`.

The planner answers: *given what we observe and what the deployment declares,
which tier could this artifact be resident in, how much of each tier would it
request, and should the gate be asked to admit anything?* It is pure and
deterministic: identical inputs produce identical plans, including the
observation-anchored timestamp (the plan records the residency observation it
was built from, not a wall-clock creation time).

### The tier vocabulary

- `Tier` — `Vram`, `RamOwned`, `PageCache`, `Nvme`.
  - `Vram` — GPU device memory.
  - `RamOwned` — process-owned anonymous RAM (`RssAnon`).
  - `PageCache` — kernel page cache behind file-backed mapping pages
    (`RssFile`); reclaimable, so it is **never** a certifiable recommendation.
  - `Nvme` — the artifact's backing origin, **not** a residency target. A byte
    on NVMe is file size, and file size is not resident memory.
- `TierBudget` — per tier: `capacity` (limit), `reserved` (already committed),
  `currently_observed` (from the residency snapshot), `headroom`
  (`capacity − reserved`), `proposed` (what this plan asks of the tier),
  `status` (`Fits` / `Tight` / `DoesNotFit` / `Unknown`), `unknown_reason`, and
  a free-text `note`.
- `MemoryBudget` — `requested_bytes`, its `basis`, and the deterministic
  `BTreeMap<Tier, TierBudget>`.
- `BackendTierCapabilities` — the tiers a backend can execute from. A
  **declared input**. M3/M5 do no runtime probing: capability *negotiation*
  (M4) and *capability-aware planning* (M5) are declaration/contract only, and
  a real device observer remains M8. The default is the conservative CPU-only
  posture (RAM yes, VRAM no, page cache no); the M5 adapter produces the view
  from a provider's declared `BackendCapabilities` — and a claims-nothing
  remote provider may therefore have **no** executable tier.

### Sizing bases — the three-GiB distinction

The same "9 GiB" means three different claims, and the planner never conflates
them:

- **"The artifact is 9 GiB on NVMe"** (`ResidencySnapshot::nvme_bytes`) — the
  `Nvme` tier records it as observation only. It is **file size**, and the plan
  never sizes a residency proposal from it.
- **"9 GiB of VRAM is available"** — declared capacity minus observed resident
  device bytes. A VRAM fit is certified **only** when both the capacity and an
  observed utilization exist (`device_bytes` is `None` in M1–M5, so a caller
  must supply it). With usage unobserved, even a 12 GiB declared capacity
  leaves the fit `Unknown` — never a silent fit.
- **"9 GiB of process-owned RAM is available"** — host headroom, evaluated
  **through the gate's own policy arithmetic**: the plan calls
  `ResourcePreflight::evaluate` on a host whose available memory has been
  reduced by the proposal (`ResourceStatus::Ok → Fits, Tight → Tight,
  Blocked → DoesNotFit, Unknown → Unknown`). Same policy, same floor, same
  statuses as `ResourceGate`.

### Sizing basis and plan status

- `ResidentBasis::ResidentEstimate` — sized from a resident-footprint estimate
  (e.g. a model resource profile). The **only** basis that can yield
  `PlanStatus::Executable`.
- `ResidentBasis::MeasuredFileSize` — sized from the measured artifact file
  size. A measured number, but an **upper bound**, not a resident footprint;
  such a plan is `AdvisoryOnly` and says so in its rationale.
- `ResidentBasis::Missing` — neither a file size nor an estimate → `Unavailable`.
- `PlanStatus` — `Executable` (certifiably fits on a recommended tier with a
  resident estimate and a non-denying gate), `AdvisoryOnly` (informative only),
  `Unavailable` (nothing certifiable), `Denied` (the gate's current posture
  denies; a denied plan stays denied).

### Gate integration — propose, then probe, never override

- `MemoryPlan::propose(&PlanInput)` is pure: it reads no environment, touches
  no gate state, performs no I/O, and never invokes a backend.
- `MemoryPlan::evaluate_against_gate(&gate)` folds the gate's **current**
  posture into the plan via `ResourceGate::preflight()` (pure arithmetic over
  the cached snapshot). It never calls `admit`/`grant_override` — probing a
  plan must not spend an operator's one-shot override. A plan the gate denies
  is demoted to `Denied`; the recommendation survives so the operator sees what
  was denied, but the denial always wins.
- `MemoryPlan::gate_proposal()` exposes the two numbers the gate reasons about
  (`proposed_owned_ram_bytes`, `proposed_vram_bytes`) — produced *through* the
  gate's arithmetic, never around it. Nothing in M3 acts on them.

### Example — 12 GiB GPU, 9 GiB model

Declared VRAM capacity 12 GiB; the VRAM observer reports 2 GiB resident. A model
whose artifact is 9 GiB:

- Vram tier: headroom = 12 − 2 = **10 GiB** → **Fits** → recommended tier is
  **VRAM**.
- Because the sizing came from the 9 GiB *file size*, the plan is
  **AdvisoryOnly** and states: *file size ≠ resident memory — 9 GiB is an upper
  bound*.
- With a resident-footprint estimate instead (e.g. 8 GiB from a model resource
  profile), the same placement is **Executable** — still a proposal; the gate
  authorizes.
- Host RAM (for comparison): 32 GiB total, 16 GiB available, default 15% floor
  = 4.8 GiB. Placing 9 GiB of owned RAM leaves 7 GiB ≥ floor → RAM also fits,
  but VRAM is preferred (least contention with the host).
- Without a VRAM observer (`device_bytes = None`), the fit stays **Unknown** —
  a 12 GiB *declared* capacity is not a *measured* 10 GiB of available VRAM.

### Explicitly unimplemented (do not assume they exist)

- No weight allocator, no in-process model load, no execution via tiered
  memory, no `mmap`, no layer/expert streaming, no prefetch, no KV-spill, no
  demotion/eviction/override of `openrt-resources::ResourceGate` (the sole
  admission authority) — admission, retry, and exit semantics unchanged. The
  planner hands a proposal to no executor.
- No runtime capability probe: `BackendTierCapabilities` is declared input only.
- M4 is the capability **contract**, not the execution implementation: actual
  `mmap`/prefetch/offload/device placement stays future work (M11+) and a
  backend never receives an authoritative command from OpenRT in M4.
- No VRAM observer (the M1 default `device_bytes = None`); no device
  measurement is imputed from NVMe bytes or host RAM.
- No quantization block sizing; no shard-grouping across multiple files; no
  runtime steers a load policy.
- No Colibri/Edge0 code is linked, imported, or vendored.

## M4 — Backend capability negotiation + advisory memory hints

Source of truth: `crates/openrt-models/src/memory.rs` (contract types),
`crates/openrt-models/src/provider.rs` (the two `ModelProvider` extension
points).

M4 adds the smallest clean interface letting OpenRT know what a backend
**claims** for memory-aware execution — without taking execution authority
from the backend. The boundary is drawn once and enforced everywhere:

> **OpenRT can negotiate capabilities with a backend, but it cannot observe or
> control execution details that the backend does not expose.**

OpenRT **proposes, observes, and sends advisory hints**. The backend **owns**
actual inference, model loading, `mmap`/offload/device placement, the KV
cache, and execution semantics. OpenRT never `mmap`s or loads weights, never
allocates backend memory, never manipulates provider tensors, never evicts
provider memory, never executes inference — and never fabricates remote model
anatomy, nor assumes an HTTP/OpenAI-compatible server supports `mmap`/offload.

### The capability contract

- `BackendCapabilities` — seven tri-state execution capabilities with
  `Supported`, `Unknown`, `Unsupported` semantics:
  - `supports_mmap`, `supports_file_backed_models`, `supports_cpu_offload`,
    `supports_gpu_offload`, `supports_residency_hints`,
    `supports_prefetch_hints`, `supports_explicit_memory_budget`.
  - Each capability is a `CapabilityState` (`Supported` / `Unsupported` /
    `Unknown`). `Supported` is claimed **only** when the provider contract
    genuinely provides it; `Unsupported` is the default; `Unknown` is used
    only when a provider explicitly cannot answer. Consumers treat `Unknown`
    exactly like `Unsupported` (never rely on it).
- `AnatomyAccess` — `LocalArtifact` vs `RemoteProvider`, the distinction M4
  will not blur:
  - **REMOTE_PROVIDER_MODEL** — OpenRT knows only provider-declared
    capabilities. **No fabricated tensor layout, no inferred residency.** A
    remote OpenAI-compatible provider does not expose tensor offsets, local
    filesystem layout, `mmap` state, or residency; OpenRT must not invent or
    infer them.
  - **LOCAL_ARTIFACT_ANATOMY** — OpenRT genuinely possesses the artifact
    (registry/host) and its anatomy is real and observed; the plan may reason
    from it. This is never declared for a remote provider.
- Conservative default: `BackendCapabilities::default()` (and the explicit
  `remote_http()` alias) is a remote provider claiming nothing. Partial serde
  configuration (e.g. `{"access":"remote_provider"}`) must not unlock
  anything — omitted capabilities stay `Unsupported`.
- The two remote providers (`LlamacppProvider`, `VllmProvider`) return
  `BackendCapabilities::remote_http()` by explicit declaration.

### The advisory hint interface

- `MemoryHint` — `model`, optional `target` tier/region, optional byte
  `range` or logical `region`, `priority`, `reason`, and an always-`true`
  `advisory` flag. Future examples: "these model ranges may be needed soon",
  "keep this region resident if possible", "backend may benefit from
  read-ahead". `advisory` is structural: there is no non-advisory hint.
- `ModelProvider::submit_memory_hint` — returns a `HintReceipt`
  (`Accepted` / `NoOp` / `Unsupported`) and **never an inference-breaking
  error for lack of support**: an unsupported backend drops the hint cleanly.
  Sending a hint allocates nothing in OpenRT, does not consume a
  `ResourceGate` override, does not change model routing or outputs, and is
  safely ignored.
- The egress guard forwards hints under the **same ONE egress authority** as
  `generate` when they become networked (Sovereign refuses a non-loopback
  hint before any socket opens), keeping `MemoryPlan proposes / ResourceGate
  authorizes / Runtime executes / Backend performs inference` intact.

### Local vs remote example

- **LOCAL MODEL:** OpenRT knows artifact anatomy (M2) → the planner (M3) can
  reason about it and size a real proposal.
- **REMOTE MODEL:** OpenRT knows only provider-declared capabilities (M4) →
  no fabricated tensor layout or residency. The plan can still use known
  artifact metadata when OpenRT genuinely possesses it, but the provider
  decides how to execute.

### Rejected approach (recorded for the audit trail)

The previously proposed "provider-side anatomy integration for remote
OpenAI-style backends, as a generic mechanism" is **not** built. A remote
provider generally does not expose its tensor offsets, local filesystem
layout, `mmap` state, or residency to OpenRT; OpenRT must not invent or infer
remote model anatomy. M4 replaces that with the conservative capability
contract above.

## M5 — Capability-aware planning

M4 declared *execution* capabilities; the M3 planner ran on a separate,
hand-fed tier input. M5 wires the two together with **one canonical adapter**
so that plans are grounded in what the provider actually claims to be able
to do.

```text
 BackendCapabilities                    BackendTierCapabilities + CapabilityNotes
 (execution claims, M4)     can_run      (executable tier view, M3)
   supports_gpu_offload  ──────────▶  can_run_from_vram
   supports_cpu_offload  ──────────▶  can_run_from_ram
   supports_mmap ∧
   supports_file_backed   ──────────▶  can_run_from_page_cache
        ↑                                        ↑
 openrt-models                         adapted in openrt-runtime only
                                                        ↓
                                    PlanInput { backend, capability }
                                                        ↓
                                         MemoryPlan (M3) — proposes
                                         ResourceGate — authorizes
```

`PlannerCapabilityView { tiers, notes }` lives in `openrt-runtime` (the only
crate that may see both contract and planner). It emits exactly three
`CapabilityNote`s — `Vram`, `RamOwned`, `PageCache` — that the planner renders
verbatim as tier rationale. `Nvme` participates via its built-in text only.

### The five things that must not be conflated

1. **Capability** — what the backend declares it *can* do (M4).
2. **Capacity** — how much of a tier exists (declared or observed, M3).
3. **Availability** — the headroom right now, after reservations (M3).
4. **Residency** — what is *actually* resident in a tier (M1 observation).
5. **Artifact size** — the file on NVMe; file size ≠ residency (M1/M3).

The planner already refused to read a fit from #5; M5 additionally refuses to
assume #1, so a plan never proposes an execution strategy the backend
declared impossible.

### Mapping rules (canonical, tested)

- `Supported → can_run = true` (candidate).
- `Unsupported` **or** `Unknown → can_run = false`; the note distinguishes the
  rationale ("unavailable" vs "status unknown"). **`Unknown` never becomes
  `Supported`**, tri-state is lossless.
- `PageCache` is conservative AND: both `mmap` and file-backed must be
  `Supported`.
- `BackendCapabilities::default()` / `remote_http()` → **no executable tier**:
  a claims-nothing remote provider is planned conservatively, never as CPU
  executable, never as VRAM executable, with nothing fabricated (no anatomy,
  no residency, no assumed filesystem).
- Mapping is pure and deterministic; identical capabilities ⇒ identical view.

### Example — 12 GiB GPU, 9 GiB artifact, 8 GiB resident estimate

| Case | Capability surface | Planner result |
|---|---|---|
| A | GPU offload `Unsupported` | VRAM excluded, no executable VRAM tier |
| B | GPU offload `Unknown` | VRAM `Unknown`/advisory, never certified |
| C | GPU offload `Supported` + observed device residency + headroom | VRAM `Fits`, candidate, recommended |
| D | GPU offload `Supported`, **no** device observation | not certified (`would_fit` stays unrealized) |

A *local* CPU/mmap-declared backend with no GPU claims still yields an
executable RAM/page-cache proposal; a remote provider yields **none** until it
declares otherwise.

### Authority is untouched

The adapter changes nothing about who authorizes. `MemoryPlan` proposes,
`ResourceGate` authorizes, a denied plan stays denied, and capability
filtering never consumes the one-shot override.

### Authority is untouched

M4/M5 keep the ResourceGate as the single authority. `propose_memory_plan`
only consults it — `preflight`/`current`/`limits` are read, `override_pending`
is never armed, and a denied plan stays denied. Planning is allocation-free:
no model load, no inference, no payload reads.

## M6 — Runtime integration (read-only planning surface)

M6 closes the M1–M5 loop from the runtime instead of from test-only planner or
adapter wiring. `openrt-runtime::Runtime` gains exactly one entry point
(`crates/openrt-runtime/src/runtime_memory.rs`):

```rust
pub async fn propose_memory_plan(
    &self,
    artifact_id: &str,
    vram_capacity_bytes: Option<u64>,
    residency: Option<ResidencySnapshot>,
) -> Result<RuntimeMemoryPlan, crate::Error>;
```

- **Registry metadata** (card + manifest) is read through `self.model_registry`
  and NSM-encoded; the payload file is never opened, so planning is
  allocation-free and side-effect free (asserted by test).
- **Provider capabilities** come from `self.provider.backend_capabilities()`
  (flowed through the `EgressGuardedProvider` wrapper unchanged) via the M5
  `PlannerCapabilityView::from_backend` bridge.
- **Gate posture** is sampled from `self.resource_gate` — consult only.
- An explicit `vram_capacity_bytes` parameter (not a magic ability) supplies
  the user/operator VRAM budget so a capacity claim can only originate from an
  in-scope explicit caller — the runtime never fabricates hardware.
- `residency` is the same contract M1 telemetry produces. When the caller omits
  it the runtime builds an unobserved snapshot anchored to the manifest's
  `created_at` (no wall-clock in the plan — determinism preserved); a
  caller-supplied snapshot is honored verbatim. A measured snapshot from a
  trusted device observer is what makes a VRAM fit *certifiable* (scenario C).
- `RuntimeMemoryPlan { plan, backend, anatomy_available, artifact_bytes }` is
  the serializable, deterministic result consumed by diagnostics (JSON).

| scenario | registry | provider | residency | result |
|---|---|---|---|---|
| A | installed | GPU unsupported | — | VRAM `Unknown`; never an executable VRAM plan |
| B | installed | GPU supported | none | VRAM `Unknown` ("unobserved"); not certified |
| C | installed | GPU supported | measured headroom | VRAM `Fits`; recommended VRAM; executable (resident-estimate basis) |
| D | installed | GPU supported | — | `ResourceGate` denial stays denied; override never consumed |
| E | installed | remote (OpenAI) | — | remote claims nothing; nothing observed |

CLI: `openrt model compatibility <id> [--vram-mb] [--ram-mb] [--json]` now
reports the runtime plan (capabilities, per-tier status, recommended tier,
`ADVISORY_ONLY`/`EXECUTABLE` and rationale), JSON is deterministic.

## M7 — Multi-shard model anatomy (metadata only)

M7 extends the M2 anatomy so a **multi-file** model (`*.safetensors.index.json`
and per-shard safetensors files) is described the same way a single-file model
is — as storage layout with tensor locations — without touching residency,
execution, or the resource authority. Scope is metadata only: no mmap, no
streaming, no offload/prefetch/eviction, no KV spill, no multi-SSD execution,
no quantization, no memory movement, no backend execution change.

### Schema (additive, version-stable)

- `ModelAnatomy.shard_files: Vec<ShardAnatomy>` with `#[serde(default)]`; each
  `ShardAnatomy { shard, name, file, size_bytes, checksum }` names one physical
  file. `checksum` stays `None` (never fabricated — shards are not re-hashed).
- `ModelAnatomy.shards` (v1 field) remains the count fallback for pre-M7
  records whose `shard_files` deserializes empty; **both** are supported, so
  old records load without a migration and `ANATOMY_SCHEMA_VERSION` is
  unchanged (=1). Helpers: `shard_count()`, `is_multi_shard()`, `shard_bytes()`.
- `TensorAnatomy.shard` is the index into `shard_files`; `offset`/`length` are
  **within that shard file**. For a v1 record `shard` is always `0` and names
  the sole file — unchanged semantics.
- `total_bytes` for a sharded model is the sum of the shard sizes (storage,
  never residency).

### Discovery (bounded, index-led)

- `register_local` stays single-file by-reference: the registered artifact is
  the shard **index**. `capture_anatomy` then: (1) tries the single-file parser
  and stores it when the format is recognized; (2) else sniffs for a
  safetensors index — a JSON object carrying `weight_map` — via
  `is_safetensors_index_file` (content-led, bounded to
  `ANATOMY_MAX_INDEX_BYTES` = 16 MiB); (3) builds multi-shard anatomy with
  `sharded_anatomy_for_index_file`; (4) falls back to the existing Unknown/error
  parity so registration outcome is byte-for-byte what pre-M7 produced.
- Reads are bounded and confined: index ≤ 16 MiB, per-shard header ≤ 16 MiB,
  no payload byte is ever read. Shard references are untrusted: absolute
  paths, `..`/`.`/empty components, backslashes, and NULs are rejected, and the
  resolved path must stay inside the canonicalized index directory (symlink
  escapes fail closed). Caps: `ANATOMY_MAX_SHARDS` = 4096, tensors still
  `ANATOMY_MAX_TENSORS`.
- Consistency is strict and fail-closed: the `weight_map` tensor set must
  **exactly** equal the union of every shard's declared tensors — a missing
  shard, duplicate/extra/mis-mapped tensor, truncated shard, malformed index,
  or size/count overflow yields an error (the registry then persists the
  Unknown fallback rather than a wrong layout). Ordering is deterministic:
  shard order from the sorted distinct shard names, tensors by `(shard,
  offset, name)`.
- GGUF is deliberately **not** multi-file: a GGUF artifact is always described
  by `anatomy_for_file`, and a registered file is that single file. A lone
  member of a sharded safetensors model registered by itself is treated as an
  ordinary single-file artifact (M2 behavior, regression-tested).

### Runtime consumption

- `propose_memory_plan` sizes `artifact_bytes` from anatomy for multi-shard
  models (sum of shard sizes — the manifest/card knows only the index member)
  and keeps the M6 card-based size for single-file models. Neither is ever
  treated as residency: the certified resident working set still comes from
  complete tensor-payload lengths, and `ResourceGate` postures are untouched.
  Storage layout ≠ residency ≠ execution.
- CLI `model compatibility` prints the shard layout (per-file sizes, storage
  total) when a multi-shard anatomy is recorded; JSON output unchanged.

## Adopted design decisions

- `RssAnon` separates owned from file pages better than `VmRSS`, so it is the
  source of `owned_ram_bytes`; `device_bytes` stays `None` rather than being
  approximated from `nvme_bytes`.
- Anatomy parses from the file *prefix* the registry is already hashing, not
  from a second read of the artifact (exact-file-size losslessness is
  preserved via the declared digest/size gate).
- Telemetry uses "Unknown over fabrication" everywhere: `Option` fields,
  `is_unobserved`, and gauge emission limited to observed values so a zero is
  never mistaken for a measurement.
- The planner sizes from a resident-footprint estimate when one exists, and
  from the measured file size otherwise — never from NVMe presence — and only a
  resident-estimate exists can certify an *executable* placement.
- The plan is deterministic by construction: sortable `Tier`, `BTreeMap` tier
  ordering, no wall-clock in the plan (its timestamp is the residency
  observation), and pure functions.
- No second provider registry: the capability/hint contract is two default
  methods on the existing `ModelProvider` trait, the same extension pattern as
  `probe_tool_support`/`probe_vision`. Hints never duplicate the
  `ToolRegistry`, `ResourceGate`, `Runtime`, or model-registry
  responsibilities.
- Hints are advisory and never touch the resource authority: no override is
  consumed, nothing is granted, and a denied plan stays denied (regression
  tests live in `openrt-runtime` at the provider + authority composition
  layer).
- The provider capability contract is conservative: remote OpenAI-compatible
  providers claim nothing local, and there is no `From<config>`/serde-default
  path that silently unlocks a capability.
- `BackendCapabilities` (execution claims, `openrt-models`) and
  `BackendTierCapabilities` (planner tier input, `openrt-resources`) stay
  distinct crates because each depends only on `openrt-core`; the **single**
  bridge between them is `openrt-runtime::PlannerCapabilityView::from_backend`
  (M5). M4 ships the contract, M5 ships the wiring, and no other code path
  translates capability tri-states into planner booleans.

## Milestones 8–13 (landed, staged)

### M8 — Quantized-length derivation (opt-in, table-derived, never certified by the table alone)

- New `TensorLengthBasis` on `TensorAnatomy` adds `Header` (M2, from the
  safetensors header) and `Quantized` (derived from a versioned ggml block-size
  table). `AnatomyOptions::derive_quantized_lengths: bool`, opt-in.
- `GGML_QUANT_TABLE_VERSION`/the block-size table in `openrt-models/src/anatomy.rs`
  derives a tensor's *length* from its declared block size and count. The table is
  versioned and never a guess — an unknown ggml type yields **no** length.
- The planner consumes a table-derived length exactly like a header-derived one
  for sizing, but **certification** of "resident-estimate basis" still requires
  the measured file size / actual observed bytes; the table alone never
  certifies execution.
- Tests: derivation, unknown-type refusal, opt-in default off, planner plumbing.
- CLI: `openrt model compatibility` prints the length basis per tensor.

### M9 — Device-residency observer (real `device_bytes` when a trusted GPU API is on-path)

- `openrt-resources::gpu` adds `GpuObservation`/`GpuObservationSource`
  (`None` | `NvidiaSmi`), `GpuDevice { name, total_mib, used_mib }`, the
  `GpuObserver` trait, `NoGpuObserver` (default), `MockGpuObserver` (test
  fixture), and `NvidiaSmiObserver` — a read-only `nvidia-smi
  --query-gpu=name,memory.total,memory.used --format=csv,noheader,nounits`
  subprocess, parse-only.
- Empty/unsupported observations aggregate to `None`, **never** `Some(0)`:
  an unmeasured card reports Unknown, restoring planner honesty.
- `Runtime` carries `gpu_observer: Arc<dyn GpuObserver>` (default `NoGpuObserver`;
  builder `.gpu_observer(...)`). `propose_memory_plan` backfills `device_bytes`
  from the observer only when the caller supplied none; a caller-supplied
  residency always wins.
- Admission stays byte-identical: observed VRAM is reported via
  `ResourcePreflight::evaluate_with_gpu` as current/headroom/used%, but with
  **no VRAM admission floor configured** the tier stays `Unknown` — observation
  never gains authority. `Runtime::record_residency_metrics` attaches the
  measured device bytes + clear note.
- `openrt doctor` gains `agent.resource_vram` (Pass when measured, Warn when
  only `total_mib` is known, Unavailable without an observer); the CLI injects
  `NvidiaSmiObserver` for the doctor only.

### M10 — NVMe/artifact bytes as an explicit family (direct read when possible)

- `StorageObservationSource` (`RegistryDeclared` | `DirectStat`) and
  `StorageObservation` in `openrt-resources::storage`; `stat_size_bytes()`
  (`std::fs::metadata`) and `observe_local_artifact()` are read-only and
  additive.
- Planner step 1 now fetches the manifest; step 2 fills the NVMe tier as:
  1. caller-supplied residency (always wins), else
  2. a **direct stat** through the manifest `BlobLocation::External { path }`
     (multi-shard = every shard file beside the index, all-or-nothing), else
  3. the registry-declared `artifact_bytes` as an honestly-labelled proxy, else
  4. `None`.
- New `RuntimeMemoryPlan.nvme_bytes_measured: bool` distinguishes measured from
  proxy in diagnostics. Never treated as residency.

### M11 — Tier hit/miss instrumentation inside a provider adapter

- `ModelProvider::load_telemetry() -> ProviderLoadTelemetry` (new, default
  unobserved) — the adapter that owns execution declares cold/warm loads and
  tier hits/misses it genuinely observed; OpenRT never fabricates them.
- `MockProvider` is the first instrumented adapter: a generation arriving at an
  idle backend is a cold load + tier miss; one arriving while a session is
  resident is a warm load + tier hit. Counters are atomics; a cancelled
  generation releases the session on drop.
- `Runtime::record_model_load_metrics`/`emit_model_load_gauges` feed only
  observed counters into the reserved `model_io.*` gauges; the
  `EgressGuardedProvider` forwards `load_telemetry`/`last_raw_response`
  unchanged (an egress guard adds and hides no telemetry).

### M12 — Durable telemetry ledger (M1 fields as digest-chained rows)

- `Runtime::record_residency_metrics` additionally appends one
  `memory.residency` ledger row (session `system`, node `system.residency`):
  status `observed`/`unknown`, note = JSON of the **observed** M1 fields
  (`owned_ram_bytes`, `file_backed_bytes`, `device_bytes`, `storage_bytes`,
  `annotations`) — all-unknown samples write an empty note, never fabricated
  values.
- `Runtime::record_model_load_metrics` appends `model.load` rows
  (`system.model_io`) carrying the observed counters + `observed_by` adapter.
- Rows go through the existing `openrt_timeseries::append_event` so they are
  digest-chained and covered by `history_verify`/`history_replay`/forks.
  Run-path gauge emission (`emit_model_load_gauges`) stays ledger-free so task
  history timing is untouched.

## M13 — the execution boundary (documented, not implemented)

M13 is the milestone that must **no-op**: any execution-semantics change —
mmap/streaming/prefetch/KV-spill, actually handing a `MemoryPlan` to a backend,
or implementing the M4 contract as execution — requires a separate approved plan
and is deliberately out of this milestone.

What M13 admits and what it still refuses:

- Admitted in M8–M12: *descriptive* observers (GGML table → lengths, GPU
  `device_bytes`, direct NVMe `stat`, provider-observed tier hits/misses) and
  *durable* records of them. None allocate weights, none change routing or
  quantization, none move memory.
- Refused until a separate plan: the planner (or runtime) performing execution,
  memory-mapping artifact bytes into the process, offloading/prefetching/
  evicting across tiers, KV-cache spill control, or `submit_memory_hint` (M4)
  becoming a functional capability instead of the advisory receipt it is.
- The M4 `HintReceipt::Unsupported` default, the `evaluate_with_gpu` "never
  authorizes" rule, and the "caller residency always wins; fallbacks are
  labelled" rule from M9/M10 remain the standing guards.