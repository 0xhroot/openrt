# P2.6 Milestone Final Report — Checkpoints, Recovery, and Dry-Run

**Status:** Complete
**Date:** 2026-09-18
**Gate:** fmt / check / clippy `-D warnings` / `cargo test --workspace`
(201 → 240 passed, stable ×3) / build — all green.

## Scope delivered (ROADMAP M2.6)

- **`openrt-checkpoint`** crate: checkpoint schema v1, state/effect
  classification (idempotent-clean / irreversible / unknown, failure
  categories), recovery policy, planner (resume/retry/skip/intervention),
  manager (CRUD + schema-validating load), dry-run reports, rollback op-log.
- **Engine fail-closed checkpointing** (`openrt-orch`): `CheckpointHook` +
  `CheckpointWriter` after every durably-recorded node result; hook failure
  aborts before the next node; cumulative-attempt resume semantics
  (`ResumeSeed`, `base_attempts` wiring fix).
- **Runtime recovery surface** (`openrt-runtime`):
  `run_workflow`, `resume_workflow`, `retry_failed`, `resume_dry_run`,
  `list_checkpoints`, `load_checkpoint`, `delete_checkpoint`; chained
  checkpoints across resumed runs; ADR-008 preflight gating.
- **CLI:** `openrt checkpoint list|inspect|delete`, `openrt task resume|retry`.
- **Events:** 11 new kinds (`CheckpointCreated`, `CheckpointLoadFailed`,
  `CheckpointDeleted`, `RecoveryStarted`, `WorkflowResumed`, `RetryScheduled`,
  `RetryExhausted`, `RollbackStarted`, `RollbackCompleted`,
  `RollbackUnavailable`, `RecoveryNeedsIntervention`) — 38 total.
- **Storage** (ADR-006): single store, new namespaces `exec.checkpoints`,
  `exec.dryrun`, `exec.rollback`; all records inherited SEC-03 checksums.

## Key invariants verified by tests

1. Runtime crash loses nothing — a second runtime resumes the persisted Final
   checkpoint and per-node cumulative attempts survive (`restart_resume...`).
2. Bounded retry across runs: a flaky node's consumed attempts are honored on
   resume (`node_failure_bounded_retry_across_runs`).
3. Uncertain/non-idempotent outcomes escalate to operator intervention and are
   never blindly replayed (`uncertain_non_idempotent_never_blindly_replayed`).
4. ADR-008: HALT blocks resume; `clear_halt` restores recovery
   (`halt_blocks_resume_until_cleared`).
5. Fail-closed: a store failure while checkpointing aborts the run but the
   prior checkpoint chain survives (`failing_store_preserves_prior_...`).
6. Dry-run is zero-side-effect and produces an auditable would-do trace
   (`dry_run_has_zero_side_effects`).
7. Checkpoint list/load/delete round-trip with events
   (`checkpoint_list_and_delete_round_trip`).

## Test count delta

- `openrt-checkpoint`: 30 unit tests.
- `openrt-orch` engine: hook coverage (AfterNode after every node; hook
  failure aborts) — 6 engine tests; runtime 17.
- Integration: `p2_6_checkpoint_recovery.rs` — 7 scenario tests.
- Workspace total 201 → 240, ×3 stable.

## Out of scope

P2.7 (DoD audit / phase gate), Phase 3+ roadmaps. ADR-009 (MoE expert cache)
is NOT part of recovery; the uncertain-outcome rule is the P2.6 checkpoint
safety invariant.

## Docs

- `docs/architecture/phase2/P2_6_CHECKPOINT_RECOVERY.md`
- `docs/architecture/OWNERSHIP.md` — Phase 2.6 change-log row (added).