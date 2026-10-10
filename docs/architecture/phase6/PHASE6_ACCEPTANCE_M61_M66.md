# Phase 6 acceptance — brief M6.1–M6.6 forensic status

Date: 2026-09-24 (remediation pass). Evidence is live-captured on this tree. A
milestone is GREEN only if production callers exist **and** real e2e tests pass
on current bytes; PARTIAL means a real subset is production-wired with green
evidence; BLOCKED means not started / no green evidence.

## Numbering reconciliation

The remediation brief redefines M6.1–M6.6 as **workflow milestones**. The
roadmap's `docs/roadmap/ROADMAP.md` M6.1–M6.9 are broader (long-running
agents, sandbox, secrets, federation, trading…). This matrix uses **brief
labels** with a roadmap cross-reference. Brief M6.1–M6.5 together complete the
"autonomous workflows with policy gates" component of roadmap M6.1; brief M6.6
is a workflow-scoped gate toward roadmap M6.9 (production release).

## Milestone status

| Brief | Brief title | Roadmap cross-ref | Status | Evidence |
|---|---|---|---|---|
| **M6.1** | Natural language → workflow | M6.1 component | **GREEN** | Production `Runtime::compile_workflow` → `WorkflowCompiler::compile` (understand → reject unsupported/ambiguous → decompose/LLM plan → capability → graph → validate → persist). E2E: `p6_workflow_compiler` test1, `p6_milestones` m62_*. CLI/TUI/WebUI all route through Runtime. |
| **M6.2** | Automatic task decomposition | M6.1 component | **GREEN** | 5 decomposition cases in `p6_milestones`: sequential ordered DAG, parallel independent branches, ambiguous goal fail-closed (`AmbiguousGoal`), unsupported goal fail-closed (`UnsupportedGoal`), distinct goals → distinct structures. Unit: `decompose` tests. |
| **M6.3** | Capability planning | M6.1 component | **GREEN** | 5 negative fail-closed cases: empty goal, permission denied (default deny), resource admission deny, Sovereign network block, unavailable model. All return structured diagnostics with how-to-fix. `p6_milestones` m63_*, `p6_workflow_compiler` tests 3–6. |
| **M6.4** | Autonomous compilation with bounded replanning | M6.1 component | **GREEN** | `ReplanRecord { original, correction_reason, attempts, final_decomp }` on `CompileOutput` + `CompiledRecord`. Correctable: structural graph failures always; capability gaps only when first plan was LLM-sourced (deterministic fallback only — never re-invokes LLM). `max_replan=0` fails closed. Tests: compiler unit + `p6_milestones` m64_*. |
| **M6.5** | Fine-grained scheduling | M6.1 component | **GREEN** | Continuous readiness loop + `max_concurrency` + resource admission in sole `WorkflowEngine` (orch 13/0 including diamond concurrency, max-bounds, dependent-immediate-start). Production path: `p6_milestones` m65 (diamond runs, join waits for all, no sibling deps). |
| **M6.6** | Final production e2e gate | toward M6.9 | **GREEN** | Full NL→compile→persist→**restart**→integrity verify→execute in `p6_milestones` m66_final_gate (LocalFileStore). Security negatives (5), invalid LLM plan fail-closed, cross-surface `list_compiled` visibility. Plus existing 11 workflow e2e tests. |

**Overall brief Phase 6:** **COMPLETE** — brief M6.1–M6.6 all GREEN.

**Scope footnote:** Brief M6.1–M6.6 cover the workflow-compiler and
fine-grained-scheduler slice of roadmap M6.1 only. Roadmap M6.2–M6.8
(sandbox/policy engine, secrets/network/air-gap, distributed workers/federation,
trading marks 0–1) and long-running supervisor takeovers remain **open**
roadmap work outside this remediation pass. `openrt-supervisor` is explicitly
**out of scope** for the brief (brief M6.1 is NL→workflow, not long-running
attended agents).

## Production-caller forensic audit (test-only does not count)

