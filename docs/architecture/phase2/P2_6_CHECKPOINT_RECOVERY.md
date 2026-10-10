# Phase 2.6 — Checkpoints, Recovery, and Dry-Run

**Status:** Implemented
**Date:** 2026-09-18
**Scope:** ROADMAP M2.6 — durable workflow checkpoints, bounded recovery
(resume/retry-only), explicit dry-run, and a rollback-safe extension of the
Phase 1 foundation store, closing the safety loop. Adds `openrt-checkpoint`
(implemented as an owned crate as planned) with the engine-side fail-closed
checkpoint hook, the runtime recovery/rollback/dry-run surface, the
`CheckpointId` domain type, 11 new event kinds (27 → 38), CLI recovery
commands, and the P2.6 integration suite. Baseline behavior unchanged
(201 → 240 tests, all green, stable ×3).

## 1. Verification (before changing anything)

- All milestones gated green: fmt / check / clippy `-D warnings` / build /
  `cargo test --workspace` (201 passed ×3 stable).
- ADR-006 (one store, namespaced records) and ADR-008 (HALT blocks recovery,
  never auto-clears) are already accepted and prescriptive — no ADR
  amendment needed. The "uncertain/non-idempotent outcomes are never blindly
  replayed" rule is P2.6's own checkpoint safety invariant, layered on the
  ADR-006 classified store (ADR-009 governs P3 MoE expert caching, not
  execution recovery). The P2.6 recovery surface applies these invariants
  directly.
- No `tempfile` dependency in the workspace (tmp dirs via
  `std::env::temp_dir().join("openrt-{tag}-{pid}-{nanos}")`), deterministic
  tests only.

## 2. Design decisions

### 2.1 Crate layout — `openrt-checkpoint`
- Owned by `openrt-checkpoint` (allocated in OWNERSHIP.md; namespaces
  `exec.checkpoints`, `exec.dryrun`, `exec.rollback`). Contains the model
  (checkpoint schema v1), effect/state classifier, recovery policy, planner,
  manager (store CRUD + schema validation), and rollback store — all under one
  crate so the persistence format and its semantics change together.
- `openrt-orch` owns the engine hook (`CheckpointHook` trait) and
  `RunProgress`/`ResumeSeed`, plus cumulative-attempt retry semantics. Data
  flows checkpoint → orchestrator hook → runtime → both crates.
- `CheckpointId` lives in `openrt-core` (shared domain primitive, like
  `SessionId`/`TaskId`).

### 2.2 Checkpoint model (`checkpoints/`)
- `Checkpoint` (schema v1): ids, session/task/workflow, kind
  (`Initial`/`AfterNode`/`Final`), status (`Running`/`Completed`/`Failed`/
  `Cancelled`), per-node `NodeStatus` (state, attempts, effect, failure
  category), derived `completed`/`pending`/`failed`/`uncertain` lists,
  `retry_counters` (cumulative attempt budget), deterministic blackboard
  snapshot, artifact refs, and `CheckpointExecution` (previous checkpoint id
  for chain linking + embedded run-report snapshot + rollback note).
- **Single source of truth:** the checkpoint record is the *only* durable
  artifact; there is no hidden or parallel store (ADR-006). Rows are written
  through `CheckpointManager` which validates schema version and list
  coherence before every write/read; invalid records refuse to load
  (`CheckpointError::Invalid`).

### 2.3 Effect & state classification (safety loop)
- Every node carries a `NodeEffect`: `IdempotentClean` (safe to replay),
  `Irreversible` (never auto-replayed), or `Unknown` (default — **never**
  auto-replayed). Recovery policy can never override this safety invariant.
- `NodeState::Running` means "outcome unknown" at snapshot time; such nodes are
  classified `Uncertain` and are never blindly re-executed. They escalate to
  operator intervention by the uncertain-outcome safety invariant (unless
  classified clean and the policy explicitly allows replaying clean uncertain
  nodes).
