# Phase 1 Audit — Security

**Status:** PASS WITH HARDENING (0 CRITICAL, 1 HIGH, 5 MEDIUM, 6 LOW).
**Date:** 2026-09-17
**Method:** source audit of every runtime-facing path (gate, tools, persistence,
telemetry, models, config, CLI) + threat-model reasoning per the roadmap Phase 1
security baseline and ADR-003 sovereignty posture. No vulnerability was
invented: every finding cites file + location + evidence.

---

## Threat model (Phase 1, as scoped)

Assets: session transcripts, task records, event log, config, host access via
tools. Trust boundaries: (a) local disk at rest, (b) the model provider channel
(loopback Ollama or mock), (c) any local user with read access to the store, (d)
a future in-process agent/host embedding the library. Threat actors: a malicious
or careless prompt/model; a local non-root user; a tampering attack on persisted
state; an embedding host that wires components incorrectly.

## Findings

| ID | Sev | File | Location | Evidence | Impact | Mitigation | Phase |
|---|---|---|---|---|---|---|---|
| SEC-01 | **HIGH** | `openrt-config/src/lib.rs` + `openrt-tools/src/builtin/filesystem_read.rs` | `PermissionsConfig::default()` (rules allow `filesystem.read`, no scope) + `FilesystemRead::run` | The default config allows `filesystem.read` by **tool id** with no `resource`/scope; the tool reads **any absolute path** up to `max_bytes` (default 64 KB) with no workspace/parent/symlink check; `RuntimeConfig.workspace_dir` is defined+validated but **never enforced** anywhere | A real model (default config + a local Ollama) can be prompted to read arbitrary local files (`/etc/shadow`, `~/.ssh/*`, dotfiles) up to 64 KB; content flows to the model, the session transcript, and the plaintext event log. The tool is described as "safe built-in" — this is the one Phase 1 builtin whose default posture is unsafe for real-provider use | Default-deny `filesystem.read` OR scope enforce to `workspace_dir` (canonicalize + reject `..`, symlink escape, absolute-outside-root); fold into `permission.tools` (P2) + approval on any out-of-workspace read; keep `--mock` demo allowance only | P2 (first permission milestone) |
| SEC-02 | **MEDIUM** | `openrt-persistence/src/store.rs` | `restrict_dir`/`restrict_file` + `LocalFileStore::open`, `atomic_write`, `append_event` | `restrict_dir` only chmods the root when it already exists; `create_dir_all` then creates the root and `sessions|tasks|executions|events` subdirs with umask-default perms (typically 0755). Event `.jsonl` + atomic-write temp files are created (0644 typical) and only later chmod'd to 0600 — a window where a prompt/result-bearing file is briefly world-readable, and subdir listings are always readable | Contrary to the documented "directories 0700 / files 0600" baseline; another local user can list the store layout and, during the creation window, read file content | Create dirs with explicit mode (`fs::DirBuilder::mode(0o700)`), create files with `OpenOptionsExt::mode(0o600)` at open, remove the post-hoc chmod race | P2 start (store hardening) |
| SEC-03 | **MEDIUM** | `openrt-persistence/src/store.rs` | whole-file writes, no checksums | Records are plain JSON; only `schema.json` version is validated; no per-record hash, no manifest, no corruption/tamper detection. Roadmap store checksum requirement is **PARTIAL** | Silent tampering/drift of session/task/execution records is undetectable; a corrupted file typically surfaces as `Corrupt` at read, but a *forged* file is indistinguishable from real state | Add per-record SHA-256 + a store-level manifest/sidecar; fails closed on mismatch (Phase 2 can chain event records via ADR-002 without changing the record format) | P2 |
| SEC-04 | **MEDIUM** | `openrt-tools/src/executor.rs` | `DirectExecutor` (pub, re-exported in `lib.rs`) | `pub use executor::DirectExecutor`; it executes tools with **no** permission or resource gate and is currently kept alive in the public API for tests/demos | A future embedding host could wire `DirectExecutor` into an agent and silently bypass the entire deny-by-default guarantee (the runtime only ever wires `GateExecutor`, so the shipped binary is not exposed — this is defense-in-depth/API hygiene) | Gate it behind `cfg(test)` or a `demo` feature; restrict to `pub(crate)`; add `#[doc(hidden)]` + explicit doc warning that it bypasses all controls | P2 |
| SEC-05 | **MEDIUM** | `openrt-runtime/src/sinks.rs` + `openrt-telemetry/src/logging.rs` | `PersistenceSink::write`, `EventSink`/`LoggingSink` | Events carry the full `prompt` (TaskCreated), tool `args` including file paths (PermissionRequested), and `result` (TaskCompleted); LoggingSink dumps `data = ?` to stderr; PersistenceSink writes JSONL to disk. OWNERSHIP credits P1 logging with "redaction hooks" — **not implemented** | Secrets/paths in prompts and tool args are plaintext in the event log (0600 after SEC-02 fix) and in stderr logs; no way to redact | Redaction hooks on sinks (regex/structured field rules); document data-at-rest as permission-protected only; revisit with P5 telemetry (ADR-005) | P2 (hooks) / P5 (telemetry) |
| SEC-06 | **MEDIUM** | `openrt-runtime/src/sinks.rs` | `PersistenceSink::write` swallows `append_event` errors | `if let Err(e) = ... { tracing::error!(...) }` — event-log writes are best-effort | On store failure, events are silently dropped (only the task records are durable, because those are awaited in the pipeline); the event log can silently diverge from actual behavior, which matters for any future audit/replay | Fail-closed policy option; surface sink health on `doctor`; harden once ADR-002's audit chain lands | P2 |
| SEC-07 | **MEDIUM** | `Cargo.toml` (`workspace`) + `crates/openrt-models/Cargo.toml` | feature `ollama` in defaults | `default = ["ollama"]` pulls `reqwest`/`hyper`/`rustls` into every default binary (Cargo.lock confirmed). Egress only happens if an operator configures a provider + selects it, so no live path today | Attack surface (TLS stack) present in the default build; contradicts "network features are separately feature-gated" | `default = []`; route CLI Ollama through an explicit feature (`openrt --features ollama`) | P2 |
| SEC-08 | **MEDIUM** | `openrt-runtime/src/runtime.rs` + `openrt-persistence/src/store.rs` | `run_task`/`spawn_task` + `update_session` | A task loads the session, runs, then `update_session` writes back the **entire** transcript (last-writer-wins); `MemoryStore`/`LocalFileStore` have no per-session lock spanning read→run→write | Two concurrent tasks on one session can clobber each other's transcript folds (lost updates); with file store, two processes can interleave writes | Per-session serialization (async mutex or append-only event projection); guard concurrent writes when adding P2 orchestrator | P2 |
| SEC-09 | **LOW** | `openrt-permissions/src/policy.rs` | `PermissionRequest::tool` | The passed `PermissionContext` (session/task/channel) is discarded (`context: PermissionContext::default()`); evaluator never considers context | Session/task-scoped rules are impossible today (only static pattern+scope rules) — a missing capability, not an exploit; needs extension for P2 `permission.tools` | Thread context through `evaluate`; add context predicates | P2 |
| SEC-10 | **LOW** | `openrt-config/src/lib.rs` | `OperatingMode` (record-only) | `mode: OperatingMode::Sovereign` is serialized/validated but **enforced nowhere**; `base_url` accepts any http(s) host (non-loopback allowed) | In Sovereign mode a misconfigured provider could still dial a remote host; no network guard (ADR-003 enforcement is Phase 3 — documented INTERFACE ONLY) | Add P3 cyber-guard; for now document that config authors must keep `base_url` loopback in Sovereign | P3 |
| SEC-11 | **LOW** | `openrt-runtime/src/doctor.rs` | `store_check` | `doctor` appends a fresh `RuntimeStarted` probe to the real event log and counts it in read-back | Event-log pollution + mild audit noise on every `openrt doctor`; meaningless WARN if read-back races | Use a side-channel probe or a `doctor`-namespaced log before P2 audit lands | P2 |
| SEC-12 | **LOW** | `openrt-persistence/src/store.rs` | `list_events`/`list_ns` | `list_events` silently skips unparsable JSONL lines (`if let Ok(ev)`); `list_ns` returns any file incl. stale `.tmp`, which can make `list_sessions` fail on a poisoned temp | Silent data loss on read; a crash between temp-write and rename can wedge listing | Report corrupt lines as `Corrupt`; sweep temp files on open | P2 |
| SEC-13 | **LOW** | `openrt-runtime/src/gate.rs` | `evaluate` → `Decision::Ask` | `Ask` is treated as denied (documented reconciliation) with an explanatory `PermissionDenied` | Every `Ask` rule fails closed — safe Default, but there is no approval workflow yet; approval UX is a Phase 2 requirement | Implement resolver → `Decision::Ask` ⇒ block/approve | P2 |

