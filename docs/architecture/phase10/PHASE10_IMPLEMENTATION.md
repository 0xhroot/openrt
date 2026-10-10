# PHASE 10 IMPLEMENTATION — Persistent Agent Intelligence

Phase: **10** (ROADMAP v0.6, milestones M10.1–M10.10).
Acceptance evidence (gates, TEST A–H matrix, audits, capability matrix):
[`PHASE10_ACCEPTANCE.md`](./PHASE10_ACCEPTANCE.md).

## 1. Scope and allocation

Phase 10 makes long-running work recoverable, rollbackable, and
reconstructable on the one immutable history ledger, and adds three layered
intelligence systems — skills, ECC (rules/hooks/instincts/genome/learning),
and AgentShield — without creating parallel authorities (OWNERSHIP,
ADR-006/ADR-007):

| Subsystem (milestone) | Crate(s) | Store namespace |
|---|---|---|
| Step 0 reconciliation + allocations (M10.1) | `openrt-shield` (new), `openrt-runtime` (gate) | `shield.findings`, `recovery` |
| Checkpoint model + recovery integrity (M10.2) | `openrt-checkpoint` (new) | `recovery.*` |
| Rollback end-to-end (M10.3) | `openrt-checkpoint` (plan/policy/rollback), `openrt-runtime` | `recovery.*` |
| Crash recovery + live handles (M10.4) | `openrt-runtime` (`recovery.rs`) | `recovery.*`, tasks |
| Time machine + TUI/WebUI surfaces (M10.5) | `openrt-runtime` (`timemachine.rs`), `openrt-tui`, `openrt-webui` | history ledger (timeseries) |
| Full skills system (M10.6) | `openrt-workspace-assets`, `openrt-runtime` (`skill.rs`, `phase5.rs`), `openrt-cli` | `skills` |
| ECC layer (M10.7) | `openrt-workspace-assets`, `openrt-selfimprove`, `openrt-runtime` (`gate.rs`) | `instincts`, `rules`, hooks, `improve` |
| Agent genome (M10.8) | `openrt-workspace-assets`, `openrt-runtime` (`phase5.rs`) | `genome` |
| AgentShield (M10.9) | `openrt-shield`, `openrt-runtime` (`gate.rs`, `phase5.rs`) | `shield.findings` |
| Final gate + docs (M10.10) | `openrt-cli`, `openrt-doctor`, `docs/` | — |

Single authorities preserved: `openrt-audit` remains the only audit chain;
`openrt-modes`/`openrt-netpol` remain the Sovereign/egress authorities;
`openrt-policy`/`openrt-sandbox`/`openrt-permissions` remain the
permission/policy/sandbox authorities; the history ledger
(`openrt-timeseries` + `openrt-verify` digests) remains the only execution
history; `openrt-workspace-assets` is the only asset store (skills,
instincts, rules, hooks, genome, candidates); `openrt-shield` is the only
security *evaluation* layer and it only consumes the authorities above.

## 2. The fail-closed contract (applies to every layer)

1. **One authority per concern** — recovery, history, skills, memory,
   workflow, tools, sandbox, policy, audit, genome, and shield each have
   exactly one owner; everything else is a wrapper or a consumer.
2. **Immutable history** — rollback, resume, fork, and time-machine
   operations never write into or erase ledger rows; every mutation appends.
   Chain verification (`history_verify`) passes before and after every
   rollback/fork test.
3. **Fail closed** — corrupted digests, failed chain verification, unknown
   references, policy incompatibility, missing operator grants, shield scan
   failures, and unparseable state all refuse with an error; there is no
   "continue anyway" path (`gate::tests::shield_scan_failure_fails_closed`,
   `corrupted_checkpoint_is_refused_by_every_consumer`).
4. **No silent policy downgrade** — shield may only add findings and
   pre-execution evaluation on top of PolicyEvaluator/ModeManager/
   SandboxManager/Safety; it never relaxes them and has no parallel kill
   switch (its halt trips the *shared* kill switch).