| Function | Production caller | Source location | Integration test |
|---|---|---|---|
| Compiler entry `WorkflowCompiler::compile` | `Runtime::compile_workflow` | `crates/openrt-runtime/src/runtime.rs:616` | `p6_workflow_compiler` test1, `p6_milestones` all |
| `understand` (NL→GoalSpec) | inside `WorkflowCompiler::compile` | `crates/openrt-compiler/src/compile.rs:730` | m62_* (provenance.goal id) |
| `reject_unsupported_or_ambiguous` | inside `compile` when no LLM plan | `crates/openrt-compiler/src/compile.rs:610` | m62_ambiguous, m62_unsupported |
| `decompose` | inside `compile` (deterministic path + replan fallback) | `crates/openrt-compiler/src/decompose.rs:128` | m62_*, m64_* |
| `parse_llm_plan` | inside `compile` when `llm_plan_json` present | `crates/openrt-compiler/src/capability.rs:320` | test8, m66_invalid_llm_plan |
| `plan_capabilities` | `plan_and_build` inside `compile` | `crates/openrt-compiler/src/capability.rs:199` | m63_* (capability checks) |
| `build_workflow` | `plan_and_build` inside `compile` | `crates/openrt-compiler/src/graph.rs:82` | m62 sequential/parallel |
| `validate_workflow` | inside `compile` (stage 6 + replan) | `crates/openrt-compiler/src/validate.rs:141` | m63_*, m64_*, test9 |
| Bounded replan (`ReplanRecord`) | inside `compile` (build-fail + validation-fail paths) | `crates/openrt-compiler/src/compile.rs` (~L265–L450) | compiler unit, m64_* |
| `persist_compiled` / `load_compiled` / `list_compiled` | `finish` / Runtime load / CLI+TUI+WebUI list | `crates/openrt-compiler/src/persist.rs`; `runtime.rs:670` | test7, m66_final_gate, m66_cross_surface |
| Scheduler `WorkflowEngine::run` (readiness loop, max_concurrency) | `Runtime::run_workflow` / checkpointed runs | `crates/openrt-orch/src/engine.rs:269` | m65, orch diamond/max tests |
| Resource admission wiring | `Runtime` checkpointed run → `with_resource_admission(ResourceGate)` | `crates/openrt-runtime/src/checkpoint.rs` | test3b, m63_resource |
| CLI `workflow {compile,inspect,validate,explain,run,list}` | `cmd_workflow` → Runtime methods | `crates/openrt-cli/src/main.rs:1380` (`WorkflowCmd` L554) | shares Runtime path with tests above |
| TUI key `w` | `compile_workflow_from_context` → `Runtime::compile_workflow` | `crates/openrt-tui/src/{app.rs:322,run.rs:102}` | same Runtime path |
| WebUI palette `workflow.*` | `palette.rs` handlers → `Runtime::compile_workflow` / `list_compiled` / `load_compiled_workflow` | `crates/openrt-webui/src/palette.rs:495–595` | `webui.rs` palette list test |

No second persistence system, DAG engine, scheduler, verifier, or capability
authority was introduced. LLM plan output is never executed without schema →
capability → permission → resource → graph validation (fail closed).

## Exact gate commands and results (this tree, post-remediation)

```
cargo fmt --all -- --check
  → empty diff                             (RC=0)

cargo check --workspace --offline
  → Finished, 0 errors                     (RC=0)

cargo test --workspace --offline
  → 654 passed / 0 failed                  (RC=0)
     log: /tmp/opencode/p6_workspace.log

cargo test -p openrt-compiler --offline
  → test result: ok. 35 passed; 0 failed   (RC=0)

cargo test -p openrt-orch --offline
  → test result: ok. 13 passed; 0 failed   (RC=0)

cargo test -p integration-tests --offline --test p6_workflow_compiler
  → test result: ok. 11 passed; 0 failed   (RC=0)

cargo test -p integration-tests --offline --test p6_milestones
  → test result: ok. 17 passed; 0 failed   (RC=0)
```

Phase 5 baseline (586/0) is contained inside the 654 workspace total; no
Phase 5 test was removed or downgraded. Prior remediation snapshot 629/0 is
also a subset (654 = 629 + 6 compiler tests + 1 diamond + 17 milestone tests + 1
doc/no-op variance — all additive).

## Honest limitations

1. Brief M6.1–M6.6 GREEN does **not** mean roadmap Phase 6 is done: M6.2–M6.8
   (sandbox, secrets, federation, trading) remain BLOCKED/open.
2. Supervisor / long-running attended agents are out of scope for the brief
   and absent from the tree.
3. Validation report emits 14 checks when `expected_outputs` is empty (15th
   check is per expected output) — documented, not a silent drop.
4. Default `PermissionsConfig` still denies `filesystem.write`; tests that
   need write supply an explicit allow rule (correct fail-closed default).
5. LLM-sourced capability-gap replan uses the **deterministic** decomposer as
   fallback (never re-prompts the model) — bounded by design; original +
   correction + final preserved in `ReplanRecord`.
6. No git commits exist (working tree all-untracked); integrity claims are
   file-level and command-level only.
