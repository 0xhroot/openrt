# ADR-006: Persistent Storage Ownership and the Foundation Store

**Status:** Accepted
**Date:** 2026-09-17
**Applies to:** Phase 1 (foundation store), Phase 2 (execution state), Phase 3
(model registry), Phase 4 (corpus/knowledge), Phase 5 (workspace)

## Context

Phases 1, 2, 3, and 5 each specify durable data (runtime state, execution/
blackboard state, model registry, workspace). Without explicit ownership, the
Phase 5 "persistent workspace" could become a competing parallel store that
duplicates or shadows records from the earlier phases.

Prior conflict: P1 "persistent state", P2 "persistent state stores blackboard
and DAG checkpoints", P3 registry storage, and P5 "persistent workspace" were
described independently with no unified layout.

## Decision

1. **One store manager.** Phase 1 introduces the **foundation store manager**:
   a local, versioned store exposing typed namespaces — content-addressed blobs,
   key-value, records, and an append-only log (the audit log uses ADR-002).
   All later phases persist through it; no phase may introduce a parallel store.

2. **Ownership boundaries** (each owner writes its records; all live in the
   foundation store):
   - P1 — sessions/tasks, configuration, permission policy, runtime KV.
   - P2 — execution state: DAG runs, blackboard snapshots, checkpoints
     (filesystem + metadata), audit-log entries.
   - P3 — model/artifact registry (base + derived manifests, ADR-001),
     benchmark results, verification and scan records (bundle versions,
     ADR-003), cache *metadata*. Cached model weights/experts live in the
     memory-hierarchy store (content-addressed; ADR-009), not the registry.
   - P4 — corpus documents, vector index (derived from documents; rebuildable),
     evidence/knowledge graph, datasets, evaluation records — each with its own
     namespace. Dataset identity digests feed ADR-001 manifests.
   - P5 — memory layers (working/episodic/semantic/workflow), skills, instincts,
     hooks, rules, genomes, execution history. The **workspace is an overlay/
     consolidation layer**: it adds versioning, timestamps, and cross-namespace
     indexes over the existing namespaces. It must not copy or duplicate
     records and cannot shadow or partition storage.

3. **Single source of truth per record type.** Consumers address storage by
   namespace + key. Future migrations move namespaces, never data copies.

## Amendment 2026-09-17 (Phase 2.3 — generic namespaced records)

The **one store** rule gains its generic record surface: `PersistenceStore`
now exposes `put_record` / `get_record` / `list_records` / `delete_record` for
validated `a-z0-9._-` `(namespace, key)` pairs (keys cannot escape their
namespace directory). Phase 2 execution namespaces persist through this API:
`exec.runs`, `exec.dag` (openrt-orch), `exec.blackboard` (openrt-blackboard),
and later `exec.checkpoints` / `exec.dryrun` (P2.6) — all in the SAME store,
so P5 cannot drift and `clear()` / `doctor` cover them. Generic records
inherit the SEC-03 checksum manifest and SEC-02 file modes on the file
backend. An untouched namespace (missing dir) reads as empty, not an error.

**References:** OWNERSHIP.md change log, P2_3_AGENT_ORCHESTRATION.md.

---

## Consequences

- No competing stores; P5 cannot drift from earlier-phase state.
- Cross-phase schema changes must be coordinated (namespaces are versioned).
- Slight indirection at P5 (overlay/indexing) is an accepted cost.
- Registry and cache are cleanly separated (metadata vs content).

**References:** ROADMAP.md Phase 1 (persistent state), Phase 2 (dependencies,
security), Phase 3 (registry), Phase 5 (persistent workspace),
Cross-Cutting (one storage backbone); ADR-001, ADR-002, ADR-009;
OWNERSHIP.md.