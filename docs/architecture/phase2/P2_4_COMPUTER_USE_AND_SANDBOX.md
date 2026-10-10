# Phase 2.4 — Computer-Use Tools (filesystem + terminal) and Python Sandbox

**Status:** Implemented
**Date:** 2026-09-17
**Scope:** ROADMAP M2.3 (filesystem `write`/`list`/`move`/`delete` +
`terminal.run` behind tool permissions) and M2.4 (Python sandbox with
filesystem/network/CPU/memory/timeout limits). Adds `openrt-computer` and
`openrt-sandbox`, wires both into `openrt-runtime`, extends `openrt-config`.
Baseline behavior unchanged (155 → 183 tests, all still green; the P2.1
browser/input belt and the P2.5 computer-use loop are NOT in this milestone).

## 1. Verification (before changing anything)

- Phase 1 registered only `echo`, `system.info` and the one jail-scoped
  `filesystem.read`. No mutating filesystem tools, no terminal, no interpreter
  sandbox existed.
- `bubblewrap` (`bwrap`) is present on the dev host and unprivileged user
  namespaces work (`unshare --user` succeeds), so real rootless kernel
  isolation was achievable without `unsafe`.
- Workspace-wide `unsafe_code = "forbid"` — confinement had to be composed
  from child processes + namespaces + the interpreter's own `-I` mode.

## 2. Design decisions

### 2.1 openrt-computer — everything stays inside the SEC-01 jail

All four filesystem tools (`write`, `list`, `move`, `delete`) and the terminal
resolve every path through `openrt_permissions::check_path`, the same pure
enforcement point as `filesystem.read`: traversal (`..`), symlink escape and
outside-root absolute paths are rejected in the tool layer, not only by
policy. Extra guards:

- `write` refuses to follow a symlink at the final component, caps content
  size and re-checks the canonicalized parent after `create_dir_all`
  (TOCTOU backstop).
- `list` returns sorted entries with a 4096-entry cap.
- `delete` removes files or empty directories only; `move` refuses to clobber
  a symlink destination.
- With no `workspace_dir` configured every tool fails closed (deny-all), same
  as `filesystem.read`.

`terminal.run` runs `/bin/sh -c` with `env_clear()` (no host secrets leak),
working-directory scoping (the cwd must resolve inside the root), a wall-clock
timeout, a per-stream output cap, and process-group management
(`process_group(0)` + `kill_on_drop(true)`; `start_kill()` on timeout targets
the whole group so background children die with the command). Documented
residual: a shell may `cd` out of its start directory — full containment of
arbitrary shell code belongs to the sandbox (2.2) and the Phase 6 production
sandbox.

### 2.2 openrt-sandbox — rootless kernel confinement for untrusted Python

`PythonSandbox` runs `python3 -I script.py` (isolated mode, no site/user
imports) inside **bubblewrap** when available:

- namespaces: `--unshare-net --unshare-pid --unshare-ipc --new-session
  --die-with-parent` — the script has no route to the host network and cannot
  leave runaway process trees behind;
- filesystem: system paths `/usr /usr/local /lib /lib64 /bin /etc` are
  read-only binds, `/proc` + a fresh `/dev` are mounted, and only a scratch
  `/sbx` workspace is writable; host paths (e.g. `$HOME`) are not mounted at
  all;
- resource limits: this bwrap build has no `--rlimit-*` flags, so a tiny
  Python prelude (`bootstrap.py`) sets `RLIMIT_AS` (memory), `RLIMIT_CPU`,
  `RLIMIT_FSIZE`, `RLIMIT_NPROC`, `RLIMIT_NOFILE` and `execv`s the real
  script (rlimits survive exec); a parent-side wall-clock timeout SIGKILLs the
  group, and captured output is capped per stream;
- bwrap init errors are surfaced as `SandboxError::Backend` (a "bwrap:" exit
  is never confused with a script that simply failed).

If `bwrap` is absent the sandbox degrades to a strict child process (clean
env, `-I`, timeout, output cap) *without* OS filesystem/network/rlimit
confinement — documented honestly, and the operator-facing guidance is to gate
`python.run` when running on a host without `bwrap`. Phase 6's production
sandbox (landlock/selinux-style) supersedes both.

### 2.3 Tool-permission integration (deny-by-default)

Policy has no default allow rule for the new tools: operators must add allow
rules (`filesystem.write`, `terminal.run`, `python.run`, …) exactly as they do
for `filesystem.read`. The gate (`PolicyEvaluator`) matches the tool id, so
the config-side capability strings are the plain tool ids. Registration is
unconditional — an unallowed tool is visible but every invocation is
`PermissionDenied`, and an allowed one is still scope-checked in the tool
layer.

### 2.4 Config

`RuntimeConfig` gained `terminal_timeout_secs`, `terminal_max_output_bytes`
and an optional `python_sandbox` (`PythonSandboxConfig`: interpreter path,
wall timeout, CPU budget, memory, max procs, output cap). All default to
sensible sandbox limits when unset.

## 3. Files changed

| Path | Change |
|---|---|
| `crates/openrt-computer/*` (new) | `filesystem.rs` (write/list/move/delete), `terminal.rs` |
| `crates/openrt-sandbox/*` (new) | `limits.rs`, `sandbox.rs` (sandbox + `python.run` tool) |
| `crates/openrt-runtime/src/runtime.rs` | registry wiring: computer + python tools, terminal/sandbox limits from config |
| `crates/openrt-config/src/lib.rs` | `PythonSandboxConfig`, terminal timeout/output knobs |
| workspace + crate `Cargo.toml` | members/deps, tokio `process` feature |
| `crates/integration-tests/tests/p2_4_computer_use.rs` (new) | end-to-end computer-use tests |
| `crates/openrt-runtime/src/doctor.rs` | clippy fix (`let_unit_value`) |

## 4. Tests added

- openrt-computer (15): write/list/move/delete round trips; unscoped/outside/
  traversal/symlink denials; terminal output capture, out-of-scope cwd, group
  timeout kill, cleared environment.
- openrt-sandbox (8; skip unless `bwrap`): runs + prints; filesystem escape
  denied (/etc write and /home read fail, cwd write ok); network denied;
  memory-cap kill; wall-timeout kill; CPU-budget enforcement; output truncation;
  fork-bomb containment (nproc/pid-ns stops forking, script exits non-zero).
- integration (5): tools registered; jail-scoped fs mutations end-to-end with
  out-of-root/traversal escapes still denied; terminal run + timeout + cwd
  scope; python sandbox runs + contains escape; dangerous tools
  deny-by-default without an allow rule.

## 5. Quality gate (all green)

`cargo fmt --all -- --check` clean · `cargo check --workspace` clean ·
`cargo clippy --workspace --all-targets --all-features -- -D warnings` clean ·
`cargo test --workspace` 183 passed / 0 failed (stable ×3) ·
`cargo build --workspace` ok.

## 6. Documented gaps (not silently accepted)

- `terminal` is *working-dir scoped*, not *contained*: shell code can `cd`
  away. True terminal containment is deferred to Phase 6 (and the python
  sandbox covers the same thing for code, not shell).
- The subprocess fallback backend (no `bwrap`) is confinement-degraded; no
  silent claim is made for it.
- RLIMIT_NPROC is uid-wide; on multi-user hosts it constrains other processes
  of the same uid while the sandbox runs. Fine for a foundation step.
- No resource-gate interplay yet (the disk/memory gate runs before tools; the
  sandbox's own limits are independent).
- P2.5 adds screen/mouse/keyboard/browser and the computer-use kill switch
  (ADR-008).