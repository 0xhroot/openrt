# PHASE 3 — FINAL REPORT & GATE DECISION

**Date:** 2026-09-18
**Scope:** Phase 3 (M3.1 → M3.9) of ROADMAP.md — "MODEL FABRIC + SOVEREIGN INFRASTRUCTURE".
**Decision:** **PASS WITH HARDENING.**

---

## 1. Executive summary

Phase 3 delivered the managed, verifiable model layer and the Provisioning/
Sovereign enforcement the roadmap calls for, on top of the Phase 1/2 runtime:
an immutable-manifest registry with a one-hop lifecycle DAG, SHA-256
verification with an ADR-001 provenance chain, ed25519-signed offline security
scanning, the curated Ctrl+P offline library, metadata-only (never implicit)
Hugging Face discovery, deterministic benchmarking, mode-independent routing,
hardware-aware placement/cache/scheduler crates, safe llama.cpp/vLLM backends
behind the Phase 1 trait, a fail-closed Sovereign mode engine, and full wiring
into `Runtime` and the CLI. The final quality gate is green (367 tests default,
376 with `--all-features`, 0 failures; fmt + `clippy -D warnings` across the
workspace in both configurations), the Sovereign refusal is proven *before any
byte is touched* by an integration test, and the remaining residuals are
documented with owners.

## 2. Milestone record (M3.1 → M3.9)

| Milestone | Delivered in | Evidence |
|---|---|---|
| M3.1 Registry + metadata + local/managed store | P3.1 | `openrt-registry` (`registry.manifests/meta/lifecycle/blobs`), ADR-010 |
| M3.2 SHA-256 verification + provenance + signed-bundle scan + install | P3.2 | `openrt-verify`, `openrt-scan`, `import_local` pipeline |
| M3.3 HF discovery (opt-in) + Ctrl+P library + import | P3.3 | metadata-only `HfClient` (`hf` feature), `curated_catalog`, local import |
| M3.4 Hardware detection + footprint + provisioning mode | P3.4 | `openrt-hierarchy` tiers + footprint placement, `openrt-modes` |
| M3.5 llama.cpp + vLLM backends | P3.5 | `openrt-provider-{llamacpp,vllm}` + `openrt-models` `openai-http` |
| M3.6 Benchmarking, router, hot-swap lifecycle | P3.6 | `openrt-bench`, `openrt-router`, registry `active/idle/retired` |
| M3.7 GPU + context schedulers + model cache | P3.7 | `openrt-sched`, `openrt-context-sched`, `openrt-model-cache` |
| M3.8 Memory hierarchy, MoE experts, expert cache, prefetch | P3.8 | `openrt-hierarchy` (`hierarchy.experts`, ADR-009) |
| M3.9 Sovereign execution mode + phase gate | P3.9 | `openrt-modes`, runtime wiring, `p3_fabric.rs`, this report |

## 3. Verification baseline

- Pre-gate baseline (Phase 2): fmt/check/clippy(`-D warnings`)/build green,
  256 tests.
- Post-gate (Phase 3): fmt / `clippy --workspace --all-targets -D warnings` /
  `clippy --workspace --all-features --all-targets -D warnings` / build /
  `cargo test --workspace` (367) / `cargo test --workspace --all-features`
  (376) all green; rerun ×3 with identical counts.

## 4. Test suite summary

| Target | Tests |
|---|---|
| `openrt-modes` | 9 |
| `openrt-verify` | 10 |
| `openrt-scan` | 9 |
| `openrt-registry` (11 default / with `hf`) | 11 |
| `openrt-router` | 8 |
| `openrt-sched` / `openrt-context-sched` | 7 / 10 |
| `openrt-model-cache` / `openrt-hierarchy` | 13 / 15 |
| `openrt-bench` | 5 |
| `openrt-provider-llamacpp` / `openrt-provider-vllm` | 2 / 2 |
| `openrt-models` (+`openai-http`) | 12 (with feature) |
| Integration `p3_fabric.rs` (real runtime + store) | 9 |
| Existing Phase 1/2 suites | 256 (unchanged, re-verified) |
| **Total** (default / all-features) | **367 / 376** |