- `FailureCategory` is persisted with each failed node (`model_failure`,
  `tool_failure`, etc.) so the recovery policy can decide which failures are
  retryable (default: transport-level model failures).

### 2.4 Recovery policy & planner
- `RecoveryPolicy`: `max_attempts` (absolute cumulative cap per node),
  `retry_backoff_ms`, `retryable` categories, `uncertain_policy`
  (`AutoWhenSafe`/`Intervene`, default `AutoWhenSafe`), `attempt_rollback`.
- `RecoveryScope`: `Resume` (continue + retry eligible failed nodes) vs
  `RetryOnly` (only failed/uncertain nodes, never new pending work).
- The planner (in `openrt-checkpoint::plan`) runs only after a 6-field
  preflight (halted? · resources? · session exists? · workflow def exists? ·
  policy ready); it produces a per-node `RecoveryAction`
  (`Resume`/`Retry`/`Skip`/`Intervention`) with non-idempotent escalation,
  and honors the scope.

### 2.5 Engine hook —`CheckpointWriter` (fail-closed)
- `WorkflowEngine` takes an optional `CheckpointHook`. Each completed node
  triggers `on_node_completed(RunProgress)`; the hook builds an `AfterNode`
  checkpoint **after the result is durably recorded**. The engine **fails
  closed**: any hook error returns `WorkflowError::Checkpoint` and aborts
  before the next node is scheduled — the prior checkpoint chain is preserved.
- `ResumeSeed` carries: `done` (completed/skipped outputs re-fed into
  `only_if`), `attempts_by_step` (cumulative prior-run attempt budgets), and
  `attempt_limits` (policy caps). The engine merges those into `StepEnv`
  (`base_attempts`), so resumed retries keep counting from where the prior run
  stopped (`total = max(limit, base+1)` attempts when capped, else
  `base + max_retries + 1`); `RetryScheduled`/`RetryExhausted` events report
  cumulative totals.
- `run_with_seed` wires `seed.attempts_by_step` into `self.env.base_attempts`
  (a `&mut self` path), fixing the bug where resumes forgot prior attempts and
  retried with a budget of 1.

### 2.6 Runtime surface
- `run_workflow` (fresh runs), `resume_workflow` (scope `Resume`),
  `retry_failed` (scope `RetryOnly`), `resume_dry_run`, `list_checkpoints`,
  `load_checkpoint`, `delete_checkpoint`.
- `seed_from_checkpoint` reconstructs initial node states for **all** states
  (Completed objects, Skipped objects, Failed/Running budgets), so a failed
  node's consumed attempts survive the restart.
- A resumed run's `Initial` checkpoint links back to the checkpoint being
  resumed (`previous_checkpoint_id`), producing one linked chain across
  incremental runs.
- `CheckpointWriter::resume_from` chains the new run's writer onto the resumed
  checkpoint id.

### 2.7 Rollback store (`rollback/`)
- `exec.rollback` records per-checkpoint action op-log entries
  (`RollbackOp` metadata) with `RollbackStarted`/`RollbackCompleted`/`RollbackUnavailable`
  events; unsupported ops are reported as `RollbackUnavailable`, never faked.

### 2.8 Dry-run (`dryrun/`)
- `resume_dry_run` runs the same load → plan → preflight path but never
  executes nodes: it persists a `DryRunReport` under `exec.dryrun`, emits
  `RecoveryStarted` with the full `RecoveryPlan`, and returns
  `WorkflowError::DryRunResult`. Plan node effects map to a faithful
  "would-do" trace (dry-run of any workflow produces an auditable trace =
  M2.6 DoD).

### 2.9 Events (11 new kinds, 38 total)
`CheckpointCreated`, `CheckpointLoadFailed`, `CheckpointDeleted`,
`RecoveryStarted`, `WorkflowResumed`, `RetryScheduled`, `RetryExhausted`,
`RollbackStarted`, `RollbackCompleted`, `RollbackUnavailable`,
`RecoveryNeedsIntervention`.

