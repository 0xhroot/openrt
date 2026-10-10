# Phase 2.1 — Security Hardening

**Status:** Implemented
**Date:** 2026-09-17
**Scope:** Phase 2 admission fixes from the Phase 1 security audit
(`docs/architecture/phase1/AUDIT_SECURITY.md`) and phase-compatibility audit
(`docs/architecture/phase1/AUDIT_PHASE2_COMPATIBILITY.md`): SEC-02 (store
dir/file creation permissions), SEC-06/PC-Q8 (event-log write failures must not
be silent), SEC-08 (concurrent session transcript loss). Solely Phase 2.1 — no
P2.2+ features, no workspace rewrites, no unrelated refactors, no weakening of
permissions to satisfy tests.

## 1. Findings addressed

| Audit ID | Severity | Title | Status |
|---|---|---|---|
| SEC-02 | MEDIUM | Store dir/file creation races the umask | IMPLEMENTED |
| SEC-06 | MEDIUM | `PersistenceSink` swallows `append_event` errors | IMPLEMENTED (surfaced) |
| PC-Q8 | MUST (Q8) | Fail-closed audit-log writes | IMPLEMENTED (surfaced; full fail-closed deferred to ADR-002) |
| SEC-08 | MEDIUM (Q11) | Session transcript last-writer-wins | IMPLEMENTED |

Deliberately out of scope for P2.1 (tracked in `AUDIT_SECURITY.md`): SEC-01
(scoped `filesystem.read` default), SEC-03 (record checksums), SEC-04
(`DirectExecutor` public), SEC-05 (redaction hooks), SEC-07 (`reqwest` default
feature). The event log remains best-effort *telemetry*, not a tamper-evident
audit; ADR-002's audit chain is the P2 milestone that owns that claim.

## 2. Findings verification (before changing anything)

- **SEC-02** — `LocalFileStore::open` called `restrict_dir` (chmod 0700)
  *before* `create_dir_all`, so freshly created root/namespace dirs inherited
  the process umask (0o777 & ~umask). `append_event` created log files and
  `atomic_write` created temp files with `File::create` (0o666 & ~umask) and
  tightened them only *after* creation — a window where the file exists with
  wider permissions, and no correction if the chmod itself fails.
- **SEC-08** — `run_task` loaded a session snapshot and ran the full pipeline;
  two concurrent runs each wrote their fold via `update_session` (overwrite
  document semantics, ADR-006). Last writer wins, silently dropping the other
  run's transcript additions. `spawn_task` captured the session at spawn time,
  so a background run that started later ran against a stale snapshot.
- **SEC-06/PC-Q8** — `PersistenceSink::write` logged `append_event` failures
  only to `tracing`; nothing programmatic (no counter, no doctor check, no
  handle) exposed that the event log had silently diverged. Required-durable
  records (session/task/execution) were already awaited and `?`-propagated in
  the pipeline — that part needed no change.

## 3. Design decisions

### 3.1 SEC-02 — create private, then tighten

- New `create_dir_private(dir)`: on unix,
  `std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)` — the
  mode is applied at creation, independent of the process umask; an
  `AlreadyExists` result is accepted. Non-unix falls back to
  `tokio::fs::create_dir_all` (no mode construct exists; documented platform
  limitation).
- `LocalFileStore::open` now creates the root and each namespace through
  `create_dir_private`, then re-applies `restrict_dir` (chmod 0o700) to
  *tighten* a pre-existing store created with a permissive mode. This replaces
  the old chmod-before-create ordering that made `restrict_dir` a no-op on new
  directories.
- `append_event` (event log) and `atomic_write` (records) create files with
  `OpenOptions.mode(0o600)` at open on unix, before any content is written.
  `atomic_write` also switches to `create_new(true)` on a unique-suffix temp
  file so the rename target inherits 0o600. `restrict_file` is kept as
  redundant enforcement.
- Permissions are never relaxed: tests assert 0o700 dirs / 0o600 files.

### 3.2 SEC-08 — per-session locks at the runtime entry points

- New `openrt-runtime/src/locks.rs` → `SessionLocks`
  (`Arc<Mutex<HashMap<SessionId, Arc<Mutex<()>>>>>`). `lock(id)` releases the
  registry lock before awaiting the per-session lock, so a busy session never
  blocks lookups for other sessions: no accidental global serialization and no
  lock-ordering hazard between sessions.
- `Runtime::run_task` takes the per-session lock, then loads the session and
  runs the pipeline while holding it.
- `Runtime::spawn_task` validates session existence up front (fail fast) but
  the `TaskHandle` no longer snapshots the session; `execute()` acquires the
  per-session lock and loads the session fresh inside it.
- Store semantics are unchanged (still overwrite-document, ADR-006);
  serialization is enforced at the runtime entry points, leaving the
  `PersistenceStore` contract untouched.

### 3.3 Event durability — best-effort telemetry, never silent

The smallest Phase-1-compatible resolution (ADR-002 audit chain is P2 work):

