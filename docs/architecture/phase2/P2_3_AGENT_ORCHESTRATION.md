# Phase 2.3 — Agent Orchestration (workflow DAG on the event bus)

**Status:** Implemented
**Date:** 2026-09-17
**Scope:** ROADMAP M2.1 (master agent + typed sub-agents + shared blackboard)
and M2.2 (workflow DAG on the event bus). Adds `openrt-blackboard` and
`openrt-orch` and extends `openrt-runtime`/`openrt-persistence`/`openrt-core`.
No Phase 3–6 features, no workspace rewrites, baseline behavior unchanged
(155 tests pass; baseline 108 preserved).

## 1. Verification (before changing anything)

- No blackboard existed; role sub-agents had no shared coordination memory.
- `PersistenceStore` exposed only typed namespaces
  (`sessions`/`tasks`/`executions`/`events`); DAG/blackboard state had nowhere
  to live under the ADR-006 "one store" rule.
- `EventKind` had no orchestration lifecycle kinds; run.rs Pipeline was the
  only execution shape (single agent, single prompt).

## 2. Design decisions

### 2.1 Generic namespaced records on the one store (ADR-006)

`PersistenceStore` gained a JSON KV API: `put_record` / `get_record` /
`list_records` / `delete_record` over a validated `(namespace, key)` pair
(both restricted to `a-z0-9._-`, so keys can never escape their namespace
directory). Implemented in both backends. On `LocalFileStore` generic records
inherit the SEC-03 checksum manifest, atomic writes, `0600`/`0700`
permissions and `clear()` sweep (record deleted ⇒ manifest entry removed).
P2.3 uses namespaces `runs`, `dag`; `exec.checkpoints` (P2.6) follows the same
path. P2.2 semantics unchanged (checksum backfill compatible).

### 2.2 openrt-blackboard — shared coordination memory

`Blackboard` is a validated key → JSON map with deterministic `keys()`/
`snapshot()` (sorted), `to_json`, `restore` (delta-persisted), and optional
write-through persistence (`Blackboard::with_store` hydrates from
`exec.blackboard` at open). No watchers/ownership inside the blackboard — the
orchestrator decides who writes; snapshots are serde data for P2.6
checkpoints and ADR-002 ingest.

### 2.3 openrt-orch — roles, DAG, engine

- `SubAgentKind` fixes the M2.1 role inventory (planner/researcher/coder/
  verifier/critic) with stable `role_prompt()` system framings that the engine
  prepends to every step task, so any `Agent` implementation can serve a role.
- `Workflow` / `WorkflowStep` are a serializable DAG: id, role, prompt,
  `depends_on`, `max_retries`, and an optional `only_if` condition
  (`BlackboardEquals`, `BlackboardExists`, `OutputContains`) evaluated against
  dependency outputs/blackboard. `topological_sort()` rejects unknown deps,
  duplicate ids and cycles.
- `WorkflowEngine` schedules **waves**: each iteration spawns every ready step
  on a tokio `JoinSet`; the wave resolves before the next starts, so
  independent steps run concurrently while ordering is deterministic. Retries
  re-run a failed step up to `max_retries`; a step that exhausts retries fails
  and its dependents are never scheduled (reported runs list only executed
  steps). A step whose `only_if` is false is skipped (its dependents still
  run).
- Orchestration events are additive on the bus: `WorkflowStarted`,
  `WorkflowStepStarted`, `WorkflowStepCompleted`, `WorkflowStepFailed`,
  `WorkflowCompleted`, all correlated (`session_id`/`task_id`) and attributed
  to the runtime's master-orchestrator `agent_id`.

### 2.4 Runtime integration

`Runtime::run_workflow(session, wf)` serializes per-session (SEC-08), records
the run as a Task + ExecutionRecord, runs the DAG over per-role `MinimalAgent`s
sharing the runtime's provider/executor/gate, persists the workflow
definition (`dag/<id>`) and `WorkflowRunReport` (`runs/<id>`), folds a summary
into the session transcript and emits `TaskCompleted`/`TaskFailed` matching
the run status. `Runtime::blackboard()` exposes the shared blackboard. The
runtime's stable master identity (`Runtime::orchestrator`) is the `agent_id`
of orchestration events.

## 3. Files changed

| Path | Change |
|---|---|
| `crates/openrt-core/src/event.rs` | +5 `EventKind`s, `all_kinds_serialize` extended |
| `crates/openrt-persistence/src/store.rs` | generic KV API + `validate_ns_key`, clear() sweeps all namespaces, missing-namespace list = empty |
| `crates/openrt-blackboard/*` (new) | blackboard + snapshot/restore + write-through |
| `crates/openrt-orch/*` (new) | agent/workflow/engine modules |
| `crates/openrt-runtime/src/runtime.rs` | `run_workflow`, `role_agents`, `blackboard()`, `orchestrator`, `Error::Orchestration` |
| `crates/openrt-runtime/src/lib.rs` | new error variant |
| `crates/{runtime,integration-tests}/Cargo.toml`, workspace `Cargo.toml` | members + deps |
| `crates/integration-tests/tests/p2_3_orchestration.rs` (new) | end-to-end DAG + failure-path tests |

## 4. Tests added

- persistence: `namespaced_records_round_trip`, `namespaced_records_tamper_
  detect_on_disk`, `clear_wipes_generic_namespaces` (14 total in crate).
- blackboard: put/get/remove/keys + reject unsafe keys; snapshot/restore JSON
  round-trip; write-through + reopen across instances (3 total).
- orch unit (echo-role agents, no model dep): sequential DAG + artifacts +
  events + `dag`/`runs` persistence; retry-then-recover; exhausted-retry ⇒
  Failed; `only_if` skip; cycle/unknown-role structural errors (4 total).
- integration: `workflow_dag_runs_end_to_end` (completed: artifacts, task/exec
  records, namespaced persistence, ordered events, correlation, transcript
  fold) and `failing_role_fails_the_workflow_and_task_records` (Failed status,
  retries recorded, dependent block never scheduled, `WorkflowStepFailed` +
  `WorkflowCompleted`+`TaskFailed` emitted) (2 total).

## 5. Quality gate (all green)

`cargo fmt --all -- --check` clean · `cargo check --workspace` clean ·
`cargo clippy --workspace --all-targets --all-features -- -D warnings` clean ·
`cargo test --workspace` 155 passed / 0 failed · `cargo build --workspace` ok.

## 6. Documented gaps (not silently accepted)

- Wave scheduling is wave-serial: a step can only run in the wave after all of
  its dependencies' wave has joined. Fine for M2.1/M2.2; a fine-grained
  "spawn as soon as deps finish" scheduler is future work.
- Cancellation: `run_workflow` is synchronous (`await`) and checks the token
  only between waves; a step's own agent run cooperates via
  `AgentContext::cancel`. No `spawn_workflow`/`TaskHandle` variant yet (P2.6
  recovery reuses the blackboard snapshots).
- Report lists executed steps; blocked steps (dependency failed) are absent.
- Role prompts are injected by prompt-framing (the runtime's `MinimalAgent`
  has no system-prompt slot), not as a provider-level system message.