# Phase 2.7 — Final Integration + Security/DoD Audit + Phase Gate

**Status:** Implemented
**Date:** 2026-09-18
**Scope:** ROADMAP M2.7 — the Definition-of-Done audit and phase gate for
Phase 2. Implements the roadmap's Phase 2 audit requirement (ADR-002 chained,
tamper-evident log) as a real subsystem (`openrt-audit`), closes the remaining
verified defects found during the audit, and documents the full DoD mapping.
Baseline behavior unchanged (240 → 256 tests, all green).

This document is the milestone record; the phase gate decision itself is in
`PHASE2_FINAL_REPORT.md` (repo root).

## 1. Verification (before changing anything)

- All milestones gated green before work: fmt / check / clippy `-D warnings` /
  build / `cargo test --workspace` (240 passed, 45 suites, stable ×3).
- ADR-002 (audit log envelope: sequence, timestamp, actor/action/subject/result,
  `prev_hash`, chain-mode fields) was accepted in Phase 1 but **absent from the
  implementation** — the audit milestone (M2.7) is what brings it in, chained
  from day one exactly as the roadmap demands, with the Phase 6 strengthening
  path ("same envelope, additive only") preserved.
- Audit findings against Phase 2 (P2.2/P2.3/P2.4/P2.5/P2.6) were re-scanned and
  the residuals tracked through the P2.6 audit. Items verified already-fixed:
  SEC-01 path jail, SEC-02 file permissions, SEC-03 checksums, SEC-04 no
  secrets, SEC-05 log redaction, SEC-06 event-log failure surfacing, SEC-08
  session serialization, SEC-12 path-scope rules honored. Remaining open items
  from the audit are fixed or honestly documented in section 6.
- No `tempfile` dependency in the workspace (tmp dirs via
  `std::env::temp_dir().join("openrt-{tag}-{pid}-{nanos}")`), deterministic
  tests only (no sleeps/flaky time-based asserts).
- Workspace-wide `unsafe_code = "forbid"` unchanged.

## 2. Design decisions

### 2.1 `openrt-audit` — the ADR-002 chained envelope as a real crate

- New leaf crate `crates/openrt-audit` (allocated in OWNERSHIP.md line 70:
  "Audit log envelope | `openrt-audit` | `audit` | chained from day one").
  Deliberately **not** folded into `openrt-persistence`: the envelope's
  semantics (canonical serialization, chaining, verification) evolve with the
  audit system in Phase 6, and the roadmap binds it to the same ADR-002
  envelope additive-only.
- Storage: `audit` namespace (ADR-006 one-store rule), records at zero-padded
  keys `{seq:020}` (string ordering == sequence ordering) plus a `tail` key
  holding `{seq, hash}`. Appends are serialized through a `tokio::sync::Mutex`.
- **Canonical hash:** SHA-256 over a versioned string
  `1|{seq}|{ts}|{actor}|{action}|{subject}|{result}|{chain_mode}|{ext}` with
  `ext` map keys sorted (`canonical_value`). Equal entries always hash equal;
  adding a field later bumps `CANON_VERSION` (Phase 6 additive path).
- **Fail-closed:** a record put that fails surfaces `AuditError::Persistence`
  — the calling path decides how to surface it, but an action is never
  considered recorded when its append failed.
- **Verify:** walks backward from the tail, recomputes each content hash (any
  edit → `Tampered(seq)`), checks the `prev_hash` link (broken link →
  `Tampered`), detects a missing record (`Missing`), and requires genesis to
  link to the empty hash. Returns the verified count.