- `EventLogHealth`: atomic `total`/`failures` counters + last-error string,
  owned by `PersistenceSink`, exposed publicly as
  `Runtime::event_log_health` (snapshot view). `PersistenceSink::write` counts
  every append as success or failure.
- `openrt doctor` gained an `event-log` check: `PASS` when `failures == 0`,
  `WARN` with count + last error otherwise.
- The runtime call path is not blocked on event appends (per EVENT_MODEL §3
  callers never stall), but failures are now observable programmatically and
  by operators instead of only in stderr.
- Required-durable records keep their existing behavior: a store `Err` on
  session/task/execution write fails the call (`?` propagation, verified by a
  new regression test).

## 4. Files changed

| File | Change |
|---|---|
| `crates/openrt-persistence/src/store.rs` | SEC-02: `create_dir_private`, open-loop tightening, `0o600` at open for log + temp files, `create_new(true)`; +2 permission tests |
| `crates/openrt-runtime/src/locks.rs` | NEW: `SessionLocks` registry + 3 unit tests |
| `crates/openrt-runtime/src/runtime.rs` | Lock wiring in `run_task`/`spawn_task`/`TaskHandle::execute`; `event_log_health` field; sink-health capture in `build` |
| `crates/openrt-runtime/src/sinks.rs` | `EventLogHealth`/`EventLogSnapshot`; `PersistenceSink` success/failure accounting |
| `crates/openrt-runtime/src/doctor.rs` | `event-log` check |
| `crates/openrt-runtime/src/lib.rs` | `pub mod locks` |
| `crates/integration-tests/Cargo.toml` | `async-trait` dev-dependency (test double only; runtime dependency graph unchanged) |
| `crates/integration-tests/tests/session_concurrency.rs` | NEW: SEC-08 end-to-end tests |
| `crates/integration-tests/tests/durability.rs` | NEW: PC-Q8/SEC-06 failure-semantics tests |
| `docs/architecture/phase1/RUNTIME_LIFECYCLE.md`, `EVENT_MODEL.md` | Updated to the new semantics |
| `docs/architecture/phase2/P2_1_SECURITY_HARDENING.md` | This document |

## 5. Tests added

| Test | Location | Proves |
|---|---|---|
| `fresh_cut_dirs_and_files_are_private` | store.rs | root + all namespaces 0o700; session/event files 0o600 |
| `existing_permissive_store_is_tightened_on_open` | store.rs | pre-existing 0o755 store chmod'd to 0o700 on reopen |
| `acquirable_uncontended` | locks.rs | basic acquire/release |
| `same_session_serializes` | locks.rs | a second run on the same session must wait |
| `different_sessions_do_not_serialize` | locks.rs | acquire for session B while A is held (50 ms bound) |
| `concurrent_runs_on_same_session_keep_every_fold` | session_concurrency.rs | 8 racing runs → 16-message transcript, all 8 results present, 8 tasks durable |
| `different_sessions_stay_independent` | session_concurrency.rs | two sessions' folds stay isolated |
| `event_log_failures_are_surfaced_not_silent` | durability.rs | all-append-failing store: task still completes via records; health shows failures; doctor `event-log` WARN |
| `record_write_failure_fails_the_call` | durability.rs | failing `save_task` propagates an `Err` out of `run_task` |

**Quality gate (2026-09-17), all green:**

- `cargo fmt --all -- --check` — PASS
- `cargo check --workspace` — PASS
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — PASS
- `cargo test --workspace` — PASS, 108 tests / 0 failures (99 before, +9 new)
- `cargo build --workspace` — PASS

## 6. Remaining security risks (documented, not silently accepted)

- **Non-unix platforms** (Windows): `create_dir_private`/file `mode` have no
  construct; behavior degrades to umask/ACL semantics (`restrict_*` are
  no-ops there). Documented limitation; no code path claims otherwise.
- **Event log is telemetry, not audit.** No checksums/chain (SEC-03), and
  `list_events` silently skips unparsable lines (SEC-12). Fail-closed
  audit-write semantics (PC-Q8) remain coupled to the ADR-002 audit chain in
  P2 — this milestone surfaces failures instead. No tamper-evidence is claimed.
- **SEC-01 / SEC-07 remain open** (SHOULD-FIX-DURING-P2 per the audit):
  `filesystem.read` is still allow-by-default with `workspace_dir` unenforced;
  `reqwest`/rustls still enter the default build through `openrt-models`'s
  default `ollama` feature. Intentionally not in P2.1 scope.
- **`SessionLocks` registry growth:** one `Arc<Mutex<()>>` per distinct
  session id in the process lifetime. Bounded by session churn; a cleanup or
  sharded design is a P2+ concern, not P1/P2.1.
- **`doctor`'s store probe** still appends a probe event directly to the store
  (bypassing the sink), so it is not counted in `event_log_health`. It does
  exercise append + read-back; this asymmetry is documented here.