## 5. Security audit — findings and remediations

| Item | Result |
|---|---|
| Mandatory SHA-256, content-addressed base artifacts | Verified fixed — recomputed, never trusted; `DigestMismatch` fail-closed (`p3_fabric` #3) |
| Activation refused on mismatch / unverified | Verified fixed — `activate` requires `scanned`; skipped hops rejected |
| Signed, versioned offline scan bundles; unsigned/stale → fail closed | `openrt-scan` strict `verify_signature`; no valid bundle → `Inconclusive`; known-bad → `Blocked` (`p3_fabric` #4,#5) |
| Unknown/unverifiable refused in Sovereign (no silent pass) | Verified — `p3_fabric` #2 proves refusal before bytes are written |
| Install/download/discovery only in Provisioning, explicit + gated; HF is feature + per-action opt-in | `ensure_materialization_allowed`/`ensure_network_allowed`; `hf` feature-gate + per-action client |
| Registry/manifests immutable after write | `ManifestImmutable` (`p3_fabric` #8) |
| Lifecycle + mode transitions appended to ADR-002 chain | `registry.transition`/`mode.transition` audited first; chain verifies (`p3_fabric` #1) |
| No secrets / no creds in logs | Held (`unsafe forbid`, SEC-04 baseline, no secrets introduced) |
| Expert cache digest-addressed; swapped/poisoned shard rejected | `openrt-hierarchy`/`openrt-model-cache` tests keyed by content digest |
| No provider-specific code in callers | Held — callers hold `Arc<dyn ModelProvider>` only |

## 6. DoD mapping (ROADMAP Phase 3, §5–§7)

| DoD item | Status | Where |
|---|---|---|
| Discover→download→verify→scan→benchmark→activate→retire through the registry, audited | Done | `openrt-registry` + `p3_fabric` #1; benchmark records via `openrt-bench` |
| Base artifacts immutable + content-addressed | Done | `ManifestImmutable`; `registry.blobs/<digest>` |
| Derived artifact verifies through provenance chain; broken chain refused | Verifier done; registry integration is Phase 4 | `openrt-verify::verify_derived` (ADR-001), §8 |
| Three backends pass one conformance suite; no provider code in callers | Contract done; live-server runs environment-dependent | `openrt-models` contract + `openrt-provider-{llamacpp,vllm}`, §8 |
| Sovereign loopback-only; P→S audited; unverifiable refused fail-closed | Done | `openrt-modes`, `p3_fabric` #2,#4,#5 |
| GPU scheduler prevents oversubscription; context scheduler enforces budgets | Done (deterministic device model) | `openrt-sched`, `openrt-context-sched` |
| Hierarchy movement / expert streaming / cache correctness; digest-addressed poisoned-entry safety | Done (layout + content-address layer) | `openrt-hierarchy`, `openrt-model-cache`, §8 scope note |

## 7. ADR conformance

| ADR | Status |
|---|---|
| ADR-001 provenance + content addressing | Base path implemented; derived chain verifier implemented, registry integration on Phase 4 |
| ADR-002 audit envelope | Held — every lifecycle + mode transition appends first, fail-closed |
| ADR-003 offline scanning + Provisioning/Sovereign modes | Implemented (`openrt-modes`, `openrt-scan`) |
| ADR-006 one store / namespaced records | Held — no parallel store; `registry.*`, `modes.state`, `verify.records`, `scan.records`, `bench.results`, `sched.*`, `cache.meta`, `hierarchy.*` |
| ADR-007 naming/namespaces | Held — allocated in OWNERSHIP §Phase 3; `context` binding avoided for model context |
| ADR-009 expert cache content addressing | Implemented (`hierarchy.experts`) |
| ADR-010 model fabric + modes | Implemented (this phase's binding ADR) |

## 8. Residuals & accepted risks (honest)

- **Derived-artifact registry integration** (owner: Phase 4). `openrt-verify`
  verifies ADR-001 chains; the registry import path installs base artifacts.
  Phase 4 writes derived manifests into `registry.manifests`, where the registry
  will walk `base_digest` through `verify_derived`.
- **Live-server provider conformance + real-GPU MoE/perf** (owner: Phase 5/6
  hardening). The shared contract suite passes without live llama.cpp/vLLM
  servers; on-machine conformance runs are environment-dependent and not part
  of CI. MoE streaming correctness is tested; multi-GB real-weight performance
  is not.
- **OS-level network denial** (owner: Phase 6 `openrt-netpol`). Sovereign
  enforcement is proven at the mode boundary, before any client call; kernel/
  namespace egress denial is later-phase.
- **Mid-session hot-swap consumer** (owner: Phase 6). Active/idle lifecycle +
  router eligibility are in place; the provider-side load/unload hand-off
  across a live session is not.
- **HF stays metadata-only** — a deliberate scope ceiling, not a gap.
- **No new event kinds** were added this phase; operators observe the fabric
  through the audit chain and CLI.

These are tracked in `docs/architecture/phase3/P3_MODEL_FABRIC_AND_MODES.md`
§5 and OWNERSHIP notes; none violates a Phase 3 Go/No-Go security requirement.

## 9. Phase 4 compatibility

- Phase 4 (multimodal, adapters, quantization, embeddings, RAG) lands *on*
  this fabric: `ManifestV1` already carries `base_digest`; `registry.manifests`
  is the namespace adapters/quantized outputs will publish into at creation
  time; digest-keyed blobs and expert shards are content-addressed at the base.
- No breaking surface was introduced: `openrt-modes`/`openrt-registry`/
  `openrt-router` are new leaf crates; `Runtime` gained three public fields;
  `Error::Modes` is additive. Phase 1/2 call sites compile unchanged.

## 10. Documentation status

- `docs/architecture/adr/ADR-010-model-fabric-and-modes.md` — binding design.
- `docs/architecture/phase3/P3_MODEL_FABRIC_AND_MODES.md` — milestone record.
- OWNERSHIP.md change log updated with Phase 3 allocations.
- No code from `.opencode/` is copied into the runtime.

## 11. Repository hygiene

- Zero commits (all phase work uncommitted per policy). No secrets in tracked
  files. Workspace-wide `unsafe_code = "forbid"`; no `tempfile`;
  deterministic, network-free tests; `ed25519-dalek 2.2` is the only new crypto
  dependency. The throwaway CLI smoke fixture generator was deleted after use.

## 12. Gate decision

**PASS WITH HARDENING**

- All Phase 3 Go/No-Go requirements verified green: full audited lifecycle,
  immutable content-addressed base artifacts, fail-closed Sovereign refusal
  proven before bytes are touched (integration-tested), audited P→S
  transition, provider contract held without caller-side provider code,
  schedulers/cache/hierarchy correctness-tested.
- Baseline stable at 367/376 tests, 0 failures ×3, across fmt / `clippy
  -D warnings` (default **and** `--all-features`) / build / test.
- Residuals are scoped, owned, and documented (Phase 4/6 owners) — no
  Go/No-Go security requirement is left open.

Phase 4 ("MULTIMODAL + KNOWLEDGE + MODEL ADAPTATION") may begin on this fabric.

## 13. Artifacts produced in Phase 3

- Crates: `openrt-modes`, `openrt-registry`, `openrt-router`, `openrt-bench`,
  `openrt-sched`, `openrt-context-sched`, `openrt-model-cache`,
  `openrt-hierarchy`, `openrt-provider-llamacpp`, `openrt-provider-vllm`; extended
  `openrt-verify` (provenance chain), `openrt-scan` (signed bundles + safetensors),
  `openrt-models` (`openai-http`), `openrt-config` (`ProviderKind`), `openrt-runtime`
  (fabric wiring + `Error::Modes`).
- CLI: `mode status|set`; `model show|status|search|install|activate|remove|
  route|benchmark|compatibility` (trusted-key scan bundles, soft-remove).
- Integration: `crates/integration-tests/tests/p3_fabric.rs` (9 tests).
- Docs: `ADR-010`, `docs/architecture/phase3/P3_MODEL_FABRIC_AND_MODES.md`,
  `PHASE3_FINAL_REPORT.md`, OWNERSHIP.md change-log entry.