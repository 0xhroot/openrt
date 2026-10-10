# OpenRT — Development Roadmap

**Status:** Draft v0.5  
**Date:** 2026-09-25  
**Owner:** OpenRT core team

OpenRT is a sovereign, local-first, agentic AI runtime/workbench. This document
defines the eight development phases (v0.1–v0.2 defined six; v0.3 adds Phase 7
Tool Fabric as a documented revision; v0.4 expands Phase 7 with **Phase 7B —
Remaining Tool Fabric**: browser automation, MCP integration, database tools,
application tools, Excel/PPT/DOCX automation, cross-tool orchestration; v0.5
adds **Phase 8 — Sandbox + Isolated Execution Fabric**, drawn from the
`project-complete-plan.txt` Phase 8 line — sandbox, resource limits, network
restrictions, filesystem isolation, timeouts, audit — which must be completed
before autonomous tool use). It is
a **planning document only** for scope; implementation is tracked in
`docs/architecture/OWNERSHIP.md` and the per-phase architecture docs.

---

## 0. Governing Constraints

These constraints apply to every phase and overrule anything written below in
case of contradiction.

1. **ECC is the development harness.** The `.opencode/` directory is the ECC
   plugin used to develop OpenRT. It must not be modified or replaced unless
   required for OpenCode integration, and it must **never** be copied into the
   OpenRT runtime architecture. ECC's TypeScript/Node code is not a reference
   for OpenRT's runtime design.
2. **OpenRT core is Rust.** A Cargo workspace is the unit of build. Non-Rust
   components (e.g., a future Web UI, sandbox runtimes) are strictly external
   adapters; the core runtime, all state, and the event system stay Rust.
3. **Provider-agnostic and modular.** Every subsystem depends on traits/interfaces,
   never on a concrete provider. No vendor, model, backend, or API is baked in.
4. **Local-first and sovereign.** No telemetry, model, data, or credential may
   leave the machine unless the operator explicitly enables network use per phase
   below and per-feature opt-in.
5. **Do not implement everything at once.** Phases are strictly sequential.
   A phase may only begin when the prior phase's Definition of Done is met.
6. **Security is not deferred.** Security requirements listed per phase are
   mandatory within that phase; they are not backlog items.
7. **Clarity of ownership.** The harness (agents, skills, instincts, learn,
   checkpoint commands) has *similar-looking* features to several OpenRT features
   (skills, memory, checkpoints, replay). OpenRT must implement its own runtime
   versions; it must not reuse or ship ECC logic as product code.

### Convention: two provider layers must coexist

Phase 1 defines the **model provider abstraction** (trait/contract) and one
concrete provider (Ollama) to prove the contract. Phase 3 extends that *same*
abstraction with llama.cpp and vLLM backends plus lifecycle/GPU management.
Phase 3 must extend — never duplicate or replace — the Phase 1 trait. Duplication
here is considered a hard architectural error.

### Convention: feature ownership map

| Feature area | Where it is built | Reference |
|---|---|---|
| Permission system (core) | Phase 1 | — |
| Tool permissions (per-tool policy) | Phase 2 (on Phase 1 core) | — |
| Vision grounding (screen capture) | Phase 2 | — |
| VLM inference pipeline | Phase 4 (consumes Phase 2 grounding) | — |
| Checkpoints/rollback (execution) | Phase 2 | ADR-007 |
| Workflow time machine (history replay/fork) | Phase 5 (on Phase 2 checkpoints) | ADR-007 |
| Context scheduler (model context) | Phase 3 | ADR-007 |
| Working memory (persistence) | Phase 5 | ADR-007 |
| Audit log envelope | Phase 2 (chaining fields from day one) | ADR-002 |
| Runtime safety/halt signal | Phase 2 (common signal bus; per-subsystem switches) | ADR-008 |
| Computer-use kill switch | Phase 2 (scope=computer) | ADR-008 |
| Trading kill switch / hard risk limits | Phase 6 (scope=trading; separate mechanism) | ADR-008 |
| Model artifact provenance (base + derived) | Phase 3 (base), Phase 4 (derived) | ADR-001 |
| Storage management / namespaces | Phase 1 foundation + per-phase ownership | ADR-006 |
| Telemetry → observability | Phase 5 (telemetry) → Phase 6 (extends) | ADR-005 |
| Offline security scanning + operating modes | Phase 3; hardened in Phase 6 | ADR-003 |
| Distributed execution (enclave) | Phase 6 | ADR-004 |
| Expert/model cache integrity | Phase 3 | ADR-009 |

The full architecture ownership map (subsystem → phase → crate → storage
namespace) is maintained in `docs/architecture/OWNERSHIP.md`.

### Convention: architecture records (ADRs)

Cross-cutting and cross-phase decisions are recorded as Architecture Decision
Records under `docs/architecture/adr/`. Roadmap sections cite them (ADR-001…
ADR-009). A decision change requires an ADR revision; the roadmap must never
contradict an accepted ADR. When a new subsystem is proposed, its crate and
storage namespace are allocated in `OWNERSHIP.md` first (ADR-007).

---

## PHASE 1 — CORE AI RUNTIME

The foundation: a Rust workspace that can run an agent program against a local
model with durable state, permissions, tools, and logs.

### 1. Objective

Stand up the OpenRT Cargo workspace and a minimal, provably correct core runtime:
an agent loop, a session/task model, an event bus, a tool registry, a permission
gate, an Ollama-backed model provider behind a trait, persistent state, logging,
a CLI, and a test harness. At the end of Phase 1 a user can launch the CLI, start
a session, and have an agent call a registered tool through a local model.

### 2. Components

- **Rust workspace** — Cargo workspace, crate layout, edition and MSRV policy,
  shared `xtask`/tooling conventions, CI scaffold.
- **Core runtime** — process/bootstrap, config loading, lifecycle (start/shutdown),
  wiring of all subsystems (dependency injection without a framework).
- **Task/session system** — session lifecycle, task queue, task lifecycle states,
  session persistence model.
- **Event bus** — typed, async publish/subscribe event bus; in-memory transport;
  event envelopes with IDs/timestamps; ordering guarantees.
- **Agent abstraction** — runtime-defined agent trait (system prompt, context,
  tool availability, loop control); not tied to any UI or provider.
- **Model provider abstraction** — trait defining chat/tool-call interface,
  streaming, error taxonomy, model capability metadata.
- **Ollama provider** — first concrete provider implementing the abstraction;
  feature-gated; never a hard dependency.
- **Tool registry** — schema-defined tools, registration, discovery, argument
  validation, execution, result envelope.
- **Permission system** — central permission evaluator: allow/deny/ask decisions,
  policy store, approval workflow, session-scoped state.
- **Resource manager foundation** — resource quota tracking abstraction (CPU/RAM/
  disk) and a minimal in-process resource monitor; full scheduling comes later.
- **CLI** — `openrt` binary: init, run, session list/resume, tool list, provider
  config, permissions, logs.
- **Logging** — structured logging (JSON + human), log levels, per-session log
  capture, redaction hooks.
- **Persistent state** — foundation store manager exposing typed namespaces
  (content-addressed blobs, key-value, records, append-only log) on local disk;
  versioned schema, atomic updates, corruption recovery. All later phases'
  persistence builds on this one store manager; no phase may introduce a parallel
  store (ADR-006).
- **Unit/integration testing** — test harness, test doubles for providers/tools,
  integration test for the agent loop.

### 3. Dependencies on previous phases

None — this is the first phase.

### 4. Major milestones

- M1.1: Workspace scaffolding + CI passes (`cargo build`, `cargo test`, lint).
- M1.2: Event bus + core runtime with in-process integration test.
- M1.3: Model provider trait + Ollama provider (feature-gated) verified by tests.
- M1.4: Tool registry + one canonical example tool + permission gate wired into
  the agent loop.
- M1.5: Persistent state + logging + CLI covering the full happy path.
- M1.6: Definition of Done audit and phase gate.

### 5. Definition of Done

- `cargo build` and `cargo test` clean; lint/clippy and `cargo fmt --check` pass.
- A local agent can complete a task that requires calling a registered tool via
  the Ollama provider, end to end through the CLI.
- Sessions/tasks survive a process restart (persistent state round-trip test).
- Permission decisions (allow/deny/ask) are honored by the tool-execution path.
- All provider and tool access goes through the public traits — no direct calls
  to a concrete provider anywhere outside its own crate/feature.
- No code from `.opencode/` is copied or referenced by the runtime.

### 6. Tests required

- Unit: event bus ordering/delivery, task/session state machine, tool schema
  validation, permission evaluator policy matrix, provider trait mock.
- Integration: agent loop with a mock model; agent loop with Ollama (feature
  gated, skipped when unavailable); session persistence across restart.
- Property/fuzz: event envelope and state serialization invariants (v1).
- Minimum coverage line: 80% on all core crates.

### 7. Security requirements

- No secrets in code, config defaults, or logs; secrets only via env/keyring;
  log redaction for detected secret patterns.
- Permission system denies by default; explicit allowlists for tools and paths.
- Persistent store files: default restrictive permissions (0600 dirs 0700),
  basic integrity checks (checksums) and corruption detection.
- Provider credentials for remote models are out of scope in Phase 1 except as
  an explicitly opt-in, never-default path.
- Input validation for all tool arguments and CLI inputs.

### 8. What must NOT be implemented yet

- No multi-agent orchestration, master agent, or sub-agent types.
- No computer-use tools (filesystem/terminal/sandbox/browser/input) beyond the
  canonical example tool used to prove the registry.
- No model registry, router, GPU scheduler, quant/offload management.
- No embeddings/vector store/RAG, no training/adaptation.
- No TUI/Web UI beyond the CLI.
- No memory subsystem (working/episodic/semantic/workflow).
- No autonomous/continuous execution, no network federation.
- No trading subsystem in any form.

---

## PHASE 2 — AGENT + COMPUTER USE

Turn the single-agent core into an orchestratable multi-agent system that can
safely operate a computer: filesystem, terminal, sandbox, browser, and
screen/input control, with recovery (checkpoints/rollback) and dry-run.

### 1. Objective

Add a master agent and typed sub-agents (planner, researcher, coder, verifier,
critic) coordinated through a shared blackboard and a workflow DAG, plus a full
computer-use tool belt with a safety loop: permissions, dry-run, checkpoints,
rollback. The runtime can act inside a machine, not just respond to one agent.

### 2. Components

- **Master agent** — the orchestrating agent that decomposes tasks and dispatches
  work; owns session/context policy.
- **Multi-agent orchestration** — agent lifecycle, spawn/monitor/teardown,
  result collection, error handling for sub-agent failures.
- **Planner** — decomposes objectives into plan steps (as a typed sub-agent).
- **Researcher** — search/read-gather sub-agent.
- **Coder** — code-focused sub-agent.
- **Verifier** — validates outputs against criteria; produces pass/fail.
- **Critic** — adversarial review of plans, outputs, and decisions.
- **Shared blackboard** — thread-safe shared workspace for cross-agent data,
  facts, artifacts, and agent-to-agent messages.
- **Workflow DAG** — explicit graph of steps/nodes with dependencies, retries,
  conditionals; runtime executes the DAG.
- **Filesystem tools** — read/write/list/move/delete with path scoping and
  traversal guards.
- **Terminal** — shell command execution tool with working-dir scoping, output
  capture, timeouts, and process-group management.
- **Python sandbox** — sandboxed interpreter for untrusted agent-authored code
  (filesystem, network, CPU/memory limits).
- **Browser** — headless/visible browser control tool (navigation, DOM, actions).
- **Screen capture** — screenshot capture tool (local display or headless).
- **Vision grounding** — map screen coordinates/elements from captured frames;
  deterministic grounding first (accessibility tree/DOM), model-assisted later.
- **Mouse** — pointer input tool (move, click, drag).
- **Keyboard** — keyboard input tool (type, shortcuts).
- **Computer-use loop** — the loop that interleaves perceive (capture/ground) →
  decide → act (input) with safety checks at every step.
- **Checkpoints** — snapshot of state (fs + relevant runtime state) at defined
  boundaries during a computer-use session.
- **Rollback** — restore to a checkpoint after failure or unsafe action.
- **Dry-run** — simulate execution of plans/workflows without applying changes;
  report what *would* happen.
- **Tool permissions** — per-tool, per-path, per-command policies layered on the
  Phase 1 permission core; scoped-approval workflows.

### 3. Dependencies on previous phases

- Phase 1 agent abstraction is the unit the master agent and sub-agents are built
  from; the event bus carries orchestration signals; session system holds the
  DAG/run state; tool registry is where all computer-use tools register; the
  permission system is extended with tool-scoped policies; resource manager
  foundation is extended to bound sandbox/terminal resource use; persistent
  state hosts execution state in the Phase 2 namespace of the foundation store
  (DAG runs, blackboard snapshots, checkpoints, audit-log entries) per ADR-006.

