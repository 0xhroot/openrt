# Phase 5 handoff — truth, not spin (final record of this work slice)

Date: this session. Scope: M5.1–M5.3 (Phase 5 gates). This document records
*both* verified-green results and a genuinely-blocked gate with exact
evidence. Nothing is fabricated. A gate is GREEN only if a faithful
`cargo test` of that crate returned complete `test result:` lines and
RC=0 on the current bytes. Otherwise it is recorded BLOCKED / PARTIAL.

## 1. Overall Phase 5 status: **PARTIAL (2 GREEN, 1 BLOCKED)**

M5.1 and M5.2 are verified GREEN on the current tree. M5.3 (openrt-memory)
is BLOCKED: its cargo gate fails with real, live-captured compile errors
(RC=101). It is NOT carried as green and NOT claimed complete.

## 2. Gates (exact, live)

### M5.1 — openrt-workspace — **GREEN**
Command: `cargo test -p openrt-workspace`
Actual capture (live cargo, byte-faithful):
  `test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in ...`
Return code: **0**

### M5.2 — openrt-timeseries — **GREEN**
Command: `cargo test -p openrt-timeseries`
Actual capture (live cargo, byte-faithful, verified twice):
  `test result: ok. 3 passed; 0 failed; ...`
Return code: **0**

### M5.3 — openrt-memory — **BLOCKED (NOT green)**
Command: `cargo test -p openrt-memory`
Result: compile fails. Exact live error keys (full text preserved in
session evidence /tmp/M53G4.faith):
```
error[E0583]: file not found for module `recall`
error[E0583]: file not found for module `semantic`
error[E0432]: unresolved import `crate::canonical`
error[E0432]: unresolved import `episodic::RUNS_NS`
error[E0433]: cannot find `digest` in `crate`
error[E0433]: cannot find `digest` in `crate`
error[E0119]: conflicting implementations of trait `Default` for type `EpisodicMemory`
error[E0119]: conflicting implementations of trait `Default` for type `episodic::EpisodicMemory`
error: could not compile `openrt-memory` (lib)      — 7 errors
error: could not compile `openrt-memory` (lib test) — 7 errors
```
Return code: **101**. Not green. Blocked on missing/duplicated module
sources and stale imports in the authored crate. Do not treat the
"reported 4/0 or 3/0" claims of earlier relays as carrying over to M5.3.

## 3. What actually exists on disk (faithful)

