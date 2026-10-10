# PHASE 8 IMPLEMENTATION — Sandbox + Isolated Execution Fabric

Scope and reconciliation are defined in `docs/roadmap/ROADMAP.md` § PHASE 8
(v0.5); ownership in `docs/architecture/OWNERSHIP.md` (Phase 8 table +
`sandbox.instances`). This document records what was built, where, and at what
honesty level — the acceptance verdict is in `PHASE8_ACCEPTANCE.md`.

## 1. Architecture

One sandbox authority (`openrt-sandbox`) and one admission flow. Execution
order inside `GateExecutor::execute` (crates/openrt-runtime/src/gate.rs):

1. **Permission policy** (deny-by-default, ADR-001) + capability requirements.
2. **Host resource admission** (`ResourceGate` — memory/disk/cpu pressure).
3. **Sandbox admission (Phase 8)** — only for tools with a registered
   requirement: profile → policy → backend availability → host resources →
   mode/egress gate. Denial emits `EventKind::PermissionDenied`, an
   `sandbox.admit` audit record, and returns `ToolError::PermissionDenied`
   with a rendered WHAT/WHY/WHERE/FIX failure — the tool body never runs.
4. **Safety gate** (ADR-008 scoped HALT).
5. **Execution** through the registered tool (for `python.run`: the manager
   path — admit → execute in bwrap → collect → verified cleanup).
6. **Audit**: every `sandbox.admit/run/destroy` decision lands in the one
   chained `AuditLog` shared with the gate (fail-closed for admissions).

Runtime wiring (crates/openrt-runtime/src/runtime.rs): a single
`Arc<SandboxManager>` is built with store, the same `AuditLog` instance the
gate uses, `ResourceGate`, `NetworkPolicy`, and the kill switch;
`PythonRun` is attached (`with_manager`) and `require("python.run", Strict)`
registers the gate-side requirement. The `Runtime` struct exposes
`pub sandbox` for CLI/tests.

Workflow admission (crates/openrt-orch): `WorkflowStep.sandbox` (serde
default, builder `.sandboxed(profile)`) declares a profile;
`WorkflowEngine::with_sandbox_admission(Arc<dyn StepSandbox>)` validates
every declared profile through `SandboxManager::check_profile` **before the
first step** and fails the run closed with
`WorkflowError::SandboxDenied { step, reason }`. The runtime attaches the
admission (crates/openrt-runtime/src/checkpoint.rs); `openrt-orch` only holds
the trait — no second policy model.

## 2. Crates / components

| Component | Location | Role |
|---|---|---|
| `model.rs` | openrt-sandbox | `SandboxId`, lifecycle `SandboxState`, `SandboxRequest/Result`, `SandboxErrorCode` + `SandboxFailure` (render), records, capability rows |
| `policy.rs` | openrt-sandbox | `SandboxProfile` (read_only/development/networked/strict/untrusted), `SandboxPolicy` + digest, validation (lexical `..` normalization) |
| `backend.rs` | openrt-sandbox | Honest detection matrix (bwrap/docker/nspawn/syscall), `resolve_backend` (no silent downgrade) |
| `executor.rs` | openrt-sandbox | bwrap argv build, bootstrap python (rlimits, `lo` up, allowlist audit hooks, clean env), capped output + overflow notify, identity-checked `kill_and_verify` |
| `manager.rs` | openrt-sandbox | `SandboxManager`: admission, requirements registry, `precheck_tool`, `run`, `cleanup_run`, `destroy`, `check_profile`, capability rows; instance records under `sandbox.instances` |
| Gate step | openrt-runtime/src/gate.rs | `with_sandbox` + deny-before-execution step (between resource and safety gates) |
| Manager path | openrt-sandbox/src/sandbox.rs | `PythonRun::run_managed` — production `python.run` execution + output mapping |
| Workflow | openrt-orch/src/{workflow,engine}.rs | step field, `StepSandbox` trait, pre-run validation, `SandboxDenied` |
| CLI | openrt-cli/src/main.rs | `openrt sandbox {capabilities,policy,list,inspect,check,run,destroy}` |
| Doctor | openrt-runtime/src/doctor.rs | `sandbox.fabric` check (capability count + strict reachability) |

Lifecycle: `CREATED → ADMITTED → RUNNING → {SUCCEEDED|FAILED|TIMEOUT|KILLED}
→ DESTROYED`, plus denial states (`ADMISSION_DENIED`/`POLICY_DENIED`/
`RESOURCE_DENIED`) persisted per instance with pid + `/proc` starttime for
identity-checked destroy.

## 3. Backend capability matrix (measured on this host, 2026-09-25)

