# Phase 1 Audit — Sovereignty & Permission Model

**Status:** PASS WITH NOTES (mode enforcement is Phase 3 by design).
**Date:** 2026-09-17
**Method:** verification against the roadmap Cross-Cutting Commitments
(local-first axiom, self-hosted verification, one-storage backbone, two
operating modes), ADR-003/005/006, OWNERSHIP.md, and the Phase 1 source.

---

## 1. Local-first axiom ("default behavior requires zero network")

| Check | Evidence | Result |
|---|---|---|
| Default provider requires no network | `Config::default()` provider is `ollama` at `http://127.0.0.1:11434` (loopback) — but Mock is the CLI/test default only under `--mock`/`OPENRT_MOCK=1`. A default `openrt task run` (no flags) would attempt a loopback Ollama call and fail on `ProviderUnavailable`. **Default behavior otherwise is fully local** | PASS-with-note |
| No network sinks | Sinks are Logging/Persistence/Channel only (`openrt-telemetry`); no egress adapter exists | PASS |
| No test depends on the network | All 99 tests use Mock (or pure parsers); Ollama parsing tests are response-string based; live Ollama requires `OPENRT_TEST_OLLAMA`/`#[ignore]` per TESTING.md | PASS |
| Network-capable library not in default build | **FAIL** — `openrt-models` default feature `ollama` compiles `reqwest`/hyper/rustls into every binary (SEC-07 / DEP-1). No runtime egress path today, but the dependency is present by default | Note |
| Ollama egress is operator-configurable only | `base_url` validated `http|https`, default loopback; nothing in `--mock` demo mode touches it | PASS |

## 2. Sovereign Execution Mode / Provisioning Mode (ADR-003)

- `Config.mode: OperatingMode {Provisioning, Sovereign}` exists, defaults to
  **Sovereign**, is serialized into config + echoed into `RuntimeStarted`
  events, and is validated.
- **Enforcement is absent by design** (ADR-003 assigns the network guard and
  mode state machine to Phase 3; `openrt-modes` is a P3 crate). Documented
  INTERFACE ONLY in the Phase 1 report. A `base_url` pointing at a remote host
  would be dialed if the provider were selected — there is no loopback/egress
  check in Phase 1.
- Recommendation: mark Sovereign enforcement as an explicit Phase 3 admission
  criterion and do not ship "Sovereign enforcement" claims before then.

## 3. Permission model (deny-by-default)

Verified behavior (`policy.rs` + `gate.rs` + config defaults):
- `Policy::deny_by_default()` — an empty rule set denies everything.
- One matching rule wins (first match in policy order), supporting `*` globs on
  the *effective identifier* (declared capability else **tool id** — Phase 1
  keying, documented) and an optional `scope` glob over extracted args
  (`path|url|address|file`).
- `Decision::Allow → PermissionGranted`; `Deny → PermissionDenied`; `Ask →
  PermissionDenied` (fail-closed; no resolver yet).
- Default config explicitly allows exactly `echo`, `system.info`,
  `filesystem.read` by tool id. `filesystem.read`'s lack of path scope is
  SEC-01.
- `required_caps` (3-part `domain.resource.action`) are annotations for
  introspection/`tool list`; they do not (yet) drive evaluation. This matches
  the documented Phase 1 reconciliation.

Sovereignty assessment of the permission model: it is a *static* policy with no
session/task/channel awareness (SEC-09) and no approval path (SEC-13) — both
are Phase 2 `permission.tools` extensions, and the enum/`Policy` structure
already accommodates them additively.

## 4. Operator ownership / open-source posture

- All runtime assets stay under the operator's `~/.openrt` (or configured
  directory): sessions, tasks, executions, event log, config. No third-party
  cloud inference, SaaS control plane, or federation exists in Phase 1.
- Self-hosted verification: nothing in Phase 1 verifies artifacts (model
  verification is Phase 3+ per roadmap). The Phase 1 signing/verification
  surface is absent because it is out of scope — the audit confirms there is
  **no** external service dependency to leak trust through.
- Telemetry cannot egress (ADR-005 §3 posture satisfied by sink set).
- Fixed, offline test suite runs with zero network (all 99 tests).

## 5. Data-at-rest / filesystem posture (sovereignty angle)

- Everything under the store root is full content (prompts, results, event
  payloads) — sovereignty-local, but protected only by filesystem permissions
  (see SEC-02 creation-mode gap, SEC-05 plaintext logging, SEC-03 no
  checksums). Satisfies "no egress" but not "hardened at rest" (that is P6
  `openrt-secrets`; Phase 1 does not claim it).

## 6. Verdict

**PASS WITH NOTES.** Phase 1 honors the local-first and one-storage axioms by
construction (loopback defaults, no egress sinks, all-local store, offline
tests) and implements a genuinely deny-by-default permission gate. The notes to
carry into Phase 2:
1. Flip `openrt-models` to `default = []` so the network client stack is
   strictly opt-in (DEP-1/SEC-07).
2. Enforce `filesystem.read` scope to a workspace root before letting any real
   model run against default config (SEC-01).
3. Record that Sovereign-mode *enforcement* (ADR-003) is deliberately Phase 3;
   do not claim it earlier.