### 4. Major milestones

- M2.1: Master agent + sub-agent types + shared blackboard (in-process).
- M2.2: Workflow DAG execution engine on the event bus.
- M2.3: Filesystem + terminal tools behind tool permissions.
- M2.4: Python sandbox.
- M2.5: Browser, screen capture, vision grounding (deterministic), mouse,
  keyboard, and the computer-use loop.
- M2.6: Checkpoints, rollback, and dry-run closing the safety loop.
- M2.7: Definition of Done audit and phase gate.

### 5. Definition of Done

- A workflow DAG mixing planner/coder/verifier/critic steps runs end to end and
  its results persist in the session.
- Computer-use loop can complete a controlled task (e.g., read a file, run a
  check, fill and submit a local form) in a sandboxed test environment.
- Every destructive tool requires permission or (in interactive mode) approval;
  dry-run of any workflow produces a faithful, auditable "would-do" trace.
- Checkpoint/rollback restores both filesystem and relevant runtime state; a
  forced failure test demonstrates recovery.
- Sandbox enforces filesystem, network, CPU, and memory limits (tested).
- No tool can escape its configured path/scope (traversal tests pass).

### 6. Tests required

- Orchestration: DAG execution order, retry and failure propagation, blackboard
  concurrency (stress).
- Computer use: scripted screen states drive grounding+input in headless test
  harness (no real display needed); deterministic grounding accuracy ≥ 95% on
  the test fixture set.
- Safety: traversal/escape attempts on fs and terminal tools; dry-run parity
  (dry-run trace == later real execution for a fixed fixture); rollback
  restores known state (including failure mid-action).
- Sandbox: escape attempts, resource exhaustion (fork bomb, memory blow), timeout
  enforcement.
- Permissions: policy matrix incl. deny default, scoped allow, ask flows.
- Safety/halt: computer-use kill switch halts mouse/keyboard/terminal actions
  mid-action; a HALT signal emitted on the bus is audited; scoped signals do not
  trigger other subsystems' mitigations.
- Audit: chained log tamper test — any modification of a past entry is detected;
  append-only write path enforced (ADR-002).

### 7. Security requirements

- **Kill switch (computer-use, panic-level):** an emergency stop for the
  computer-use loop (cover keyboard/mouse/terminal/browser) that halts computer
  actions regardless of agent state. It is a mechanism adapter wired to the
  common runtime safety/halt signal (scope=computer); it never triggers other
  subsystems' mitigations. Operator-only; no agent or self-optimization process
  may arm/disarm it. *Required in Phase 2, not deferred to Phase 6.* (ADR-008)
- Total path scoping: every fs/terminal/browser action has a jail boundary;
  absolute deny-list (e.g., credential stores, keyrings) enforced in the tool
  layer, not only in prompts.
- Terminal: no interactive TTY for agents; subprocess isolation, timeouts,
  output-size caps; explicit env scrubbing.
- Sandbox: no host filesystem or network access by default; memory/CPU/process
  limits enforced by the OS, not by convention.
- Screen capture and input require explicit operator opt-in per session and are
  disabled in headless servers unless launched with the flag.
- Checkpoints/rollback must never write credentials or secrets into snapshot
  state (secrets excluded and redacted in traces).
- Credentials for interactive destinations (browser logins) are out of scope
  entirely in Phase 2.
- All computer-use actions are recorded to an append-only audit log with a
  **chained, tamper-evident envelope** defined at creation time — sequence,
  timestamp, actor/action/subject/result, `prev_hash` and chain-mode fields —
  so tampering is detectable from Phase 2 onward and Phase 6 strengthens
  verification without an incompatible format migration (ADR-002). This log is
  the security basis for Phase 6's audit system.
- The runtime safety/halt signal (broadcast over the event bus) used by the
  computer-use kill switch is established in Phase 2; every signal emission and
  subsystem reaction is audited (ADR-008).

### 8. What must NOT be implemented yet

- No model registry/router/GPU scheduling or multiple model backends beyond the
  Phase 1 provider.
- No quantization, offloading, or MoE expert streaming.
- No RAG, embeddings, vector/knowledge graph, OCR/vision model pipelines.
- No full VLM inference — only deterministic grounding over captured frames.
- No TUI/Web UI, command palette, or visual session browser.
- No memory subsystem, skills/instincts/hooks learning, or self-improvement.
- No autonomous long-running agents without operator supervision.
- No trading subsystem; no wallet/broker connectors.
- No model installation/discovery (Hugging Face/Ctrl+P).

---

## PHASE 3 — MODEL FABRIC + SOVEREIGN INFRASTRUCTURE

Give OpenRT a managed, verifiable model layer: discovery, installation,
verification, benchmarking, routing, and hardware-aware execution that runs
entirely on the operator's own hardware.

### 1. Objective

Build the "model fabric": a registry-driven lifecycle for local models with
hardware estimation, SHA256 verification, security scanning, benchmarking,
routing, hot-swapping, GPU/context scheduling, caching, and multiple local
backends (Ollama, llama.cpp, vLLM) behind the Phase 1 abstraction — including
a VRAM→RAM→NVMe memory hierarchy and MoE expert streaming/caching/ prefetch.

### 2. Components

- **Model registry** — canonical, immutable-manifest store of known models,
  versions, revisions, base/derived artifacts, metadata, and status; local-first
  source of truth (ADR-001, ADR-006).
- **Hugging Face model discovery** — *opt-in network* catalog/metadata discovery;
  download never implicit.
- **Ctrl+P model library** — built-in curated model manifest/collection for
  offline/air-gapped setups.
- **Model metadata** — structured metadata schema (arch, params, license, family,
  tags, modality, quantization levels).
- **Hardware compatibility** — device detection (GPU vendor/arch, CPU, RAM, NVMe,
  driver) and compatibility matrix against model requirements.
- **VRAM/RAM/storage estimation** — model footprint estimation across the memory
  hierarchy for given quantization/offload strategies.
- **Model installation** — staged install into a managed store with download
  resume/verification; no installs without explicit operator consent.
- **SHA256 verification** — mandatory content-addressable verification of every
  downloaded/imported artifact; immutable base-artifact store keys. Base models
  are immutable — never modified in place; any change is a new artifact.
- **Derived artifacts + provenance chain** — adapters, trained, merged, and
  quantized outputs are first-class artifacts whose manifest records base model
  SHA256, adapter hashes, training recipe/config digest, dataset identity,
  parent artifact digests, and transformation metadata; verification walks the
  chain to immutable bases (ADR-001).
- **Security scanning** — static scan of artifacts (format validation,
  known-bad pattern/signature checks, provenance metadata) before activation,
  driven by **signed, versioned offline signature bundles** (bundle id, content
  digest, offline refresh channel, signature key pinning). Scan results record
  the bundle version; unknown/unverifiable artifacts fail closed in Sovereign
  Mode (ADR-003).
- **Model benchmarking** — deterministic benchmark harness (latency, throughput,
  memory) publishing results into the registry.
- **Model router** — dispatch requests to the best available model/backend/
  quantization per routing policy (capability, cost, latency, availability).
- **Hot swapping** — swap active model/backend/quantization without losing session
  continuity where the contract permits.
- **Model lifecycle** — states (discovered → verified → scanned → benchmarked →
  active/idle/retired), transitions, activation policy, uninstall.
- **GPU resource scheduler** — allocate GPU memory/slots across models and
  sessions; OOM prevention; preemption policy.
- **Context scheduler** — manage live context windows (sizing, allocation,
  recycling/eviction) per session and model.
- **Model cache** — LRU/ARC-style cache for loaded model layers/weights across
  the memory hierarchy; cooperative sharing of layers in VRAM.
- **Ollama/llama.cpp/vLLM provider abstraction** — three backends implementing
  the Phase 1 provider trait; feature-gated, runtime-selectable.
- **Provisioning mode** — declarative provisioning of models/services to a
  machine profile. Network is *conditionally allowed* purely for explicit,
  permission-gated materialization (model downloads, HF access, security-bundle
  refresh) and is never implicit. Content entering this way must pass
  verification before it can be used in Sovereign Mode (ADR-003).
- **Sovereign execution mode** — hard guarantee: can run with all network
  interfaces blocked; loopback only; network-dependent features disabled.
  Distinct from Provisioning Mode: no downloads, no discovery, no bundle
  refresh occur here. Any artifact whose provenance or scan cannot be verified
  against the installed signed bundles is **refused activation** (operator
  override, if any, is explicit and audited) — never a silent pass (ADR-003).
- **VRAM → RAM → NVMe model hierarchy** — explicit slab placement of weights by
  tier with automatic promotion/demotion.
- **MoE expert streaming** — stream expert weights to/from storage as needed.
- **Expert cache** — cache frequently-used experts in faster tiers, addressed by
  **content digest of the expert shard** (derived from the verified artifact
  manifest), never by name/slot; entries are immutable and eviction deletes
  (ADR-009).
- **Prefetching** — predict and preload experts/layers based on usage patterns.

### 3. Dependencies on previous phases

- The **model provider trait** from Phase 1 is the contract all three backends
  implement; the event bus carries scheduler/router/install events; session system
  binds sessions to scheduled GPU/context allocations; permission system gates
  installs/downloads and network use; persistent state stores registry,
  benchmarks, and cache metadata; CLI exposes new subcommands; the Phase 2
  audit log records lifecycle actions.

### 4. Major milestones

- M3.1: Registry + metadata + local/managed store (offline-capable).
- M3.2: SHA256 verification + provenance manifests + signed-bundle security
  scanning + install pipeline.
- M3.3: HF discovery (opt-in) + Ctrl+P library + import.
- M3.4: Hardware detection + footprint estimation + provisioning mode.
- M3.5: llama.cpp and vLLM backends behind the Phase 1 trait (Ollama from Phase 1).
- M3.6: Benchmarking, router, hot swapping, model lifecycle.
- M3.7: GPU + context schedulers + model cache.
- M3.8: Memory hierarchy, MoE expert streaming, expert cache, prefetching.
- M3.9: Sovereign execution mode (fully offline test) + phase gate.

### 5. Definition of Done

- A model can be discovered, downloaded, SHA256-verified, scanned, benchmarked,
  activated, hot-swapped, and retired entirely through the registry — recorded in
  the audit log.
- Base artifacts are immutable and content-addressed; a derived artifact
  (simulated Phase 4 artifact: adapter/merged/quantized fixture) verifies through
  its provenance chain to its immutable base, and a broken chain is refused.
- All three backends pass the same provider conformance test suite with no
  provider-specific code in callers.
- Sovereign execution mode passes with loopback-only networking (verified by a
  passing network-blocked integration run); Provisioning→Sovereign transition is
  audited, and unverifiable artifacts are refused (fail-closed) in Sovereign Mode.
- GPU scheduler prevents oversubscription (OOM avoided in stress test); context
  scheduler enforces per-session budgets.
- Memory-hierarchy movement, expert streaming, and cache have measurable
  correctness tests (identical outputs regardless of placement); expert-cache
  reads are content-addressed and reject swapped/poisoned entries.

### 6. Tests required

- Conformance: same provider test suite run against all three backends.
- Verification: SHA256 mismatch/truncation/partial-download tests; derived-artifact
  provenance chain tests (valid base→adapter→merge→quantize chain verifies;
  tampered parent, wrong recipe digest, or unrecorded dataset identity fails).
- Security scan: poisoned/embedded data fixtures detected; corrupted archives
  rejected before store write; signed-bundle integrity tests (unsigned/stale
  bundle refused); fail-closed tests in Sovereign Mode for unknown artifacts.
- Router: policy matrix, fallback on model failure, hot-swap mid-session.
- Schedulers: GPU contention, OOM starvation, preemption, context recycling
  under load.
- Hierarchy/expert: output-equivalence when weights live in VRAM vs RAM vs NVMe;
  expert streaming correctness under cache miss and prefetch hit/miss; expert
  cache keyed by content digest (swapped shard invalidates and falls back or
  fails safe).
- Offline: full provisioning+run with network disabled; Provisioning→Sovereign
  transition and audit recorded.

### 7. Security requirements

- Mandatory SHA256 over content-addressed base artifacts; activation refused on
  mismatch. Derived artifacts verify only through their provenance chain
  (ADR-001) — never by direct hash comparison to a base.
- Model artifacts scanned before activation; scanning uses signed, versioned
  offline signature bundles; unsigned/stale bundles or unknown/unverifiable
  artifacts **fail closed** in Sovereign Mode (explicit, audited operator
  override only). No silent pass (ADR-003).
- Install/download/discovery is only valid in Provisioning Mode and is always
  explicit and permission-gated; HF network access is a two-step opt-in
  (feature + per-action). Sovereign Mode performs no network operation (ADR-003).
- Registry/manifests immutable after write; includes provenance, license, and
  scan-bundle-version records.
