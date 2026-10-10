# Phase 1 — Runtime Lifecycle

**Status:** Implemented
**Date:** 2026-09-17
**Applies to:** `openrt-runtime`, `openrt-agents`, `openrt-tools`, `openrt-telemetry`, `openrt-persistence`

## 1. Bootstrap

1. `RuntimeBuilder::build()`:
   - `config.validate()` (rejects `max_turns == 0`, empty providers, dup ids,
     bad `base_url`, unknown default provider, bad telemetry format).
   - Store: `StoreKind::File` → `LocalFileStore::open(dir)` (default
     `~/.openrt` / `openrt_persistence::default_store_dir()`), `Memory` → `MemoryStore`.
   - Event bus built with the **persistence sink first**, extra builder sinks
     appended after; dispatcher owns total order.
   - `RuntimeStarted` emitted (mode from config).
   - Provider from builder override or `build_default_provider`; `ProviderKind::Mock`
     → `MockProvider::tool_sequence`, `Ollama` → `OllamaProvider` (only with the
     `ollama` feature, else `Error::OllamaFeatureDisabled`).
2. `Runtime::shutdown()` emits `RuntimeStopping`. Full drain happens when the
   last `EventBus` clone (including the Runtime's) drops.

## 2. Session lifecycle

| Step | Durable | Event |
|---|---|---|
| `create_session(title)` | `MemoryStore`/file | `SessionCreated` (data: title) |
| `load_session(id)`, `list_sessions()` | store read | — |

Sessions are message-ordered transcripts (`Session.content: Vec<Message>`).
Every completed task folds user prompt + assistant result into the transcript;
failed/cancelled tasks append `[task <status>: <error>]` as an assistant message.

## 3. Task lifecycle

`TaskStatus` is an explicit enum state machine (`Created → Queued → Running →
Waiting → Completed|Failed|Cancelled`); `transition()` enforces legality
(terminal states are final; `Running` stamps `started_at`, terminal stamps
`completed_at`).

### 3.1 Pipeline (`run_pipeline` / `Pipeline::run`) — exact procedure

1. If `cancel.is_cancelled()` before start: mark `Cancelled`, save, return.
2. **TaskCreated** → durable `save_task` + event (`{prompt}`).
3. Stamps `agent_id`, → **Running**, durable save + **TaskStarted**.
4. `ExecutionRecord` created (Running), saved.
5. **AgentStarted**.
6. Agent context = history snapshot + cancellation token.
7. Race: `select! { biased; cancel, agent.run }`. Outcomes:
   - `AgentResult.ended == Cancelled` or `AgentError::Cancelled` ⇒ **Cancelled**.
   - Agent error ⇒ task.error, **Failed**.
   - Otherwise: records `FinalOutput` turn, sets `result` ⇒ **Completed**.
8. **AgentCompleted** (`{agent_id, outcome}`).
9. Terminal transition + event: **TaskCompleted** (`{result, turns}`) or
   **TaskFailed** (`{reason: cancelled|error, error}`).
10. Fold transcript, `update_session`; close out `ExecutionRecord`
    (`status`, `completed_at`, `result`, `error`, `turn_count`) and save; final
    `save_task`. Terminal task is inspectable via `store.list_tasks`.

### 3.2 Agent loop (MinimalAgent)

`history + (input) → provider.generate`
- While `finish_reason == ToolCalls`: map to `PendingToolCall`, append an
  assistant tool-call message, execute each call through the gate,
  feed results back as tool messages, check cancellation each iteration.
- A plain text response is the terminal answer.
- `max_turns` (default 6) reached without an answer ⇒ `AgentEndReason::LimitReached`.
- Any gate denial surfaces as `AgentError::Tool` → pipeline **Failed** with a
  `permission` error message.

### 3.3 Cancellation

- `CancellationToken` (core, async-free) with `future()` future; awaited in the
  pipeline race and polled inside the agent loop between turns.
- `TaskHandle` (`spawn_task`) keeps the token: `cancel()` then `execute()`.
- Determinism note: `biased` select means an already-cancelled token always
  wins the race, so background cancellation is not racy.

### 3.4 Background tasks

`Runtime::spawn_task(session_id, prompt)` → `TaskHandle` (validates the session
exists up front); `execute()` loads the session fresh *under the per-session
lock* and spawns the run, returning the final `Task`. Usable for a future
HTTP/stdio queue without changing the pipeline.

Concurrent task runs against the **same session are serialized per session id**
(`SessionLocks`, see `P2_1_SECURITY_HARDENING.md`): the transcript fold can
never be overwritten by a racing run (SEC-08), while different sessions proceed
in parallel.

## 4. Failure behavior

- A denied tool **never runs**: gate denies before `ToolInvocationStarted`,
  so no tool-side side effects occur (covered end-to-end by
  `integration-tests/tests/gate_deny.rs`).
- Provider unavailability (`ProviderUnavailable`, `OllamaFeatureDisabled`,
  `Model` errors) surfaces as task **Failed** with the provider error text in
  `task.error`; the session transcript records the failure marker.
- Store write failures during the run fail the pipeline before the agent
  starts (durable-first: TaskCreated is persisted before any work).
- Record writes (session/task/execution) are **required-durable**: any `Err`
  propagates and fails the call. Event-log appends are best-effort telemetry,
  but failures are **never silent** — `PersistenceSink` counts them in
  `Runtime::event_log_health` and `openrt doctor` reports an `event-log`
  `WARN` when any append failed (SEC-06 / PC-Q8).

## 5. Sinks & durability ordering

Persistence sink is registered before user sinks, so the event log and any
consumer sink agree on the same total order for every event; the dispatcher
serializes `write` across all sinks sequentially (see `EVENT_MODEL.md`).

The persistence sink accumulates event-write health (`total`/`failures` + last
error), exposed on `Runtime::event_log_health` and reported by `doctor`'s
`event-log` check.