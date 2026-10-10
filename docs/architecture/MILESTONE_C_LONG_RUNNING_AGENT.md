# Milestone Part C — Long-Running Agent Execution

**Status:** Landed · **Date:** 2026-10-08
**Scope:** the scorecard Part C milestone: make an agent task run as long as it
needs — retry with backoff, staged context with an honest guard/compaction
path, an optional idle watchdog for streaming providers, cooperative
pause/resume, and operator surfaces (CLI + TUI). This doc is the milestone
report; acceptance evidence and residuals are at the bottom.

Companions: [AGENT_RECOVERY.md](./AGENT_RECOVERY.md) (recovery authority),
[PHASE13_IMPLEMENTATION.md](./phase13/PHASE13_IMPLEMENTATION.md) (final phase),
[MEMORY_FABRIC.md](./MEMORY_FABRIC.md) (memory fabric, unchanged).

## 1. What Part C delivers

| Requirement | Mechanism |
|---|---|
| Retry / backoff | transient provider errors retried `provider_retries` times with deterministic bounded exponential backoff; retries block on cancel/pause so an operator action is never swallowed |
| Staged prompts | request assembly is staged: (A) environment framing, (B) context guard vs the provider's declared window, (C) honest compaction with an explicit marker |
| Context / truncation guard | an assembled request may never exceed `context_window × context_guard_ratio`; `None` window disables the guard (no fabricated window) |
| Multi-iteration agent loop | existing turn/tool-call loop retained; `max_turns` / `max_tool_calls` still bound it |
| Tool → result → model continuation | unchanged (Part 5/6/13 invariants) |
| Explicit completion detection | unchanged: only a structured final answer (or the new `AgentEndReason::Paused`) ends a run |
| Task/iteration/idle limits | `max_turns`, `max_tool_calls`, `provider_retries`, bounded backoff, optional idle watchdog |
| Cancellation / pause / resume | cancellation unchanged; new resettable `PauseToken` gives cooperative pause at turn boundaries; resume re-runs from the preserved transcript |
| Checkpoint / recovery | unchanged recovery authority; `Paused` is accepted as a recoverable state, `task_resume` routes through it |
| No artificial termination | the idle watchdog is opt-in (`agent_idle_timeout_ms`), applies only to streaming providers, and its firing is a **transient error** (backoff-retried), never a silent completion |

Hard constraints honored: `MemoryPlan`-style *propose, then test, never
override*; `ResourceGate` remains the only admission authority; sandbox,
permissions, and audit are untouched; no provider is switched or installed
silently; nothing is committed.

## 2. Design decisions (recorded)

- **`Paused` is not terminal.** `TaskStatus::Paused` is excluded from
  `is_active()`, but resume is always possible: `Paused → Running` and
  `Paused → Cancelled` are valid transitions (`crates/openrt-core/src/task.rs`).
- **Cooperative pause.** The agent checks the pause token at turn boundaries
  and inside `backoff_sleep`/`call_provider` selects; a paused run returns
  `AgentEndReason::Paused` with `turns`/messages intact and persists a `Paused`
  task. It never interrupts a mid-generation non-streaming provider call.
- **Resume re-runs the same prompt.** On pause the runtime folds
  `transcript[1..]` into the session (skipping message index 0 = the input), so
  resume re-runs the original input without duplicating the instruction.
- **Compaction never drops the framing or the input.** Compaction walks from
  the oldest turn, keeps the tail verbatim, re-seats the system framing above
  the marker, and inserts an explicit `(context compaction)` marker at index 1.
  An input that alone exceeds the budget is an honest `AgentError::Internal`
  refusal — never a truncated instruction.
- **Token estimate is a sizing heuristic only.** `estimate_tokens = (chars+3)/4`
  (≈4 chars/token); it is used for the guard budget, never reported as a
  measurement, never persisted.
- **Backoff is deterministic.** `min(max_ms, base × 2^(attempt-1))`, defaults
  `base 500 ms`, `max 30 s`, `max ≥ base` validated at config load.
- **Idle ≠ completion.** A stream ends only on `done:true`; a gap longer than
  the configured budget is `ModelError::Transport("stream idle: …")`, a
  transient error subject to the same bounded retry. `streaming_enabled =
  idle_timeout_ms.is_some() && capabilities.supports_streaming` — a provider
  without streaming, or without an explicit idle budget, keeps the plain
  non-streaming call and is never time-boxed by an artificial deadline.
- **`StreamingUnsupported` is a one-shot fallback.** It degrades to a single
  `generate` call, never a retry loop.
- **`PauseToken` lives in `openrt-core`.** Resettable, level-based,
  async-free (hand-rolled wakers mirroring `CancellationFut`); `is_paused()`,
  `wait_paused()`/`wait_resumed()`, `pause()`/`resume()`. Keeps `openrt-core`
  free of a tokio dependency.
- **`openrt-agents` now depends on `tokio`** (regular dependency, moved from
  dev-deps) for `select!`/`time`/`sync`/`spawn`.

## 3. Surfaces

- **Agent** (`crates/openrt-agents/src/minimal.rs`): `provider_retries`,
  `provider_empty_response_retries`, `retry_backoff_base_ms`,
  `retry_backoff_max_ms`, `idle_timeout_ms`, `context_guard_ratio` turn limits;
  `AgentEndReason::Paused`; `staged_request` (A/B/C), `backoff_sleep`
  (`SleepEnd`), `pause_wait`, `estimate_tokens`, `backoff_delay`.
