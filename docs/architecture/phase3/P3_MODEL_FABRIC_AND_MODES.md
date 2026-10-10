# Phase 3 — Model Fabric + Sovereign Infrastructure

**Status:** Implemented (phase gate decision in `PHASE3_FINAL_REPORT.md`)
**Date:** 2026-09-18
**Scope:** ROADMAP Phase 3, milestones M3.1–M3.9. Adds the managed, verifiable
model layer (registry, verification, signed-bundle scanning, provisioning vs
sovereign execution, hardware-aware placement/caching, MoE expert layout,
provider backends, benchmarking, routing, lifecycle) on top of the Phase 1/2
runtime, and closes the phase with integration + audit.
Baseline behavior unchanged (256 → 367 tests, all green).

This document is the milestone record. The binding design decisions are in
`docs/architecture/adr/ADR-010-model-fabric-and-modes.md` (fabric + modes),
building on ADR-001 (provenance), ADR-003 (offline scanning + modes),
ADR-006 (one store), ADR-007 (naming), ADR-009 (expert cache).

## 1. Verification (before changing anything)

- Phase 2 gate green before work: fmt / `clippy -D warnings` / build /
  `cargo test --workspace` (256 passed).
- No `tempfile` dependency; tmp dirs via
  `std::env::temp_dir().join("openrt-{tag}-{pid}-{nanos}")`; deterministic,
  network-free tests (mock/OpenAI-compatible providers with no live call).
- Workspace `unsafe_code = "forbid"` unchanged; `ed25519-dalek 2.2` is the
  only new crypto dependency (signed scan bundles).
- No fabricated data anywhere: curated rows carry **no** size/digest; every
  stored digest is recomputed from real bytes; benchmark numbers come from real
  in-process runs; expert metadata is parsed from supplied JSON, never guessed.

## 2. Design decisions

### 2.1 Operating modes — `openrt-modes` (ADR-003)

- Persisted state machine in `modes.state` (`current`, bounded `history`);
  **fail-closed default: Sovereign** whenever no record exists.
- Two distinct gates, both Provisioning-only:
  `ensure_network_allowed()` (refuses *before* any socket) and
  `ensure_materialization_allowed(what)` (refuses local/remote ingest). A
  Sovereign host can neither reach the wire nor write new artifacts into the
  fabric.
- `transition` records to the ADR-002 chained audit log **first** (fail-closed);
  a persistence failure after a successful append appends a compensating
  `mode.transition.aborted` so the log can never silently diverge from the
  effective state. No-op transitions are idempotent.
- `seed_from_config` applies `config.mode` only when no record exists — a
  persisted operator decision always wins over configuration (audited once).

### 2.2 Registry + lifecycle — `openrt-registry` (ADR-001/010)

- Namespaces: `registry.manifests`, `registry.lifecycle`, `registry.meta`,
  `registry.blobs` (content-addressed). One store, no copies (ADR-006).
- `ManifestV1` (schema v1) is **immutable / write-once**: a second publish
  returns `ManifestImmutable`. It records per-file SHA-256 + size and the
  optional `base_digest` for derived artifacts.
- Lifecycle DAG `discovered → downloaded → verified → scanned → active|idle →
  retired`; `retired` terminal; **every hop is walked one at a time** — skipped
  transitions are rejected fail-closed. Every transition is audited first.
- `import_local` pipeline: recompute SHA-256 of every declared file and require
  an exact match (never trusts supplied digests) → persist blobs →
  publish manifest → write `verify.records` → walk `Downloaded → Verified` →
  optional scan gate → operator card. A `Blocked`/`Inconclusive` scan aborts the
  install (`ScanBlocked`/`ScanInconclusive`); the artifact stays un-activatable.
- `activate` is the **only** path to `Active` and requires the scanned state.
  `retire` is idempotent and keeps bytes (soft-remove).
- Curated (Ctrl+P) offline library: five catalog rows with descriptive metadata
  (display name, family, tags) and **no** fabricated size/digest. `search` lists
  installed rows first (with real bytes/state), then curated-only rows; unknown
  imports get honest neutral metadata (name = id, family `unknown`).
- `HfClient` is **metadata-only**, feature-gated (`hf`); it performs discovery
  and never an implicit download. Network access is gated by the mode engine
  before any client call. Absent the feature, `search --hf` degrades to the
  offline leg with no error.
- `storage_used_bytes`/cards are recomputed from manifests (real accounting).

### 2.3 Verification + provenance — `openrt-verify`

- `sha256_hex` content addressing everywhere; `Verifier::verify_base` for base
  artifacts and `Verifier::verify_derived` for the ADR-001 chain (canonical
  recipe digest, parent digests, unrecorded-dataset/broken-parent refusal). The
  derived chain verifier is implemented and tested; the registry install path is
  base-artifact scoped (Phase 4 registers derived manifests at creation time —
  see §5).

