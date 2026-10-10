# Phase 1 Audit — Dependency Graph & Workspace Posture

**Status:** PASS (with one hardening item during Phase 2)
**Date:** 2026-09-17
**Audit scope:** `Cargo.toml` (workspace + all 12 members), `Cargo.lock`, feature
graph, lint posture, imported panic/unsafe surface.
**Auditor method:** read every manifest + `cargo tree`/`cargo check`/`cargo
clippy --all-features`/`cargo test`/`cargo build` at audit time (all green).

---

## 1. Direction rule (per `PHASE1_IMPLEMENTATION.md`)

Documented invariant: *"Dependency direction is strict: `core ← config ←
telemetry/persistence ← models/tools/permissions/resources ← agents ← runtime
← cli`. Nothing below `runtime` depends on `openrt-runtime`."*

Verified edges (from manifests):

```
                       +---------------------+
                       |    integration-tests |   dev-deps only
                       +----------+----------+
                                  | (dev)
                        +---------v---------+
                        |     openrt-cli    |   core, config, models, tools,
                        +---------+---------+   telemetry, persistence, runtime
                                  |
                        +---------v---------+
                        |   openrt-runtime  |   core, config, telemetry,
                        +---------+---------+   persistence, models, tools,
                                  |             permissions, resources, agents
      +-----------+  +------------+---------+---------+-----------+---------+
      |   agents  |  | tools               ...          |  models  |  ...
      |  core     |  | resources permissions           |  core    |
      +-----------+  +---------------------------------+-----------+
```

Concrete edges:
- `openrt-core` → (external only: serde, serde_json, thiserror, uuid, chrono).
  Zero async, zero I/O, zero policy — verified.
- `openrt-config` → (external only: serde, serde_json, thiserror, url).
  **Does not depend on `openrt-core`** — even lighter than the documented
  "core ← config" arrow allows. No issue.
- `openrt-telemetry` → `openrt-core`.
- `openrt-persistence` → `openrt-core`.
- `openrt-models` → `openrt-core` + `reqwest` (optional, feature `ollama`).
- `openrt-tools` → `openrt-core`, `openrt-resources`, `openrt-permissions`.
- `openrt-permissions` → `openrt-core`.
- `openrt-resources` → `openrt-core` + `libc`.
- `openrt-agents` → `openrt-core`, `openrt-models`, `openrt-tools`.
- `openrt-runtime` → the 8 owned crates above + serde/tokio/tracing/uuid.
- `openrt-cli` → core, config, models, tools, telemetry, persistence, runtime.
- `integration-tests` → dev-deps on the owned crates (not `openrt-cli`).

**Verdict:** no cycles; `openrt-runtime` is the single assembly point; anything
below it is free of runtime awareness; `openrt-cli` contains no business logic
(it only shells out to runtime/store — see AUDIT_EXECUTION_FLOW.md). One
acyclic trick is intentional and correct: `PersistenceSink` lives in
`openrt-runtime/src/sinks.rs`, not `openrt-telemetry`, so telemetry never
deps persistence while still satisfying "persistence sink first".

## 2. Feature graph

```
openrt-models : default = ["ollama"] ; ollama = ["dep:reqwest"]
openrt-runtime: default = []         ; ollama = ["openrt-models/ollama"]
openrt-cli    : (no feature overrides — inherits both)
```

- Ollama code paths are `#[cfg(feature = "ollama")]` in
  `openrt-runtime::gate`-adjacent plumbing and `doctor.rs`; `Config` and
  `build_default_provider` degrade to `Error::OllamaFeatureDisabled` without
  the feature. Structure is correct.
- **Finding DEP-1 (MEDIUM):** `openrt-models` ships `ollama` in its *default*
  feature set, so `reqwest` + `hyper` + `rustls` are compiled into every
  default binary (confirmed in `Cargo.lock`). The runtime never connects
  unless configured + provider selected, so this is **not** a live egress
  path in the default posture — but a network-capable client stack is part of
  the default build, which is at odds with the roadmap's local-first axiom
  ("default behavior requires zero network"; network features should be
  separately feature-gated). Recommendation (Phase 2, before P3 Provisioning
  Mode): `default = []` on `openrt-models` and route the CLI's Ollama use
  through an explicit feature, preserving the "never a hard dependency"
  contract in the *dependency* sense, not just the *code* sense.

## 3. Lint / safety posture

- `[workspace.lints.rust] unsafe_code = "forbid"` — verified present.
- The single exception is `openrt-resources` (`[lints.rust] unsafe_code =
  "allow"`), containing exactly two `unsafe` expressions, both in
  `system.rs:98-101` inside one documented `statvfs` wrapper with safety
  comments and EINTR/`last_os_error` handling. Everything else in the
  workspace is `unsafe`-free (grep confirms).
- Panic surface outside `#[cfg(test)]`: exactly one `unreachable!()` in
  `openrt-models/src/mock.rs:130` (Hang mode — provably unreachable because
  the preceding `pending::<()>().await` never resolves); one `panic!` in an
  integration test. No `todo!`/`unimplemented!`/`dbg!` in non-test code.
- Quality gate re-run at audit time: fmt ✓, check ✓, clippy `-D warnings`
  (all targets, all features) ✓, build ✓; `cargo test --workspace` = **99/99
  passing** (core 19, config 8, telemetry 5, persistence 4, models 11,
  permissions 11, resources 11, tools 11, agents 5, runtime 9, integration 5,
  plus the config/models doc-less targets that report 0).

## 4. Non-goal validation

- No `dev-dependencies` leak into runtime builds; external deps are versioned
  centrally in `[workspace.dependencies]` (no duplicate versions).
- MSRV 1.81 / edition 2021 / resolver "2" consistent across the workspace.
- `integration-tests` lib target is an empty marker (by design).

## 5. Dependency-graph verdict

**PASS.** The graph matches the documented ownership map (OWNERSHIP.md),
imposes the intended direction discipline, and centralizes versions. The one
caveat is DEP-1 (default Ollama feature pulling `reqwest`), tracked as a
SHOULD-FIX-DURING-PHASE-2 item — it does not block Phase 2 interface work.