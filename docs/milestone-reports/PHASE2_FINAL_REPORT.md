# PHASE 2 — FINAL REPORT & GATE DECISION

**Date:** 2026-09-18
**Scope:** Phase 2 (M2.1 → M2.7) of ROADMAP.md — "AGENT + COMPUTER USE".
**Decision:** **PASS WITH HARDENING (upheld at Phase 2 gate).**

---

## 1. Executive summary

Phase 2 delivered a sovereign, local-first multi-agent runtime that can safely
operate a computer: master agent + typed sub-agents over a workflow DAG,
filesystem/terminal/python-sandbox/browser/screen/input tool belt behind a
deny-by-default permission gate, an ADR-008 operator kill switch, durable
checkpoints with bounded recovery and dry-run, and — completed in M2.7 — the
ADR-002 **chained, tamper-evident audit log** the roadmap has required from day
one. The final quality gate is green (256 tests, 0 failures, clean fmt /
check / clippy `-D warnings` / build across the workspace), the Phase 2 audit
conducts every Go/No-Go DoD item, and the remaining residuals are documented
with owners.

## 2. Milestone record (M2.1 → M2.7)

| Milestone | Delivered in | Evidence |
|---|---|---|
| M2.1 Master agent + sub-agents + blackboard | P2.3 | `openrt-orch`, `openrt-blackboard` |
| M2.2 Workflow DAG engine | P2.3 | `openrt-orch::WorkflowEngine` |
| M2.3 Filesystem + terminal tools | P2.4 | `openrt-computer` (SEC-01 jail) |
| M2.4 Python sandbox | P2.4 | `openrt-sandbox` (bubblewrap + P2.7 fail-safe) |
| M2.5 Browser/screen/grounding/mouse/keyboard + loop + kill switch | P2.5 | `openrt-computer`, `openrt-safety` |
| M2.6 Checkpoints, rollback, dry-run | P2.6 | `openrt-checkpoint` |
| M2.7 DoD audit + phase gate | P2.7 | this report + `P2_7_FINAL_INTEGRATION_AUDIT.md` |

## 3. Verification baseline

- Pre-gate baseline (P2.6): fmt/check/clippy(`-D warnings`)/build green, 240
  tests, stable ×3.
- Post-gate (P2.7): fmt/check/clippy(`-D warnings`)/build green, **256 tests /
  0 failures across 30 Rust test targets + 18 doc-test suites**, verified
  against the final gate ×3 (same counts each run).

## 4. Test suite summary

| Target | Tests |
|---|---|
| integration (p2_2/p2_3/p2_4/p2_5/p2_6/p2_7 + durability/event_ordering/gate_deny/persistence_roundtrip/session_concurrency) | 34 |
| openrt-core / telemetry / tools / agents / models | 24 / 5 / 16 / 5 / 11 |
| openrt-persistence / resources / safety | 14 / 11 / 5 |
| openrt-config | 8 |
| openrt-orch / openrt-blackboard | 8 / 3 |
| openrt-computer | 25 |
| openrt-sandbox | 10 |
| openrt-checkpoint | 30 |
| openrt-audit | 7 |
| openrt-permissions | 23 |
| openrt-runtime | 17 |
| **Total** | **256** |

New tests added in P2.7: openrt-audit (7), engine cancel + run-report
persistence (2), sandbox fail-safe (2, including a real bug found: fallback
argv), computer-use bounds (2), SEC-09 context threading (1), runtime doctor
sandbox/screen (1), integration `p2_7_audit_chain.rs` (3).

## 5. Security audit — findings and remediations

Re-audited Phase 2 per ROADMAP "Tests required" and "Security requirements".

| Item | Result |
|---|---|
| SEC-01 total path scoping / jail | Verified fixed (M2.4) |
| SEC-02 restrictive file permissions | Verified fixed (P1+P2) |
| SEC-03 checksums / corrosion detection | Verified fixed |
| SEC-04 no secrets in code/config/logs | Verified fixed |
| SEC-05 secret-pattern log redaction | Verified fixed |
| SEC-06 event-log failures surfaced, never silent | Partial (best-effort sink); the P2.7 **audit chain is the fail-closed record** |
| SEC-08 session/task serialization | Verified fixed |
| SEC-09 permission context threading | **Fixed in P2.7** (`PermissionRequest::tool` no longer drops the caller context) |
| SEC-12 path-scope rules honored | Verified fixed |
| SEC-11 doctor store probe | Writes a `RuntimeStarted` to the real event log (LOW, documented) |
| Sandbox silent unconfined fallback | **Fixed in P2.7** (opt-in `allow_unconfined_fallback`, fail-safe default) |
| `input.mouse` unbounded coordinates | **Fixed in P2.7** (display-bounds + fail-closed unknown geometry) |
| H1 workflow cancellation | **Fixed in P2.7** (`Cancelled` status, prompt; race removed) |
| H2 attempt-budget overflow | **Fixed in P2.7** (saturating math) |
| Swallowed run-report persistence error | **Fixed in P2.7** (propagates) |