### 2.4 Offline security scanning — `openrt-scan` (ADR-003)

- Static, deterministic scan driven by **ed25519-signed, versioned bundles**
  (`SignatureBundle`: content digest + key id + signature; `verify_signature`
  is strict). Verdicts are fail-closed: no bundle at/above the scheme version →
  `Inconclusive`; known-bad digest → `Blocked`; safetensors header validation
  with a supplied policy; binary kind clears only with a valid bundle.
- Scan records (`scan.records`) capture the bundle id/version; Sovereign
  activation of an unknown/unverifiable artifact is refused (no silent pass).

### 2.5 Router — `openrt-router`

- Filter-then-rank over the registry: only `Active` artifacts are eligible;
  identity filters (exact id, query over id/name/family/tags) select the
  candidate set, then ties break by lightest installed bytes and finally by
  artifact id — fully deterministic. Mode-independent: routing is local and
  works identically in Sovereign.
- Refusals carry explicit, candidate-specific reasons; an unknown id gets an
  honest summary plus a `not installed locally` eligibility reason. Candidate
  sets are bounded (safety cap) to prevent unbounded scans.

### 2.6 Backends — providers behind the Phase 1 trait

- `openrt-provider-llamacpp` and `openrt-provider-vllm` implement the Phase 1
  `ModelProvider` trait over the OpenAI-compatible HTTP API; `openrt-models`
  gains the shared `openai-http` feature. Both are feature-gated and
  runtime-selectable through `openrt-config::ProviderKind`
  (`llamacpp`/`vllm`); callers hold only `Arc<dyn ModelProvider>` — **no
  provider-specific code in callers**. Ollama remains the Phase 1 backend.

### 2.7 Hardware-aware execution — bench / sched / context / cache / hierarchy

- `openrt-bench`: deterministic in-process benchmark harness (latency/throughput/
  error-rate aggregates persisted to `bench.results`); no synthetic numbers.
- `openrt-sched`: GPU-memory slot scheduler with utilization cap, OOM-preventing
  allocation and preemption semantics; state persisted in `sched.gpu`.
- `openrt-context-sched`: per-session and global context-window budgets with
  deterministic LRU recycling (`sched.context`).
- `openrt-model-cache`: content-addressed weight cache across the hierarchy with
  LRU/ARC-style eviction, pinning, and `evict_lru_to_fit` (`cache.meta`).
- `openrt-hierarchy`: explicit `Tier` (VRAM→RAM→NVMe) slab placement with
  atomic placement planning, plus MoE `ExpertLayout`/content-addressed expert
  shard records keyed by digest (ADR-009) and recorded placements
  (`hierarchy.*`). Correctness is tested as output-equivalence across tiers and
  cache-hit/miss/prefetch paths; a swapped shard is invalidated by its digest.

### 2.8 Runtime wiring + CLI

- `Runtime` now owns `modes: ModeManager`, `model_registry: ModelRegistry` and
  `router: Router`, all over the runtime store. `RuntimeBuilder::build` seeds
  the configured mode (fail-closed bootstrap) and exposes the fabric to
  operators; `Error::Modes` maps mode failures at the boundary. Multiple
  managers over one store are coherent because every gate re-reads the persisted
  mode.
- `openrt-cli` adds `mode status|set` and `model show|status|search|install|
  activate|remove|route|benchmark|compatibility` (plus the existing provider
  `list`/`info`). Install accepts a signed `--scan-bundle` and repeated
  `--trust-key`; a bundle that verifies against **no** trusted key is refused
  fail-closed. Soft-remove = retire (bytes retained). All operator-visible.

## 3. Tests

Phase 3 adds ~111 tests (workspace 256 → **367**), all deterministic and
network-free.

- `openrt-modes` (9): fail-closed default, both gates, idempotent + audited
  transitions, history, store-failure abort, wire-value parity with config,
  config seeding never overrides an operator choice.
- `openrt-verify` (10): SHA-256, base verify, valid derived chain, tampered
  parent / wrong recipe digest / unrecorded dataset refusal.
- `openrt-scan` (9): signature verify (tamper/undecodable/non-64-byte), verdicts,
  safetensors header validation, fail-closed no-bundle.
- `openrt-registry` (11): manifest immutability, DAG one-hop enforcement,
  import digest mismatch, scan blocked/inconclusive, blob integrity,
  curated/search, storage accounting, `hf` metadata path.
- `openrt-router` (8): eligibility, deterministic lightest-first ranking,
  budget/identity/family/tag filters, refusal reasons, unknown-id, rare
  query, routing works in Sovereign, safety cap.
