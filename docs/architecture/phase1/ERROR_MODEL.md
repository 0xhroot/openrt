# Phase 1 — Error Model

**Status:** Implemented
**Date:** 2026-09-17

## 1. Principle

Errors are typed at the layer that owns them and **mapped** as they cross layer
boundaries; the runtime aggregates them at the single public boundary
(`openrt-runtime::Error`). Raw `anyhow` style stringification never leaks out.

## 2. Per-crate error types

| Crate | Type | Notes |
|---|---|---|
| `openrt-core` | `CoreError`, `TaskError` | `TaskError::IllegalTransition{from,to}` for state machine violations. |
| `openrt-config` | `ConfigurationError` | `NotFound(PathBuf)` \| `Invalid(String)` \| `Io{path, source}`. |
| `openrt-models` | `ModelError` | `is_permanent()` distinguishes permanent (config/unreachable) vs transient (timeout/retryable) — queued retry policy groundwork. |
| `openrt-permissions` | `PermissionError` | `InvalidCapability`, `InvalidScope`, rule construction. |
| `openrt-tools` | `ToolError` | Named variants: `NotFound{tool}`, `PermissionDenied{tool,reason}`, `ResourceDenied{tool,reason}`, `Execution{tool,source}`, `InvalidArguments{tool,detail}`. |
| `openrt-runtime` | `Error` | **Aggregate boundary type** (below). |

## 3. Runtime aggregate error (`openrt-runtime::Error`)

```
Config(ConfigurationError)
Persistence(PersistenceError)
Model(ModelError)
Tools(ToolError)
Agents(AgentError)      // internal; mapped by the pipeline before surfacing
Task(TaskError)         // internal transition guard
SessionNotFound(SessionId)
ProviderUnavailable(String, String)   // id, reason
OllamaFeatureDisabled                 // built without "ollama"
Join(String)                          // task handle join error
Core(TaskError)                       // core errors reaching the boundary
```

All variants implement `Display` via `thiserror`; `SessionNotFound` pinpoints
the missing session id. `From` impls let `?` work inside runtime assembly; the
pipeline internally maps `AgentError`/`TaskError` to the appropriate
`PipelineOutcome` (see lifecycle doc) rather than propagating them.

## 4. Mapping rules

| Situation | Outcome |
|---|---|
| Config invalid at build | `Error::Config` early, before any store/event work |
| Provider missing / unreachable | `Error::ProviderUnavailable` (build) or task **Failed** with provider text |
| Ollama without feature | `Error::OllamaFeatureDisabled` (build) |
| Tool denied by policy | `ToolError::PermissionDenied` → agent error → task **Failed**; error text contains `permission` |
| Resource gate denies | `ToolError::ResourceDenied` → task **Failed** |
| Cancellation | task **Cancelled** (not an error) |
| Store failure mid-run | pipeline fails before agent start (durable-first) |

## 5. User- vs internal-facing

- **CLI**: prints `error: <Display>` to stderr, exit code 1 (`doctor` returns 1
  when any check is FAIL).
- **Transcript**: failed/cancelled tasks are recorded as
  `[task <status>: <error>]` assistant messages, so the session is never left
  without an explanation.
- **Internal**: structured `tracing` events carry event ids and correlation
  fields; logs alone can reconstruct the failure path.
- Error text is treated as **data**, never parsed by the runtime; permanence is
  decided by type (`ModelError::is_permanent`), not string matching.