- **Core** (`crates/openrt-core/src/cancel.rs`, `task.rs`, `event.rs`):
  `PauseToken`/`PauseWait`; `TaskStatus::Paused`; `TaskPaused`/`TaskResumed`
  events (already mirrored in the TUI status renderer).
- **Config** (`crates/openrt-config/src/lib.rs`): `agent_retry_backoff_base_ms`,
  `agent_retry_backoff_max_ms`, `agent_idle_timeout_ms` (`None` default),
  `agent_context_guard_ratio` (0.7), each validated at load.
- **Runtime** (`crates/openrt-runtime/src/{run,runtime,recovery}.rs`):
  `Pipeline::run(…, pause)`, `PipelineOutcome::Paused`; `ActiveTask { cancel,
  pause }` registry; `TaskHandle::pause()`/`resume()`; `task_pause`,
  `task_resume` (live signal clear or durable recover), `task_recover` accepts
  `Paused`.
- **Mock provider** (`crates/openrt-models/src/mock.rs`): `with_streaming()`,
  `with_context_window(Option<usize>)`, `with_stream_gap(Duration)`; streaming
  emits Token deltas then unit `StreamEvent::Done`.
- **CLI** (`crates/openrt-cli/src/main.rs`): `task pause <ID>` (cooperative;
  no-op return for terminal, refuses stale-active) and `task resume-task <ID>`
  (distinct from workflow `task resume`); both `--json`-aware.
- **TUI** (`crates/openrt-tui/src/agent/state.rs`): `/pause` and `/resume`
  slash commands beside `/cancel`.

## 4. Validation evidence

| Suite | Result |
|---|---|
| `openrt-core` | 43 passed (incl. 4 new `PauseToken` tests) |
| `openrt-agents` | 31 passed (6 new Part C: backoff curve, guard compaction keep-input+system, input-over-budget refusal, stream-gap idle detection, pre-paused boundary stop, pause-during-backoff) |
| `openrt-models` (all features) | 126 passed (incl. streaming/gap tests) |
| `openrt-runtime` | 177 passed (incl. `task_pause` → durable `Paused` → `task_resume` → `Completed`, terminal refusal, live-signal clear) |
| `openrt-config` | 19 passed (incl. Part C field validation + `runtime.generation` param validation) |
| `openrt-tui` | 681 passed |
| Full workspace | all suites green, 0 failures; `cargo fmt --check` clean; `cargo clippy --workspace --all-targets` no errors |

**Live e2e (verified against the real CLI + file store, mock provider):**
`task run` → Completed; `task pause <Completed>` → honest no-op
(`paused:false`, exit 0); `task resume-task <Completed>` → refused
(`illegal task transition: Completed -> Running`); a crafted durable `Paused`
task resumed through `task resume-task` re-ran to `Completed` with a real
result. All store/audit/residency authorities untouched.

## 5. Residuals

- A durable `Paused` task produced by a **terminated** CLI process resumes via
  `task resume-task` (recover path); pausing a task in another live process is
  refused fail-closed (`StaleActive`) — the pause gesture is intentionally
  in-process.
- The context-guard token estimate is a sizing heuristic; a future tokenizer
  backend would replace `estimate_tokens` behind the same budget contract.
- Real-network long-horizon soak of the idle watchdog remains exercised via
  the mock gap tests; no live-wan soak was run in this milestone.

## 6. Final integration status (2026-10-08)

Statuses are the honest result of the final integration pass against a live
Ollama backend (`qwen3.5-9b:latest`, `qwen2.5:7b`):

| Check | Result |
|---|---|
| Long-running loop, live model | **PASS** — `qwen2.5:7b` wrote `calc.py`/`test_calc.py`, ran `python3 -m unittest`, burned turns and ended **Failed** with the explicit "agent stopped at the configured iteration limit (runtime.max_turns / runtime.max_tool_calls); state is preserved" message; a follow-up run on the same session completed the suite. Avoidance of an artificial deadline holds. |
| Limit reached, state preserved | **PASS** — session reused after the limit failure; outcomes/tool logs intact. |
| `resume-task` on durable `Interrupted` | **PASS** — live recovery to **Completed**. |
| Durable `Paused` → resume → `Completed` | **PASS** — re-verified this session on the file store via the real CLI (`task resume-task b2fc… --mock` → `mock final: say hello`). |
| Live pause under a real model | **NOT VALIDATED** — three attempts at pausing a running task each raced the provider's empty-response flake (`MODEL_TOOL_CALL_PARSE_ERROR: provider returned an empty assistant response without a tool call (retries exhausted)`); the run failed at the same boundary the pause signalled. The bounded empty-response retry worked as designed (no infinite loop); the `Paused` mid-run transition just was not captured under real generation this session. |
| Multi-day soak | **NOT VALIDATED** — no live-wan soak was run (unchanged residual). |
| Generation params wired to the live backend | **PASS** — `runtime.generation` (`temperature`, `max_tokens`, `context_window`, …) reaches the provider call; `max_tokens=7` produces the empty-response failure path, `max_tokens=40` completes, `temperature` observed. `num_predict` verified on the live wire; `num_ctx` (context window clamp vs Ollama's declared 8192) verified at the unit level. |