## Security-positive controls verified (no finding)

- **Deny-by-default policy** and the gate ordering resolve→permission→resource→execute; denied tools provably never run (unit + integration test).
- **Zero network egress sinks**; only Logging/Persistence/Channel sinks exist.
- **Feature-gated Ollama** code path with `OllamaFeatureDisabled` degradation.
- **Config strictness:** `#[serde(deny_unknown_fields)]` everywhere; validated on load and by `RuntimeBuilder::build`.
- **`unsafe` confined** to the single documented `statvfs` wrapper (2 expressions); everything else `forbid`.
- **Atomic record writes** (temp + rename, same fs) with 0600 — the *write path* is atomic; only the *creation-mode* race (SEC-02) is defective.
- **Cancellation** is deterministic (`biased`), so a hung/infinite provider cannot wedge the runtime, and cancel wins over a racing completion.
- **No shell/exec/command tool** exists in Phase 1 at all (only echo/system.info/filesystem.read); no code-injection surface from arbitrary JSON args.
- **Tool registry name validation** (lowercase `a-z0-9._-`) + duplicate rejection.

## Verdict

**PASS WITH HARDENING.** No CRITICAL. One HIGH (SEC-01 — default-open,
unscoped `filesystem.read`) is a *config+runtime-enforcement* gap rather than an
exploitable defect in the shipped default path (default provider is the mock,
and gate_deny proves the gate works); it must be resolved at the top of Phase 2's
permission work. The MEDIUM findings are store-permission modeling (SEC-02),
integrity (SEC-03), API hygiene (SEC-04), plaintext-at-rest-and-logs (SEC-05),
best-effort event persistence (SEC-06), default network-client dependency
(SEC-07), and session-write concurrency (SEC-08). Nothing here blocks Phase 2
interface design; everything here blocks Phase 2 "done".