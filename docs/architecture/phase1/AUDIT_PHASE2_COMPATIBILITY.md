# Phase 1 Audit — Phase 2 / Phase 3 Compatibility

**Status:** COMPATIBLE overall; 5 interface surfaces need extension before
Phase 2 "done"; 2 refactors required during Phase 2.
**Date:** 2026-09-17
**Labels:** COMPATIBLE = existing P1 surface supports the requirement with no
change; NEEDS INTERFACE EXTENSION = additive change to a P1 trait/enum/contract;
REQUIRES REFACTOR = P1 structure must be reshaped (non-additive);
BLOCKED = cannot proceed without an earlier fix.
**Reference:** OWNERSHIP.md Phase 2/3 tables, ADR-001..009, ROADMAP.md
Phase 2/3 requirements + Tests Required + Security requirements.

---

## 1. Phase 2 component-by-component

| P2 component | P1 foundation | Label |
|---|---|---|
| `openrt-orch` (master agent, orchestration, DAG, sub-agents) | `Agent` trait is a single `run(ctx,input) -> AgentResult`; `AgentContext` snapshots history + cancel, serializable; `Pipeline` is single-agent. Multi-agent control flow, sub-agent dispatch, DAG state all new | NEEDS INTERFACE EXTENSION (extend `Agent` contract + orchestration loop in `openrt-runtime`/new `openrt-orch`) |
| `openrt-blackboard` (exec.blackboard) | ADR-006 one-store-manager exists but `PersistenceStore` trait is fixed-arity (sessions/tasks/executions/events) | NEEDS INTERFACE EXTENSION (add typed/KV namespaces or a generic `namespace+key` contract) |
| `openrt-computer` (filesystem/terminal/browser/screen/input tools) | `Tool`/`ToolSpec`/registry extensible (`register`/`register_custom`, name validation, `provider_tools` schema export); `GateExecutor` interposes all tools uniformly; DirectExecutor is the only ungated path (SEC-04) | COMPATIBLE |
| Vision grounding (deterministic) | `openrt-resources` GPU trait is a no-op stub; screen capture/grounding are entirely P2 | COMPATIBLE (no P1 conflict) |
| Checkpoints / rollback (`openrt-checkpoint`) + dry-run | Task/session records are full-state documents; but no snapshot-of-execution API and `AgentContext` is history-only (no provider/agent-internal state to resume) | NEEDS INTERFACE EXTENSION (checkpoint namespace + resumable agent state) |
| `permission.tools` (tool permissions) | `Policy` + `PolicyRule{pattern,action,scope}` + `Decision::Ask` (P1 treats Ask=denied) is the right substrate | COMPATIBLE, with NEEDS INTERFACE EXTENSION for approval (resolver → Ask resolves), context-aware rules (SEC-09), and out-of-workspace fs scope (SEC-01) |
| Audit log envelope (`openrt-audit`, ADR-002) | `DomainEvent` envelope + total-order bus + append-only event log exist; the log is best-effort (SEC-06) and unparsable lines are skipped (SEC-12) | COMPATIBLE (envelope format is new & additive); REQUIRES REFACTOR of sink failure semantics to fail-closed for the audit chain |
| Safety/halt signal (`openrt-safety`, ADR-008) | Event bus + `EventKind` (additive enum) + `CancellationToken` parent/child hierarchy — built exactly for a broadcast HALT that fans out to child tokens | NEEDS INTERFACE EXTENSION (subscription/filter API over the bus; `HALT{scope}` kinds; operator-only access controls) |
| Computer-use kill switch (on safety) | Cancellation is deterministic and `biased`; mechanics ready | COMPATIBLE (P2 adapter) |

## 2. Phase 3 touch-points

- **Model provider trait** (`openrt-models::ModelProvider` + `capabilities()`) is
  the contract P3 backends (llama.cpp/vLLM — `openrt-provider-*`) implement.
  Feature-gated Ollama module is the reference pattern. **COMPATIBLE.**
- **Registry/verification/scanning (ADR-001/003)** need `registry.manifests`,
  `verify.records`, `scan.records` namespaces. Same store-manager limitation as
  blackboard: `PersistenceStore` must grow generic namespaces.
  **NEEDS INTERFACE EXTENSION.**
- **Provisioning/Sovereign enforcement (ADR-003, `openrt-modes`)** — Phase 1
  carries only `Config.mode` (INTERFACE ONLY). Enforcement is entirely P3;
  **COMPATIBLE** (no P1 conflict; the config field already round-trips and is
  emitted in `RuntimeStarted`).
- **Memory-hierarchy / content-addressed caches (ADR-009)** — no P1 conflict;
  P1 store has no parallel cache. **COMPATIBLE.**

## 3. The 18 critical architectural questions

