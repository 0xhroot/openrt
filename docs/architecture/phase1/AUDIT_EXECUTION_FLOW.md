# Phase 1 Audit — Public API & Execution Flow

**Status:** PASS (claims verified against source; two discrepancies noted)
**Date:** 2026-09-17
**Audit scope:** `openrt-runtime` (Runtime/RuntimeBuilder/TaskHandle/Pipeline/
GateExecutor/doctor/PersistenceSink), `openrt-tools` (Tool/ToolSpec/ToolRegistry/
GateExecutor contract), `openrt-agents` (Agent/MinimalAgent), `openrt-cli`
subcommand dispatch, `openrt-core` state machines.
**Method:** line-by-line read of `runtime.rs`, `run.rs`, `gate.rs`, `doctor.rs`,
`sinks.rs`, `minimal.rs`, `store.rs`, `bus.rs`, `main.rs` + the integration
tests; trace of one `openrt task run`.

---

## 1. Public API surface (verified signatures)

`openrt-runtime::Runtime` — pub fields `config, provider, agent, executor,
registry, store, bus`; methods `create_session(&str) -> Session`,
`load_session(SessionId)`, `list_sessions()`, `run_task(SessionId, &str) ->
Task`, `spawn_task(SessionId, &str) -> TaskHandle`, `doctor() -> DoctorReport`,
`shutdown()`. `TaskHandle { task_id(), cancel(), execute() -> Task }`.
`RuntimeBuilder { config/store/with_provider/sink/bus_capacity/build }`.

Everything the CLI prints is read through `runtime.store` / `runtime.provider` /
`runtime.registry`; the CLI contains **no** orchestration logic (verified —
`main.rs` only parses args, opens a tokio runtime, and forwards to runtime
methods).

`openrt-tools` — `Tool`, `ToolSpec { name, description, version,
required_caps, parameters_schema }`, `ToolRegistry { builtins, register,
register_custom, get, contains, specs, names, provider_tools }`,
`ToolExecutor`, `DirectExecutor`, `ToolOutput`, `ToolError`.

`openrt-agents` — `Agent`, `MinimalAgent`, `AgentContext (session_id, task_id,
history, cancel)`, `AgentTurnLimits`, `AgentResult`, `AgentEndReason`,
`AgentError`.

`openrt-core` — `TaskStatus` state machine with `transition()` guard,
`DomainEvent`/`EventKind`, `CancellationToken`, `ExecutionRecord`, ids.

## 2. Bootstrap (verified in `runtime.rs::build`)

1. `config.validate()` — rejects `max_turns==0`, empty providers, duplicate
   provider ids, missing `base_url`/bad scheme for `ollama`, unknown
   `default_provider`, bad telemetry format. Verified.
2. Store from `config.persistence.kind` (`File`→`LocalFileStore::open(default
   ~/.openrt)`, `Memory`→`MemoryStore`), or builder override.
3. Event bus: `PersistenceSink` registered **first**, builder sinks appended —
   total order per sink (bus dispatcher awaits each sink sequentially,
   `bus.rs:86-91`). `RuntimeStarted` emitted with `config.mode`.
4. Provider: builder override, else `build_default_provider` (`Mock` →
   `MockProvider::tool_sequence`; `Ollama` → `OllamaProvider` behind
   `#[cfg(feature="ollama")]`, else `Error::OllamaFeatureDisabled`).
5. `policy_from_config` → `resource_gate_from_config` → `ToolRegistry::builtins`
   → `GateExecutor(registry, policy, resources, bus)`.
6. `MinimalAgent(config.runtime.default_agent, provider, executor, limits,
   tools)`; turn limits from `config.runtime.max_turns`.

All matches `RUNTIME_LIFECYCLE.md` §1.

## 3. Task execution (verified in `run.rs::Pipeline::run`)