### 2.10 CLI
- `openrt checkpoint list [--workflow ID] [--json]`,
  `openrt checkpoint inspect ID`, `openrt checkpoint delete ID`.
- `openrt task resume --session S --workflow W --checkpoint C` (scope Resume),
  `openrt task retry --session S --workflow W --checkpoint C` (scope
  RetryOnly). Both load the persisted DAG from `exec.dag`, run with the
  default safe policy (all effects `Unknown`), and print per-step attempts.

## 3. Tests

- `openrt-checkpoint`: 30 unit tests (classification, policy, planner, manager
  CRUD/validation, chaining, dry-run, rollback store).
- `openrt-orch`: engine tests now include the checkpoint hook (AfterNode hook
  called after every node; hook failure aborts before the next node) → 17
  runtime tests, 6 engine tests.
- `openrt-runtime`: checkpoint writer + recovery unit coverage.
- `crates/integration-tests/tests/p2_6_checkpoint_recovery.rs` — 7 scenario
  tests exercising the real (in-memory + LocalFileStore) runtimes:
  1. `restart_resume_from_final_checkpoint_across_runtime` — a runtime crash
     loses nothing; a *second* runtime resumes the persisted Final checkpoint,
     plan/code steps complete exactly once more (cumulative attempts are
     preserved), and the checkpoint chain grows 3 → 7 fully linked records.
  2. `node_failure_bounded_retry_across_runs` — a flaky provider fails the
     code node; the failed checkpoint retries on resume, succeeds, and
     `RetryScheduled`/`RetryExhausted` carry cumulative attempt numbers.
  3. `uncertain_non_idempotent_never_blindly_replayed` — a `Running` snapshot
     (unknown outcome) escalates to `RecoveryNeedsIntervention`; no node is
     replayed (uncertain-outcome safety invariant).
  4. `halt_blocks_resume_until_cleared` — ADR-008: while the kill switch is
     engaged the resume is refused with a RecoveryNeedsIntervention/Preflight
     event and nothing changes; clearing halt resumes normally.
  5. `failing_store_preserves_prior_checkpoint_chain` — a store failure while
     writing a checkpoint aborts the run but the previously persisted
     checkpoint chain stays loadable (fail-closed).
  6. `dry_run_has_zero_side_effects` — dry-run of a failing workflow returns a
     `WouldComplete` plan with no session/task/blackboard/run-record changes
     and no checkpoint rows written.
  7. `checkpoint_list_and_delete_round_trip` — manager list/load/delete plus
     `CheckpointDeleted` event.

Test helpers: `FlakyProvider` (fails the first N `generate()` calls with a
retryable `ModelError::Transport`), `FailCheckpointStore` (delegation store
that fails `put_record` on `checkpoints` after the first success), `wait_for`
(event assertions).

## 4. ADR/roadmap DoD mapping

| Requirement | Where |
|---|---|
| Checkpoint/rollback restores relevant runtime state | `resume_workflow`/`retry_failed` restore node states, blackboard, retry budgets |
| Forced-failure test demonstrates recovery | integration tests 1–5 |
| Dry-run produces a faithful auditable "would-do" trace | `resume_dry_run` → `WouldComplete` + `DryRunAction` trace |
| Recovery blocked while halted, never auto-clears | ADR-008 preflight + test 4 |
| Uncertain/non-idempotent never blindly replayed | uncertain-outcome classifier + test 3 |
| One storage namespace per subsystem | `exec.checkpoints` / `exec.dryrun` / `exec.rollback` (ADR-006) |

## 5. Not in this milestone

- P2.7 (Definition-of-Done audit / phase gate) and all roadmaps beyond M2.6.
- Cross-checkpoint filesystem/knowledge fuses via a real VLM/knowledge
  pipeline (P4).