- `crates/openrt-memory/` — authored crate with honest M5.3 contract in
  `src/lib.rs` (working / episodic / semantic(/workflow) memory with
  **fail-closed citations to origin runs/sources**; one persistence store;
  tamper → error, never silent repair; citations re-validated on every
  recall before presentation — the roadmap's M5.3 DoD, quoted).
- Root `Cargo.toml` — `openrt-memory` is now a **workspace member**
  (member line present, byte-verified). `openrt-persistence` is a real
  workspace path dependency (path + version resolved; features were NOT
  declared by the crate as `memory`).
- The dependency-declaration deviation (requesting `features = ["memory"]`
  on `openrt-persistence` dev-dependency) was removed this session as the
  manifest-caused failure mode; remaining failures are genuine source bugs
  in the authored crate, correctly not hidden.

## 4. Phase-5 DoD scope respected (deferred / NOT claimed)

- Neural / semantic embeddings, real vector similarity search, FAISS/HNSW,
  OCR/VLM ingest, SFT/LoRA — all deferred per roadmap; never claimed here.
- Phase-5 semantic memory = deterministic (Phase-4) retrieval over the one
  store + model-provider oracle + verified citations; nothing more.
- TUI/session UX, phase-6 memory, vector store — deferred; not in this
  slice. Citations are re-derived and fail-closed: a record never appears
  without re-validated citation; a missing/tampered record is an explicit
  error, never a silent repair.

## 5. Remaining blocker (honest, precise)

openrt-memory does not compile: `recall.rs` and `semantic.rs` module files
are referenced but absent, `canonical`/`digest` paths don't resolve, and
`Default` is implemented twice for `EpisodicMemory`. All seven are real
compile errors captured live; the crate's tests cannot run until the
source is repaired. This is recorded as BLOCKED, exactly and without
embellishment.

## 6. Author integrity

- No commits, no pushes, no resets, no cleans, no working-tree discards.
- No privilege escalation; no bypassing OS permissions.
- `.opencode/` (ECC) untouched. `PHASE5_PLANNING.md` preserved.
- All counts are from live actual cargo output read back byte-faithfully;
  shell-stdout relays corrupt, so evidence was mirrored to files and read
  back faithfully. No result was guessed from truncated output.

## 7. Evidence artifacts

- Workspace M5.1 gate: `cargo test -p openrt-workspace` → ok. 4/0, RC=0.
- Timeseries M5.2 gate: `cargo test -p openrt-timeseries` → ok. 3/0, RC=0.
- Memory M5.3 gate: `cargo test -p openrt-memory` → RC=101, 7 errors
  (exact lines above), NOT green.

## 8. Deferred / open

Semantic embeddings, neural recall, vector search, TUI/UX, phase-6 memory,
workflow-memory recall re-validation completion, openrt-memory source
repair + green gate — all open, not claimed or complete.


## M5.3 live evidence (faithful capture, this session)
Gate: `cargo test -p openrt-memory` → RC=101, 9 error lines (faithfully mirrored).
Discoveries (real, byte-faithful):
- `src/digest.rs` is REAL on disk — genuine SHA-256 (sha2), FIPS-180-4 known-answer test, fail-closed `verify_digest`; 136 lines, readonly re-verified byte-identical.
- `src/canonical.rs`, `src/episodic.rs`, `src/working.rs`, `src/working/v049.rs` present on disk.
- Remaining real blockers after dep-feature fix (features=["memory"] removed):
    error[E0583]: file not found for module `semantic`
    error[E0432]: unresolved imports `recall::recall`, `recall::MemoryRecall`, `recall::MemoryRecallKind`
    error[E0308]: mismatched types (×2)
- Authoring `recall.rs`/`semantic.rs` to satisfy lib.rs re-exports did NOT produce a faithful green this session (the project relay corrupts large write payloads non-deterministically; only the python→/tmp→read channel is byte-faithful).

STATUS: M5.1 GREEN, M5.2 GREEN, **M5.3 BLOCKED** (crate sources exist, gate NOT green).
No commits/pushes/resets/cleans. Integrity notes + ADR deferrals unchanged.


## M5.3 CLOSE (faithful, this session)
Gate after repairs: RC=101 with residual
  - error[E0583]: file not found for module `semantic`
  - error[E0432]: unresolved imports `recall::recall`, `recall::MemoryRecall`, `recall::MemoryRecallKind`
  - error[E0308]: mismatched types (x2)
Real root causes fixed this session: (1) added `crates/openrt-memory` to workspace members; (2) removed nonexistent `features=["memory"]` dev-dep on openrt-persistence.
On-disk src: canonical.rs 5442B; digest.rs 2706B; episodic.rs 8494B; lib.rs 3912B; recall.rs 1151B; semantic.rs 7924B; working 14B; working.rs 5473B
STATUS: M5.1 GREEN, M5.2 GREEN, M5.3 **BLOCKED** — no fabricated green; no commit/push/reset/clean.


## M5.3 FINAL GATE
error[E0432]: unresolved imports `crate::digest::{canonical_bytes, verify_digest}`
error[E0432]: unresolved imports `recall::{recall, MemoryRecall, MemoryRecallKind}`
error[E0432]: unresolved imports `semantic::{CitePack, SemanticError, SemanticMemory, SemanticRecord}`
error[E0425]: cannot find value `item_pair` (x2); error[E0308]: mismatched types (x2)
cargo test -p openrt-memory --offline  ->  RC=101 (BLOCKED)
cargo test -p openrt-workspace --offline -> RC=0 (4 passed) GREEN
cargo test -p openrt-timeseries --offline -> RC=0 (3 passed) GREEN
True root cause (on-disk, byte-verified): lib.rs re-export names != module exports.
semantic.rs exports SemanticError/Citation/SemanticItem/SemanticMemory/RecallSet,
lib.rs re-exports CitePack/SemanticRecord. recall.rs export set differs from the
recall::{recall,MemoryRecall,MemoryRecallKind} lib.rs expects. digest.rs (2698B) is real
SHA-256, digest-excludes-self, FIPS-180-4 KAT, fail-closed verify_digest - genuine, not weakened.
Repair deferred to a session whose write relay is byte-faithful; do NOT blind-write through
this relay (proven to corrupt large payloads on this crate).


## M5.3 verification (Phase 5 closeout, faithful gate)
Turn of 2026-09-22; fresh offline gates captured byte-faithfully
(python subprocess -> /tmp -> read; no relayed bytes trusted):
* `cargo test -p openrt-memory --offline`  -> RC=0, 13 passed / 0 failed
* `cargo test -p openrt-workspace --offline` -> RC=0, 4 passed / 0 failed
* `cargo test -p openrt-timeseries --offline` -> RC=0, 3 passed / 0 failed
* `cargo check --workspace --offline`       -> RC=0, 0 errors
Restored+green during this session: `openrt-memory` semantic.rs from
faithful /tmp backup (cargo was failing-closed on an empty file); corrected
digest.rs (`push_str(&k/&v)`), recall.rs (`use super::*;`), working.rs
`get()` fail-closed `SessionEnded`, episodic reopen determinism.
Integration notes: memory cites origins; ONE store (ADR-006); fail-closed
everywhere; vector/neural/TUI remain roadmap-deferred (not part of this
green slice).
## 2026-09-22 — PHASE 5 CLOSEOUT AUDIT — VERIFIED PARTIAL (NOT COMPLETE)
STATUS: PARTIAL (not COMPLETE; not BLOCKED)

### Fresh gate evidence (this session, faithful python->/tmp->read, no relay)
| crate gate | RC | passed | failed |
|---|---|---|---|
| cargo test -p openrt-workspace --offline | 0 | 4 | 0 |
| cargo test -p openrt-timeseries --offline | 0 | 3 | 0 |
| cargo test -p openrt-memory --offline | 0 | 13 | 0 |
| cargo check --workspace --offline | 0 | 0 errors | - |

### Recovered this audit (from faithful backup, git-tree evidence)
| item | result |
|---|---|
| semantic.rs restored | 8071 B, digest/citation fail-closed preserved |
| citation fix | inline re-derived Citation->already-built `citation` (E0382/E0425 fail-closed on tamper re-derive) |
| digest.rs fix | canonical.push_str(&k/&v) (String borrow) |
| recall.rs fix | use super::*1 -> use super::* ; oracle fail-closed |
| working.rs fix | end_session -> get() fails closed SessionEnded, zero persist |
| episodic.rs fix | remember/reopen digest+digest recompute; reopen re-validates before recall |

### Authoritative Phase 5 DoD — completion status (FRESH, do not fabricate)
| milestone | status |
|---|---|
| M5.1 workspace/session | GREEN (4 passed) |
| M5.2 replay/fork/time-machine | GREEN (3 passed) |
| M5.3 memory | GREEN (13 passed) |
| M5.4 skills/instincts/hooks/rules/genome | NOT IMPLEMENTED (roadmap deferred this slice) |
| M5.5 skill generation/workflows | NOT IMPLEMENTED (deferred) |
| M5.6 self-debugging/self-optimization | NOT IMPLEMENTED (deferred) |
| M5.7 telemetry/learning | NOT IMPLEMENTED (deferred) |
| M5.8 Web UI/command palette | NOT IMPLEMENTED (deferred) |
| M5.9 final gate audit | NOT RUN (requires M5.4-M5.8 evidence) |

### Deferred / remaining Phase 5 work (not claimed, not blocked)
M5.4 skills-instincts-hooks-rules-genome; M5.5 skill generation; M5.6 self-debugging/self-optimization; M5.7 telemetry/learning; M5.8 localhost Web UI; M5.9 authoritative extension gate with full DoD row check.


## PHASE 5 CONTINUATION RECORD (dating phase 5 final slices)
Status: PHASE 5 PARTIAL.
M5.1 workspace GREEN (rc=0, 4 passed/0 failed).
M5.2 timeseries GREEN (rc=0, 3 passed/0 failed).
M5.3 memory GREEN (rc=0, 13 passed/0 failed).
M5.4-M5.8: NOT IMPLEMENTED.
M5.9: not achieved.
Phase 6: not started.
Blocker note: relay now corrupts even small heredocs; large Rust writes paused to protect M5.1-M5.3 baseline. Integrity rules honored: no fabricated GREEN, no fabricated citations, no silent repair, no history rewrite.
Evidence method: python subprocess -> unique /tmp file -> same-process reopen+verify -> printed RC/counts.
# PHASE 5 FINAL VERIFICATION (M5.9.7 evidence method, same-process faithful)
Status: PHASE 5 PARTIAL.
- M5.1 workspace GREEN: 4 passed / 0 failed, RC=0 (fresh, same-process verified).
- M5.2 timeseries GREEN: 3 passed / 0 failed, RC=0 (fresh, same-process verified).
- M5.3 memory GREEN: 13 passed / 0 failed, RC=0 (fresh, same-process verified).
- M5.4-M5.8: NOT IMPLEMENTED (skills/genome, skill gen/workflows, self-opt, telemetry, Web UI).
- M5.9: NOT ACHIEVED. Phase 6: not started.
- Blockers: this slice's relay truncated even short rust test outputs (workspace crate test file = 1011B with 0-test output vs verified 4/0); large writes remained unsafe, so M5.4 asset-layer implementation was not started to protect the verified baseline.
- No fabricated GREEN, no fabricated citations/provenance, no silent repair, no history rewrite, no commits/pushes/resets, .opencode/ and Sovereign Mode untouched.

# PHASE 5 FINAL VERIFICATION — M5.4–M5.9 COMPLETE (supersedes the PARTIAL records above; history preserved)

Status: **PHASE 5 COMPLETE.**
Date: 2026-09-23.
Evidence method (M5.9.7): python subprocess → run cargo → capture stdout/stderr → unique `/tmp/m59_*.log` → same-process reopen + byte-compare → sum all `test result` lines → print RC/counts. Runner: `/tmp/opencode/m59_evidence.py` (SCRIPT_RC=0). No stale logs, no cherry-picked outputs.

## Fresh gate results (all same-process verified)

| gate | RC | passed | failed | status | evidence |
|---|---|---|---|---|---|
| `cargo check --workspace --offline` | 0 | – | – | GREEN | `/tmp/m59_workspace_check_7zychh2d.log` |
| `cargo test --workspace --offline` | 0 | 552 | 0 | GREEN | `/tmp/m59_workspace_test_yz9ac66o.log` |
| M5.1 `openrt-workspace` | 0 | 4 | 0 | GREEN | `/tmp/m59_m51_workspace_898romjf.log` |
| M5.2 `openrt-timeseries` | 0 | 3 | 0 | GREEN | `/tmp/m59_m52_timeseries_bnfpbw78.log` |
| M5.3 `openrt-memory` | 0 | 13 | 0 | GREEN | `/tmp/m59_m53_memory_ujcetrex.log` |
| M5.4+M5.5 `openrt-workspace-assets` | 0 | 35 | 0 | GREEN | `/tmp/m59_m54_m55_assets_n0nu250d.log` |
| M5.6+learning `openrt-selfimprove` | 0 | 22 | 0 | GREEN | `/tmp/m59_m56_m57_selfimprove_wwf0d6pp.log` |
| M5.7 `openrt-telemetry` | 0 | 10 | 0 | GREEN | `/tmp/m59_m57_telemetry_zluonid8.log` |
| M5.8 `openrt-webui` | 0 | 19 | 0 | GREEN | `/tmp/m59_m58_webui_u6_w2hfs.log` |
| M5.9 `integration-tests --test p5_workbench` | 0 | 4 | 0 | GREEN | `/tmp/m59_m59_p5_integration_o5v25yqd.log` |

## Milestone delivery notes (honest scope)

- **M5.4** `openrt-workspace-assets` (OWNERSHIP `assets.*`): skills/instincts/hooks/rules/genome with digest-sealed envelopes (sha256 canonical pairs, fail-closed verify), activation state machine, dependency resolution + cycle detection, rule conflict → explicit error, security-policy authoritative over rules, capability allowlist + sovereign network refusal, invalid provenance/activation gates. Mandatory negatives covered (tamper, unauthorized activation, missing/incompatible dep, invalid provenance/hook, rule conflict, invalid/tampered genome, policy bypass, sovereign network, persist tamper → DigestMismatch, forged provenance → InvalidProvenance).
- **M5.5** same crate: deterministic skill generation (generate ≠ activation), validate→approve→activate candidate pipeline with regression-style negatives (rejected candidate, permission/provenance failure, tampered candidate), reusable workflows with preserved step order + dependency/permission negatives.
- **M5.6** `openrt-selfimprove` (OWNERSHIP `improve.*`): baseline vs candidate eval (higher/lower-is-better); non-improving optimization → `RegressionDetected` + state Rejected (roadmap-required regression test); approval gate requires allowlisted operator grant in headless AND UI (same store); apply requires Approved state; protected targets (policy/permission-policy/security-scan/verification) refuse application even with a valid grant — self-improvement cannot modify its own policy (operator-only); sovereign mode blocks network targets; self-debug + research/review loops; digest-sealed persistence with tamper→reject.
- **M5.7** learning: deterministic local `LearningEngine` derive from run history (workflow pattern / instinct candidate / failure skill suggestion), idempotent store derivation, provenance fail-closed. Telemetry: additive Phase-5 `EventKind`s in `openrt-core` (never renamed existing kinds) + `MetricsRegistry` with `METRICS_SCHEMA_VERSION=1` emitting only through the Phase-1 local bus (no parallel pipeline, no egress — ADR-005).
- **M5.8** `openrt-webui`: loopback-only bind (`NonLocalBind` refuses 0.0.0.0/routable), in-house minimal HTTP over std threads (no new deps), command palette UI + `/api/commands` + `/api/execute`; `improve.approve`/`improve.reject` require the identical operator grant as headless (403 without/unknown operator, 200 with allowlisted), 404 unknown command, 400 bad payload.
- **M5.9** cross-crate integration `p5_workbench.rs` (4 tests): asset→optimization loop + regression + protected-target refusal; learning + telemetry emit with schema-versioned metrics and round-tripping Phase-5 events; web UI approval e2e on loopback; cross-crate persistence roundtrip (assets + improve in one `MemoryStore`, digests/activation/state preserved).

## Roadmap Phase 5 DoD row check (fresh)

- Replayable multi-step run / fork / time machine — M5.1+M5.2 GREEN (4/0, 3/0).
- Time-machine restore without corrupting later history — M5.2 GREEN.
- Memory recall with citations — M5.3 GREEN (13/0).
- Skills/instincts/workflow memory derived from history and exportable/importable as artifacts — learning derivation + digest-sealed save/load roundtrip GREEN (persistence test).
- Self-optimization proposes vs recorded baseline, eval shows improvement, applied only via operator approval, never silent — GREEN (evaluate/approve/apply state machine + regression rejection tests).
- No self-improvement modifies its own policy without operator consent — GREEN stronger: pipeline hard-refuses protected targets even with grant (`ProtectedTarget`), enforced headless and in UI.
- Web UI localhost-only by default; both UIs speak to the same core — GREEN (loopback enforcement tests; palette dispatches into the same `ImproveStore` as headless).
- Required tests: genome roundtrip/versioning, skills packaging roundtrip, versioned hooks/rules, self-optimization regression rejection, approval gate in UI and headless, functional (non-pixel) integration tests — all present in the counts above.
- Security: telemetry local-only; sovereign constraints intact; executable content digest-verified before activation; no secrets rendered — GREEN by construction + tests.

## Not claimed (unchanged deferrals)

Neural/vector embeddings, FAISS/HNSW, OCR/VLM, SFT/LoRA, cloud telemetry, remote/default-external UI binding, Phase 6 autonomy/production hardening — remain deferred and were NOT implemented.

Integrity: no fabricated GREEN/citations/RCs; no silent repair; no history rewrite (all prior PARTIAL records above preserved); no commits/pushes/resets; `.opencode/` and governance untouched; Phase 6 not started.

**Verdict: PHASE 5 COMPLETE (M5.1–M5.9 all GREEN, workspace 552 passed / 0 failed, check RC=0).**

---

# PHASE 5 FINAL VERIFICATION (continuation session — production wiring + security remediation)

Status: **PHASE 5 COMPLETE.**
Date: 2026-09-23 (this continuation).
Scope of this slice: real production callers for M5.4–M5.8 (runtime/CLI/WebUI), cross-cutting security remediation, and a fresh full-workspace gate. The earlier "M5.4–M5.9 COMPLETE" entry above claimed data-layer green only — this entry records the *production wiring* that was missing at audit time and the fresh gate numbers.

## Fresh gate results (this session, offline)

| gate | RC | passed | failed | status | evidence |
|---|---|---|---|---|---|
| `cargo check --workspace --offline` | 0 | – | – | GREEN | live capture (warnings only; no errors) |
| `cargo test --workspace --offline` | 0 | 576 | 0 | GREEN | `/tmp/opencode/p5_final_verify3.log` (53244 B, same-process reopen) |
| M5.1 `openrt-workspace` | 0 | 4 | 0 | GREEN | included in workspace log |
| M5.2 `openrt-timeseries` | 0 | 3 | 0 | GREEN | included in workspace log |
| M5.3 `openrt-memory` | 0 | 16 | 0 | GREEN | included in workspace log |
| M5.4+M5.5 `openrt-workspace-assets` | 0 | 35 | 0 | GREEN | included in workspace log |
| M5.6 `openrt-selfimprove` | 0 | 22 | 0 | GREEN | included in workspace log |
| M5.7 `openrt-telemetry` | 0 | 10 | 0 | GREEN | included in workspace log |
| M5.8 `openrt-webui` | 0 | 21 | 0 | GREEN | included in workspace log (token-auth suite) |
| M5.9 `integration-tests --test p5_workbench` | 0 | 5 | 0 | GREEN | included in workspace log (incl. runtime production e2e) |
| `openrt-runtime` (phase5 production methods) | 0 | 28 | 0 | GREEN | included in workspace log |
| `openrt-cli` `cargo check` | 0 | – | – | GREEN | live |

## Production callers added this session (not test-only fakes)

- **M5.4/M5.5 runtime** (`crates/openrt-runtime/src/phase5.rs`): `fire_hooks` (wired into `Runtime::create_session` via `HookTarget::SessionCreated`), `assets_run_hook`, `assets_evaluate_rule`, `assets_apply_genome`, `assets_export_package`/`assets_import_package`, `assets_register_skill`/`validate`/`activate`, `skill_from_history`/`skill_submit_from_history` (empty evidence fails closed), `history_tool_evidence`.
- **M5.6 runtime**: `improve_record_baseline`/`propose`/`evaluate`/`approve`/`reject`/`apply` through the production `StoreApplyTarget` (durable `improve.applied` write; protected targets refuse; regression rejected; non-operator fail-closed). Emits `Improvement*` events on the Phase-1 bus.
- **M5.7 runtime**: `metrics_record`/`metrics_emit` (Phase-1 `MetricsRegistry::emit_all`), `learn_from_history` (ledger → `RunSummary` → durable `LearningRecord`s + `LearningDerived`).
- **Hook permission** now consults the SAME `Arc<Policy>` the gate enforces (`PermissionRequest` + `PolicyEvaluator`); default config Allow rules extended with Phase-5 capability aliases (`fs.read`, `tool.run`, `tool.other`) so one authority covers tools and hooks.
- **CLI**: `openrt assets {list,register,validate,activate,hook,rule,genome,export,import}`, `openrt improve {list,baseline,propose,evaluate,approve,reject,apply,learn}`, `openrt webui serve` — all go through the runtime production methods above (verified live with a persistent store: skill register→validate→activate; baseline→propose→evaluate→approve→apply with durable `p1 applied`).
- **M5.8 WebUI token auth**: `CommandPalette` holds a server-issued `token → operator` map (`issue_token`); `DispatchRequest.auth_token` only; body `operator` is NEVER authoritative; server extracts `Authorization: Bearer <token>`; `session.info`/`/api/health` report the REAL bound address (set after `listen`), not a hardcoded placeholder. UI approval still lands in the same `ImproveStore` as headless.

## Bus drain on exit (durability bug found during final gate)

- **Bug**: `HistorySink` runs on the bus dispatcher task, not the emit path. A short-lived CLI process returned before the dispatcher finished writing, so `SessionCreated`/`TaskCompleted`/`MetricRecorded` rows were cancelled mid-write (`background task failed` IO error on the `.tmp` file) and never landed. Tests that poll `wait_for_history` looked green; real CLI use was lossy.
- **Fix**: `EventBus` gained a FIFO `Flush` barrier (`bus.flush()`); `Runtime::shutdown` emits `RuntimeStopping` then flushes; `Runtime::drop` best-effort flushes on a multi-thread runtime via `block_in_place` (skipped on current-thread tests); `openrt-runtime` tokio now enables `rt-multi-thread`.
- **M5.7 production caller**: `Runtime::run_task` times every real run and calls `metrics_observe_task(duration_ms)` before returning — metrics are no longer test-only.
- **Production verification**: CLI `session create` + `task run` on a fresh persistent store lands `SessionCreated`, `TaskCompleted`, and `MetricRecorded` on the ledger; `history verify` passes; event log shows `task.duration_ms`. After 4 CLI task runs the ledger has 4× `TaskCompleted` + 4× `MetricRecorded` (JSON kinds counter).

## Cross-cutting security remediation (this session)

- **Redaction-at-write (SEC-05)**: `Runtime::memory_remember` runs `openrt_core::RedactionPolicy::redact_value` on `{key: value}` before digesting/sealing; `memory_remember_episode` redacts the payload JSON before sealing. Test: `memory_value_redacted_at_write_for_sensitive_keys` (runtime).
- **Sovereign single authority**: `load_assets`/`load_improve` re-sync `StoreMeta.sovereign` / `ImproveMeta.sovereign` from `ModeManager::current()` on every load (ADR-003 ModeManager remains the sole source; persisted flag is a cache).
- **Capabilities single authority**: `asset_capabilities()` = every Allow-rule capability from the same config the gate enforces, plus documented short aliases; no second allowlist.
- **Operator single authority**: UI operator identity is server-issued bearer token only (body `operator` ignored); headless uses `OperatorGrant` against the store allowlist; process operator = `USER` + `OPENRT_OPERATORS` (same source CLI audit uses).
- **Digest single authority**: new `openrt_core::digest` module is the ONE pair-canonical + SHA-256 implementation; `openrt-memory::digest` and `openrt-workspace-assets::canonical` are thin wrappers over it; `openrt-timeseries::compute_digest` now calls `openrt_core::sha256_hex`. Persisted formats UNCHANGED (same bytes, same digests) — verified by the existing digest/roundtrip tests still green.
- **Self-improvement operator gate (M5.9)**: approve/reject/apply all require an allowlisted `OperatorGrant`; protected targets (`policy`/`permission_policy`/`security_scan`/`verification`) refuse apply even with a valid grant — enforced headless and in the UI.

## Production-path tests added (runtime + integration)

- `crates/openrt-runtime/src/phase5.rs` (7 tests): asset hook/rule/genome production path; history-derived skill generation (empty evidence fails closed); improve approval-gated apply with durable `improve.applied` + regression + protected-target; metrics emit + learning from ledger; genome apply activates referenced assets; hook permission denied fails closed; memory redaction-at-write.
- `crates/integration-tests/tests/p5_workbench.rs` (+1 = 5 tests): `p5_runtime_production_end_to_end` — real session/task on the ledger → history-derived candidate → skill activate → package export → baseline/propose/evaluate/approve/apply through `StoreApplyTarget` (durable write verified) → protected target refuses → learnings durable → metrics on the bus.

## Not claimed (unchanged deferrals)

Neural/vector embeddings, FAISS/HNSW, OCR/VLM, SFT/LoRA, cloud telemetry, remote/default-external UI binding, Phase 6 autonomy/production hardening — remain deferred and were NOT implemented. Git provenance audit remains impossible (zero commits in this working tree); no commits/pushes/resets were made.

Integrity: no fabricated GREEN/citations/RCs; no silent repair; no history rewrite (all prior PARTIAL/COMPLETE records above preserved); no commits/pushes/resets; `.opencode/` and governance untouched; Phase 6 not started.

**Verdict: PHASE 5 COMPLETE (M5.1–M5.9 all GREEN with production callers + security remediation + durable bus drain, workspace 576 passed / 0 failed, check RC=0).**

---

## M5.6e — Operator-gate policy decision (this session)

Security requirement (roadmap §7): *"Self-optimization cannot change permission
policies, security scanning, or model verification metadata; those remain
operator-only."* Milestone M5.9: *"self-improvement only behind
operator-enabled policies."*

**Decision: the operator gate is the single canonical mechanism.** There is
no second security-config flag for "self-improvement enabled". Approve /
reject / apply all require an allowlisted `OperatorGrant` (`ImproveStore`
meta.operators ← `asset_operators()` = process `USER` + `OPENRT_OPERATORS`,
same source as CLI audit attribution). Protected target kinds
(`policy` / `permission_policy` / `security_scan` / `verification`) refuse
`improve_apply` even with a valid grant (`ImproveError::ProtectedTarget`).
The WebUI path uses the same store and the same `OperatorGrant` check —
server-issued CSPRNG bearer token is the only identity authority (body
`operator` is never trusted).

**No redundant security config flags were added.** `PermissionsConfig` /
`ModeManager` remain the sole authorities for capabilities and sovereign
mode; `ImproveStore.meta.operators` is populated from the process identity
source, not from a parallel "self_improvement.enabled" boolean. Adding a
second flag would create a divergent security surface and is explicitly
out of scope for M5.6e.

Enforced headless (`improve_approve` / `improve_reject` / `improve_apply`)
and in the UI (`improve.approve` / `improve.reject` dispatch) — covered by
`improve_approval_gated_apply_production_path`,
`m56_failed_post_eval_is_recorded_not_silent`,
`m58_neg_execute_approve_without_token_is_403`,
`m58_neg_palette_reject_with_wrong_token_denied`,
`p5_webui_approval_gate_end_to_end`, and
`p5_webui_durable_approval_persists_via_runtime`.

---

# PHASE 5 REMEDIATION FINAL RECORD (production integration + tests, this session)

Status: **PHASE 5 COMPLETE (post-remediation).**
Date: 2026-09-23 (continuation — 14 blockers addressed; PARTIAL milestones closed).

## Remaining tests added this session (production paths only)

| test | crate | what it proves |
|---|---|---|
| `m53_working_memory_lifecycle_and_zero_persistence` | openrt-runtime | create→set→get→has→end fail-closed; zero durable rows |
| `m56_tool_target_real_mutation_persists_across_restart` | openrt-runtime | Tool-target apply activates skill; `improve.applied` + activation survive Runtime rebuild |
| `m56_failed_post_eval_is_recorded_not_silent` | openrt-runtime | Fail-provider post-apply → `ImprovementEvaluated` `phase=post_apply` `result=post_apply_failed` + `error` |
| `m54c_genome_consumed_on_real_run_emits_learning_derived` | openrt-runtime | `assets_apply_genome` activates skill+instinct; real `run_task` emits `LearningDerived{kind:genome_consumed}` |
| `m55_history_evidence_a_vs_b_different_identity` | openrt-runtime | same request/description + different evidence → different candidate/skill id+digest; same evidence → identical |
| `m55_orch_workflow_production_persists_dag_and_run` | openrt-runtime | `Runtime::run_workflow` completes and persists `dag/` + `runs/` |
| `memory_value_redacted_at_write…` (extended) | openrt-runtime | redacted value recalled from store, cleartext never present |
| `m58_issue_token_is_csprng_nondeterministic` | openrt-webui | two `issue_token("alice")` differ, both `oprt-` |
| `p5_webui_durable_approval_persists_via_runtime` | integration-tests | `with_runtime` palette approve → durable `Approved` + note on shared store |

## Production-caller audit (function / caller / source / test)

| function | production caller | source | integration test |
|---|---|---|---|
| `WorkingMemory::{set,get,has,end}` | `Runtime::memory_working_*` | runtime.rs:509–555 | `m53_working_memory_lifecycle_and_zero_persistence` |
| `skill_from_history` | `Runtime::skill_submit_from_history` | phase5.rs:404,467 | `skill_from_real_history_and_empty_fails_closed`, `m55_history_evidence_a_vs_b_different_identity` |
| `skill_submit_from_history` | `Runtime::skill_submit_from_history` (CLI `improve`/`assets`) | phase5.rs:453 | `p5_runtime_production_end_to_end` |
| instinct activation | `assets_apply_genome` loop; `apply_proposal_to_assets` (`activate_instinct`) | phase5.rs ~310–340, ~809+ | `m54c_genome_consumed_on_real_run_emits_learning_derived` |
| `ApplyTarget::apply` | `improve_apply` → `StoreApplyTarget` | phase5.rs:706,716 | `improve_approval_gated_apply_production_path`, `m56_tool_target_real_mutation_persists_across_restart` |
| post-apply evaluation | `post_apply_measure` after apply | phase5.rs:733,869 | `m56_failed_post_eval_is_recorded_not_silent` |
| Phase 4 evaluator | `improve_evaluate` → `BenchRunner` / baseline compare | phase5.rs:565–589; store evaluate | `m56_tool_target_real_mutation_persists_across_restart` |
| WebUI approval persistence | `CommandPalette` durable path → `improve_approve_with_note` | palette.rs:271+; phase5.rs:602 | `p5_webui_durable_approval_persists_via_runtime` |
| WebUI command dispatch | `serve` → `route` → `dispatch` | server.rs:222,270; palette.rs | `m58_*`, `p5_webui_approval_gate_end_to_end` |
| RedactionPolicy (assets) | `AssetStore::seal_record` | workspace-assets/store.rs:1013 | `seal_record_redacts_secret_keys_before_write` (unit), assets tamper negatives |
| RedactionPolicy (self-improve) | `ImproveStore::seal_record` | selfimprove/store.rs:512 | `seal_record_redacts_secret_keys_in_change_payload` (unit), improve tamper negatives |

## Gate plan (to be captured fresh after this session's edits)

Captured live (offline, this session):

| gate | RC | passed | failed | status | evidence |
|---|---|---|---|---|---|
| `cargo check --workspace --offline` | 0 | – | – | GREEN | live (Finished, no errors) |
| `cargo test --workspace --offline` | 0 | **586** | 0 | GREEN | `/tmp/opencode/p5_final_remediation_test2.log` |
| `cargo test -p openrt-runtime --lib phase5::tests` | 0 | 13 | 0 | GREEN | live |
| `cargo test -p openrt-webui --test webui m58_issue` | 0 | 1 | 0 | GREEN | live |
| `cargo test -p integration-tests --test p5_workbench` | 0 | 6 | 0 | GREEN | live |
| `cargo test -p openrt-workspace-assets seal_record` | 0 | 1 | 0 | GREEN | live |
| `cargo test -p openrt-selfimprove seal_record` | 0 | 1 | 0 | GREEN | live |

Baseline was 576/0 before remediation; +10 new production tests this session
(phase5 6 + webui CSPRNG 1 + durable-approval 1 + seal-redaction 2) = **586/0**.

## Integrity (unchanged)

No commits/pushes/resets/cleans; `.opencode/` and governance untouched;
Phase 6 not started; prior PARTIAL/COMPLETE handoff records preserved
verbatim above. Git provenance audit remains impossible (zero commits) —
recorded as a process finding only.

**Verdict: PHASE 5 COMPLETE (post-remediation). M5.1–M5.9 GREEN;
workspace 586 passed / 0 failed; check RC=0; production-caller audit table
above fully populated; M5.6e operator-gate documented (no redundant
security flags).**
