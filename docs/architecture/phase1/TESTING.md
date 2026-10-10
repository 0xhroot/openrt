# Phase 1 — Testing Strategy

**Status:** Implemented
**Date:** 2026-09-17

## 1. Layers

Phase 1 keeps three testing layers:

1. **Unit tests** — inside each crate (`#[cfg(test)]`), fast, no I/O beyond
   temps, deterministic.
2. **Integration tests** — `integration-tests` workspace member, exercises the
   real `Runtime` end-to-end.
3. **Quality gate** — the commands from §1 must all pass before anything is
   "done".

## 2. The mock provider

`MockProvider` (`openrt-models::mock`) is the default test provider:

| Mode | Behavior |
|---|---|
| `Echo` | echoes the last user text as an answer |
| `ToolSequence` | one scripted `ToolCalls` turn (`echo`, `system.info`, …) then a final text answer |
| `Fail` | returns permanent `ModelError` |
| `Slow` | sleeps ~200ms per call (used by cancellation tests) |
| `Hang` | never returns (surface for hang tests; use sparingly) |

Network rules:

- No test depends on the network. Ollama features compile but live tests must be
  gated (env `OPENRT_TEST_OLLAMA` and/or `#[ignore]`).
- The CLI honors `--mock`/`OPENRT_MOCK=1` for fully offline demos.

## 3. What the tests prove (key properties)

| Property | Where | Mechanism |
|---|---|---|
| Denied tool **never executes** | `gate.rs::denied_tool_never_executes` (counter probe) + `integration-tests/tests/gate_deny.rs` | recognizably side-effecting tool + zero invocations asserted |
| Denials don't hit the executor | `gate.rs` event assertions | no `ToolInvocation*` for denied tools |
| Persistence survives restart | `integration-tests/tests/persistence_roundtrip.rs` | `LocalFileStore::open` → run → reopen → same session/task/events |
| Events are totally ordered | `integration-tests/tests/event_ordering.rs` | `ChannelSink` snapshot; strict relative order `RuntimeStarted → … → TaskCompleted` |
| Cancellation is deterministic | `run.rs::cancellation_produces_cancelled_task`, `runtime.rs::background_task_cancels` | slow provider + biased select; cancelled task persisted |
| Task state machine legality | `core::task` tests | `IllegalTransition` from terminal states |
| Config validation | `config` tests | dup providers, bad `base_url`, unknown fields |
| Pure parsers | `resources` | sample-based `/proc` parser tests, no host dependence for the assertions |

## 4. Running

```sh
cargo test --workspace                # everything (unit + integration + doctests)
cargo test -p openrt-runtime          # one crate
cargo test -p integration-tests       # end-to-end only
```

## 5. Quality gate (must ALL pass)

```sh
cargo fmt --all -- --check
cargo check --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo build --workspace
```

Clippy runs with `-D warnings`; `unsafe_code` is `forbid` workspace-wide
(scoped `allow` only in `openrt-resources` for the single `libc::statvfs` call,
documented there).

## 6. Isolation & hygiene

- Temp files/`LocalFileStore` tests use unique uuid temp dirs cleaned up on exit.
- `MemoryStore` is the default for unit-level runtime tests; file-store tests
  are explicit integration tests.
- Bus tests bound capacity (`bus_capacity(16)`) to keep test memory bounded.
- Logs: `init_logging` is idempotent so embedding hosts/tests that install a
  subscriber don't panic.