5. **Imported skills never arrive ACTIVE** — `import_package` forces
   `ActivationState::Registered`; activation always requires the operator
   path and passes the security scan first.
6. **No raw skill code** — skill execution goes only through the existing
   tool/sandbox/policy path (WorkflowEngine/ToolRegistry/PolicyEvaluator/
   SandboxManager); hooks execute with the same permission checks as tools.
7. **Model output never changes permissions** — instinct/learned content,
   proposals, and any model-authored material are inert until an operator
   approval gate activates them; the approval path is the existing one and
   is audited.
8. **Provenance survives everything** — skill id+version+digest, genome
   id/version/digest, checkpoint digests, and per-ledger-row digests are
   recorded at creation and re-verified at every consumer.
9. **Remote marketplace = honest UNAVAILABLE/BLOCKED** under Sovereign Mode;
   a local catalog exists, but no network backend is implemented (Phase 11
   territory).

## 3. The subsystems (end to end)

### 3.1 Checkpoint / recovery / rollback / time machine

```
run_workflow  ──►  run_workflow_checkpointed   (the ONE checkpointing path;
   │                                            checkpoint_workflow was a
   │                                            dead duplicate and was removed)
   ├─ checkpoint: run id, history cursor, digest chain, genome + policy
   │    references, task/node states (openrt-checkpoint model)
   ▼
boot (Runtime::build)  ─►  boot_recover
   │  discover incomplete work → classify (classify.rs) → reconcile
   │  interrupted/stale work is NEVER marked successful; stale sandbox is
   │  recorded without being touched → RecoveryRecord (recovery.*)
   ▼
resume:  resume_workflow (checkpoint.rs)  /  task_recover (recovery.rs)
   │  uncertain non-idempotent steps are not blindly duplicated;
   │  recovery evidence + records are written to the ledger
   ▼
rollback:  validate → policy compat (policy.rs) → plan (plan.rs) →
   operator grant → restore → verify → audit   (rollback.rs)
   │  restores supported state ONLY; ledger rows are never rewritten;
   │  chain verification passes before and after
   ▼
time machine (timemachine.rs, all over the one ledger):
   history_timeline / history_state_as_of / history_fork (+fork_commit,
   fork_verify) / history_branch_compare / history_forks /
   history_return_to_current
```

### 3.2 Skills lifecycle

```
execution history ─► skill_submit_from_history (content, not identity)
   └─ lands Registered/Generated — never active
        ▼
validate (seal) ─► security_scan_for_activation (manifest scan; findings to
   │               shield.findings; refuses flagged content) ─► test
   │               (skill_test_run) ─► operator approve ─► activate
   ▼
execute:  skill_load → compiled graph / WorkflowEngine through the real
   gate (PolicyEvaluator ∩ caller ∩ skill ∩ tool ∩ sandbox) — effective
   permissions are an intersection, never an expansion
   ▼
recorded: skill id + version + digest; execution lands in the ledger
```

Install path: `assets_install_skill` → `import_package` forces **Registered**
(`store.rs`), then the same validate → scan → approve → activate chain.
Export/import roundtrip preserves content and re-forces Registered; tampered
packages fail digest verification on import. Loader validation
(`skill.rs::skill_load`) refuses bad tool/model compatibility, unknown
versions (never substituted), and workflow skills without a compiled graph.

### 3.3 ECC layer

- **Rules** — one rule layer seeded from the existing asset store, evaluated
  at the gate with exact scope/subject matching
  (`gate::tests::asset_rule_layer_*`); a deny rule overrides policy allow, an
  allow rule can never grant, and scope mismatches never deny.
- **Hooks** — SessionCreated/SessionEnded/RunStarted/RunCompleted/
  TaskStarted/TaskCompleted/AssetActivated/SkillActivated/WorkflowStarted/
  WorkflowCompleted fired at real runtime points; a tool-call hook that
  fails closed blocks execution (`m10_gate_tool_call_hook_fails_closed`);
  hooks run under the same permission checks as tools.
