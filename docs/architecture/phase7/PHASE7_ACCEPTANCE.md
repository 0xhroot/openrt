# PHASE 7 ACCEPTANCE — Tool Fabric / Computer Control

**Phase 7 (ROADMAP v0.3, M7.1–M7.10) is COMPLETE.**
Gate run: 2026-09-24, offline workspace, host = Omarchy / Hyprland 0.56.2 /
Wayland.

## Gates

| Gate | Command | Result |
|---|---|---|
| Format | `cargo fmt --all -- --check` | **RC=0** |
| Check | `cargo check --workspace --offline` | **RC=0**, no warnings |
| Tests | `cargo test --workspace --offline` | **RC=0 → 734 passed / 0 failed** (up from 708 at Phase 6B) |

Per-suite (regressions + new):

| Suite | Count | Result |
|---|---|---|
| `p7_tool_fabric` (new) | 8/0 | discovery→profile→backend→**real Hyprland ops**→verify; deny-by-default; kill scope; fixture scanners; metachar refusal |
| `openrt-platform` unit (new) | 18/0 | profile/fixtures, 5 config parsers, resolver priority, sim contract, capability statuses |
| `openrt-safety` | 6/0 | includes extended `computer.*` → `KillScope::Computer` |
| `p6_milestones` | 17/0 | 6A regression unchanged |
| `p6_workflow_compiler` | 11/0 | 6A regression unchanged |
| `p6b_production` | 10/0 | 6B regression unchanged |

## Acceptance criteria (10/10 GREEN on this host)

| # | Criterion | Evidence |
|---|---|---|
| 1 | Production caller | Six `computer.*` tools registered in `runtime.rs` through `ToolRegistry` + `GateExecutor`; exercised in `p7_tool_fabric::computer_window_tool_through_production_runtime` and `..._env_reports_...` |
| 2 | Environment discovery | `EnvironmentProfile::host()` + `DiscoveryInputs` fixtures; `computer.env` returns `profile`, `summary`, `resolver` |
| 3 | Capability discovery | `CapabilityMatrix` rows with `SUPPORTED/PARTIAL/PERMISSION_REQUIRED/UNAVAILABLE/UNSUPPORTED` + non-empty reason for every non-SUPPORTED row (asserted) |
| 4 | Real backend selection | `BackendResolver` layer 1 selects `hyprland` when `hyprctl` present (unit + live test); decision log recorded |
| 5 | Real operation | Live test: `hyprctl clients -j` window list, `hl.dsp.focus` focus, `grim` capture, `wl-copy`/`wl-paste` clipboard round-trip |
| 6 | Observation | `ComputerControl::observe()` returns backend-tagged `Observation` (windows, monitors, cursor) |
| 7 | Verification | `focus_and_verify` re-observes active window with one bounded retry; test asserts structured `Verify` error instead of fake success on mismatch |
| 8 | Permission enforcement | `computer_tools_deny_by_default`: all six tools `PermissionDenied` without allow rules; allowed caps run |
| 9 | Failure handling | Unknown action → `InvalidArguments`; shell metacharacters → refusal; pointer without ydotool → structured `UNAVAILABLE` (never ok) |
| 10 | Integration tests | `crates/integration-tests/tests/p7_tool_fabric.rs` (8 tests) + crate unit tests (18) |

## Capability matrix — this host (Hyprland/Wayland)

| Environment | Backend | Result | Notes |
|---|---|---|---|
| **Linux / Hyprland / Wayland** | `hyprland` | **GREEN** | windows/monitors/focus/close/workspace, capture (grim), clipboard (wl-copy/paste), keyboard (wtype), launch — **pointer UNAVAILABLE** (no ydotool/dotool), a11y PARTIAL (AT-SPI present, tree not wired) |
| Linux / generic Wayland | `wayland` | PARTIAL | capture/clipboard/keyboard only; window ops UNSUPPORTED (no compositor IPC) |
| Linux / Sway | `sway` | coded, not proven | no sway host here |
| Linux / i3 (X11) | `i3` | coded, not proven | no i3-msg/xdotool host here |
| Linux / bspwm | `bspwm` | coded, not proven | no bspc host here |
| Linux / KDE | `kde` | BLOCKED-as-UNAVAILABLE | kdotool absent → honest UNAVAILABLE rows |
| Linux / GNOME | `gnome` | PARTIAL | shell Eval restricted → UNAVAILABLE/partial rows |
| **Windows** | `windows` | **BLOCKED** | non-Windows host → UNAVAILABLE; on-Win32 surface UNSUPPORTED (unsafe FFI forbidden workspace-wide); architecture-ready only |
| Headless / CI | `sim` | GREEN (harness) | forced via `runtime.computer_backend = "sim"`; never auto-selected on a real host |