- No telemetry or usage data may leave the machine in sovereign mode; the mode
  is tested under network denial.
- Security baseline: no secrets, no logs of credentials; lifecycle and mode
  transitions appended to the audit log with tamper-evident chaining (ADR-002).
- Expert streaming/cache must not write model weights into world-readable files;
  cache entries are content-addressed and tied to verified artifact manifests
  (ADR-009).

### 8. What must NOT be implemented yet

- No knowledge pipeline (no embeddings, vector DB, RAG, OCR, VLM inference).
- No model training or adaptation (no SFT/LoRA/QLoRA/DPO, no datasets/eval).
- No memory subsystem or self-improvement.
- No TUI/Web UI beyond CLI.
- No autonomous/distributed execution or model federation.
- No trading subsystem.
- No new terminal/computer-use capabilities beyond what Phase 2 delivered; the
  Phase 2 computer-use loop remains available unchanged.

---

## PHASE 4 — MULTIMODAL + KNOWLEDGE + MODEL ADAPTATION

Give OpenRT the ability to understand documents/media, retrieve knowledge with
evidence, and adapt models to the operator's own data.

### 1. Objective

Add a multimodal ingestion/understanding pipeline (documents, scanned pages, OCR,
handwriting, engineering drawings, images, audio), a local knowledge layer
(embeddings → vector DB → retrieval → RAG with citations and an evidence graph,
plus an optional knowledge graph), and a model-adaptation kit (SFT, LoRA/QLoRA,
DPO, datasets, eval, adapters, quantization) integrated with the Phase 3 model
registry.

### 2. Components

- **PDF/document ingestion** — parse and ingest PDFs and common document formats
  with structure/schema extraction.
- **Scanned document processing** — pipeline for page images → text/document
  structure.
- **OCR** — local OCR engine for printed text.
- **Handwriting** — handwriting recognition (local model, supporting context/
  strokes when available).
- **Engineering drawings** — ingestion/parsing of technical drawings (vector
  layers, annotations, symbols) into structured records.
- **Images** — image ingestion, metadata, captioning/understanding via local VLM.
- **Audio** — audio ingestion (transcription and/or understanding via local models).
- **VLM pipeline** — generalized visual-language inference pipeline (images,
  documents, screens) as a first-class model capability.
- **Embeddings** — local embedding models behind a provider trait (Phase 1 style).
- **Vector database** — local vector store (built-in or provider-abstracted, no
  remote default).
- **Retrieval** — retrieval API: hybrid (dense + lexical), filtering, chunking.
- **Reranking** — local reranking model for retrieval quality.
- **Local RAG** — retrieval-augmented generation over ingested corpora; fully local.
- **Citations** — every RAG answer carries source citations, provenance, offsets.
- **Evidence graph** — graph of claims ↔ source passages ↔ documents (grounding
  and auditability of answers).
- **Knowledge graph** — optional entity/relation graph on top of the evidence
  graph; share the same underlying store (evolves, does not duplicate).
- **SFT** — supervised fine-tuning pipeline (local).
- **LoRA** — low-rank adaptation training.
- **QLoRA** — quantized LoRA for constrained hardware.
- **DPO** — direct preference optimization training.
- **Dataset management** — dataset ingestion, curation, versioning, splits,
  privacy handling.
- **Evaluation** — eval harness for adapted vs base models (fixed benchmarks +
  operator-defined).
- **Adapter management** — adapter registry, merge/unmerge, hot-attach to models;
  every adapter is a derived artifact with a provenance manifest (base model
  SHA256, training recipe/config digest, dataset identity, parent digests,
  transformation metadata) per ADR-001.
- **Quantization** — quantize models/adapters (base + after adaptation) as a
  transformation that produces **derived artifacts** (new provenance-manifest
  entries; the base artifact is never modified), integrated with the Phase 3
  memory hierarchy.
- **Model registry integration** — trained/adapted/quantized models and adapters
  flow back into the Phase 3 registry as first-class, **provenance-verified**
  derived artifacts; the chain is written at training/conversion time, not
  retroactively (ADR-001).

### 3. Dependencies on previous phases

- Phase 1 traits underlay the embedding/VLM providers and the permission-gated
  ingestion paths; the event bus carries pipeline jobs; persistent state and
  session system hold documents/datasets/artifacts.
- Phase 3 registry is where adapter/quantized models are registered, verified
  (SHA256), and lifecycle-managed; the GPU/context schedulers budget training and
  inference; the memory hierarchy/quantization is the substrate for QLoRA and
  quantized deployment; security scanning applies to datasets too.
- Phase 2 vision grounding feeds (and is upgraded by) the Phase 4 VLM pipeline —
  Phase 4 does NOT re-create grounding; it generalizes the same pathway.

### 4. Major milestones

- M4.1: Document/media ingestion (PDF, scanned, OCR, images, audio).
- M4.2: VLM pipeline; handwriting and engineering-drawing specialty routes.
- M4.3: Embeddings + vector DB + retrieval + reranking (local RAG v1).
- M4.4: Citations + evidence graph (+ optional knowledge graph on the same store).
- M4.5: Dataset management + SFT/LoRA/QLoRA/DPO (feature-gated training).
- M4.6: Evaluation + adapter management + quantization.
- M4.7: Model registry integration (adapted models/adapters as verifiable
  artifacts) + phase gate.

### 5. Definition of Done

- A mixed corpus (PDFs, scans, drawings, audio transcripts) is ingested and
  answerable via local RAG where every answer carries resolvable citations with
  evidence-graph provenance.
- OCR/handwriting/drawing accuracy meets internally set thresholds on a
  designated fixture set (thresholds recorded in the eval harness).
- An SFT/LoRA/QLoRA/DPO training run completes on the operator's local hardware,
  the adapter hot-attaches to a Phase 3 model, and base-vs-adapted eval results
  are recorded in the registry.
- All adapted/quantized artifacts are **derived artifacts** with complete
  provenance manifests (base SHA256, recipe/config digest, dataset identity,
  parent digests, transformation metadata) that verify through the Phase 3
  provenance chain (ADR-001).
- Every pipeline step is permission-gated and auditable; nothing ingests or
  trains without explicit consent on specific data.

### 6. Tests required

- Ingestion: format fixtures (PDF variants, scans, drawings, audio); malformed/
  hostile inputs (zip bombs, malformed PDFs) rejected with bounded resource use.
- OCR/handwriting/drawing accuracy benchmarks run in CI on fixture sets.
- RAG: retrieval correctness (recall@k on a labeled set), reranking lift,
  citation integrity (every cited span actually present in its source).
- Vector DB: persist/reload, rebuild-from-evidence-graph, corruption handling.
- Training: SFT/LoRA/QLoRA/DPO smoke+integration runs on small fixtures; eval
  (base vs adapted) recorded; adapter attach/detach/merge.
- Provenance: every training/quantization run emits a provenance manifest that
  round-trips through the Phase 3 registry and re-verifies; a manifest with a
  wrong dataset identity or recipe digest is rejected (ADR-001).
- Quantization: parity checks of quantized outputs; size/memory assertions.
- Security: dataset/artifact scanning (Phase 3) extended to training corpora.

### 7. Security requirements

- Ingestion sandboxing: every ingest path runs bounded (memory/CPU/timeouts);
  hostile document payloads cannot escape parsing.
- Documents, datasets, and tuned artifacts are private by default; permission
  gates on corpus/dataset creation, training, and adapter export.
- No training/eval data or embeddings may egress the machine except explicit,
  per-operation operator consent.
- Every training/adaptation/quantization run records its inputs (dataset digest,
  base artifact, recipe/config digest) into the provenance manifest and the
  audit log before the derived artifact may activate (ADR-001, ADR-002).
- Datasets scanned before training (Phase 3 scanning extended).
- Attribution/evidence integrity: evidence graph and citations are append-only;
  tampering detected.
- A negotiator requirement: any source whose license prohibits local/shared
  adaptation is surfaced during dataset/model import, not after training.

### 8. What must NOT be implemented yet

- No memory subsystem (working/episodic/semantic/workflow memory) — retrieval here
  is per-corpus RAG, not cross-session memory; Phase 4 stays out of session memory.
- No skills/instincts/hooks/rules learning or self-improvement.
- No TUI/Web UI.
- No autonomous agents, distributed workers, or federation.
- No trading subsystem.

---

## PHASE 5 — AI WORKBENCH + MEMORY + SELF-IMPROVEMENT

Turn the runtime into a workbench with rich UIs, a persistent workspace with full
execution history (replay/fork/time-machine), multi-modal memory, and frameworks
for skills/instincts/hooks/rules plus self-debugging and self-optimization loops.

### 1. Objective

Deliver operator-facing workbench surfaces (TUI, Web UI, command palette), a
persistent session/execution workspace with full history, replay, fork, and a
"workflow time machine", a layered memory system (working/episodic/semantic/
workflow), and the frameworks that let OpenRT encode skills, instincts, hooks,
rules, an agent genome, and self-improvement loops (self-debugging,
self-optimization, research/review, telemetry, learning from execution history).

### 2. Components

- **TUI** — terminal user interface over the core runtime.
- **Web UI** — local web interface (served on localhost; fully optional).
- **Command palette** — fast command/action entry surface in both UIs.
- **Sessions** — first-class session UI: create, switch, inspect, suspend/resume.
- **Persistent workspace** — durable workspace model: state, files, runs, config
  all in one coherent, versioned layout. The workspace is an **overlay/
  consolidation layer** over the existing foundation-store namespaces (sessions,
  execution, registry, memory, assets) — it adds versioning, timestamps, and
  cross-namespace indexes but never copies or duplicates records (ADR-006).
- **Execution history** — append-only, replayable record of every run/action.
- **Replay** — step-through execution of a recorded run.
- **Fork** — branch a recorded run into a divergent execution.
- **Workflow time machine** — time-navigation over sessions/history: inspect any
  point, fork from it, rewind.
- **Working memory** — per-session short-term state available to agents.
- **Episodic memory** — records of past runs/events for later recall and learning.
- **Semantic memory** — facts/knowledge accumulated and retrievable (builds on
  Phase 4 retrieval).
- **Workflow memory** — reusable patterns of successful workflows.
- **Skills** — packaged, reusable capabilities (prompt+tool+workflow bundles) the
  runtime can invoke.
- **Instincts** — lightweight learned behavioral patterns/heuristics.
- **Hooks** — user/operator-defined event hooks into runtime events.
- **Rules** — declarative constraint/guardrail sets that govern agent behavior.
- **Agent genome** — the full, serializable specification of an agent's identity/
  behavior (prompt, tools, rules, instincts, skills, memory profile) as a
  versionable artifact.
- **Skill generation** — generate skills from successful runs/patterns.
- **Reusable workflows** — workflows promoted to reusable, parameterized assets.
- **Self-debugging** — runtime detects its own failures, reproduces, diagnoses.
- **Self-optimization** — runtime proposes/validates improvements (routing,
  tools, plans) against recorded baselines.
- **Research/review loops** — closed loops that research an issue and review
  proposed changes before applying them (with operator review gates).
- **Telemetry** — *local-only* instrumentation of runtime behavior: the single
  metrics/events pipeline (over the P1 event bus + structured logging), with a
  versioned metric/event schema. Phase 6 observability extends this subsystem —
  it does not create a parallel architecture (ADR-005).
- **Learning from execution history** — derive instincts/skills/workflow memory
  from recorded history (local inference only).

### 3. Dependencies on previous phases

- Phase 4 retrieval/evidence graph powers semantic memory and citation-grounded
  recall; Phase 4 evaluation harness is the yardstick for self-optimization.
- Phase 2 checkpoints/rollback/DAG are the substrate the time machine and
  replay/fork operate on; Phase 3 router/model lifecycle is what
  self-optimization tunes.
- Phase 1 event bus, session system, persistent state, permission system, and
  CLI are the skeletons of the workbench.
- Phase 6 (autonomous) is explicitly NOT pulled forward: any self-improvement
  loop here runs interleaved with operator approval, not unattended.

### 4. Major milestones

- M5.1: Persistent workspace + session UX in TUI.
- M5.2: Execution history, replay, fork, and workflow time machine.
- M5.3: Memory layer (working, episodic, semantic over Phase 4 retrieval,
  workflow memory).
- M5.4: Skills, instincts, hooks, rules, agent genome.
- M5.5: Skill generation + reusable workflows.
- M5.6: Self-debugging and self-optimization research/review loops (gated).
- M5.7: Local telemetry + learning from execution history.
- M5.8: Web UI + command palette.
- M5.9: Phase gate (self-improvement only behind operator-enabled policies).

### 5. Definition of Done

- A non-trivial multi-step task completed in the TUI is fully replayable; a run
  can be forked at any checkpoint and diverge cleanly into a new session.
- The time machine can restore view/state at any recorded point without corrupting
  later history.