1. **Q1 — Is the dependency direction law (nothing below `runtime` knows
   `runtime`) enforceable in CI?** Yes in practice (clippy + watched imports);
   not yet enforced by a tool (no `cargo-public-api`/lint). Label:
   COMPATIBLE — add a CI lint in P2.
2. **Q2 — Can Phase 2 add tools without touching the gate?** Yes — tools
   register via `ToolRegistry::register`, all execution flows through
   `GateExecutor`. COMPATIBLE.
3. **Q3 — Can a Phase 2 tool be made ungated by accident?** Only via the
   public `DirectExecutor` (SEC-04) — refactor it out of the public API.
   NEEDS INTERFACE EXTENSION.
4. **Q4 — Does the permission model extend to scoped, context-aware rules?**
   Partly: `scope` globs + Ask exist; context predicates (session/task/channel)
   do not (SEC-09). NEEDS INTERFACE EXTENSION.
5. **Q5 — Is `Ask` a first-class decision Phase 2 can resolve?** No — Ask
   currently maps to denied (documented). A resolver/approval channel must be
   added (SEC-13). NEEDS INTERFACE EXTENSION.
6. **Q6 — Is the event surface additive for `HALT` and audit events?**
   Yes — `EventKind` is an additive enum and the log is append-only. COMPATIBLE.
7. **Q7 — Can consumers filter/subscribe to events (needed for `openrt-safety`
   and approval)?** No — sinks are push-all; no topic filter. NEEDS INTERFACE
   EXTENSION.
8. **Q8 — Is event ordering strong enough for a chained audit log (ADR-002)?**
   Yes structurally (single-dispatcher total order), but persistence is
   best-effort (SEC-06) and silent-skip on unparsable lines (SEC-12) —
   REQUIRES REFACTOR for fail-closed audit writes.
9. **Q9 — Can the store host more namespaces (blackboard, checkpoints,
   registry)?** Not yet — `PersistenceStore` is fixed-arity. NEEDS INTERFACE
   EXTENSION (add generic `namespace+key` + versioned namespace registry, per
   ADR-006).
10. **Q10 — Is restart durability proven?** Yes — `persistence_roundtrip.rs`
    reopens the same dir and reloads session/task/events. COMPATIBLE.
11. **Q11 — Can concurrent tasks on one session be safe for a P2 orchestrator?**
    No — session transcript writes are last-writer-wins (SEC-08). REQUIRES
    REFACTOR (per-session serialization) before multi-agent.
12. **Q12 — Does cancellation support Global HALT fan-out?**
    Yes — `CancellationToken::child()` + `cancel()` cascades to children; the
    pipeline race is `biased`/deterministic. COMPATIBLE.
13. **Q13 — Is the model abstraction provider-agnostic for P3 backends?**
    Yes — trait + capabilities + feature-gated provider; agent consumes
    `Arc<dyn ModelProvider>`. COMPATIBLE.
14. **Q14 — Does anything depend on Ollama being present?** No — Mock is the
    test/demo default; `OllamaProvider` only under the feature + config. Wiring
    is provider-agnostic. COMPATIBLE (with DEP-1 caveat re default build).
15. **Q15 — Is config schema forward-compatible?** Additive fields are fine;
    `deny_unknown_fields` makes typos fail loudly. COMPATIBLE.
16. **Q16 — Is the CLI a thin adapter with no feature living only in a UI?**
    Yes — every CLI path calls runtime/store/provider/registry. COMPATIBLE.
17. **Q17 — Are Phase 2 tests writable today (deny-never-runs, ordering, restart,
    cancellation)?** Yes — the three integration suites are reusable templates.
    COMPATIBLE.
18. **Q18 — Are the security-critical invariants for P2 testable in P1 code?**
    Mostly — gate_deny (permission), ordering, restart, cancellation exist;
    missing: corruption/tamper tests (SEC-03), approval-path tests, concurrency
    stress, and a hard `filesystem.read` scope test (SEC-01). NEEDS INTERFACE
    EXTENSION / test additions.

## 4. Verdict

**READY FOR PHASE 2 (interface/architecture work), with five extension points
and two refactors that must land before Phase 2 "done":**

- Extend: `PersistenceStore` to generic namespaces (Q9); event subscription +
  Ask resolution + context-aware rules (Q5/Q7/Q4); resumable agent state for
  checkpoints; `HALT{scope}` kinds.
- Refactor: `DirectExecutor` out of the public API (Q3); fail-closed audit-log
  writes (Q8) and per-session write serialization (Q11).
- Hardening prerequisites carried from AUDIT_SECURITY.md: SEC-01 (scoped
  `filesystem.read`), SEC-02 (store dir/file creation modes), SEC-03
  (record checksums).

No BLOCKED items exist for phase 2 theoretical design; the two refactors and the
three hardening items are Phase 2 *admission criteria for the relevant
milestones*, not blockers for launching Phase 2.