# Phase 6 implementation — workflow compiler + fine-grained scheduler

Date: 2026-09-24. Scope of this slice: workflow compiler (`openrt-compiler`),
fine-grained concurrent scheduler inside the existing `WorkflowEngine`, and
production callers on Runtime / CLI / TUI / WebUI, with real integration tests
and a forensic acceptance audit. Phase 5 remains **frozen foundation**
(586/0 preserved inside the 654/0 workspace gate below).

## 1. Invariants honored

- Compiler produces a PLAN only; `openrt_orch::WorkflowEngine` remains the sole
  execution authority (no second DAG engine, scheduler, verifier, or store).
- Executable graph persists under the existing `dag` namespace; compiled
  metadata under `compiled` (OWNERSHIP Phase 6 table).
- Fail closed: schema / capability / permission / DAG / Sovereign / resource
  gates return structured WHAT/WHY/WHERE/FIX diagnostics; raw LLM plan JSON is
  never executed without validation.
- No invented capabilities; no permission bypass; no mandatory cloud path;
  bounded replanning only (default 1 attempt; `max_replan=0` disables).
- LLM plan is an optional *override* of the deterministic decomposer; when a
  capability gap fails the LLM plan, the fallback is **deterministic
  decompose only** (never re-prompts the model). Original + correction reason +
  final plan preserved in `ReplanRecord`.

## 2. Crate: `openrt-compiler`

| File | Role |
|---|---|
| `goal.rs` | NL → `GoalSpec` (provenance-preserving; no fabricated inputs) |
| `decompose.rs` | deterministic rule-based decomposition; optional LLM plan override |
| `capability.rs` | capability plan against real tool names + policy + mode; `parse_llm_plan` schema validation |
| `graph.rs` | builds existing `openrt_orch::Workflow` + `Provenance` |
| `validate.rs` | 14–15-point `ValidationReport` (15th check only when `expected_outputs` non-empty) |
| `persist.rs` | `compiled` + `dag` round-trip via foundation `PersistenceStore`; `CompiledRecord.replan` |
| `diagnostics.rs` | structured compile diagnostics (`UnsupportedGoal`, `AmbiguousGoal`, `MissingCapability`, `UnavailableModel`, …) |
| `modes.rs` | `Explain \| Compile \| Validate \| Execute` |
| `compile.rs` | `WorkflowCompiler` entry: understand → reject unsupported/ambiguous → decompose/LLM plan → capability → graph → validate → **bounded replan** → persist |

Constants: `COMPILED_NS = "compiled"`, `COMPILER_VERSION = "0.1.0"`.

Remediation additions:
- `ReplanRecord { original, correction_reason, attempts, final_decomp }`
  exported and attached to both `CompileOutput` and `CompiledRecord`.
- `reject_unsupported_or_ambiguous` (stage 1b) fails closed when the request
  has no actionable verb/file/network marker, or mixes create/write with
  delete/remove without sequencing.
- Bounded replan correctability expanded: structural graph failures always;
  capability gaps only when the first plan was LLM-sourced.

Gate: `cargo test -p openrt-compiler --offline` → **35 passed / 0 failed**.

## 3. Fine-grained scheduler (`openrt-orch`)

`WorkflowEngine` replaced the wave barrier with a continuous readiness loop:

- Fields: `max_concurrency` (0 = unlimited), optional `admission` callback.
- Builders: `with_max_concurrency`, `with_resource_admission`.
- Loop: spawn ready steps up to limits → `joinset.join_next()` (exactly one
  completion) → process outcome → checkpoint fail-closed → re-evaluate.
- Admission deny with idle engine → `WorkflowError::Condition("resource admission denied: …")`.

Wired from production Runtime in `checkpoint.rs` via
`.with_resource_admission(...)` over the live `ResourceGate`.

Timing proof (unit): diamond `a→{b,c,d}→e` branches overlap pairwise, join
waits for all (`diamond_branches_run_concurrently_and_join_waits_for_all`);
`max_concurrency` bounds peak in-flight; dependent starts immediately after
completion (`fine_grained_scheduler_starts_dependent_immediately_after_completion`).

Gate: `cargo test -p openrt-orch` → **13 passed / 0 failed**.

## 4. Production callers (not test-only)

| Surface | Entry |
|---|---|
| Runtime | `compile_workflow`, `load_compiled_workflow`, `run_compiled`, `compile_and_run` (`runtime.rs:616+`) |
| Checkpointed run | resource admission on every spawn (`checkpoint.rs`) |
| CLI | `openrt workflow {compile,inspect,validate,explain,run,list}` (`main.rs:1380`) |
| TUI | key `w` → `App::compile_workflow_from_context` (`run.rs:102`, `app.rs:322`) |
| WebUI | palette `workflow.{compile,list,inspect,validate}` (`palette.rs:495+`) |

Policy authority is the same `permission_policy` the gate enforces
(`PolicyEvaluator` over `PermissionRequest::capability`); mode comes from live
`ModeManager`; tools from live `ToolRegistry::names()`.

All surfaces share the same store → compiled workflows are visible across CLI,
TUI, and WebUI without recompilation.

## 5. Integration tests

### `p6_workflow_compiler.rs` → **11 passed / 0 failed**

1. NL → compile_and_run end-to-end (provenance + `compiled`/`dag`/`runs`).
2. Parallel independent branches (no sibling deps; aggregate depends on all).
3. Empty-goal fail-closed + admission-dependent-spawn denial.
4. Resource limit blocks compile with structured diagnostic.
5. Permission denial (default deny of `filesystem.write`).
6. Sovereign blocks network goal.
7. Persistence round-trip + re-run via orchestrator.
8. Invalid / cyclic LLM plan rejected (`InvalidLlmPlan` / non-empty diags).
9. Provenance + named validation checks on persisted record.
10. List/inspect/validate surface via Runtime methods (CLI/TUI/WebUI share).
11. (covered above)

### `p6_milestones.rs` (brief M6.2–M6.6) → **17 passed / 0 failed**

- **M6.2** five decomposition cases: sequential ordered DAG, parallel
  independent branches, ambiguous fail-closed, unsupported fail-closed,
  distinct goals → distinct structures.
- **M6.3** five capability-planning negatives: empty, permission, resource,
  Sovereign, unavailable model.
- **M6.4** LLM capability-gap → bounded replan with full provenance
  (`ReplanRecord` on output + persisted record); `max_replan=0` fails closed.
- **M6.5** diamond workflow runs through production `compile_and_run`;
  every step reports; join waits for all.
- **M6.6** final gate: full e2e with `LocalFileStore` restart recovery +
  integrity (`compiler_version`, goal digest id, validation) + re-execute;
  five security negatives; invalid LLM plan leaves empty store; cross-surface
  `list_compiled` visibility.

## 6. Workspace gates (post-remediation)

```
cargo fmt --all -- --check          → RC=0
cargo check --workspace --offline   → RC=0
cargo test --workspace --offline    → 654 passed / 0 failed (RC=0)
```

Phase 5 baseline 586/0 preserved (log: `/tmp/opencode/p5_final_remediation_test2.log`).
Workspace log: `/tmp/opencode/p6_workspace.log`.

## 7. Acceptance

See `PHASE6_ACCEPTANCE_M61_M66.md` for the brief M6.1–M6.6 matrix (all GREEN)
with scope footnote for roadmap M6.2–M6.9 and supervisor.