- Memory layers answer recall queries with citations to their origin runs/sources.
- Instincts/skills/workflow memory are demonstrably derived from execution
  history and are exportable/importable as artifacts.
- Self-optimization proposes a change against a recorded baseline, and an eval
  (Phase 4 harness) must show the improvement — applied only through operator
  approval (never silently).
- No self-improvement feature can modify its own policy without operator consent.
- Web UI binds to localhost only by default; both UIs speak to the same core.

### 6. Tests required

- History: replay fidelity (byte-identical trace for deterministic fixture);
  fork isolation; time-machine restore integrity.
- Memory: retrieval with citation integrity; episodic/semantic recall accuracy on
  fixture sessions; conflict resolution when episodic facts are overridden.
- Skills/instincts: extraction correctness, packaging round-trip (export/import),
  versioned rules/hooks execution.
- Genome: artifact round-trip, versioning, application to a fresh session.
- Self-optimization: a regression test where "optimization" that does not beat the
  baseline must be rejected; approval-gate enforced in UI and headless mode.
- TUI/Web: integration tests against the core (no pixel tests required; functional
  flows only).

### 7. Security requirements

- Memory is private by default and scoped per operator identity; cross-user memory
  is out of scope.
- Execution history and memory must never capture secrets (redaction enforced at
  write time, verified by tests).
- Skills/instincts/hooks/rules and genomes are **executable content** — treated as
  untrusted imports: signature/checksum verification (Phase 3 machinery), opt-in
  import, and sandboxed dry-run before activation.
- Self-optimization cannot change permission policies, security scanning, or
  model verification metadata; those remain operator-only.
- Hooks execute locally with the same permission checks as tools.
- Telemetry is local-only; any sharing is an explicit, documented, opt-in release
  gate (blocked in sovereign mode).
- The workbench surfaces never render or log unredacted secrets.

### 8. What must NOT be implemented yet

- No long-running unattended/autonomous agents or distributed workers.
- No model federation or migration across machines.
- No trading subsystem.
- No production hardening (audit system, encryption-at-rest of secrets, network
  isolation enforcement, supply-chain gates) — those land in Phase 6 (the Phase 2
  audit log continues to accumulate).

---

## PHASE 6 — AUTONOMOUS + PRODUCTION SOVEREIGN RUNTIME

Make OpenRT an unattended, hardened, production-grade sovereign runtime — and, as
an explicit optional domain, an autonomous trading subsystem with strict safety
rails.

### 1. Objective

Harden OpenRT for production: long-running and autonomous workflows, distributed
workers with model federation/migration, a production sandbox, a security policy
engine, audit, encrypted secrets, network isolation, air-gapped operation,
supply-chain verification, and production observability. Then deliver an
**optional, high-risk** autonomous trading subsystem gated by hard risk limits
and a kill switch, progressing strictly backtest → paper → controlled live.

### 2. Components

- **Long-running agents** — agents that persist across sessions in supervised or
  opt-in unsupervised modes.
- **Autonomous workflows** — workflows that run unattended under explicit,
  high-assurance policies (budgets, stop conditions, approval mats).
