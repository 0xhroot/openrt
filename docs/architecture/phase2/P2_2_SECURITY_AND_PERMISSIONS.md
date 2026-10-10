# Phase 2.2 — Security & Permission Boundary

**Status:** Implemented
**Date:** 2026-09-17
**Scope:** Remaining Phase 1 audit P2 work (SEC-01, SEC-03, SEC-04, SEC-05,
SEC-07, SEC-12) plus the ASK approval boundary. All fixes live in existing P1
crates (permissions/tools/runtime/persistence). No Phase 3–6 features, no
workspace rewrites, no weakening of permissions to satisfy tests.

## 1. Findings addressed

| Audit ID | Severity | Title | Status |
|---|---|---|---|
| SEC-01 | HIGH | `filesystem.read` allow-by-default, unbounded | IMPLEMENTED |
| SEC-03 | MEDIUM | records have no integrity protection | IMPLEMENTED (detection; chain = ADR-002) |
| SEC-04 | MEDIUM | `DirectExecutor` public in release builds | IMPLEMENTED (`demo` feature) |
| SEC-05 | MEDIUM | event payloads can carry secrets | IMPLEMENTED (sink-boundary redaction) |
| SEC-07 | SHOULD | `reqwest`/rustls in default build | IMPLEMENTED (feature-gated) |
| SEC-12 | MEDIUM | corrupt event lines silently skipped | IMPLEMENTED |
| — (P2) | — | human approval for conditional tool use | IMPLEMENTED (ASK + `RuleEffect::Ask`) |

## 2. Verification (before changing anything)

- SEC-01: `ToolRegistry::builtins()` used `FilesystemRead::new()` reading any
  path; `workspace_dir` was never consulted; policy's default Allow meant every
  read succeeded.
- SEC-03: sessions/tasks/executions persisted with no side metadata; an edited
  file round-tripped as valid as long as it parsed.
- SEC-04: `DirectExecutor` was unconditionally `pub`.
- SEC-05: `PersistenceSink`/`LoggingSink` wrote `ev.data` verbatim.
- SEC-07: `openrt-models` had `default = ["ollama"]` → reqwest in every library.
- SEC-12: `list_events` used `if let Ok(ev) = ...` — corrupt lines were dropped
  silently.
- ASK: `Decision::Ask` existed but `GateExecutor` hard-coded it as deny; no way
  to attach a resolver.

## 3. Design decisions

### 3.1 SEC-01 — filesystem jail (`openrt-permissions::path_scope`)

- `ScopeRoot::new(root)` canonicalizes and fails closed at construction;
  `check_path(_with_root)` rejects relative/`..`/out-of-tree paths and denies
  `.ssh`/`.gnupg` base names. `resolve_without_traversal` resolves the nearest
  existing ancestor, canonicalizes it, rejoins the tail — symlink escapes are
  caught.
- `FilesystemRead` carries an optional `ScopeRoot`: `new()` denies everything
  (the SEC-01 default), `scoped(root)` confines reads (cap enforcement
  unchanged). Jail bypass → `ToolError::ScopeViolation`, distinct from policy
  denial.
- `ToolRegistry::with_filesystem_root(dir)` swaps the scoped tool in; the
  runtime wires `config.runtime.workspace_dir` → scoped registry, else the
  deny-all default. Policy Allow is now necessary but not sufficient.

### 3.2 SEC-03 — per-record checksum manifest

- `checksums.json` sidecar at store root: relative path → sha256 (new `sha2`
  + `hex` workspace deps), cached in-memory under a lock, loaded at `open`.
- `write_record` hashes the final bytes, records + atomically persists the
  manifest (under a manifest lock). `read_record` verifies recorded entries;
  mismatch → `PersistenceError::Corrupt` (fail closed). Missing entries are
  accepted unverified for pre-P2.2 stores (backward compatible; next write
  re-registers). Events are not hashed — SEC-12 makes them self-describing;
  tamper-evidence is ADR-002's audit chain.

### 3.3 SEC-04 — gate the ungated path

- `openrt-tools`: `[features] default = [] ; demo = []`.
- `DirectExecutor` → `#[cfg(any(test, feature = "demo"))]` + `#[doc(hidden)]`;
  its re-export is gated too. A default release build cannot link it.
- `openrt-agents`/`openrt-runtime` enable the feature in `[dev-dependencies]`.

### 3.4 SEC-05 — redaction at the sink boundary

- `openrt-core::redaction` → `RedactionPolicy` (`redact_value(&mut Value)`),
  case-insensitive field-name matching (api_key, token, secret, password,
  passphrase, authorization, cookie, private_key, ...), placeholder
  `REDACTED = "[REDACTED]"`, extendable via `with_extra_names`.
- Each sink redacts **its own clone**: the live bus event is untouched.
  `PersistenceSink` + `LoggingSink` redact; `ChannelSink` stays raw. Documented
  as a boundary, not a general PII censoring layer.

### 3.5 SEC-07 — network stack is opt-in

- `openrt-models`: `default = []`; `ollama = ["dep:reqwest"]` (trait + mock
  always compile).
- `openrt-cli`: `default = ["ollama"]`, `ollama = ["openrt-runtime/ollama"]` —
  the operator CLI still gets Ollama by default; libraries stay
  dependency-light; `--no-default-features` = dependency-clean binary.

### 3.6 SEC-12 — corrupt events are an error, never a skip

- `list_events` → `Corrupt` on the first unparsable non-empty line
  (whitespace-only skipped). No silent divergence.