- **Instincts** — `instinct_submit_from_history` derives evidence-backed
  instincts from real ledger rows (lands Registered, unactivated);
  approval gate = validate (`Registered → Validated`) → operator activation
  (`Validated → Active`), wrapped as `assets_validate_instinct` /
  `assets_activate_instinct` (CLI `instinct validate|activate`). Active
  instincts influence run context within a hard bound only
  (`m10_instinct_influence_on_run_context_is_bounded`) — never permissions.
- **Learning** — `openrt-selfimprove` derives evidence-backed learnings
  deterministically and locally (`m57_learning_derivation_is_deterministic_and_local`,
  `m10_learning_records_are_evidence_backed`); proposals require evaluation
  before approval and approval before apply (all negative-tested).
- **Hooks × permissions** — `hook_permission_denied_fails_closed`.

### 3.4 Agent genome

- Extended schema validated fail-closed at apply time
  (`m10_genome_runtime_validation_fails_closed`).
- `session_apply_genome` records id/version/digest into the session and
  refuses in-flight or archived sessions
  (`m10_session_apply_genome_records_id_version_digest`,
  `m10_session_apply_refuses_in_flight_and_archived`).
- Genome application activates only the referenced, already-validated assets
  (`genome_apply_activates_referenced_assets`) — it cannot mint approvals.
- Evolution goes through the approval gate only
  (`m10_genome_evolution_is_approval_gated`); version isolation pins exact
  references (`genome_version_isolation_pins_exact_references`).

### 3.5 AgentShield

- One evaluation model: `SecurityFinding` (severity → ALLOW / WARN /
  REQUIRE_APPROVAL / DENY / HALT) produced by `openrt-shield`
  (`detectors.rs` dangerous commands, credential access, filesystem escape,
  network escalation, sandbox bypass; `scan.rs` extension/manifest scans).
- Integrated once at the runtime gate (`gate.rs`): scan failure fails
  closed, Sovereign deny overrides policy allow, high severity routes
  through the resolver or fails closed without one, halt trips the shared
  kill switch and blocks execution.
- Findings persist to `shield.findings` and are audited by ids/categories —
  never secrets.
- Prompt-injection detection reuses the Phase 9 detector/types; no second
  injection implementation exists.
- Extension scanning (`scan_extension`) computes implied capabilities and is
  surfaced through doctor's `extension-scan` row.

## 4. Milestone design notes

### M10.1 — Step 0 reconciliation + allocations
The new `openrt-shield` crate owns the finding model and detectors; the
runtime gate owns evaluation-once integration; `openrt-runtime::recovery`
owns reconciliation at boot. Doctor rows `recovery`, `time-machine`,
`agentshield`, `extension-scan` all PASS on this host.

### M10.2 — Full checkpoint model + recovery integrity
`openrt-checkpoint` (model/manager/classify/policy/plan/rollback) stores run
id, history cursor, digest chain, and genome/policy references. Digest
sealing and tamper refusal are tested at both levels
(`checkpoint_creation_seals_digest_and_tamper_fails_integrity`,
`corrupted_checkpoint_is_refused_by_every_consumer` — every consumer
refuses, not just the writer).

### M10.3 — Rollback end-to-end
Seven-step path (validate → policy compatibility → plan → operator grant →
restore → verify → audit). In-flight/uncertain nodes are classified
(`classify.rs`) so resume is never a blind duplicate. The ledger is
append-only: `rollback_restores_state_and_never_rewrites_history` verifies
chain before and after.

### M10.4 — Crash recovery at boot + live handles
`boot_recover` (runtime.rs build path) discovers incomplete work, reconciles
interrupted tasks (never marking them successful), records stale sandboxes
without touching them, and writes `RecoveryRecord`s. `task_recover`
re-runs interrupted plain tasks to completion and refuses live/terminal/
workflow-backed tasks. Live task handles (`live_handle_spawn_cancel_execute_lifecycle`)
spawn, observe, and cancel with recorded terminal states.