- **Distributed workers** — multiple machines/nodes running OpenRT workers that
  share work over a secure channel **inside an operator-owned enclave** (the
  operator's own nodes on a private network). Distributed execution is an
  enclave-internal capability only; it implies no internet/egress, and Sovereign
  Mode denies any non-enclave contact. Nodes authenticate mutually (operator-
  generated, per-node identity), and rogue/anomalous nodes are quarantined
  (ADR-004).
- **Model federation** — cooperative model serving/replication **across enclave
  nodes only**; never third-party/cloud inference by default; replica exchanges
  are verified (ADR-001, ADR-004).
- **Model migration** — move/transfer models between enclave nodes with
  end-to-end verification (SHA256 + provenance carry-through).
- **Production sandbox** — hardened execution environment for untrusted/autonomous
  work (stronger than Phase 2 sandbox: selinux/landlock-style confinement).
- **Security policy engine** — declarative, versioned security policy DSL enforced
  at every subsystem boundary; operator-only changes.
- **Audit system** — complete, tamper-evident audit trail built on the Phase 2
  chained envelope **without changing its fundamental format or entry
  serialization**: strengthens verification (stronger signatures, centralized
  retention, key management) and adds aggregation/search only via additive
  fields (ADR-002).
- **Encrypted secrets** — secrets at rest (keyring/TPM-backed where available) and
  in transit; no plaintext secret persistence.
- **Network isolation** — per-role/per-workflow network namespaces/policies;
  default-deny egress.
- **Air-gapped operation** — full offline operation incl. model/library/
  verification, using the Phase 3 **signed, versioned offline security/
  signature bundles** (bundled, signature-key pinned) and offline provenance
  verification; no network canaries (ADR-003).
- **Supply-chain verification** — provenance signatures, SBOM, pinning, and
  repro verification for models, code, dependencies, and updates.
- **Production observability** — metrics, health, dashboards that **extend the
  Phase 5 telemetry subsystem**: adds retention policy, aggregations, health
  surfaces, and optional piped export adapters over the same single pipeline
  and schema — it is not a parallel telemetry architecture. Egress is opt-in
  and blocked in Sovereign Mode (ADR-005).
- **Autonomous trading subsystem** *(explicitly optional, high-risk)*:
  - **Market-data agents** — ingest market data (offline/market-approved sources
    only).
  - **Technical/fundamental/macro analysis** — analysis agents per discipline.
  - **Bull/bear debate** — adversarial thesis/debate between directional agents.
  - **Trader agent** — action selection.
  - **Risk engine** — position, exposure, drawdown, and bandit-armed limits.
  - **Portfolio manager** — allocation across allowed instruments/policies.
  - **Backtesting** — historical simulation with realistic costs/slippage.
  - **Paper trading** — live data, simulated fills, no capital.
  - **Controlled live execution** — real execution only under escalating,
    operator-managed permission and limits, per-instrument.
  - **Wallet/broker connectors** — adapter-based, per-broker, permissioned.
  - **Hard risk limits** — enforced at the connector layer, not advisory.
  - **Kill switch (trading)** — a mechanism adapter wired to the common runtime
    safety/halt signal (scope=trading): immediate halt of execution and
    order cancellation/flattening per configured policy. It remains a separate
    mechanism from the computer-use kill switch and never performs computer-use
    halts or vice versa (ADR-008).

### 3. Dependencies on previous phases

- Everything: Phase 1 traits/session/event/permissions; Phase 2 DAG/sandbox/
  audit-log/checkpoints and the common runtime safety/halt signal (ADR-008);
  Phase 3 registry/verification/sovereign+provisioning modes (ADR-001, ADR-003);
  Phase 4 retrieval/eval (analysis + signals); Phase 5 skills/memory/
  self-optimization and local telemetry (extended by Phase 6 observability,
  ADR-005), plus the Phase 2 audit log feeding the Phase 6 audit system
  (ADR-002).
- The trading subsystem stands on: Phase 1 permission system (hard-gated),
  Phase 4 eval (backtest validity), Phase 5 history/time machine (post-mortems),
  and Phase 6's sandbox/risk/audit (execution rails).

### 4. Major milestones

- M6.1: Long-running agents + autonomous workflows with policy gates.
- M6.2: Hardened production sandbox + security policy engine + audit system.
- M6.3: Encrypted secrets + network isolation + air-gapped operation +
  supply-chain verification.
- M6.4: Distributed workers + model federation + model migration + observability.
- M6.5: Trading mark 0 — scaffolding, data, connectors (no execution).
- M6.6: Trading mark 1 — analysis agents, bull/bear debate, trader agent, risk
  engine, portfolio manager.
- M6.7: Trading mark 2 — backtesting + paper trading gate.
- M6.8: Trading mark 3 — controlled live execution with hard risk limits and
  kill switch (staged enablement).
- M6.9: Final production/security audit + phase gate.

### 5. Definition of Done

- Autonomous workflows run to completion unattended with all stop conditions,
  budgets, and approval mats honored in integration tests.
- Audit system is tamper-evident (chained), searchable, and centralizes the
  Phase 2 log; no security-relevant action happens unrecorded.
- Air-gapped run (no network interface) passes production checks end-to-end,
  including offline model/library verification.
- Secrets never exist in plaintext on disk; keyring/TPM path used where available.
- Distributed worker/federation tests pass with mutated/rogue-node detection
  (node identity and artifact verification enforced) inside the enclave; a node
  that attempts non-enclave contact is quarantined (ADR-004).
- Observability is verified as an extension of Phase 5 telemetry: the same
  pipeline/schema, no duplicate counters (ADR-005).
- **Trading:** backtest engine validated against a known dataset (deterministic,
  no look-ahead); paper trading runs N days with zero unpermitted orders; live
  execution is reachable only through a documented staged enablement with hard
  limits, kill-switch tests (order cancellation within SLA), and full audit.

### 6. Tests required

- Autonomous: stop-condition enforcement (budget, time, error), crash recovery
  and restart semantics, supervisor takeovers.
- Distributed: worker failure/rejoin, partition tolerance, message integrity,
  identity spoofing rejected, artifact tamper rejection, non-enclave contact
  quarantined (ADR-004).
- Air-gap: full suite runs with networking disabled.
- Secrets: at-rest encryption, key rotation, no plaintext leakage in logs/memory
  dumps (best-effort checks).
- Audit: tamper detection tests (any modification to the chain is detected).
- Observability: metrics reflect simulation of load; health signals fire.
- **Trading-specific:**
  - Backtest correctness (no look-ahead, realistic fills) — regression suite.
  - Hard risk limit enforcement at the connector layer (adversarial limit
    breaches rejected in tests).
  - Kill-switch (trading): halt + order-cancellation/flatten within SLA under
    load; scoped HALT(computer) must NOT cancel orders and HALT(trading) must
    NOT halt computer actions (ADR-008).
  - Paper→live promotion cannot occur without explicit config + attestation.
  - Connector adapters are permissioned, sandboxed, and their failures leave no
    partial state.

### 7. Security requirements

- Default-deny network egress per worker/workflow; network isolation enforced at
  the OS level where available.
- Air-gapped mode is a first-class tested mode, not a configuration hope:
  signing/verification supplies are bundled.
- Supply chain: models, crates, binaries, configs, and updates are
  pin-verified with provenance; broken-verification fails closed.
- Encrypted secrets at rest and in transit; operator-only rotation.
- The security policy engine is append-only and operator-only; runtime agents
  cannot modify it (tested).
- **Trading safety rails (absolute):**
  - Hard risk limits enforced in the connector layer (position, exposure,
    drawdown, per-instrument cap) with fail-closed behavior.
  - Kill switch (trading, scope=trading) halts execution and cancels/flattens
    within SLA per configured policy; it is a separate mechanism from the
    computer-use kill switch (scope=computer), connected only via the common
    runtime safety/halt signal; neither performs the other's actions
    (ADR-008).
  - Live capital requires explicit operator enablement per instrument, with
    attestation; no auto-promotion from paper.
  - Broker/wallet credentials only via the encrypted secrets subsystem; no
    plaintext anywhere.
  - Every order lifecycle event is audited with tamper-evidence.
  - A losing-connection / data-feed-failure policy (halt, don't guess).

### 8. What must NOT be implemented yet

- Nothing is explicitly deferred beyond this phase for the components listed;
  however, **optionality is preserved**: the trading subsystem may be scoped out
  of a release without blocking the rest of Phase 6.
- Post-Phase-6 items (out of roadmap scope): third-party cloud inference,
  cross-operator federation, SaaS control planes, margined or leveraged
  instruments, anything not operator-owned is explicitly **out of scope** for
  OpenRT's sovereignty model.

---

## Cross-Cutting Commitments

- **Phase gates:** each phase ends with a gate (DoD audit) before the next may
  start. Tentative phases may be re-planned only by a documented revision of this
  document.
- **One model, multiple front-ends:** UIs (CLI/TUI/Web) are adapters over the same
  core; no feature may exist only inside a UI.
- **Everything is testable:** features not covered by the phase's Tests Required
  are not "done".
- **Local-first axiom:** default behavior requires zero network. Any network
  feature is separately feature-gated and permission-gated.
- **Self-hosted verification:** model/artifact verification (SHA256, provenance)
  is a runtime feature from Phase 3 onward and never an external service.
- **Naming namespaces (ADR-007):** overloaded terms are name-spaced at the crate
  level — `context` (model context only), `memory` (persistence layers),
  `session` (lifecycle), `checkpoint` (execution snapshots), `registry` (model/
  artifact registry), `cache` (memory-hierarchy caches). New subsystems must be
  allocated a crate + storage namespace in `OWNERSHIP.md` before implementation.
- **One storage backbone (ADR-006):** all durable data lives in the foundation
  store manager's typed namespaces; no phase introduces a parallel store, and
  the P5 workspace is a consolidation overlay, never a duplicate.
- **Two operating modes (ADR-003):** Provisioning Mode (conditionally networked,
  for explicit materialization) vs Sovereign Execution Mode (network-blocked,
  fail-closed on unverifiable artifacts). Mode transitions are audited.

---

## Revision History

| Version | Date | Change |
|---|---|---|
| 0.1 | 2026-09-17 | Initial 6-phase roadmap (planning only, no code). |
| 0.2 | 2026-09-17 | Applied ADR-001…009: model artifact provenance (base/derived), chained audit-log envelope from Phase 2, signed offline scanning bundles + Provisioning/Sovereign modes, enclave-scoped distributed execution, observability extends telemetry, storage ownership (ADR-006), naming namespaces (ADR-007), dual kill switches on a common safety signal (ADR-008), content-addressed expert cache (ADR-009). Reference OWNERSHIP.md. |
| 0.3 | 2026-09-24 | **Phase 7 — Tool Fabric** added as a documented revision (was out-of-scope for the original 6-phase doc; scope drawn from `project-complete-plan.txt` Phase 7 Tools/Tool Fabric). Priority: cross-platform computer control (environment adapters, capability discovery, backend resolution) over premature Office/MCP breadth. Reconciles with Phase 2 computer-use ownership: Phase 7 builds the platform abstraction layer the Phase 2 tools sit on; no second tool registry/gate. See `docs/architecture/phase7/` and OWNERSHIP Phase 7. |
| 0.4 | 2026-09-24 | **Phase 7 split into 7A/7B; Phase 7B — Remaining Tool Fabric added.** 7A (M7.1–M7.10) delivered the platform layer and is frozen (GREEN). 7B adds M7B.1–M7B.10: Tool Fabric core metadata/discovery/error model, browser automation (real backend), MCP integration (stdio client, fail-closed import), database tools, application tools, Excel/PPT/DOCX automation, cross-tool orchestration, final 7B gate. §8's deferral of "full MCP client/server, database tools, Excel/PPT/Word automation" is **lifted for 7B** (it applied to the 7A pass). Same hard constraints: one ToolRegistry/one GateExecutor/one policy, real backends only, honest platform matrix, deny-by-default for imported MCP tools, high-risk ops (raw SQL, uploads, external network, office overwrite) stay behind the existing approval path. |
| 0.5 | 2026-09-25 | **Phase 8 — Sandbox + Isolated Execution Fabric added as a documented revision** (scope drawn from `project-complete-plan.txt` "PHASE 8 — Sandbox: container execution, resource limits, network restrictions, filesystem isolation, timeouts, audit — must be completed before allowing autonomous tool use"). Phases 5/6/7 (7A frozen, 7B GREEN) are not rebuilt: Phase 8 extends the existing `openrt-sandbox`/`ResourceGate`/`netpol`/PathJail/GateExecutor into one unified Sandbox abstraction with a real admission flow (Permission Policy → Sovereign Mode → Sandbox Admission → Resource Admission → Isolated Execution → Verification → Audit), honest backend capability reporting (bwrap / docker / systemd-nspawn / seccomp detected at runtime — no fake containers, no silent downgrade), execution-level network modes, disposable lifecycle with verified cleanup, escape-testing classification, and risk-driven sandbox requirements for autonomous tool use. Milestones `M8.1`–`M8.10` follow the `M<phase>.<n>` convention. See `docs/architecture/phase8/` and OWNERSHIP Phase 8. |
| 0.6 | 2026-09-25 | **Phase 9 — Multimodal Intelligence Fabric added as a documented revision** (consolidates `project-complete-plan.txt` Phases 9/10/11 — OCR+multimodal, semantic RAG, advanced verification — into one pipeline phase: ingest → retrieve → ground → verify). Phases 1–8 (8A frozen, 8 GREEN) are not rebuilt: Phase 9 extends the Phase 4 deterministic foundations (`openrt-ingest`, `openrt-knowledge`, `openrt-embeddings`, `openrt-vectorindex`) with the missing **real** backends — Tesseract OCR + pdftoppm page rendering + layout analysis, image/VLM pipeline (`openrt-vlm`), audio/video (ffprobe/ffmpeg, ASR), embedding providers behind the existing trait, hybrid BM25+dense retrieval with reranking (`openrt-retrieval`), append-only evidence/knowledge graph (`openrt-evidence`), answer verification + hallucination firewall + honest cross-model status, and prompt-injection defense over UNTRUSTED evidence. Hard constraints: capability honesty (structured UNSUPPORTED/CORRUPT/AMBIGUOUS/UNAVAILABLE/MODEL_MISSING — no fake AI in production paths), provenance (page/bbox/timestamp/extractor/model/version/confidence) survives chunking→citation, TEST 1 deterministic RAG regression stays green, no second retrieval/citation/gate/audit authority, Sovereign Mode stays fail-closed. Milestones `M9.1`–`M9.10`. See `docs/architecture/phase9/` and OWNERSHIP Phase 9. |

---

## PHASE 7 — TOOL FABRIC

Cross-platform tool fabric whose first and highest-priority subsystem is
**computer control** that works across Windows, Linux (X11 and Wayland:
Hyprland, Sway, i3, bspwm, KDE, GNOME, and other compositors discovered at
runtime) without assuming any single display server, desktop environment, or
universal keybinding model.

> **Reconciliation (2026-09-24):** The original roadmap defined six phases;
> Phase 2 already delivered the basic computer-use tools (`screen.*`,
> `input.*`, `browser.*`, filesystem/terminal/python) over an X11-leaning
> `ScreenBackend`. Phase 7 does **not** rebuild those tools or introduce a
> second tool registry, gate, or permission system. It adds the missing
> platform-abstraction layer: environment discovery, capability negotiation,
> compositor/window-manager adapters, and a stable high-level ComputerControl
> API that the existing (and later) tools call. Milestone ids `M7.1`–`M7.10`
> follow the `M<phase>.<n>` convention of phases 1–6.
>
> **Phase 7A / 7B split (2026-09-24, v0.4):** Phase 7 ships as two passes.
> **7A** (M7.1–M7.10, complete and frozen) delivered the platform layer —
> `EnvironmentProfile`, `ComputerBackend`, `ComputerControl`, `computer.*`
> tools, acceptance docs. **7B** delivers the *remaining tool fabric*:
> browser automation (M7.8 enlarged), MCP integration (M7.9 enlarged),
> database tools, application tools, Excel/PPT/DOCX, cross-tool
> orchestration, and a final gate. 7B milestones are numbered `M7B.1`–`M7B.10`
> (per the 7B mission) and reconciled to the `M<phase>.<n>` convention here:
> M7B.1≡M7.1 (core metadata/registration extension), M7B.2≡M7.8 (browser),
> M7B.3≡M7.9 (MCP), M7B.4–M7B.8 are new (database / application / Excel /
> PPT / DOCX — §8's earlier deferral is lifted by this revision, documented in
> the v0.4 history row), M7B.9–M7B.10 are new (cross-tool orchestration +
> final 7B gate). Phase 7A scope and docs remain authoritative for 7A.

### 1. Objective

Give the agent a stable, intent-level computer-control surface
(`open_application`, `focus_window`, `capture_screen`, …) that discovers the
actual session (display server, compositor, desktop, tools, permissions) and
routes each operation through a deterministic backend priority — preferring
native compositor/platform APIs over accessibility, portals, display-server
helpers, input injection, and finally configured-shortcut fallbacks. The agent
never thinks in raw `Super+D` / `xdotool` / `hyprctl` terms.

### 2. Components

- **EnvironmentProfile** — dynamic discovery of platform, session type,
  desktop/WM/compositor, display server, input/window/capture/clipboard/
  accessibility capabilities, available backends, configured shortcuts and
  launchers, and security constraints. Never hardcodes one user's config.
- **Configuration intelligence** — scanners for Hyprland/Sway/i3/bspwm(+sxhkd)
  configs that extract keybindings and launch commands as *environment
  knowledge* (fallback metadata), not as core primitives.
- **ComputerControl API** — one high-level, backend-agnostic tool surface for
  screen, windows, input, clipboard, applications, observation, and
  verification. No raw backend commands as the primary public API.
- **BackendResolver + ComputerBackend** — capability-based backend contract
  with structured `SUPPORTED` / `UNSUPPORTED` / `PERMISSION_REQUIRED` /
  `UNAVAILABLE` diagnostics and deterministic priority:
  native compositor → accessibility → portal/protocol → display server →
  input injection → configured shortcut.
- **WM/compositor adapters** — Hyprland, Sway, i3, bspwm, KDE, GNOME (DBus/
  portal/a11y), plus generic X11 and Wayland backends. Only real mechanisms;
  no fake backends.
- **Windows backend** — first-class Win32/UIA/native input path, with window
  management / accessibility / input / capture / clipboard as independent
  capabilities.
- **Observation → action → verification** — every action supports observe,
  act, and verify (window tree, active window, pointer, application state,
  accessibility tree where available).
- **Tool Fabric capability registry** — computer capabilities declare name,
  category, platform/session support, permission, risk, required backend, and
  observation/verification support through the *existing* `ToolSpec` /
  capability system (no duplicate registry).
- **Application discovery** — desktop entries, PATH, configured launchers,
  running processes/windows; no hardcoded universal app names.
- **Window identity** — stable `WindowRef` (compositor id / X11 id / HWND /
  PID / app_id + title as metadata) that survives re-identification.
- **Screen capture + accessibility** — observation abstraction with
  resolution/monitor/scale/timestamp/backend metadata; a11y tree preferred
  over raw coordinates when supported.
- **Browser/MCP foundations (7A)** — design hooks only in the 7A pass:
  browser automation and MCP tools register through the same capability
  mechanism; no duplicate registry (7B implements the real backends).
- **Browser automation (7B)** — structured `browser.*` tools (open, navigate,
  tabs, find, interact, screenshot, download/upload, evaluate) behind a real
  backend (Firefox BiDi/WebDriver over localhost) with honest
  UNAVAILABLE/PARTIAL classification when no backend exists; no fake GREEN.
- **MCP integration (7B)** — stdio MCP client with discovery, schema
  validation, trust/capability mapping, fail-closed import: imported tools
  register on the *existing* ToolRegistry with `mcp.*` caps, never bypassing
  the gate.
- **Database tools (7B)** — `database.*` tools (connect, list_tables,
  describe_table, query, insert/update/delete, transaction) over structured
  DB clients (SQLite locally, PostgreSQL when configured); raw SQL is
  high-risk, audited, never a shell passthrough.
- **Application tools (7B)** — application-adapter framework (identity,
  platform support, capabilities, permissions, backend, verification)
  exposing `application.*` tools on top of ComputerControl + desktop-entry
  discovery.
- **Excel/PPT/DOCX automation (7B)** — `excel.*`/`ppt.*`/`docx.*` tools
  producing real OOXML artifacts with provenance + digest verification;
  render/export through real document tooling where available, honest
  UNAVAILABLE otherwise. Never GUI macros.
- **Cross-tool orchestration (7B)** — multi-step workflows composing
  browser/database/office/artifact tools through the existing workflow
  engine and gate, with artifact provenance validated between steps.

### 3. Dependencies on previous phases

- Phase 1: tool registry, `ToolSpec`/`Tool`, permission gate, event bus.
- Phase 2: existing computer-use tools and kill switch (ADR-008); Phase 7
  layers platform adapters *under* them rather than replacing them.
- Phase 6: production gate `required_caps` enforcement, Sovereign fail-closed
  defaults, audit chain for computer-use decisions.

### 4. Major milestones

- M7.1: Tool Fabric core — capability metadata model + registration through
  the existing tool/capability registry.
- M7.2: Environment/capability discovery — `EnvironmentProfile` construction
  from live session inspection (env vars, processes, config dirs).
- M7.3: Computer Control abstraction — high-level API + `ComputerBackend`
  contract + `BackendResolver` priority order.
- M7.4: Linux X11/Wayland support — real X11 backend; Wayland treated as
  first-class (no xdotool/XTest assumption); structured unsupported/
  permission-required diagnostics.
- M7.5: WM/compositor adapters — Hyprland, Sway, i3, bspwm (+sxhkd), KDE,
  GNOME adapters with config-driven knowledge (keybindings as fallback only).
- M7.6: Windows support — Win32/UIA/native input/clipboard/capture path with
  independent capability failure; no WSL/PowerShell-only assumption.
- M7.7: Observation + accessibility — screen observation metadata, window
  tree, active window, a11y tree where available; verify_* operations.
- M7.8: Browser/tool integration foundations — browser UI control behind the
  same fabric when structured APIs are unavailable.
- M7.9: MCP/tool registry integration — capability metadata shaped so future
  MCP tools register through the same mechanism (no second registry).
- M7.10: Final Tool Fabric gate (7A) — acceptance, environment test matrix,
  forensic audit, docs (`PHASE7_COMPUTER_CONTROL.md`, `PHASE7_ACCEPTANCE.md`).

**Phase 7B milestones** (`M7B.*`, see the 7A/7B reconciliation note above):

- M7B.1: Tool Fabric core — authoritative tool metadata (category, risk,
  platform support, permissions, verification, side effects) + discovery
  (`tools.list`), normalized error model (UNAVAILABLE / PERMISSION_DENIED /
  POLICY_DENIED / RESOURCE_DENIED / INVALID_INPUT / NOT_FOUND / TIMEOUT /
  BACKEND_FAILURE / VERIFICATION_FAILED / INTEGRITY_FAILURE with WHAT/WHY/
  WHERE/FIX), reusing the existing ToolRegistry/GateExecutor.
- M7B.2: Browser automation — real browser backend (Firefox BiDi over
  localhost) + `browser.*` tools (open/navigate/back/forward/reload/close/
  tabs/get_page/find/click/type/select/submit/download/upload/screenshot/
  evaluate/wait) behind capability + policy gates; observation/verification.
- M7B.3: MCP integration — stdio MCP client, discovery (initialize,
  tools/list), schema validation, trust levels, capability mapping; imported
  tools registered on the existing ToolRegistry under `mcp.*` with
  fail-closed permissions.
- M7B.4: Database tools — SQLite/PostgreSQL backends behind `database.*`
  tools; read/write/query/transaction permission separation, structured
  errors, audit (no credentials/secrets in logs).
- M7B.5: Application tools — application-adapter framework +
  `application.*` tools (list/launch/verify) reusing ComputerControl and
  desktop-entry discovery.
- M7B.6: Excel automation — `excel.*` tools creating/reading/modifying real
  XLSX (OOXML) artifacts with provenance + digest verification.
- M7B.7: PPT automation — `ppt.*` tools creating real PPTX + render/export
  where a real document backend exists.
- M7B.8: DOCX automation — `docx.*` tools creating real DOCX + export where
  a real document backend exists.
- M7B.9: Cross-tool orchestration — multi-step workflow composing
  browser→database→office tools with provenance validation between steps;
  e2e tests TEST 1–5.
- M7B.10: Final Phase 7B gate — full workspace tests, 7A regression gate
  (Phase 7A stays GREEN), forensic production-caller audit, platform matrix,
  docs (`PHASE7B_IMPLEMENTATION.md`, `PHASE7B_ACCEPTANCE.md`).

### 5. Definition of Done

- Agent can ask “what computer-control capabilities are available here?” and
  receive a structured capability matrix (AVAILABLE / PARTIAL /
  PERMISSION_REQUIRED / UNAVAILABLE with reasons).
- Production path: discover environment → build profile → resolve backend →
  perform operation through ComputerControl API → observe → verify.
- Personal keybindings are used only as environment knowledge/fallback, never
  as the universal implementation of an intent.
- No second tool registry, permission system, or gate; every computer
  capability still goes through `ToolSpec.required_caps` + GateExecutor.
- Capability matrix honestly classifies each supported environment; “works
  everywhere” is never claimed without matrix evidence.

### 6. Tests required

- Environment discovery unit tests (synthetic env vars + config fixtures).
- Config-driven adapter tests for Hyprland/Sway/i3/bspwm fixtures (not the
  user's personal config).
- Backend contract tests (discover → monitors → capture → windows → focus →
  input → clipboard → verify), accounting for capability differences.
- At least one real local end-to-end test through the production Tool Fabric
  API on the actual host environment.
- Permission/deny-by-default tests for every new computer capability.

### 7. Security requirements

- Every operation passes capability check, permission policy, Sovereign Mode,
  tool policy, safety policy, and target validation.
- Dangerous actions (process kill, privileged commands, credential-adjacent
  interaction, system settings) require explicit authorization per existing
  OpenRT policy.
- No silent fallback to insecure/nonexistent mechanisms on Wayland; return
  `PERMISSION_REQUIRED` / `UNSUPPORTED` / `UNAVAILABLE` with exact reason.
- Computer-use kill switch (ADR-008, scope=computer) covers all new input/
  window tools.

### 8. What must NOT be implemented yet

- Claiming GREEN for environments without a real backend and capability matrix.
- A second tool registry or parallel permission/audit path.
- Rewriting Phase 2 tools wholesale (they keep working; Phase 7 adapters sit
  underneath and beside them).
- Modifying the user's `~/.config` or treating one machine's keybindings as
  universal primitives.
- ~~Full MCP client/server, database tools, Excel/PPT/Word automation.~~
  *Deferral lifted by Phase 7B (v0.4): these are now in scope as M7B.3,
  M7B.4, M7B.6–M7B.8, gated by the same requirements as the rest of Phase 7
  (no duplicate registry/permission system, real backends only, honest
  platform matrix).*

---
## PHASE 8 — SANDBOX + ISOLATED EXECUTION FABRIC

The execution-isolation layer that makes autonomous tool use safe: every tool
execution that can touch the filesystem, network, or OS resources runs inside
an admitted, observable, disposable sandbox with enforced limits, verified
cleanup, and a chained audit record — never as an unconfined host process.

> **Reconciliation (2026-09-25, v0.5):** Added as a documented revision per
> the cross-cutting rule; scope drawn from `project-complete-plan.txt` Phase 8
> ("Sandbox: container execution, resource limits, network restrictions,
> filesystem isolation, timeouts, audit — must be completed before allowing
> autonomous tool use"). Phases 1–7 are **not rebuilt**: `openrt-sandbox`
> (bwrap Python sandbox), `openrt-resources` `ResourceGate`, `openrt-netpol`
> (egress intent + mode gate), `openrt-computer` PathJail/terminal process
> control, and `GateExecutor` policy checks all stay authoritative where they
> exist; Phase 8 adds the missing unified sandbox abstraction, the admission
> step between policy and execution, execution-level network policy, container
> backend detection, disposable lifecycle with verified cleanup, and
> risk-driven requirements for autonomous runs. Milestones `M8.1`–`M8.10`
> follow the `M<phase>.<n>` convention.

### 1. Objective

Make "runs in a sandbox" a first-class, verifiable property of every tool
execution instead of a per-tool implementation detail: one sandbox authority,
one admission flow, honest capability reporting, and evidence (integration +
escape tests) that isolation is real.

### 2. Components

- **Unified Sandbox abstraction** — `SandboxRequest` / `SandboxPolicy` /
  `SandboxHandle` / `SandboxResult` / `SandboxCapability` with explicit
  lifecycle states (`CREATED → ADMITTED → RUNNING → {SUCCEEDED | FAILED |
  TIMEOUT | KILLED} → DESTROYED`, plus `ADMISSION_DENIED`, `POLICY_DENIED`,
  `RESOURCE_DENIED`), an authoritative sandbox identity (id, policy digest,
  backend, parent run), and one production crate (`openrt-sandbox`) as the
  single authority.
- **Admission flow** — Permission Policy → Sovereign Mode → Sandbox Admission
  → Resource Admission → Isolated Execution → Verification → Audit. The
  sandbox sits between policy and execution; tools with a sandbox requirement
  cannot bypass it through the registry or gate.
- **Filesystem isolation** — policy-driven mounts (ro/rw) over the existing
  PathJail canonicalization rules; explicit allowlists, no ambient host
  access; escape-resistant path handling verified by tests.
- **Resource limits** — `ResourceGate` host-level admission feeding per-
  sandbox profiles: memory/CPU/process (nproc) rlimits, output-size caps that
  turn flooding into a controlled failure (kill + `RESOURCE_DENIED`), disk
  quotas where enforceable.
- **Network isolation at execution level** — network modes
  (`NONE` / `LOCAL_ONLY` / `ALLOWLIST` / `FULL`) enforced by the backend
  (kernel network namespace where available), DNS behavior included in the
  contract, gated by existing mode/netpol policy (Sovereign blocks egress).
  Where only a weaker mechanism exists (e.g. interpreter-level allowlists),
  the capability is reported at its true isolation level — never as kernel
  enforcement.
- **Container execution** — `ContainerBackend` detection over real engines
  (bubblewrap namespaces, Docker daemon, systemd-nspawn, …) returning honest
  `AVAILABLE` / `UNAVAILABLE` / `PERMISSION_REQUIRED` / `UNSUPPORTED` with
  reasons; structured `CONTAINER_UNAVAILABLE` errors instead of silent
  downgrade to an unconfined process.
- **Disposable sandbox lifecycle** — create → execute → collect artifacts/logs
  → audit → destroy, with **verified** cleanup (no orphan processes — PID-
  identity-safe kill checked against `/proc` start time; no leftover work
  dirs/sockets; `CLEANUP_FAILED` on verification failure).
- **Syscall/security hardening** — runtime detection of seccomp/Landlock/
  `no_new_privs` support and enforcement where the offline toolchain allows;
  honest `UNAVAILABLE` reporting (never a claim without enforcement).
- **Autonomous tool sandbox** — risk → sandbox-profile requirement evaluated
  at gate admission (high-risk/untrusted work must not silently run on the
  host); `sandbox.*` events in the existing audit architecture; workflow
  steps may declare a sandbox profile that admission validates before the
  step runs.
- **Observability** — `openrt sandbox list|inspect|capabilities|policy|run`
  CLI plus read-only WebUI/TUI status surfaces over the same manager (no
  feature exists only in a UI).

### 3. Dependencies on previous phases

- Phase 2/6: `python.run`/`terminal.run` production callers, kill-switch and
  halt integration, Sovereign fail-closed defaults, `GateExecutor`.
- Phase 6: `ResourceGate` resource admission, audit chain (`openrt-audit`),
  `openrt-secrets`, mode manager.
- Phase 7B: normalized error model (WHAT/WHY/WHERE/FIX, structured
  `{ok:false,error:{...}}` outputs) that sandbox denials report through.

### 4. Major milestones

- M8.1: Unified sandbox core — request/policy/handle/result/capability types,
  identity + policy digest, explicit lifecycle states, `openrt-sandbox` as the
  single authority reusing the existing bwrap/subprocess backends.
- M8.2: Resource enforcement — `ResourceGate` decision feeding sandbox
  admission, memory/CPU/process rlimits, output-flood kill, `RESOURCE_DENIED`.
- M8.3: Filesystem isolation — policy mounts (ro/rw), jail integration,
  traversal/symlink escape coverage.
- M8.4: Network isolation — `NONE`/`LOCAL_ONLY`/`ALLOWLIST`/`FULL` at
  execution level incl. DNS, gated by mode/netpol, honest per-mode isolation
  levels.
- M8.5: Container execution — `ContainerBackend` detection matrix (bwrap /
  Docker / systemd-nspawn / …) with structured unavailability, no fake
  container, no silent downgrade.
- M8.6: Disposable sandbox — create→run→collect→audit→destroy with verified
  cleanup (process tree + artifacts), `CLEANUP_FAILED` on failed verification.
- M8.7: Syscall/security hardening — seccomp/Landlock/`no_new_privs`
  capability detection + enforcement where supported, honest `UNAVAILABLE`
  otherwise (`SYSCALL_DENIED`).
- M8.8: Escape testing — classified test suite (`SAFE_UNIT` / `SAFE_HOST` /
  `DESTRUCTIVE` categories; destructive never run directly on the host)
  across filesystem, network, resource, timeout, process-limit, env/secret,
  and cleanup dimensions.
- M8.9: Autonomous tool sandbox — risk-driven profile requirements at gate
  admission (production enforcement for high-risk tool capabilities),
  `sandbox.*` audit events, workflow-step sandbox requirements.
- M8.10: Final Phase 8 gate — full workspace tests, Phase 7 regression stays
  GREEN, forensic production-caller audit, sandbox capability matrix, docs
  (`PHASE8_IMPLEMENTATION.md`, `PHASE8_ACCEPTANCE.md`).

### 5. Definition of Done

- There is exactly one sandbox authority (`openrt-sandbox`) and one admission
  flow; no tool reaches execution while bypassing a declared sandbox
  requirement.
- Network/filesystem/resource/timeout/process restrictions are enforced by
  the execution mechanism (kernel namespace / rlimit / kill), not merely
  requested by policy; every enforcement point has an integration test.
- Backend availability is reported honestly (`AVAILABLE`/`UNAVAILABLE`/
  `PERMISSION_REQUIRED`) with per-capability reasons; unsupported isolation
  is a structured error, never a silent host run.
- Disposable sandboxes are destroyed with **verified** cleanup; failures are
  `CLEANUP_FAILED` and audited.
- Autonomous/high-risk tool runs require an admitted sandbox before tools
  execute (the stated precondition for autonomous tool use).
- Existing Phase 7 gate/tests stay GREEN; no second policy engine, resource
  manager, registry, or audit system.

### 6. Tests required

- Admission unit tests: each denial code (`ADMISSION_DENIED`,
  `POLICY_DENIED`, `FILESYSTEM_DENIED`, `NETWORK_DENIED`, `RESOURCE_DENIED`,
  `TIMEOUT`, `PROCESS_LIMIT`, `SYSCALL_DENIED`, `CONTAINER_UNAVAILABLE`,
  `BACKEND_UNAVAILABLE`, `CLEANUP_FAILED`, `VERIFICATION_FAILED`) with
  WHAT/WHY/WHERE/FIX.
- Real bwrap integration tests on the host: FS escape (traversal, symlink),
  network NONE (TCP + DNS denied), resource limits, timeout kill, process
  limit, env/secret isolation.
- Disposable lifecycle test: create→run→destroy verifies no orphan process
  (PID identity check) and no leftover work dir; cleanup failure path.
- Container detection test: honest status when Docker daemon is absent (no
  fake GREEN), no unconfined fallback without explicit policy opt-in.
- Escape-test suite categorized and runnable (destructive cases only in
  sandboxed/CI-safe contexts).
- Gate/workflow/autonomous enforcement tests: sandbox-required capability
  denied at admission, workflow step requirement validated, Phase 7
  regression.

### 7. Security requirements

- Sandbox sits between policy and execution for every capability that
  declares a requirement; deny-by-default when the requirement cannot be
  met.
- No inheritance of ambient environment/secrets (explicit allowlist only);
  no host socket/FS/network access beyond policy mounts and network mode.
- No silent isolation downgrade: degraded/fallback execution must be an
  explicit policy opt-in and is reported at its true isolation level.
- All sandbox denials, lifecycle transitions, and cleanup failures are
  recorded in the chained audit log (`sandbox.*` actions).
- Escape tests never execute destructive payloads directly on the host.

### 8. What must NOT be implemented yet

- A second permission engine, resource manager, audit log, or tool registry —
  the gate/mode/`ResourceGate`/`openrt-audit` paths stay authoritative.
- Root-only or kernel-patch mechanisms (custom LSM, netfilter modules) or
  new third-party dependencies for isolation (offline constraint: workspace
  `unsafe_code = "forbid"` stands).
- Claiming GREEN for backends without a real mechanism on this host (e.g.
  Docker with no running daemon, seccomp without an enforceable binding).
- GUI/desktop-application sandboxing (computer tools stay platform-abstracted
  per Phase 7; GUI isolation is future work outside this phase) or replacing
  the platform-abstraction tool model of Phase 7.

---

## PHASE 9 — MULTIMODAL INTELLIGENCE FABRIC

One honest pipeline over documents, media, knowledge, and verification: real
multimodal ingestion with provenance-preserving extraction (text, layout, OCR,
images, audio, video), real embedding providers behind existing traits, hybrid
retrieval with reranking and resolvable citations, an append-only evidence
graph, and answer verification with a hallucination firewall — every backend
reporting its true availability, never a simulated stand-in for intelligence.

> **Reconciliation (2026-09-25, v0.6):** Added as a documented revision per
> the cross-cutting rule; scope consolidates `project-complete-plan.txt`
> Phase 9 (PDF/OCR/layout/images/VLM/audio/video), Phase 10 (ingestion →
> embedding → retrieval → reranking → citations → evidence) and Phase 11
> (deterministic + tool verification, LLM verifier, evidence graph,
> hallucination defense) into a single phase, because they are one pipeline
> (ingest → retrieve → ground → verify) and must not ship as three
> independently-wired halves. Phase 4 (documented above) shipped the
> deterministic foundations — `openrt-ingest` (text/Markdown/CSV/JSON +
> hand-rolled PDF text extraction, content-based format detection, honest
> capability probes), `openrt-knowledge` (deterministic retrieval with
> fail-closed citation re-resolution against persisted chunk records),
> `openrt-embeddings` (trait + hash-based reference embedder) and
> `openrt-vectorindex` (cosine index with persist/reload) — but no OCR/VLM/
> ASR backend ever executes, the capability probes have no production caller,
> no neural embedding provider is wired, hybrid retrieval/reranking/evidence
> graph/knowledge graph/answer verification are documented-only, and the
> OWNERSHIP-allocated crates `openrt-retrieval`, `openrt-evidence` and
> `openrt-vlm` were never created. Phase 9 does **not** rebuild the
> deterministic core (it stays the regression baseline and citation
> authority); it adds the missing real backends, creates the three allocated
> crates before use (ADR-007), and allocates the answer-verification layer.
> Milestones `M9.1`–`M9.10` follow the `M<phase>.<n>` convention.

### 1. Objective

Make OpenRT able to read and answer over a real mixed corpus — PDFs (born-
digital and scanned), HTML, office files, images, audio, video — and to prove
its answers: every derived artifact and every retrieved span keeps full
provenance (page/bbox/timestamp/extractor/model/version/confidence), retrieval
is hybrid (lexical + dense where a real embedding backend exists), citations
resolve against persisted evidence, and answers pass a verification stage that
can block unsupported claims. Capability honesty is part of the contract:
where a model or engine is unavailable on the host, the pipeline reports
structured `UNAVAILABLE`/`MODEL_MISSING` (with preserved capability metadata)
and deterministic tests continue — never a fake backend, never silent
substitution.

### 2. Components

- **Multimodal ingestion core** — content-based format detection upgraded to
  structured statuses (`SUPPORTED` / `UNSUPPORTED` / `CORRUPT` / `AMBIGUOUS`)
  over an expanded format surface (PDF, HTML, office, PNG/JPEG, audio, video);
  a normalized document model carrying page/region/bbox/timestamp provenance
  through chunking; bounded, jail/sandbox-scoped extraction; production
  callers (CLI `ingest *`, runtime) for a subsystem that so far has none.
- **OCR + scanned documents + layout** — real OCR backend (Tesseract) with
  word-level boxes and confidence from its TSV output; real page rasterization
  (pdftoppm) so scanned pages route through OCR while born-digital pages keep
  the text path; layout analysis (line/block/column/heading detection, reading
  order) computed from real extracted boxes, not guessed.
- **Images + VLM** — image ingestion with header-parsed dimensions/format/
  color metadata; a first-class VLM pipeline (`openrt-vlm`, allocated in
  Phase 4) over the existing model-provider infrastructure with an images
  transport (local Ollama-compatible endpoint), honest
  `AVAILABLE`/`UNAVAILABLE`/`MODEL_MISSING` reporting; engineering-drawing
  route (regions/labels from OCR, semantic parts via VLM when available).
- **Audio + video** — ffprobe/ffmpeg-backed format probing and decoding; ASR
  backend behind a provider trait (real engine where present, structured
  `MODEL_MISSING` otherwise); video frame sampling on a fixed interval,
  extracted audio track, timeline model with temporal events and provenance
  (t₀/t₁, source track, extractor).
- **Embeddings + vector store** — embedding **provider abstraction** in front
  of the existing trait (Ollama `/api/embed`-style local backend + the
  existing hash-based reference embedder explicitly labeled as reference/
  lexical), backend capability detection, and the vector index wired into the
  production retrieval path (today it has no production caller).
- **Hybrid retrieval + reranking** — new `openrt-retrieval` crate (allocated
  Phase 4): lexical (BM25) + dense retrieval with score fusion, metadata
  filters, and reranking backends (deterministic lexical-feature reranker;
  model-based cross-encoder where a real model is available) — each result
  labeled with its `retrieval_source`; the deterministic retrieval path and
  exact citations of `openrt-knowledge` remain authoritative until parity is
  proven (TEST 1 stays green).
- **Evidence graph + citations + knowledge graph** — new `openrt-evidence`
  crate (allocated Phase 4): claims ↔ source passages ↔ documents edges,
  append-only with tamper detection; citations that must re-resolve against
  persisted evidence; optional knowledge-graph entities/relations on the same
  store (evolving, never duplicated).
- **Answer verification + hallucination firewall** — verification layer that
  combines deterministic evidence checks (every cited span present, claim
  entailed by cited passages) with model-backed verification where a real
  model is reachable, plus **cross-model verification** that honestly reports
  `INDEPENDENT` only when two genuinely independent backends ran (never the
  same model twice); a hallucination firewall that blocks/flags unsupported
  claims at production answer paths, with explicit resource/time limits.
- **Prompt-injection defense** — retrieved documents are UNTRUSTED evidence:
  security-flag detection (instruction-override, exfiltration bait, tool-calls-
  as-text, …) at ingest/retrieval time, trust labels carried on evidence
  records, and a context builder that wraps evidence so a hostile document can
  never be interpreted as operator instruction.
- **Surfaces** — CLI `openrt ingest {inspect,extract,ocr,analyze}`, `openrt
  knowledge {search,semantic-search,hybrid-search,evidence}`, `openrt verify
  {claim,answer}`, `openrt vision capabilities`, `openrt multimodal
  capabilities`; doctor rows for OCR/VLM/ASR/video/embeddings/reranker/evidence/
  verifier; integration suites + demo fixtures; `docs/architecture/phase9/`.

### 3. Dependencies on previous phases

- Phase 4: `openrt-ingest` detection/normalize/chunk/corpus store, `openrt-
  knowledge` retrieval + citation authority, `openrt-embeddings` trait,
  `openrt-vectorindex`, memory semantic path (must keep working).
- Phase 3: model registry/provenance (ADR-001), `openrt-models` provider
  trait, router, modes (Sovereign fail-closed defaults, Provisioning for
  backend discovery), GPU/context schedulers for VLM/ASR budgets.
- Phase 6: runtime wiring, doctor check framework, `ResourceGate` bounds,
  audit chain (`openrt-audit`), gate/permission enforcement for ingest tools.
- Phase 7B: `openrt-office` OOXML readers (office → structured content),
  normalized error model (WHAT/WHY/WHERE/FIX) for every new failure,
  `openrt-fabric` artifact records for derived documents.
- Phase 8: sandbox admission for bounded/hostile parsing (resource/time/output
  limits), honest backend detection patterns (AVAILABLE/UNAVAILABLE/…).

### 4. Major milestones

- M9.1: Multimodal ingestion core — detection statuses (`SUPPORTED`/
  `UNSUPPORTED`/`CORRUPT`/`AMBIGUOUS`), normalized document model with
  page/region/bbox/timestamp provenance, HTML + office extraction paths,
  bounded extraction, production callers (CLI/runtime), TEST 1 baseline
  (deterministic RAG + exact citations) pinned before anything changes.
- M9.2: OCR + scanned PDF + layout — Tesseract backend (boxes + confidence),
  pdftoppm page rendering, born-digital vs scanned page routing, layout
  analysis from real boxes, `document.ocr` capability + doctor row.
- M9.3: Images + VLM — image metadata/regions, `openrt-vlm` provider over the
  model-provider infrastructure with images transport, capability matrix
  (honest `UNAVAILABLE`/`MODEL_MISSING`), engineering-drawing route.
- M9.4: Audio + video — probe/decode via ffprobe/ffmpeg, ASR provider
  (real engine or structured `MODEL_MISSING`), frame sampling + timeline +
  temporal events, timestamped multimodal chunking.
- M9.5: Embeddings + vector store — provider abstraction with local neural
  backend + explicitly-labeled reference embedder, backend capability
  detection, vector index wired into production retrieval (no more zero
  production callers).
- M9.6: Hybrid retrieval + reranking — `openrt-retrieval` crate: BM25 + dense
  fusion, filters, rerankers (deterministic + model-backed where real),
  `retrieval_source` provenance, TEST 1 regression + recall/lift fixtures.
- M9.7: Evidence graph + knowledge graph — `openrt-evidence` crate: claims ↔
  passages ↔ documents, append-only + tamper detection, citations resolve
  against the graph, optional entity/relation layer on the same store.
- M9.8: LLM verifier + cross-model verification — verification layer over
  provider infrastructure with deterministic evidence checks first, model
  verification with resource/time caps, honest cross-model `INDEPENDENT`
  status (≥2 independent backends or an explicit honest degradation).
- M9.9: Prompt-injection defense — detector categories + security flags on
  UNTRUSTED evidence, trust labels through retrieval, hardened context
  builder, malicious-fixture tests.
- M9.10: Final Phase 9 gate — CLI + doctor + WebUI/TUI read-only surfaces,
  integration suites (TEST 1–13, multimodal + engineering-drawing demo
  fixtures), forensic production-caller audit, FINAL ACCEPTANCE + CAPABILITY
  matrices, `docs/architecture/phase9/PHASE9_IMPLEMENTATION.md` +
  `PHASE9_ACCEPTANCE.md`; Phases 5–8 suites stay GREEN.

### 5. Definition of Done

- A mixed corpus (born-digital PDF, scanned PDF, HTML, DOCX, PNG/JPEG image,
  audio, video) ingests through one pipeline with per-modality provenance
  (page/bbox/timestamp/extractor/model/version/confidence) that survives
  chunking, retrieval, citation, and verification.
- Every modality/backend reports honest availability; unsupported inputs are
  structured `UNSUPPORTED`/`CORRUPT`/`AMBIGUOUS`/`MODEL_MISSING` — no fake
  OCR/VLM/ASR/embeddings/reranker anywhere in a production path.
- Hybrid retrieval + reranking + evidence-grounded citations answer questions
  with resolvable sources; deterministic citation authority (TEST 1) remains
  green or is provably superseded with equal strictness.
- The verification layer blocks/flags unsupported claims at a real runtime
  answer path; cross-model independence is reported honestly.
- Ingested evidence is UNTRUSTED by default: injection flags + trust labels
  enforced by a context builder, with hostile-fixture tests.
- New/extended subsystems are allocated in OWNERSHIP (ADR-007) with
  namespaces; single authorities preserved (one retrieval/citation/evidence/
  gate/audit/registry path); phases 5–8 gates stay GREEN.

### 6. Tests required

- TEST 1: existing deterministic RAG + exact-citation behavior stays green
  (regression pin before and after retrieval changes).
- TEST 2: scanned PDF → OCR → chunks → retrieval → answer with page/bbox
  provenance (real Tesseract + pdftoppm on host).
- TEST 3: born-digital PDF keeps the text path; page/offset provenance
  correct; malformed/hostile PDFs rejected bounded (`CORRUPT`).
- TEST 4: audio → format probe → decode → ASR (or structured `MODEL_MISSING`)
  → timestamped transcript provenance.
- TEST 5: video → frame sampling + audio extraction → timeline/temporal
  events → multimodal chunks with t₀/t₁ provenance.
- TEST 6: hybrid retrieval + reranking (fusion, filters, `retrieval_source`,
  rerank lift) with honest backend availability.
- TEST 7: citations resolve against persisted evidence; evidence graph edges
  append-only; tampering detected; optional KG on the same store.
- TEST 8: hallucination firewall — unsupported claim blocked/flagged at the
  production answer path; supported claim passes; limits enforced.
- TEST 9: prompt-injection fixtures (instruction-override/exfiltration/
  tool-calls-as-text) flagged; trust labels flow through context builder;
  injected instructions never treated as operator directives.
- TEST 10: capability honesty — every unavailable backend returns structured
  unavailability with reason; no fake GREEN in doctor or capability matrices.
- TEST 11: ingestion bounds — resource/time/output limits on hostile inputs;
  sandbox admission honored where required (Phase 8 integration).
- TEST 12: production-caller forensic audit — every Phase 9 subsystem has a
  production caller (CLI/runtime/doctor); test-only callers do not count.
- TEST 13: end-to-end reasoning → retrieval → evidence → verification with
  multimodal + engineering-drawing demo fixtures and cross-model honesty.

### 7. Security requirements

- Ingestion runs bounded (memory/CPU/timeouts/output caps, jail/scoped paths);
  hostile documents (zip bombs, malformed PDFs, decompression bombs) fail
  closed with structured errors — Phase 8 sandbox admission where risk
  requires it.
- Retrieved documents are UNTRUSTED: flagged at detection, labeled through
  retrieval, wrapped by the context builder; never executed as instructions;
  injection categories audited through the existing chained audit log.
- No egress of corpus content, embeddings, or evidence except explicit
  operator consent (Sovereign Mode keeps model/network paths fail-closed;
  backend discovery/provisioning happens only in Provisioning Mode).
- Evidence graph, citations, verdicts append-only and tamper-evident; every
  verification records what evidence, which backend, which version, when.
- Cross-model/LLM verification calls carry explicit time/step/resource limits
  and cannot become an unbounded loop or a hidden network dependency.
- No fake-AI in production paths: test doubles live only in explicitly named
  fixtures; capability matrices state the exact reason for every non-GREEN
  row.

### 8. What must NOT be implemented yet

- No replacement of the deterministic retrieval/citation authority until
  TEST 1-equivalent proof exists; no second citation or evidence store
  alongside `openrt-knowledge`'s persisted records.
- No cloud/model download inside Sovereign Mode; no mandatory external API for
  any modality; no silently-downloaded "random" models.
- No fake OCR/VLM/ASR/embeddings/reranker substitutes (regex-as-OCR,
  fixed-vectors-as-embeddings, echo-as-verifier, same-model-twice-as-cross-
  model) in any production path.
- No training/fine-tuning/quantization expansion (Phase 4 M4.5–M4.7 remains
  its own scope); no rebuild of registry/scan/bench/router (Phase 3 owns).
- No second tool registry, gate, permission system, audit log, mode manager,
  or model registry — Phase 9 consumes the existing authorities.
- Handwriting specialized models and full vector-layer CAD parsing remain
  honest future work unless a real local backend is present (report
  `UNAVAILABLE`, do not claim).

---

## PHASE 10 — PERSISTENT AGENT INTELLIGENCE

> **Reconciliation (2026-09-25, ROADMAP v0.7):** Phase 10 consolidates the
> original plan's Recovery + Time Machine, Full Skills System, and
> ECC / AgentShield work into one mega-phase on top of the frozen Phase 5–9
> foundations. It creates exactly one new crate (`openrt-shield`, namespace
> `shield.findings`) and one new namespace family (`recovery.*`); everything
> else extends existing authorities — `openrt-checkpoint` (checkpoints/
> rollback), `openrt-timeseries` (history/time machine), `openrt-workspace-
> assets` (skills/instincts/hooks/rules/genome), `openrt-selfimprove`
> (learning/approvals), `openrt-orch` (WorkflowEngine), `openrt-runtime`
> (gate/doctor/CLI wiring), `openrt-permissions`/`openrt-policy`/
> `openrt-safety`/`openrt-modes`/`openrt-sandbox`/`openrt-audit` (security
> authorities), `openrt-tui`/`openrt-webui` (surfaces). No second recovery/
> history/skill/permission/policy/audit/sandbox/genome/AgentShield authority.

Make OpenRT recover long-running work safely, roll back supported state,
reconstruct execution state through a real time-machine interface, and run a
complete versioned skill system plus the ECC layer (rules/hooks/instincts/
genome/continuous learning) under a single AgentShield security intelligence
layer.

### 1. Objective

Deliver: (a) a complete persistent recovery subsystem — full checkpoints,
validated rollback, crash recovery with partial-task reconciliation, resume,
live task handles, and a time-machine model with TUI/WebUI surfaces — all on
the immutable history ledger; (b) a full skills system — complete manifest,
one authoritative store with full lifecycle ops, loader validation, execution
through WorkflowEngine/ToolRegistry/PolicyEvaluator/SandboxManager, effective
permission intersection, versioning, safe install (imported content can never
arrive active), export, security scan, testing, history-derived generation,
compilation, and an honest offline marketplace; (c) the ECC layer — rules and
hooks enforced at real runtime points, instincts extracted from history and
approval-gated, evidence-backed continuous learning, and a validated agent
genome applied to fresh sessions with version isolation and approval-gated
evolution; (d) AgentShield — one security evaluation layer producing
`SecurityFinding`s (severity-informed actions ALLOW/WARN/REQUIRE_APPROVAL/
DENY/HALT) over the existing policy/kill-switch/sandbox/audit authorities,
with detectors for dangerous commands, credential access, prompt injection
(Phase 9 detector reused, never duplicated), excessive permissions, filesystem
escape, network escalation, unsafe MCP/extension content, and sandbox-bypass
attempts.

### 2. Major milestones

- M10.1: Step 0 reconciliation + allocations (`openrt-shield`,
  `recovery.*`, `shield.findings`).
- M10.2: Full checkpoint model (run/history-cursor/digest/genome/policy
  references) + recovery integrity fail-closed.
- M10.3: Rollback end-to-end (validate → policy compat → plan → operator →
  restore → verify → audit) + in-flight/uncertain node marking so resume is
  never a blind duplicate.
- M10.4: Crash recovery at boot (discover incomplete work, reconcile,
  resume point, recovery events/records) + live task handles.
- M10.5: Time-machine model (timeline, state-as-of, fork/branch compare,
  return-to-current) + TUI/WebUI time-machine surfaces.
- M10.6: Full skills system (manifest, registry ops, loader, execution,
  permissions, versioning, install/export/scan/test, history-derived
  generation, compilation, offline marketplace).
- M10.7: ECC — rules enforced at the gate, complete hook lifecycle, instinct
  extraction/approval/application, evidence-backed continuous learning.
- M10.8: Agent genome — extended schema, validation, fresh-session
  application with id/version/digest recording, approval-gated evolution.
- M10.9: AgentShield — `SecurityFinding`/severity/action model, detectors,
  gate integration, extension scanning, audit/persistence surfaces.
- M10.10: Final gate — CLI/doctor/TUI/WebUI, forensic production-caller
  audit, security audit, regression gate, `docs/architecture/phase10/`.

### 3. Definition of Done

- Crash recovery: a workflow checkpointed mid-flight survives a simulated
  runtime restart; incomplete work is discovered, reconciled (never marked
  successful), resumed at a safe point with no duplicate execution of
  uncertain non-idempotent steps, and the recovery is recorded.
- Rollback: a validated checkpoint restores supported state through an
  explicit authorized path; the immutable history ledger is never rewritten
  (chain verification passes before and after).
- Time machine: timeline/state-as-of/fork/branch-compare/return-to-current
  work through Runtime APIs over the one ledger; TUI and WebUI expose them
  with no UI-local state.
- Skills: execution history materially derives a candidate skill (content,
  not just identity) → provenance → validation → security scan → test →
  operator approval → activation → execution through the real gate/sandbox →
  recorded skill id+version+digest; imported packages can only arrive
  REGISTERED/PENDING and tampered packages are rejected.
- Effective skill permissions are the intersection of caller, skill, tool,
  and sandbox policy — a skill can never exceed its caller, and model output
  never mutates permissions/policy.
- Rules/hooks/instincts are enforced at real runtime points; instincts and
  learnings carry evidence; a genome is validated, applied to a fresh
  session (recorded id/version/digest), and evolves only through the
  approval gate.
- AgentShield denies/halts dangerous operations before execution with
  findings persisted (`shield.findings`) and audited (ids/categories, never
  secrets); Phase 9 injection defense is consumed, not duplicated; shield
  cannot weaken policy and has no parallel kill switch.
- Phases 5–9 suites stay GREEN; no fabricated backends anywhere.

### 4. Tests required

- Recovery: checkpoint creation/integrity; rollback; crash/restart; resume;
  partial-task reconciliation; stale-sandbox recovery; corrupted-checkpoint
  refusal; time-machine view; fork after historical point; live task handle.
- Skills: register; validate; install; activation gate; execution;
  permission denial; sandbox enforcement; version resolution;
  export/import roundtrip; tampered package; security scan;
  history-derived generation.
- ECC: rule enforcement; hook enforcement; instinct creation/approval/
  application; learning derivation; genome validation; fresh-session genome
  application; genome version isolation.
- AgentShield: dangerous command; network escalation; filesystem escape;
  credential access; prompt injection; malicious tool; excessive
  permissions; extension scan; sandbox bypass; halt path.
- Integration: history→skill generation; skill→workflow execution;
  workflow→AgentShield; AgentShield→sandbox; genome→fresh session;
  recovery→resume; TUI/runtime shared state; WebUI/runtime shared state.
- Mandatory E2E: TEST A crash recovery; TEST B time machine; TEST C skill
  lifecycle; TEST D skill import security; TEST E genome; TEST F
  AgentShield; TEST G prompt injection; TEST H full Phase 10 loop.

### 5. Security requirements

- Imported skill content can never arrive active; activation always requires
  the operator path and passes the security scan first.
- Instincts, genomes, hooks, and rules can never grant permissions or bypass
  policy/sandbox/approval; model output never mutates persistent security
  state (permissions, shield rules, sandbox policy, Sovereign Mode,
  detectors) — all such changes go through the existing operator approval
  path and are audited.
- Rollback/restore never erase or rewrite immutable execution history, and
  never recover corrupted state as trusted (fail closed on digest/chain/
  reference/policy-compatibility failures).
- AgentShield consumes the existing authorities (PolicyEvaluator,
  ModeManager, SandboxManager, Safety/KillSwitch, audit chain, Phase 9
  trust/injection types); it adds findings and pre-execution evaluation, not
  a parallel policy engine or kill switch; findings never contain secrets.
- Skills execute only through the existing tool/sandbox/policy path;
  hooks execute with the same permission checks as tools.

### 6. What must NOT be implemented yet

- No Phase 11 work (no plugin registry beyond the shared scanning interface,
  no autonomous marketplace network backend — remote skill sources report
  honest `UNAVAILABLE`/`BLOCKED` under Sovereign Mode).
- No rebuild of Phases 5–9 authorities; no parallel recovery/history/skill/
  memory/workflow/tool/sandbox/policy/audit/genome/shield stores.
- No self-modifying security: no autonomous rewrite of permissions, shield
  rules, sandbox policy, Sovereign Mode, approval requirements, or
  detectors.