**Never claims "works everywhere"** — every non-green cell carries the exact
mechanism missing.

## Milestone checklist (ROADMAP v0.3)

- [x] M7.1 Tool Fabric core (`openrt-platform` crate, OWNERSHIP allocated pre-implementation)
- [x] M7.2 Environment + capability discovery (`profile.rs`, `capability.rs`)
- [x] M7.3 ComputerControl abstraction (`control.rs`, `ComputerBackend`, `ControlOp`)
- [x] M7.4 Linux X11/Wayland (`backends/x11.rs`, `backends/wayland.rs`; Wayland first-class)
- [x] M7.5 WM/compositor adapters (hyprland/sway/i3/bspwm/kde/gnome)
- [x] M7.6 Windows (honest surface; BLOCKED on this host)
- [x] M7.7 Observation + verification (`observe`, `focus_and_verify`)
- [x] M7.8 Browser capability hooks (matrix row defers to openrt-computer `browser.*`; no duplicate registry)
- [x] M7.9 MCP/registry integration point (tools register via existing ToolSpec; browser/MCP future tools use `computer.*` caps pattern)
- [x] M7.10 Final gate (fmt/check/test all green; forensic audit below)

## Forensic audit (Phase 7 code)

- **No `unsafe`** in `crates/openrt-platform` (workspace `forbid` upheld; Windows FFI explicitly declined).
- **No shell metacharacter paths**: launch rejects `|;&><`$`; argv passed as process args, never concatenated into `sh -c`.
- **`~/.config` read-only**: discovery uses `is_dir()`/`read_to_string`; config parsers take fixture `&str`; no write/create/remove outside `temp_dir()` test fixtures.
- **No second authority**: one ToolRegistry, one GateExecutor, one policy — `computer.*.all` caps are ordinary capability requests.
- **Kill scope (ADR-008)**: `tool_scope("computer.*") = KillScope::Computer` with regression test; trading scope unchanged.
- **Audit chain**: `gate.rs::is_computer_tool` includes `computer.` → permission decisions recorded in the chained audit log.
- **No fake success**: unavailable ops return `ok:false` + status + reason (asserted for pointer injection).
- **Daemon stdio leak fixed**: `wl-copy` waits only on parent exit (stdio nulled) — no `wait_with_output` hang.

## Deliverables

| Artifact | Path |
|---|---|
| Crate | `crates/openrt-platform/` |
| Integration tests | `crates/integration-tests/tests/p7_tool_fabric.rs` |
| Architecture doc | `docs/architecture/phase7/PHASE7_COMPUTER_CONTROL.md` |
| Acceptance (this file) | `docs/architecture/phase7/PHASE7_ACCEPTANCE.md` |
| OWNERSHIP allocation + change log | `docs/architecture/OWNERSHIP.md` (Phase 7 section; 2026-09-24 row) |
| Roadmap | `docs/roadmap/ROADMAP.md` v0.3 (`## PHASE 7 — TOOL FABRIC`) |
| Config knob | `RuntimeConfig.computer_backend` (`openrt-config`) |
| Runtime field | `Runtime::computer: Arc<ComputerControl>` |

## Verdict

**PHASE 7 COMPLETE — all three gates green (fmt RC=0, check RC=0,
734 passed / 0 failed), 10/10 acceptance criteria GREEN on this host,
capability matrix honest per environment, Phase 6 regression untouched
(17+11+10 / 0).**
