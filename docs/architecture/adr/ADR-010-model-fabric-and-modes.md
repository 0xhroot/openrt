# ADR-010: Model Fabric — Registry, Verification Chain, and Mode-Enforced Operations

**Status:** Accepted
**Date:** 2026-09-18
**Applies to:** Phase 3 (registry, verify, scan, bench, router, schedulers,
cache, hierarchy, providers, modes); Phase 4 (derived artifacts, adapters);
Phase 6 (air-gapped operation, supply-chain verification)

## Context

Phase 3 builds the model fabric: a registry-driven lifecycle for local models
with discovery, installation, SHA256 verification, security scanning,
benchmarking, routing, hot-swap foundations, GPU/context scheduling, caches,
and local backends behind the Phase 1 `ModelProvider` trait, plus a VRAM→RAM→NVMe
hierarchy and MoE expert streaming foundations (ADR-009). The roadmap must
interoperate with three already-accepted ADRs without introducing a parallel,
inconsistent engine:

- ADR-001 (base vs derived artifacts, immutable content-addressed store, proof
  chain) — verification semantics.
- ADR-003 (Provisioning vs Sovereign execution modes, signed offline bundles,
  fail-closed activation) — operating modes and scanning.
- ADR-009 (expert cache content addressing, cache/registry separation) — memory
  hierarchy naming.

Prior conflict risk: without a binding decision, install/discovery/scan/activate
could each re-derive "what mode may a network action happen in", verification
could be duplicated between registry and scan, and the router/schedulers could
sprout their own activity/state model inconsistent with the registry lifecycle.

## Decision

1. **State and mode are distinct, both typed.**
   - The registry lifecycle (`discovered → downloaded → verified → scanned →
     active | idle → retired`) describes an *artifact*. `discovered` is
     metadata-only; `downloaded`/`installed` holds content; `active` means
     loadable by a backend; `currently loaded` is backend runtime state,
     tracked separately and never conflated with artifact state.
   - The execution *mode* (`provisioning` | `sovereign`) describes the
     *operator's machine policy* (ADR-003). It is a persisted state machine in
     the `modes.state` namespace with explicit, audited transitions. It is not
     derived from network state and never changes implicitly.

2. **Network use is mode-gated at the access point.** Only `openrt-modes`
   decides whether a networked operation may start. In `sovereign` mode every
   network entry point (HF discovery, remote install/download, provider
   fallback, telemetry egress) is refused before any client call is made —
   a refused operation is proven to never reach the network client. In
   `provisioning` mode network is *conditionally allowed* for explicit,
   operator-initiated, permission-gated materialization only. Local file
   import (no network) is permitted in both modes but is still permission-gated,
   scanned, and verified.

3. **Two-phase verification, single registration.**
   - **Verify (`openrt-verify`)** records SHA-256 of artifact content
     (`SHA256(content)`), registry manifests are immutable and content-addressed
     (ADR-001). Base artifacts verify by digest; derived artifacts verify by
     walking the provenance chain to immutable base roots and recomputing the
     recipe digest. Never by direct comparison to a base hash.
   - **Scan (`openrt-scan`)** applies signed, versioned offline signature
     bundles (ADR-003) and records the bundle version used. A bundle whose
     signature, digest, freshness, or key id cannot be confirmed is refused
     outright.
   - Activation requires **verify pass AND scan pass**. In `sovereign` mode an
     inconclusive or stale scan is a refusal (fail-closed), with only an
     explicit, per-artifact, audited operator override (ADR-003).
   - One registration point: the registry refuses to record an artifact as
     verified/scanned unless the corresponding records were written through
     the same `openrt-verify`/`openrt-scan` surfaces.

4. **Crate boundaries (ADR-007/OWNERSHIP).** New crates own exactly these
   concepts: `openrt-registry` (registry, manifests, lifecycle, install, HF
   discovery, Ctrl+P library), `openrt-verify`, `openrt-scan`, `openrt-bench`,
   `openrt-router`, `openrt-sched` (GPU), `openrt-context-sched` (context),
   `openrt-model-cache`, `openrt-hierarchy` (VRAM→RAM→NVMe + expert store),
   `openrt-modes`, `openrt-provider-llamacpp`, `openrt-provider-vllm`. Registry
   blobs and metadata live in the one shared store (ADR-006) under the
   allocated namespaces `registry.blobs` (binary, content-addressed),
   `registry.manifests`, `registry.lifecycle`; `cache.meta` and `hierarchy.*`
   hold cache/hierarchy records; expert shard content is content-addressed
   (ADR-009).

5. **Router and schedulers are deterministic, explainable, and registry-aware.**
   - Router: eligibility = filters (mode/privacy → capability → availability →
     context → backend → hardware → policy), ranked only after filtering.
     Every rejected candidate carries an explicit reason; a route with no
     eligible model is an explicit rejection, never a silent remote fallback.
     Routing history is stored only when it contains sufficient valid
     measurements; no invented benchmark numbers.
   - GPU/context schedulers enforce budgets; oversubscription is a denial, never
     an over-commit. Changes are recorded in `sched.gpu` / `sched.context`.

6. **Backend adapters are real.** `openrt-provider-llamacpp` and
   `openrt-provider-vllm` speak the real server HTTP APIs (OpenAI-compatible
   `/chat/completions`), are feature-gated like Ollama, report typed
   capabilities, and return explicit `Unsupported` errors for operations the
   backend cannot perform. No fake backends are introduced.

7. **MoE / hierarchy honesty.** Tier placement (VRAM→RAM→NVMe) and expert
   caching are real foundations, but an artifact's expert layout only exists if
   its manifest declares expert shards with per-shard digests. Dense models get
   layer-slab placement only — never fabricated expert metadata, never fake
   streaming claims. Expert streaming/cache is keyed by content digest (ADR-009)
   and cache misses re-verify before caching.

## Consequences

- Install/scan/activate flows share one mode manager and one lifecycle state
  model; no subsystem can silently operate in the wrong mode.
- Network actions have a single audit trail (`mode.transition`,
  `discovery.request`, `model.install`, `model.download`) and can be proven
  (tests) not to reach the network client in Sovereign Mode.
- Verification is split (content digest vs threat scan) which keeps local
  SHA256 verification instant while scanning remains bundle-versioned.
- Phase 4 (adapters/quantization) and Phase 6 (air-gap, supply chain) extend
  the same manifests, bundles, and record namespaces — no new paradigm.

**References:** ROADMAP.md Phase 3 (all components, DoD, tests, security
requirements); ADR-001, ADR-003, ADR-006, ADR-007, ADR-009; OWNERSHIP.md
Phase 3 allocation table and change log.