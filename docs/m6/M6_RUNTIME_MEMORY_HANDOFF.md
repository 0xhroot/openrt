# M6 — Runtime Integration of Capability-Aware Memory Planning — HANDOFF

## Executive summary

M6 gives the **runtime** a single, read-only memory-planning entry point so
the M1–M5 planner stack (tier vocabulary, residency observation, anatomy,
capabilities, gate) is reachable from real runtime code instead of test-only
wiring. Exactly one integration point was added; nothing about execution
semantics changed.

```
registry (card+manifest, NSM-encoded) ─┐
provider backend_capabilities() ───────┼─▶ propose_memory_plan ─▶ RuntimeMemoryPlan
ResourceGate posture (read only) ──────┘        (openrt-runtime)    (deterministic JSON)
```

## Integration point (exactly one API)

`crates/openrt-runtime/src/runtime_memory.rs`:

```rust
pub async fn propose_memory_plan(
    &self,
    artifact_id: &str,            // registry card + manifest are read (never the payload)
    vram_capacity_bytes: Option<u64>, // explicit user/operator budget — never fabricated
    residency: Option<ResidencySnapshot>, // M1-format observation; None ⇒ unobserved+manifest-anchored
) -> Result<RuntimeMemoryPlan, crate::Error>
```

Supporting surface:
- `RuntimeMemoryPlan { plan: MemoryPlan, backend: BackendCapabilities, anatomy_available: bool, artifact_bytes: Option<u64> }`.
- `crate::Error::Registry(#[from] openrt_registry::RegistryError)` so a
  missing artifact fails closed with the registry's `NotFound`.
- `openrt-runtime::RuntimeMemoryPlan` re-export + `pub mod runtime_memory`.

## What the plan is / is not

| proposes | does NOT |
|---|---|
| declared `artifact_bytes` (from manifest/card, never payload reads) | load a model |
| resident estimate (sum of tensor lengths **only** when all are known) | run inference |
| per-tier status via `MemoryPlan::propose().evaluate_against_gate()` | touch the payload file (side-effect-free, asserted) |
| recommended tier + `ADVISORY_ONLY`/`EXECUTABLE` + rationale | arm or consume the gate's override |
| provider capability claims (flows through `EgressGuardedProvider`) | allocate or move memory |

`ResourceGate` behavior: planning samples `preflight`/`current`/`limits` only;
a denied plan stays denied, and `override_pending` stays untouched (test
`d_gate_denial_keeps_the_plan_denied_and_never_consumes_override`).

## Scenario table (each tested)

| | registry | provider | residency | outcome |
|---|---|---|---|---|
| A | installed | GPU unsupported | — | VRAM `Unknown` ("VRAM unavailable: backend does not support GPU offload."); never executable VRAM |
| B | installed | GPU supported | none | VRAM `Unknown` ("unobserved"); not certified |
| C | installed | GPU supported | measured headroom | VRAM `Fits`; recommended VRAM; **executable** (resident-estimate basis) |
| D | installed | GPU supported | — | gate denial stays denied; override never consumed |
| E | remote (OpenAI-compatible) | — | — | claims nothing local; nothing fabricated |

Resident estimates: NVMe `currently_observed` uses declared size as an
**upper bound**, never a claimed footprint (rationale line states this).
Timestamps: with no residency input the plan anchors to `manifest.created_at`
— `ResidencySnapshot::unknown()`'s wall-clock would otherwise leak across
seconds — so repeated planning is byte-deterministic (test).

## Example — real runtime plan (mock backend `openrt model compatibility`)

```
artifact:        local/demo-qwen
declared size:   9437184 bytes (~9 MiB)
budgets:         8192 MiB VRAM / 32768 MiB RAM (advisory)
best-fit tier:   vram
backend:         mock ("mock")
capabilities:    gpu_offload=unsupported cpu_offload=unsupported mmap=unsupported file_backed=unsupported
anatomy:         available (registry record, never re-read from the payload)
memory plan:
  VRAM:          VRAM — VRAM unavailable: backend does not support GPU offload.
  Owned RAM:     Owned RAM — RAM unavailable: backend does not support CPU execution.
  Page cache:    Page cache — page cache unavailable: backend does not support mmap.
  NVMe:          NVMe — NVMe is artifact storage only; no executable NVMe tier is claimed
  recommended:    none
  status:         ADVISORY_ONLY
  - sized from measured artifact size 9437184 B — an upper bound, NOT a claimed resident footprint (file size != resident memory)
  - VRAM: VRAM unavailable: backend does not support GPU offload.
  ...
  - the ResourceGate's current posture admits; authorisation for any actual placement remains with the gate
```

The same command accepts `--json` for the deterministic `RuntimeMemoryPlan`
(backend capability tri-states, per-tier `budget` with `capacity` /
`currently_observed` / `headroom`, `recommended_tier`, `status`, `rationale`).
The curated-only case (no declared size) keeps the historical "not installed"
exit signal instead of fabricating a size. A GPU-certified VRAM fit (scenario
C) is exercised in runtime tests with a measured `ResidencySnapshot`.

## Tests (all in `openrt-runtime::runtime_memory::tests`)

12 tests added — scenarios A–E, real-runtime+gate wiring, resident-estimate
completeness rule, missing-anatomy safety, unknown-id fail-closed,
determinism, no-execution/no-payload-read side-effect check, stable JSON.

## Gate tally

| command | result |
|---|---|
| `cargo test -p openrt-runtime --offline runtime_memory` | **12/12** |
| `cargo fmt --all -- --check` | GREEN |
| `cargo check --workspace --offline` | GREEN (0 errors) |
| `cargo clippy --workspace --all-targets --offline` | 0 errors; no new warnings from M6 code |
| `cargo test --offline --workspace` | **2474 passed; 0 failed; 1 ignored** (M5 baseline 2462; +12 M6) |
| `cargo test -p openrt-provider-llamacpp --features llamacpp` | 4/4 |
| `cargo test -p openrt-provider-vllm --features vllm` | 4/4 |

## Side-effect analysis

Planning reads registry records (in-memory maps), samples gate posture, and
calls `provider.backend_capabilities()` (a pure getter flowed through the
egress guard). No I/O on the artifact payload, no writes, no inference, no
locks beyond the registry's existing read handles. Asserted by test.

## Remaining execution gap (M7 → M13)

The plan is **advisory**: no component yet hands a `MemoryPlan` to a backend,
and nothing moves bytes. A certified VRAM fit needs a production device
observer supplying real `ResidencySnapshot.device_bytes` (M9 in the fabric
roadmap, supported at the contract level by M6's `residency` parameter). Any
execution-semantics change (mmap/streaming/prefetch/KV-spill, or actually
honoring a `MemoryPlan`) is explicitly out of this milestone and requires a
separately approved plan. Next recommended milestone: M7 (registry multi-shard
anatomy).