## 6. DoD mapping (ROADMAP Phase 2, §5–§7)

Each Phase 2 DoD item is met; see the detailed table in
`docs/architecture/phase2/P2_7_FINAL_INTEGRATION_AUDIT.md` §4. The formerly
missing audit requirement — "chained log tamper test; append-only write path
enforced (ADR-002)" — is now satisfied by `openrt-audit` and the
`p2_7_audit_chain` tamper test.

## 7. ADR conformance

| ADR | Status |
|---|---|
| ADR-002 audit envelope (chained, additive, Phase-6-safe) | **Implemented (P2.7)** — `openrt-audit`; the P1 compatibility note's "REQUIRES REFACTOR of sink failure semantics to fail-closed" is the fail-closed append path |
| ADR-006 one store, namespaced records | Held — audit uses the shared `PersistenceStore` (`audit` namespace) |
| ADR-007 naming/namespaces | Held — `audit` namespace allocated in OWNERSHIP §2 |
| ADR-008 kill switches | Held — HALT blocks recovery, never auto-clears; signal + reactions audited |

## 8. Residuals & accepted risks (honest)

- Keyboard-typed text is not redacted from tool args/logs (Phase-2 residual;
  log redaction covers detected secret patterns).
- Per-session screen opt-in: the Phase 2 explicit opt-in is the deny-by-default
  tool permission (M2.5); session-granular flags deferred (documented).
- SEC-07: CLI defaults to `ollama` when unconfigured (intentional scope).
- SEC-10 provider/artifact tampering → Phase 3 registry scope.
- Audit full-store-custody limitation (chain rebuild) → Phase 6 strengthened
  verification, additive-only format.
- Doctor probe writes a `RuntimeStarted` event to the real store (LOW).

These are tracked in the applicable milestone docs and OWNERSHIP notes; none
blocks Phase 3 entry.

## 9. Phase 3 compatibility (Step 8 of the audit)

- Phase 3 is **not started**: no P3 components (registry, verify, scan,
  router, providers beyond the P1 Ollama feature-gate) were implemented.
- P2.7 adds no breaking surface: `openrt-audit` is a new leaf crate; the
  `Error::Audit` variant and the `halt()` return-type change are additive
  within the workspace (all call sites updated); the ADR-002 envelope is
  versioned-additive by design.
- The P1 → P3 touch-points audited in `AUDIT_PHASE2_COMPATIBILITY.md` remain
  valid; the two "NEEDS INTERFACE EXTENSION" items (generic namespaces,
  agent-context resumability) were resolved additively during P2.

## 10. Documentation status

- All Phase 2 milestone docs published under `docs/architecture/phase2/`
  (P2_1 … P2_7). OWNERSHIP.md change log covers P2.3 → P2.7; audit namespace
  and crate allocations were already prescriptive.
- No code from `.opencode/` is copied into the runtime (DoD).

## 11. Repository hygiene

- Zero commits (all milestones uncommitted per phase policy). No secrets in
  tracked files. Workspace-wide `unsafe_code = "forbid"`; no `tempfile`;
  deterministic tests only. Doc-tests/unit tests isolated from the real
  `~/.openrt` store except the documented doctor probe.

## 12. Gate decision

**PASS WITH HARDENING**

- All Phase 2 Go/No-Go DoD items verified green (including the ADR-002 chained
  tamper test and the kill-switch audit requirement).
- Residuals are capability/scope notes with Phase 3/6 owners — none violates a
  Go/No-Go security requirement.
- Baseline is stable at 256 tests / 0 failures ×3 across fmt / check / clippy
  (`-D warnings`) / build / test.

Phase 3 ("MODEL FABRIC + SOVEREIGN INFRASTRUCTURE") may begin.

## 13. Artifacts produced in P2.7

- `crates/openrt-audit` (+ workspace dep wiring) — ADR-002 chained audit log.
- Runtime/gate/checkpoint audit wiring; `halt()` fail-closed recording;
  `cancel_workflow()` + engine H1/H2 hardening; run-report propagation.
- `openrt-sandbox` fail-safe (`allow_unconfined_fallback`) + fallback argv fix.
- `openrt-computer` mouse bounds + real X11 geometry.
- `openrt-permissions` SEC-09 context threading.
- CLI exit code/validation/`--json`/session-not-found; doctor `sandbox` +
  `screen` checks.
- `crates/integration-tests/tests/p2_7_audit_chain.rs` (3 tests).
- Docs: `docs/architecture/phase2/P2_7_FINAL_INTEGRATION_AUDIT.md`,
  `PHASE2_FINAL_REPORT.md`, OWNERSHIP.md change-log entry.

## 14. Post-gate notes

- Continue to honor "no commits unless requested"; the working tree holds all
  Phase 2 work for the eventual Phase 2 commit/PR.
- Start Phase 3 with the registry/verify/scan crates, reusing the audit
  envelope ("same envelope, additive only") for supply-chain records.
- Carry the SEC-05/SEC-12 residuals and the doctor-probe note into Phase 3's
  hardening backlog.