- **Documented limitation:** a full-store-custody attacker can truncate and
  rebuild the whole chain; this matches the roadmap ("Phase 6 strengthens
  verification without an incompatible format migration").

### 2.2 Wiring — what is recorded (ADR-002 + ADR-008)

- `Runtime` owns `Arc<openrt_audit::AuditLog>` (shared store) and a typed
  `Error::Audit(#[from] AuditError)`. `Runtime::audit(actor, action, subject,
  result, ext)` exposes the log to operators/system.
- **`permission.decision`** — the gate records every computer-use permission
  outcome (`is_computer_tool` covers `filesystem.*`, `terminal.*`, `screen.*`,
  `input.*`, `browser.*`, `python.*`; excludes benign `echo`/`system.info`):
  results `allowed`, `denied`, and `kill_blocked` (a tool blocked by the HALT
  is *both* blocked and recorded). An *allow* whose audit append fails returns
  `ToolError::Internal` before the tool runs — no un-audited computer action.
- **`safety.halt`** — `Runtime::halt()` records `actor=operator`,
  `result=tripped`, `ext.reason`, and now returns `Result<bool, Error>` so a
  recorder failure is surfaced, not swallowed. (3 call sites updated.)
- **`checkpoint.recovery`** — after `RecoveryStarted` in `checkpoint.rs`, the
  engine's decision (`ready` / `blocked` / `needs_intervention`) is appended
  before any node is scheduled; append failure aborts the recovery run.
- Event kinds: **none added** (stays at 38) — the audit log is a separate
  write-arm of the same store, not a bus event.

### 2.3 Engine hardening verified by the audit (H1, H2)

- **H1 — cancellation:** each step now races the agent call against
  `ctx.cancel.future()` inside `tokio::select!`, and a cancelled cancel-token
  overrides step failures so a cancelled run reports `WorkflowStatus::Cancelled`
  (not a misleading `Failed`). `engine_cancel` is a real child token on the
  runtime, tripped by `Runtime::cancel_workflow()`.
- **H2 — attempt math:** cumulative attempt counters use saturating arithmetic.
- **Run-report persistence:** the workflow run-report `put_record` failure was
  swallowed (`let _ =`); it now propagates as `WorkflowError::Persistence`, so a
  run that ran cannot masquerade as durable.

### 2.4 Sandbox fail-safe (verified defect: silent fallback)

- Before P2.7, `python.run` silently fell back to the unconfined `subprocess`
  backend when bubblewrap was absent. Now the degraded backend is **opt-in**:
  `PythonSandbox::with_backend(.., Subprocess)` + `allow_unconfined_fallback`
  (tool builder and `python_sandbox.allow_unconfined_fallback` config; default
  `false`). With the fallback disabled and no `bwrap`, `python.run` refuses
  (`ToolError::Internal`, message points at the config flag).
- Fixing the opt-in path's test uncovered a real argv bug in the fallback
  runner (`python <python> -I script` — the interpreter binary was passed as a
  *script*); corrected to spawn the interpreter directly with `-I`.

### 2.5 Computer-use coordinate bounds (verified defect)

- `input.mouse` validated nothing: an out-of-bounds coordinate was dispatched
  to the backend. Now every target is bounds-checked against the backend's
  display before dispatch; unknown geometry (`0x0`) fails closed so no action
  ever targets an unvalidated coordinate. `X11Screen` resolves real geometry
  once via `xdotool getdisplaygeometry` (cached in a `OnceLock`); the log
  documented the old stub returning `(0,0)` as compliant.

### 2.6 SEC-09 — the request context was dropped

- `PermissionRequest::tool` ignored the caller-supplied
  `PermissionContext` (always `default()`), so policies could not route
  session/channel-aware. The context now threads through (tested).

### 2.7 CLI + doctor (operator-facing polish)

- `openrt task run` exits `1` when the task did not complete (scripts can
  observe failure).
- Empty `task run` prompt and `session create` title are rejected.
- `openrt task list --json`; `task list --session <unknown>` errors
  "session not found".
- `openrt doctor` gains `sandbox` (bubblewrap availability + fallback posture)
  and `screen` (backend geometry; fail if `input.mouse` would reject every
  target) checks.

## 3. Tests

- `openrt-audit`: 7 unit tests — chain append+verify; empty verify; tampered
  **content** detected at the edited record; tampered **link** detected at the
  follower; missing record detected; 20 concurrent appends stay contiguous and
  verify; store failure surfaces fail-closed.
- `openrt-orch`: engine H1/H2 — `cancelling_the_engine_stops_promptly_and_reports_cancelled`
  (hang agent → `<5s` → `Cancelled`) and
  `run_report_persistence_failure_surfaces_not_swallowed` (delegation store
  failing only `exec.runs`).
- `openrt-runtime`: `Error::Audit`, `halt()` recording, doctor `sandbox` +
  `screen` coverage.
- `openrt-sandbox`: 10 tests including `missing_bwrap_refuses_by_default` and
  `explicit_unconfined_fallback_runs` (backend-explicit, host-independent).
- `openrt-computer`: 25 tests including OOB click/move/drag rejection and
  fail-closed unknown display.
- `openrt-permissions`: 23 tests including `tool_request_preserves_permission_context`.
- Integration `p2_7_audit_chain.rs` (3 tests, real runtime + gate + store):
  1. `permission_decisions_and_halt_are_chained_and_verify` — allowed →
     `permission.decision/allowed`, denied → `denied`, HALT →
     `safety.halt/tripped` with reason, post-halt action → `kill_blocked`;
     chain verifies at every step.
  2. `tampered_audit_record_is_detected` — rewrites a stored record's hash via
     the shared store; `verify()` fails with `Tampered(seq)`.
  3. `cancelling_a_running_workflow_reports_cancelled` — a blocking provider
     keeps the engine mid-step; `cancel_workflow()` returns `Cancelled`
     promptly and the chain still verifies.

## 4. ROADMAP DoD mapping (M2.7 + Phase 2 audit tests)

| Phase 2 DoD item | Status | Where |
|---|---|---|
| Workflow DAG (planner/coder/verifier/critic) end-to-end, results persist | Done (M2.3) | `openrt-orch` + `p2_3_orchestration.rs` |
| Computer-use loop completes a controlled task | Done (M2.5) | `p2_4`, `p2_5` (fill & submit form fixture) |
| Destructive tools require permission/approval; dry-run auditable trace | Done (M2.6) | gate + `resume_dry_run` (`p2_6`) |
| Checkpoint/rollback restores state; forced-failure recovery | Done (M2.6) | `p2_6` scenario 1–5 |
| Sandbox enforces fs/network/CPU/memory limits | Done (M2.4) + fail-safe (P2.7) | `openrt-sandbox` tests; `allow_unconfined_fallback` opt-in |
| No tool escapes configured path/scope | Done | SEC-01 jail + traversal tests |
| HALT halts mouse/keyboard/terminal mid-action; audited; scoped | Done (M2.5) + audited (P2.7) | `p2_5` kill test; gate `kill_blocked` recording |
| **Audit (Test required): chained log tamper test, append-only, ADR-002** | **Done (P2.7)** | `openrt-audit::verify` + `p2_7` tamper test; serialized append mutex |

## 5. Not in this milestone / residuals (honest)

- **Per-session screen opt-in** (roadmap: "screen capture and input require
  explicit operator opt-in per session"): in Phase 2 the explicit opt-in is the
  deny-by-default tool permission (M2.5). Session-granular opt-in flags are
  deferred; documented, not silently dropped.
- **Keyboard-typed text is not redacted** from tool args/logs (a Phase-2
  residual noted in the security audit; log-redaction covers detected secret
  *patterns*, not free-form typos).
- SEC-07: CLI defaults to `ollama` when unconfigured — intentional DoD scope.
- SEC-10 provider/artifact tampering matters are Phase-3 registry scope.
- `audit` cost-of-custody limitation: a full-store attacker can rebuild the
  chain (Phase 6 "strengthened verification", additive only).
- STEP-6 recovery verification is covered by tests, not a runtime manual trace.

## 6. Gate summary

Full counts and the decision are in `PHASE2_FINAL_REPORT.md` (repo root). In
short: 240 → 256 tests, 0 failures, clean fmt/check/clippy (`-D warnings`)/
build across the workspace; the audit conducts every Phase 2 Go/No-Go
requirement, the ADR-002 tamper test passes, and open residuals are documented
with owners (Phase 3/6).