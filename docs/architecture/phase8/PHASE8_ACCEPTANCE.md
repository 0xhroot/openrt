# PHASE 8 ACCEPTANCE — Sandbox + Isolated Execution Fabric

## Final status

**PHASE 8 COMPLETE** — one sandbox authority (`openrt-sandbox`), one
admission flow between policy and execution, real bwrap enforcement with
honest capability reporting, verified disposable cleanup, workflow/gate
enforcement with production callers, classified escape coverage, and the
final gate green. Design and limitations: `PHASE8_IMPLEMENTATION.md`.

## Gates (2026-09-25, this host)

| Gate | Command | Result |
|---|---|---|
| Format | `cargo fmt --all` | RC=0 |
| Types/build | `cargo check --workspace --offline` | RC=0 |
| Full test suite | `cargo test --workspace --offline` | **RC=0 — 867 passed / 0 failed, 143 suites** (log `/tmp/opencode/p8_full2.log`) |
| Phase 7 regression (frozen) | included above — all `p7*` suites green (baseline 825/142 remains green) | PASS |
| CLI smoke (live) | `openrt sandbox capabilities/policy/check/run/list/inspect/destroy`, `openrt doctor` | all correct, RC=0 |

Phase 8 net-new tests: **+42** (825 → 867). Key suites:
`openrt-sandbox` lib **41**, `openrt-orch` **16**, `openrt-runtime` **35**,
integration `p8_sandbox` **7**.

## Milestone evidence

| M | Verdict | Evidence |
|---|---|---|
| M8.1 Unified core | GREEN | `model/policy/backend/executor/manager` in `openrt-sandbox`; `run_success_disposable_verified` (identity, policy digest, lifecycle, `sandbox.admit/run` audit) |
| M8.2 Resource enforcement | GREEN | `resource_gate_denies_admission` (RESOURCE_DENIED + audit), `output_flood_becomes_controlled_kill`, rlimit bootstrap (`process_limit_enforced_via_nproc`) |
| M8.3 Filesystem isolation | GREEN | `filesystem_escape_denied_through_manager`, `rw_mount_in_policy_prefix_executes`, `escape_write_outside_host_is_denied` (host marker absent) |
| M8.4 Network isolation | GREEN | `network_none_blocks_tcp_and_dns`, `local_only_blocks_host_keeps_loopback`, `allowlist_blocks_others_allows_listed`, `sovereign_blocks_networked_modes_provisioning_allows`, `network_none_blocks_loopback_from_sandbox` |
| M8.5 Container detection | GREEN | capability matrix: docker `Unavailable` (daemon down), nspawn `PermissionRequired`, bwrap `Available`; `CONTAINER_UNAVAILABLE` / structured unavailability tests — no fake container, no silent downgrade (`BACKEND_UNAVAILABLE` fail-closed without opt-in) |
| M8.6 Disposable lifecycle | GREEN | `keep_cleanup_and_destroy`, `destroy_unknown_instance_is_structured_failure` (VERIFICATION_FAILED), `cleanup_failure_is_structured_and_recoverable` (CLEANUP_FAILED + `sandbox.destroy:fail/ok` audit + retry), PID-identity `kill_and_verify` |
| M8.7 Syscall hardening | GREEN | honest `syscall.filter Unavailable` matrix row; `SYSCALL_DENIED` admission denial (gate test) — no claim without enforcement |
| M8.8 Escape testing | GREEN (classified) | SAFE_UNIT (sandbox lib) + SAFE_HOST (`p8_sandbox` + manager probes) executed; DESTRUCTIVE categorized and deliberately not run on the bare host (documented) |
| M8.9 Autonomous tool sandbox | GREEN | `sandbox_requirement_denies_before_execution` (gate, body never runs), `precheck_tool_*` deny/allow, workflow `SandboxDenied` fail-closed (orch + integration, serde default for old workflows), `python_run_is_managed_and_carries_sandbox_identity` + `sandbox.*` audit query, `ambient_env_is_not_inherited` |
| M8.10 Final gate | GREEN | this file + `PHASE8_IMPLEMENTATION.md`, fmt/check/test green, doctor `sandbox.fabric` row, CLI observability, production-caller audit below |

## Definition of Done (ROADMAP § PHASE 8.5)

- **One authority, one flow** — `SandboxManager` + gate step between policy
  and execution; no registered tool can reach execution while bypassing its
  declared requirement (proven: counting tool never executes on denial).
- **Enforced, not requested** — netns/rlimits/kill/workspace removal are
  kernel/OS mechanisms with integration tests for each restriction
  (filesystem, network, timeout, output flood, nproc, env).
- **Honest backends** — `Available`/`Partial`/`Unavailable`/
  `PermissionRequired` with reasons; unsupported isolation = structured
  error (`SYSCALL_DENIED`, `CONTAINER_UNAVAILABLE`, `BACKEND_UNAVAILABLE`).
- **Verified cleanup** — destroy verifies process tree (identity-checked)
  and workspace removal; failures are `CLEANUP_FAILED`, audited, retryable.
- **Autonomous precondition** — `python.run` (the autonomous code-execution
  tool) requires an admitted Strict sandbox at gate time; workflow steps
  declare profiles validated before execution.
- **No second system** — policy, `ResourceGate`, `openrt-audit`, mode/netpol,
  kill switch, registry all reused; Phase 7 tests stay green.

## CLI / observability evidence (live host)

- `openrt sandbox capabilities` — honest matrix (bwrap Available, docker
  Unavailable with reason, syscall.filter Unavailable with reason, …).
- `openrt sandbox policy` — 5 profiles with digests, networks, fallback.
- `openrt sandbox run --profile strict --code "…"` —
  `sandbox=<uuid> state=succeeded backend=bubblewrap network=none cleanup_verified=true`.
- `openrt sandbox list|inspect|destroy` — persistent `sandbox.instances`
  registry; destroy verified; unknown id → structured `VERIFICATION_FAILED`.
- `openrt doctor` — `PASS sandbox.fabric … python.run -> strict`.

## Honest limitations (recorded, not claimed GREEN)

1. seccomp/Landlock: unavailable offline under `unsafe_code = "forbid"` →
   `SYSCALL_DENIED` at admission when required; never a claim.
2. Allowlist networking is interpreter-level (python audit hooks,
   `Partial`), not kernel netfilter; non-python allowlist payloads rejected.
3. Docker daemon down / nspawn rootless on this host → structured
   unavailability; only bubblewrap runs production.
4. LocalOnly loopback is best-effort (`Partial`); host/DNS stay unreachable.
5. Managed execution covers `python.run` only today; terminal/filesystem
   tools keep Phase 2/7 PathJail controls (requirement mechanism is generic
   and gate-tested, but unregistered for tools that do not actually execute
   in the manager — no false claims).
6. DESTRUCTIVE escape cases are classified, never executed on the bare host.
7. Disk quotas / GUI sandboxing / WebUI-TUI surfaces: not claimed
   (ROADMAP §8 and component notes).