### M10.5 — Time machine + surfaces
Timeline/state-as-of/fork/branch-compare/return are Runtime methods over the
one ledger (`timemachine.rs`), fail-closed on tampered ledgers and
out-of-range forks. TUI: `f` fork, `c` compare, `a` as-of, `v` return
(+ timeline view), errors surface on the status line, never fatal. WebUI
palette: `history.tail`, `history.timeline`, `history.fork`,
`history.compare`, `history.state`, `history.return` — dispatch goes through
the same Runtime methods (no UI-local state; `require_operator` on writes).

### M10.6 — Full skills system
Manifest, registry ops, loader validation, execution through the real gate,
permission intersection, exact version resolution (never substituted),
export/import (forces Registered), security scan, testing, history-derived
generation, compilation to a linked graph, and a local catalog marketplace
with remote reported `UNAVAILABLE` under Sovereign.

### M10.7 — ECC
Rule layer at the gate, full hook lifecycle at real runtime points,
instinct extraction/approval/application with bounded influence,
evidence-backed deterministic learning, and approval-gated proposals
(baseline → propose → evaluate → approve → apply).

### M10.8 — Agent genome
Extended schema, runtime validation fail-closed, fresh-session application
recording id/version/digest, referenced-asset activation only, approval-gated
evolution, exact version isolation.

### M10.9 — AgentShield
`SecurityFinding`/severity/action model, seven detector families, single
gate integration, extension scanning, persisted + audited findings, shared
kill switch for HALT.

### M10.10 — Final gate
Full workspace gate (fmt/check/test/doctor), live CLI smoke transcript,
forensic production-caller audit, security audit, regression proof, and this
document + the acceptance document. Evidence in
[`PHASE10_ACCEPTANCE.md`](./PHASE10_ACCEPTANCE.md).

## 5. Production surfaces

### CLI (all verified against a real store this host)

| Area | Commands |
|---|---|
| Recovery | `recovery scan\|list\|inspect\|restore\|rollback\|resume` |
| History / time machine | `history verify\|tail\|replay\|fork\|fork-commit\|fork-verify\|compare\|return\|list\|timeline` |
| Tasks | `task run\|list\|resume\|retry` |
| Skills | `skill list\|inspect\|load\|execute\|compile\|test\|marketplace` |
| Shield | `shield scan\|findings\|inspect` |
| Assets | `assets list\|register\|generate\|validate\|activate\|candidate-validate\|candidate-approve\|candidate-activate\|hook\|rule\|genome\|export\|import` |
| Instincts | `instinct list\|inspect\|generate --session\|validate\|activate --operator` |
| Genome | `genome list\|inspect\|evolve` |
| Learning | `improve list\|baseline\|propose\|evaluate\|approve\|reject\|apply\|learn` |
| Doctor | `doctor` (44 rows) |

### TUI
Recovery/history views; time machine keys `f` (fork), `c` (compare),
`a` (state as-of), `v` (return to current); timeline of the shared ledger.

### WebUI palette
`history.tail`, `history.timeline`, `history.fork`, `history.compare`,
`history.state`, `history.return`, `recovery.*`, skill/genome/improve
commands — every dispatch is a Runtime method over the shared store.

## 6. Test architecture

- `crates/integration-tests/tests/p10_*.rs` — 64 tests in eight suites:
  `p10_recovery` (2), `p10_recovery_extra` (7), `p10_skills` (12),
  `p10_shield` (10), `p10_ecc` (14), `p10_ecc_extra` (3),
  `p10_integration` (8), `p10_e2e` (8, TEST A–H).
- Crate units: `openrt-shield` 39, `openrt-checkpoint` 35,
  `openrt-runtime` 78, `openrt-timeseries` 6, `openrt-workspace-assets` 8,
  `openrt-tui` 10, `openrt-webui` 26, `openrt-safety` 6,
  `openrt-selfimprove` 24 (1 lib + 23 `tests/improve.rs`), `openrt-orch` 18.
- Full workspace gate: **1283 passed / 0 failed / 0 warnings** — includes
  every Phase 5–9 suite unchanged.
