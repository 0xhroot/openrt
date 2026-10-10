# Phase 1 — Implementation Notes

**Status:** Implemented
**Date:** 2026-09-17
**Companion to:** `docs/roadmap/ROADMAP.md` (v0.2), `docs/architecture/OWNERSHIP.md`, `docs/architecture/adr/`

This document records **how** Phase 1 (Core AI Runtime) was actually built: the
crate layout, the runtime wiring, and every reconciliation between the plan and
the code. It is the source of truth for `IMPLEMENTED` vs `INTERFACE ONLY`
labels in the §32 report.

---

## 1. Crate layout

| Crate | Responsibility |
|---|---|
| `openrt-core` | Pure domain primitives: ids (`Id<T>`), `Session`/`Task`/status state machines, `Message`/`PendingToolCall`, `DomainEvent`/`EventKind`, `ExecutionRecord`, `CancellationToken`, timestamps, shared core errors. **No async, no I/O, no policy.** |
| `openrt-config` | Config model, loading (`OPENRT_CONFIG` → `./openrt.json` → `~/.openrt/config.json`), validation, `OperatingMode` concept (enforcement is Phase 3, ADR-003). |
| `openrt-telemetry` | Event **emission** (bounded single channel, sequential dispatcher) + tracing logging. Event **types** live in `openrt-core`. |
| `openrt-persistence` | Foundation store: `PersistenceStore` trait, `MemoryStore`, JSON-file `LocalFileStore`, event log. |
| `openrt-models` | `ModelProvider` abstraction, model request/response types, `MockProvider` (Echo / ToolSequence / Fail / Slow / Hang), Ollama client (feature-gated). |
| `openrt-permissions` | Capability model (strict `domain.resource.action`), `PermissionRequest`, `Policy` (deny-by-default), `PolicyEvaluator`, glob matching. |
| `openrt-resources` | Pure `/proc` + `statvfs` parsers, `SystemInfo`, `ResourceLimits`/`ResourceGate`, GPU detection trait (no-ops in P1). |
| `openrt-tools` | `Tool` trait, `ToolSpec` (with `required_caps`), registry, `ToolExecutor` (gated) + `DirectExecutor` (test/demo), builtins: `echo`, `system.info`, `filesystem.read`. |
| `openrt-agents` | `Agent` trait, `AgentContext` (history + cancellation), `AgentTurnLimits`, behaviors (`Completed` / `LimitReached` / `Cancelled`), `MinimalAgent` loop. |
| `openrt-runtime` | Assembly: `Runtime`/`RuntimeBuilder`, session + task entry points, `GateExecutor`, execution pipeline, `PersistenceSink`, doctor. |
| `openrt-cli` | `openrt` binary: `version`, `doctor`, `session create/list`, `task run/list`, `model list/info`, `tool list`, `config show`. |
| `integration-tests` | End-to-end tests across crates (deny-never-executes, persistence round-trip, event ordering). |

Dependency direction is strict: `core ← config ← telemetry/persistence ←
models/tools/permissions/resources ← agents ← runtime ← cli`. Nothing below
`runtime` depends on `openrt-runtime`.

## 2. Workspace posture

* Single workspace, `resolver = "2"`, shared versions from `[workspace.dependencies]`.
* `[workspace.lints.rust] unsafe_code = "forbid"`; the **sole** unsafe call
  (`libc::statvfs` in `openrt-resources`) is scoped-opted back in via
  `[lints.rust] unsafe_code = "allow"` in that crate's `Cargo.toml`.
* Quality gate (must all pass): `cargo fmt --all -- --check`; `cargo check --workspace`;
  `cargo clippy --workspace --all-targets --all-features -- -D warnings`;
  `cargo test --workspace`; `cargo build --workspace`.
* `integration-tests` is a workspace member holding end-to-end suites (its lib
  target is an empty marker).

## 3. Runtime wiring (how a task actually runs)

```
builder.config → validate → choose store (config.persistence.kind)
  → EventBusBuilder (persistence sink first, then client sinks) → emit RuntimeStarted
  → provider (builder override, else build_default_provider from config.providers,
    Mock or Ollama behind feature "ollama")
  → policy_from_config + resource_gate_from_config
  → ToolRegistry::builtins() + GateExecutor(registry, policy, resources, bus)
  → MinimalAgent(provider, executor, limits, tools)
  → Runtime { config, provider, agent, executor, registry, store, bus, dispatcher }
```

`Runtime::run_task(session_id, prompt)` loads the session, snapshots a
`Pipeline`, and runs it inline. `Runtime::spawn_task` returns a `TaskHandle`
(cancel token + execute). The pipeline is the single path through provider →
agent → gate → store; see `RUNTIME_LIFECYCLE.md`.

## 4. Reconciled design decisions

1. **Permission matching keys on tool id in Phase 1.** `Capability` is strictly
   3-part (`domain.resource.action`) and is an *annotation* on `ToolSpec.required_caps`.
   The gate keys decisions on the **requested tool id** (a policy rule's
   `capability` field is matched against the tool id unless a capability is
   declared); scope is extracted from args (`path | url | address | file`).
   `Decision::Ask` is treated as **denied** (no resolver in Phase 1).
2. **Cancellation** is cooperative + a `tokio::select!` race (`biased`) so a
   non-cooperative provider still terminates; task surfaces as
   `Task.status = Cancelled` with a `TaskFailed{reason:"cancelled"}` event
   (no separate `TaskCancelled` event kind in Phase 1).
3. **Events are correlated by id**: the gate parses UUID strings out of
   `ToolExecutionContext` to correlate `Permission*` / `ResourceCheck` /
   `ToolInvocation*` to their session/task. Task-level events use the ids
   directly. Agent id is carried in event *data*, not the correlation field.
4. **Resource config mapping**: `max_disk_bytes → min_free_disk_bytes`;
   `max_memory_bytes` is *headroom percentage* `→ max_mem_used_ratio =
   1.0 - (pct/100).clamp(0,1)`. `None` = no explicit limit.
5. **Doctor never fails overall**; unreachable Ollama is a `WARN` (local-first
   environments are legitimate). Provider probe builds a throwaway
   `OllamaProvider` from config — no downcasting of the live provider.
6. **Mock is the default test provider.** Ollama tests are gated by live
   environment / documented; no test depends on the network.
7. **`openrt-resources` routines parse `/proc`** (`/proc/meminfo`, `/proc/cpuinfo`,
   `/proc/uptime` via statvfs for disk) with tiny sample-based unit tests; the
   parsers are pure functions (`MemInfo::parse`, `CpuInfo::parse`).
8. **`ToolExecutor` vs `DirectExecutor`**: `GateExecutor` enforces permission +
   resource policy and emits correlated events; `DirectExecutor` bypasses both
   and exists only for tests and offline demos.
9. **`tool list` capabilities column** shows `required_caps` (annotation);
   `empty` means the tool declares no capability (and is matched by tool id).

## 5. Explicitly out of scope (Phase 1)

Terminal/browser/arbitrary fs/package-manager tools, TUI/web UI, multi-agent,
audit chain (Phase 2, ADR-002), scheduling (Phase 3, ADR-003), network guard
(Phase 3), GPU acceleration, model downloads, unauthenticated Ollama variants.
Sovereignty = no egress network sink (`PersistenceSink` + `LoggingSink` only).