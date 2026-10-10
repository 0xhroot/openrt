# Phase 1 — Event Model

**Status:** Implemented
**Date:** 2026-09-17
**Applies to:** `openrt-core` (types), `openrt-telemetry` (emission), `openrt-runtime` (producers)

## 1. Types live in `openrt-core`

`DomainEvent` (envelope) and `EventKind` (taxonomy) are defined in `openrt-core`
so any crate can *describe* events without depending on emission machinery.

### Envelope

```
struct DomainEvent {
    event_id:   EventId,            // Id<EventTag> UUID
    kind:       EventKind,
    at:         Timestamp,
    session_id: Option<SessionId>,  // correlation, not content
    task_id:    Option<TaskId>,
    agent_id:   Option<AgentId>,
    data:       serde_json::Value,  // content payload
}
```

Correlation fields are *identities*, never payload — consumers join on
`session_id`/`task_id`, never embed ids in `data`.

## 2. Taxonomy (Phase 1)

| Kind | Producer | Data |
|---|---|---|
| `RuntimeStarted` | runtime builder | `{mode}` |
| `RuntimeStopping` | `Runtime::shutdown` | — |
| `SessionCreated` | `Runtime` | `{title}` |
| `TaskCreated` | pipeline | `{prompt}` |
| `TaskStarted` | pipeline | `{agent_id}` |
| `TaskCompleted` | pipeline | `{result, turns}` |
| `TaskFailed` | pipeline | `{reason: cancelled\|error, error}` |
| `AgentStarted` | pipeline | `{agent_id}` |
| `AgentCompleted` | pipeline | `{agent_id, outcome}` |
| `PermissionRequested` | gate | `{tool}` |
| `PermissionGranted` | gate | `{tool}` |
| `PermissionDenied` | gate | `{tool, reason}` |
| `ResourceCheck` | gate | snapshot summary |
| `ToolInvocationStarted` | gate | `{tool}` |
| `ToolInvocationCompleted` | gate | `{tool, output-kind}` |

Future kinds (TaskQueued, audit events, …) extend the enum; the stored log is
append-only.

## 3. Ordering guarantees

- Single bounded channel (`mpsc`) into one dispatcher; dispatcher awaits each
  sink sequentially ⇒ **total order per sink**.
- Callers never block on sink speed (bounded queue; slow sink never stalls the
  runtime call path).
- The persistence sink is registered **first**, so the event log and any other
  sink observe the identical order (verified by
  `integration-tests/tests/event_ordering.rs`).
- `biased` race in the pipeline guarantees Task events are a proper
  sequence (no reordering race between cancellation and completion events).

## 4. Correlation details

- Task-level events are correlated directly by `sid`/`tid` from the pipeline.
- Gate events arrive inside an agent that only knows `ToolExecutionContext`
  UUID strings; `GateExecutor::correlate()` parses them back to
  `SessionId`/`TaskId`, so `Permission*` / `ResourceCheck` /
  `ToolInvocation*` share the task's correlation ids.
- `agent_id` is carried as *data* on the agent bookends (the correlation field
  stays optional consistently).

## 5. Sinks (local only, sovereignty)

- `LoggingSink` (default, `tracing`): envelope lines with correlation fields.
- `PersistenceSink`: append to the store's event log (drives `doctor`'s
  event-log check and the history). Appends are best-effort telemetry, but
  write failures are **surfaced** — counted in `Runtime::event_log_health` and
  shown as an `event-log` `WARN` by `openrt doctor` (SEC-06 / PC-Q8). This is
  *not* a tamper-evident audit log: checksums and chaining are ADR-002/P2 work
  (SEC-03).
- `ChannelSink` (in-memory): tests and event snapshots.

No egress sink exists in Phase 1 (ADR-005 — sovereignty). Event log retention
and audit (ADR-002) are Phase 2.