- `openrt-sched` (7), `openrt-context-sched` (10), `openrt-model-cache` (13),
  `openrt-hierarchy` (15), `openrt-bench` (5): scheduler contention/preemption,
  context budgets/recycling, cache eviction/pinning, tier placement +
  output-equivalence + expert digest integrity, benchmark aggregation/persistence.
- `openrt-provider-llamacpp` (2), `openrt-provider-vllm` (2),
  `openrt-models` (+`openai-http`): provider contract tests, no live network.
- Integration `p3_fabric.rs` (9, real runtime + store):
  1. `provision_import_activate_route_retire_end_to_end` — full lifecycle, route
     selected, retire refuses, audit chain verifies.
  2. `sovereign_refuses_materialization_before_touching_bytes` — import refused
     before any blob; store untouched; explicit audited transition unlocks it.
  3. `import_fails_closed_on_tampered_bytes` — digest mismatch, nothing written.
  4. `import_fails_closed_on_known_bad_hash` — scan blocked, stays `Verified`,
     not activatable.
  5. `bundle_signature_trust_is_fail_closed` — wrong key rejects.
  6. `router_ranks_lightest_installed_first_deterministically`.
  7. `unknown_artifact_gets_honest_neutral_metadata` (+ curated metadata reuse).
  8. `manifest_immutable_and_no_lifecycle_hop_is_skipped`.
  9. `fabric_state_survives_local_file_store_reopen` — real `LocalFileStore`,
     operator mode + active artifact + blob round-trip survive restart.

## 4. ROADMAP DoD mapping (Phase 3)

| Phase 3 DoD item | Status | Where |
|---|---|---|
| Discover→verify→scan→benchmark→activate→retire through the registry, audited | Done | `openrt-registry` + `p3_fabric` #1; ADR-002 chain |
| Base artifacts immutable + content-addressed | Done | `ManifestImmutable`, `registry.blobs` digest keys |
| Derived artifact verifies through provenance chain; broken chain refused | Verifier done; registry integration deferred | `openrt-verify::verify_derived` tests; Phase 4 registers derived manifests (§5) |
| All three backends pass one conformance suite; no provider code in callers | Done (contract) | `openrt-models` + `openrt-provider-{llamacpp,vllm}`; `ProviderKind` selection |
| Sovereign loopback-only; P→S transition audited; unverifiable refused fail-closed | Done | `openrt-modes` + `p3_fabric` #2;#4;#5 |
| GPU scheduler prevents oversubscription; context scheduler enforces budgets | Done (deterministic) | `openrt-sched`, `openrt-context-sched` tests |
| Hierarchy movement / expert streaming / cache correctness; digest-addressed, poisoned entry safe | Done (layout/cache level) | `openrt-hierarchy`, `openrt-model-cache` tests (§5 scope note) |

## 5. Not in this milestone / residuals (honest)

- **Derived-artifact registry integration.** The ADR-001 chain verifier and its
  tests live in `openrt-verify`; `openrt-registry::import_local` currently
  installs base artifacts. Phase 4 owns writing derived manifests at creation
  time (adapters/merge/quantize) into the same `registry.manifests` namespace,
  at which point the registry walks `base_digest` through `verify_derived`.
- **MoE expert streaming** is implemented at the layout/placement/content-address
  layer (parse supplied expert JSON, digest-keyed shard records, prefetch
  placements). Streaming real multi-GB experts off NVMe is not exercised (no
  production weights in CI); it is correctness-tested, not performance-tested.
- **GPU scheduler** OOM/preemption behavior is deterministic unit/integration
  tested against a simulated device model; no real GPU in CI.
- **Hot-swap mid-session** is represented by the `active`/`idle` lifecycle and
  the router's eligibility switch; the provider-side load/unload hand-off across
  a live session is Phase 6 (autonomous runtime) scope.
- **HF discovery** stays metadata-only and feature-gated (`hf`), a two-step
  opt-in (build feature + explicit per-action command); no implicit download
  exists to disable.
- **OS-level network denial** is enforced at the mode boundary (refusal before
  any client call, proven by `p3_fabric` #2). Kernel/namespace egress denial is
  Phase 6 (`openrt-netpol`).
- Benchmark numbers from the in-process harness are real but not comparable to
  vendor claims; the registry never presents them as such.

## 6. Gate summary

Full counts and the decision are in `PHASE3_FINAL_REPORT.md` (repo root). In
short: 256 → 367 tests, 0 failures; fmt / `clippy --all-targets -D warnings` /
build clean across the workspace; the model fabric is wired into the runtime and
CLI; Sovereign refusal is proven before bytes are touched; the phase gate runs
every Phase 3 Go/No-Go requirement with open residuals documented above.