- Listings filter strictly: records `.json`, events `.jsonl`, dirs ignored;
  `open()` sweeps stale `*.tmp` atomic-write leftovers. `clear()` removes the
  manifest too.

### 3.7 ASK approval boundary

- `openrt-config`: `RuleEffect::Ask` (serde `"ask"`).
- `openrt-permissions::ask`: `ApprovalRequest{request_id,tool,args,scope,context}`,
  `PermissionDecision{Allow,Deny}`, async `PermissionResolver`, `MockResolver`.
- `GateExecutor::with_resolver(..., Option<Arc<dyn PermissionResolver>>)`
  (`new` = None). On `Decision::Ask`: no resolver → hard deny (fail closed);
  resolver Allow → run + `EventKind::PermissionApproved`; Deny/error → deny.
  `Ask` is never implicitly `Allow`; a resolver is consulted only for `Ask`
  outcomes and can never override a policy `Deny`.
- `RuntimeBuilder::permission_resolver(...)`; `policy_from_config` maps
  `Ask → RuleAction::Ask`.

## 4. Files changed

| File | Change |
|---|---|
| `crates/openrt-permissions/src/path_scope.rs` | NEW SEC-01 jail (9 tests) |
| `crates/openrt-permissions/src/ask.rs` | NEW ASK types + resolver (2 tests) |
| `crates/openrt-permissions/src/lib.rs`, `Cargo.toml` | module/exports; `async-trait`; dev-dep `tokio` |
| `crates/openrt-core/src/redaction.rs` | NEW `RedactionPolicy` + `REDACTED` (5 tests) |
| `crates/openrt-core/src/event.rs` | `EventKind::PermissionApproved` (+ all_kinds list) |
| `crates/openrt-core/src/lib.rs` | `pub mod redaction` + exports |
| `crates/openrt-tools/src/error.rs` | `ToolError::ScopeViolation` |
| `crates/openrt-tools/src/builtin/filesystem_read.rs` | scoped (`new` deny-all, `scoped`) (7 tests) |
| `crates/openrt-tools/src/registry.rs` | `with_filesystem_root` |
| `crates/openrt-tools/src/executor.rs`, `lib.rs`, `Cargo.toml` | SEC-04 `demo` feature gating |
| `crates/openrt-agents`, `openrt-runtime` `Cargo.toml` | dev-deps enable `demo` |
| `crates/openrt-persistence/src/store.rs` | SEC-03 manifest + SEC-12 listing/tmp-sweep (5 new tests) |
| `crates/openrt-persistence/Cargo.toml`, workspace `Cargo.toml` | `sha2`, `hex` |
| `crates/openrt-config/src/lib.rs` | `RuleEffect::Ask` |
| `crates/openrt-runtime/src/gate.rs` | `with_resolver`, `resolve_ask`, Ask→RuleAction (4 new tests) |
| `crates/openrt-runtime/src/runtime.rs` | `permission_resolver`, scoped registry wiring |
| `crates/openrt-runtime/src/sinks.rs` | redacted persisted clone (+1 test) |
| `crates/openrt-telemetry/src/sink.rs` | `LoggingSink` redaction |
| `crates/openrt-models`/`openrt-cli` `Cargo.toml` | SEC-07 feature split |
| `crates/integration-tests/tests/p2_2_security.rs` | NEW e2e: jail default, workspace scoping, ASK deny/approve |
| `crates/integration-tests/tests/gate_deny.rs` | fixed async bus-drain flake (`wait_for`) |
| `docs/architecture/phase2/P2_2_SECURITY_AND_PERMISSIONS.md` | this document |

## 5. Tests added

path_scope (9), ask (2), redaction (5), filesystem_read (7), gate ASK (4),
sinks redaction (1), store SEC-03/12 (5), p2_2_security e2e (4). Headline
proofs: edited record fails closed on read; corrupt event line is an error;
stale tmp swept; unscoped default runtime denies `filesystem.read`; scoped
runtime reads inside/denies outside; Ask without resolver fails closed; Ask
with resolver approves and records `PermissionApproved`; resolver cannot
override a policy Deny; stored event redacted while live event untouched.

**Quality gate (2026-09-17), all green:**

- `cargo fmt --all -- --check` — PASS
- `cargo check --workspace` — PASS
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — PASS
- `cargo test --workspace` — PASS, 143 tests / 0 failures (108 + 35 new;
  p2_2/gate_deny also stable across 5 repeated runs)
- `cargo build --workspace` — PASS

## 6. Remaining security risks (documented, not silently accepted)

- **Checksums are tamper *detection*, not tamper *evidence*:** an attacker who
  can rewrite a record and `checksums.json` is undetected. The ADR-002 audit
  chain (a later P milestone) owns evidence; SEC-03 keeps the manifest
  mechanism and backward compatibility ready for it.
- **Redaction is name-based**, case-insensitive; a value under a non-sensitive
  key (e.g. an inline bearer value without a key) is not caught, and nested
  arrays of plain objects with sensitive inner keys rely on recursion. This is
  a boundary, not a general DLP layer.
- **`MockResolver` is deny-by-default** but allows `"*"` via `allow_all` — call
  sites must choose explicitly; the gate treats a resolver `Err` as a hard
  deny, never a pass.
- **Non-unix platforms** still lack store mode constructs (unchanged from
  P2.1); the filesystem jail (path checks) is platform-independent.
- **`gate_deny.rs`'s comment** («filesystem.read allowed») now describes the
  *policy* level only — the tool jail (SEC-01) confines the actual read when
  no workspace is configured; no test relied on the old behavior.