```
backend.bubblewrap        Available      (real namespace backend; runs production)
backend.subprocess        Partial        (clean env + timeout + caps, NO OS confinement; explicit opt-in only)
backend.docker            Unavailable    (daemon not running)  → CONTAINER_UNAVAILABLE
backend.systemd_nspawn    PermissionRequired (root/userns needed) → honest status
network.none              Available      (kernel netns: --unshare-net)
network.local_only        Partial        (netns + loopback up best-effort; host/DNS unreachable)
network.allowlist         Partial        (python audit hooks — interpreter level, not netfilter)
network.full              Available      (shares host netns; gated by mode/netpol admission)
syscall.filter            Unavailable    (no seccomp/Landlock binding offline; unsafe_code=forbid) → SYSCALL_DENIED
```

Container isolation requests without a working engine are structured
`CONTAINER_UNAVAILABLE` errors; `require_syscall_filter` is an admission
denial (`SYSCALL_DENIED`), never a claim.

## 4. Profiles and enforcement

| Profile | Network | Min isolation | Notes |
|---|---|---|---|
| `read_only` | none | namespace | no rw mounts |
| `development` | none, local_only | namespace | admits rw mounts in policy prefixes |
| `networked` | all four | — | egress still gated by Sovereign/netpol |
| `strict` | none | namespace | production default for `python.run` |
| `untrusted` | none | namespace | tightest limits |

Enforcement is by mechanism, not by request: netns (`--unshare-net`),
bwrap bind mounts + tmpfs over the policy, `RLIMIT_AS/CPU/NPROC/TIME` +
output byte caps with kill, wall-clock timeout → process-group kill with
PID-identity verification, clearenv + explicit env allowlist (no ambient
inheritance), `/sbx` workspace, verified cleanup (`CLEANUP_FAILED` on
failure). Requested network modes are additionally gated by
mode/netpol admission: Sovereign blocks `allowlist`/`full` before any
execution (test `sovereign_blocks_networked_modes_provisioning_allows`).

## 5. Production callers and honesty mapping

- `python.run` (runtime-wired): runs `run_managed`; output carries
  `sandbox_id/state/profile/isolation/network/policy_digest/cleanup_verified`.
  Outcomes: success JSON; `Timeout` → `ToolError::Internal` (sandbox error);
  `Killed` ≠ `output_limit` → `ToolError::KillSwitchEngaged` (ADR-008 halt);
  admission denials → `ToolError::PermissionDenied` with rendered failure.
- Gate: `require("python.run", Strict)` — denial proven by
  `sandbox_requirement_denies_before_execution` (tool body never executes).
- Workflow: `WorkflowStep.sandboxed("…")` validated before step 1.
- CLI: operator-facing introspection + `run` through the same
  admission/limits/cleanup flow (actor `cli`), `destroy` verifies cleanup.
- Doctor: `sandbox.fabric` row (honest WARN when strict unreachable).

## 6. Escape-test classification (M8.8)

- **SAFE_UNIT** — sandbox-crate self-tests (no host effect by construction).
- **SAFE_HOST** — probes executed *inside* the real backend asserting the
  host is untouched: fs escape (`filesystem_escape_denied_through_manager`),
  network none TCP+DNS, loopback/LocalOnly, env isolation, nproc,
  timeout/output-flood kill, cleanup verification — plus the runtime-level
  `p8_sandbox` integration suite (identity output, workflow admission,
  host marker file absent, loopback egress blocked, wall timeout).
- **DESTRUCTIVE** — bare-host tampering (killing PID 1, corrupting
  `/proc`, disabling host networking). **Classified and intentionally NOT
  executed** on this host; documented here so the category exists without
  ever running destructive payloads outside a disposable VM/CI context.

## 7. Honest limitations (deferred, not claimed GREEN)

- seccomp/Landlock enforcement: `SYSCALL_DENIED` honest (offline +
  `unsafe_code = "forbid"`); no claim of syscall filtering.
- Allowlist networking is interpreter-level (python audit hooks), reported
  `Partial`; non-python allowlist payloads are rejected at admission.
- Docker/nspawn unusable on this host → structured unavailability, no fake
  container.
- Disk quotas: not enforceable unprivileged → not claimed (host
  `ResourceGate` disk check still applies at admission).
- Managed execution currently covers `python.run` only; `terminal.run` /
  filesystem tools keep their Phase 2/7 PathJail + kill-switch host
  controls (roadmap: phases not rebuilt). The requirement mechanism is
  generic and gate-tested for future extension — but registering a
  requirement for a tool that does not actually execute inside the manager
  would be a false claim, so it is not done.
- WebUI/TUI sandbox status surfaces: not added; the CLI/`Runtime::sandbox`
  is the authoritative observability (no feature exists only in a UI).
- GUI/desktop sandboxing: out of scope per ROADMAP §8 (future work).