Order: pre-start cancel check → `save_task` (durable, Created) + `TaskCreated`
{prompt} → agent_id + `Running` stamps `started_at` (durable) + `TaskStarted` →
`ExecutionRecord` (Running) saved → `AgentStarted` → snapshot history into
`AgentContext` + cancel → `select! { biased; cancel.future(), agent.run }` →
`AgentCompleted` → terminal transition + `TaskCompleted`/`TaskFailed{reason:
cancelled|error}` → fold prompt+result (or `[task <status>: <error>]` marker
on failure/cancel) into session → `update_session` → close `ExecutionRecord`
→ final `save_task`. Durability is write-ahead relative to events (events flow
through the same durable store but are async via the sink — see SEC‑06).

Cancellation semantics confirmed: `biased` guarantees an already-cancelled
token beats a racing completion; a non-cooperative/generator provider is still
terminated because the whole `agent.run` future is dropped when the cancel arm
wins. Pre-cancelled tasks terminal immediately without touching the agent.

## 4. Gate (verified in `gate.rs`)

Order: resolve (unknown → `ToolError::NotFound`, denied) → `PermissionRequested`
{tool,args} → `PolicyEvaluator.evaluate` → Allow (`PermissionGranted`) | Deny
(`PermissionDenied`, return) | Ask (**treated as Denied**, documented) →
`ResourceCheck` → `ToolInvocationStarted` → tool run → `ToolInvocationCompleted`.
Denied calls return before any tool-side execution (`denied_tool_never_executes`
unit test + `integration-tests/tests/gate_deny.rs`). Confirmed the guarantee
"A denied tool must not execute" holds end-to-end.

## 5. Agent loop (verified in `minimal.rs`)

history + input → `provider.generate`; `FinishReason::ToolCalls` →
`PendingToolCall`s → assistant tool-call message → each call through
`executor.execute` (the gate) → tool result fed back → loop; plain text =
terminal answer; `max_turns` reached → `AgentEndReason::LimitReached`;
cancellation checked between turns and before each provider call; any gate
denial → `AgentError::Tool` → task `Failed` with "permission" in the error
text (asserted by gate_deny.rs).

## 6. CLI flow (verified in `main.rs`)

`Cli { --log, --memory, --mock }` global flags + `doctor|version|model list|
info|tool list|session create|list|task run|list|config show`. Config
resolution: `OPENRT_CONFIG` → `./openrt.json` → `~/.openrt/config.json`.
`--memory` → `MemoryStore`; `--mock`/`OPENRT_MOCK=1` → MockProvider. Exit codes:
0 success/healthy (doctor with 0 fails), 1 on error or any doctor FAIL.

## 7. Discrepancies / notes

- **EXEC‑01 (INFO):** `EventKind::{SessionLoaded, ModelInvocationStarted,
  ModelInvocationCompleted, ErrorOccurred}` are declared but never emitted in
  Phase 1. The event log is additive and these are forward-surfaces, so this
  is harmless — flagging so no one asserts them in tests yet.
- **EXEC‑02 (INFO):** `TaskStatus::Queued/Waiting` are reachable states in the
  enum but Phase 1 never transitions into them; `TaskHandle` is the only
  "queued" stage. Harmless; ready for a P2 scheduler.
- **EXEC‑03 (INFO):** `openrt-config` has no dependency on `openrt-core`
  (enables `Config` to be parsed before core types exist). Supply-chain
  direction unaffected.
- **EXEC‑04 (INFO):** `doctor` appends a synthetic `RuntimeStarted` probe to
  the real event log on every run (see SEC‑11).

## 8. Execution-flow verdict

**PASS.** The documented pipeline in `RUNTIME_LIFECYCLE.md`, `EVENT_MODEL.md`
and `ERROR_MODEL.md` matches the code to the letter, including the exact
event sequence asserted by `integration-tests/tests/event_ordering.rs`
(`RuntimeStarted → SessionCreated → TaskCreated → TaskStarted → AgentStarted →
PermissionRequested → PermissionGranted → ResourceCheck →
ToolInvocationStarted → ToolInvocationCompleted → AgentCompleted →
TaskCompleted`, all correlated to the same session+task).