# ADR-007: Naming Namespaces and Crate Ownership

**Status:** Accepted
**Date:** 2026-09-17
**Applies to:** all phases (cross-cutting)

## Context

Several roadmap terms are overloaded: "context", "memory", "session",
"checkpoint", "registry", "cache". Using them interchangeably across
subsystems is a reliable source of design and implementation bugs.

Prior conflict (flagged): P1 "session system" vs P5 "Sessions UI"; P3 "context
scheduler" vs model context; P3 "model cache" vs P4 vector store vs P5 "memory";
P2 "checkpoints" vs P5 "time machine"; P3 "registry" vs P5 skills/assets.

## Decision

Rust crates are the namespace unit; each overloaded term is bound to exactly one
crate/domain:

| Term | Means | Crate |
|---|---|---|
| `context` | model/context-window management only (P3) | `openrt-context-sched` |
| agent context | the prompt/composition state of an agent (P1 agent abstraction tooling); a distinct concept from `context` scheduling and from `session.content` | `openrt-agent` |
| `session` | P1 session/task lifecycle only | `openrt-session` |
| conversation content | the messages/payload of a session, never "context" | `openrt-session` (`.content`) |
| `memory` | persistence layers only (P5) | `openrt-memory-*` |
| corpus retrieval (P4) | retrieval/RAG, never "memory" | `openrt-retrieval` |
| weight movement (P3) | hierarchy/cache, never "memory" | `openrt-hierarchy`, `openrt-model-cache` |
| `checkpoint` | P2 execution snapshots (fs + runtime state) | `openrt-checkpoint` |
| time machine (P5) | history replay/fork, different concept | `openrt-timeseries` |
| `registry` | model/artifact registry only (P3) | `openrt-registry` |
| skills/instincts/hooks/rules/genomes (P5) | workspace assets, never "registry" | `openrt-workspace-assets` |
| `cache` | P3 memory-hierarchy caches (content-addressed) | `openrt-model-cache` |
| vector/embedding store (P4) | corpus-derived index, not "cache" | `openrt-vectorindex` |
| `audit` | the chained audit log (ADR-002) | `openrt-audit` |
| telemetry (P5) / observability (P6) | single pipeline, P6 extends | `openrt-telemetry` → `openrt-observability` |
| safety/halt | common runtime safety signal bus (ADR-008) | `openrt-safety` |

Rules:
1. A new subsystem must be allocated a crate (and storage namespace per
   ADR-006) in `docs/architecture/OWNERSHIP.md` **before** implementation.
2. Headings and component names in the roadmap already follow this map.
3. Reviews and CI enforce crate-boundary usage (no cross-import of internal
   internals).
4. When two features genuinely share a domain, they extend the same crate;
   they never steal another term.
5. The three-way distinction is mandatory: `context` (scheduler, P3), agent
   context (prompt composition, P1/P2), and `session.content` (conversation
   payloads, P1) are never used interchangeably in prose or code.

## Consequences

- Mechanical but high value; prevents cross-feature confusion early.
- One authoritative glossary (OWNERSHIP.md).
- Enforcement requires crate-boundary discipline at review time.

**References:** ROADMAP.md Cross-Cutting (naming namespaces); ADR-006;
OWNERSHIP.md.

---

## Amendment 2026-09-17 (Phase 1 implementation)

The terminology discipline above is unchanged. Its **crate bindings** were
re-aligned to the implemented Phase 1 workspace (see the reconciliation note in
`OWNERSHIP.md` Phase 1). Mapping of the old sketch names to today's crates:

| Old sketch name | Now implemented as |
|---|---|
| `openrt-session` | `openrt-core` (session/task types), `openrt-runtime` (lifecycle), `openrt-persistence` (`session`/`task` namespaces) |
| `openrt-events` | `openrt-core` (event types), `openrt-telemetry` (bus/emission) |
| `openrt-agent` | `openrt-agents` |
| `openrt-provider`, `openrt-provider-ollama` | `openrt-models` (trait; Ollama behind a feature) |
| `openrt-log` | `openrt-telemetry` |
| `openrt-store` | `openrt-persistence` |

`context`/`session`/`conversation content`/`agent context`/`telemetry` meanings
are unchanged; ROADMAP.md prose refers to the glossary which remains
authoritative in OWNERSHIP.md.

---

## Amendment 2026-09-17 (Phase 2.3 — orchestration bindings)

The Phase 2 orchestration crates are now implemented under the allocated
names: master agent + multi-agent orchestration + workflow DAG =
`openrt-orch` (namespaces `exec.runs`, `exec.dag`), shared blackboard =
`openrt-blackboard` (namespace `exec.blackboard`, execution-scoped).
`blackboard` here means "shared in-run coordination memory", distinct from the
P5 `openrt-memory-*`/`memory.*` persistent long-term memory; `workflow memory`
in P5 refers to long-term workflow learning, not the P2.3 run-local DAG. See
OWNERSHIP.md change log.