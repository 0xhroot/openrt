# Phase 7 — Tool Fabric / Cross-Platform Computer Control

> **Status:** implemented (see `PHASE7_ACCEPTANCE.md` for the gate record).
> ROADMAP v0.3, milestones M7.1–M7.10.

## Objective

A platform-abstraction layer under the existing Phase 2 computer tools that
makes computer control honest and portable: discover *where* we are, report
what is actually possible, resolve a real backend through a fixed priority,
and expose high-level operations through the **existing** ToolSpec/GateExecutor
(no second registry, gate, or permission system).

## Non-negotiables (mission)

| Rule | How it is enforced here |
|---|---|
| Keybindings are environment knowledge, not core primitives | `config_scan` parsers only *record* hints; tools prefer `computer.app launch` (`LaunchApp`) over `SendKey("SUPER+Q")` |
| Backend priority: native compositor → a11y → portal → display server → input injection → configured shortcut | `BackendResolver` (`openrt-platform/src/backend.rs`) layers 1–6; decision log returned with every resolve |
| Wayland first-class; X11 never a silent fallback | Layer 4 picks `Wayland` for `SessionType::Wayland`, `X11` only for `SessionType::X11` |
| Structured `SUPPORTED/UNSUPPORTED/PERMISSION_REQUIRED/UNAVAILABLE` diagnostics | `CapabilityStatus` + `CapabilityReport {capability, status, reason, backend}`; every non-SUPPORTED row carries a non-empty `reason` |
| No fake backends | Windows/GNOME/KDE/generic-Wayland return honest `UNAVAILABLE`/`UNSUPPORTED` rows when the mechanism is missing; sim is only ever *forced* or last-resort |
| No raw shell-by-default | Launch paths reject shell metacharacters (`\|;&><`$`) and invoke `hyprctl`/`swaymsg`/`i3-msg` argv directly or `xdg-open` |
| `~/.config` read-only | Scanners take `&str` fixtures; discovery only `is_dir()`/`read_to_string` probes |
| observe → plan → action → observe → verify | `ComputerControl::observe()`, `focus_and_verify()` (single bounded retry) |
| Capabilities claimed only when proven | Test `discovery_selects_backend_and_runs_real_ops` executes the op whenever the matrix says `is_usable()` |

## Components

```
openrt-platform/
  profile.rs      EnvironmentProfile, SessionType, DesktopKind, DiscoveryInputs
  capability.rs   CapabilityStatus, ComputerCapability, CapabilityReport, CapabilityMatrix
  config_scan.rs  hyprland.conf / sway-i3 / bspwmrc / sxhkdrc / .desktop parsers (fixture-driven)
  backend.rs      ComputerBackend trait, ControlOp, OperationResult, Observation,
                  BackendResolver (6-layer priority), BackendKind
  control.rs      ComputerControl (profile + backend + matrix + focus_and_verify)
  apps.rs         read-only .desktop entry discovery (system + user dirs)
  tools.rs        computer.env/window/screen/input/clipboard/app tool wrappers
  backends/       hyprland, sway, i3, bspwm, kde, gnome, wayland, x11, windows, sim
```

### Capability matrix (this host: Omarchy/Hyprland 0.56.2, Wayland)

| Capability | Status | Reason |
|---|---|---|
| env_discover | SUPPORTED | discovery path |
| window_list/focus/close, workspace_switch, monitor_list | SUPPORTED | `hyprctl -j` + `hl.dsp.*` Lua dispatch |
| screen_capture | SUPPORTED | `grim` |
| clipboard_get/set | SUPPORTED | `wl-paste` / `wl-copy` |
| keyboard_inject | SUPPORTED | `wtype` |
| application_launch | SUPPORTED | `hl.dsp.exec_cmd` / `xdg-open` (no shell) |
| pointer_inject | UNAVAILABLE | no ydotool/dotool; hyprctl has no warp |
| accessibility_tree | PARTIAL | AT-SPI bus present; tree API not wired in this pass |
| browser_surface | UNAVAILABLE | owned by openrt-computer `browser.*` tools |

Windows on this host: every window/input/capture op is `UNAVAILABLE`
("windows backend not usable on this OS"); UIA/GDI/SendInput are `UNSUPPORTED`
(unsafe FFI forbidden workspace-wide). KDE/GNOME/i3/bspwm: honest
`UNAVAILABLE` rows when `kdotool`/`gdbus`/`i3-msg`/`bspc`/`xdotool` are
missing.

## Production wiring

- `RuntimeConfig.computer_backend`: `"sim"` forces the harness; explicit ids
  pin a backend; `None`/`"auto"` discovers the host and resolves naturally.
- `Runtime::computer` — `Arc<ComputerControl>` shared by all six tools.
- Tools registered in `runtime.rs` next to the Phase 2 screen tools, through
  `openrt_tools::ToolRegistry` (unique names, `computer.*.all` caps).
- Gate: `is_computer_tool` includes `computer.` (audit-logged decisions).
- ADR-008: `tool_scope("computer.*") == KillScope::Computer`.
- Policy: deny-by-default; tests add explicit allow rules
  (`computer.env.all`, `computer.window.all`, …).

## Tests

| Suite | File | Coverage |
|---|---|---|
| Unit (18) | `crates/openrt-platform` | discovery fixtures, 5 config parsers, desktop entries, resolver priority, sim contract, matrix statuses, control verify |
| Integration (8) | `crates/integration-tests/tests/p7_tool_fabric.rs` | computer.env through production gate; deny-by-default for all six tools; kill-scope; **live discovery → Hyprland window list → focus_and_verify → grim capture → wl-copy/wl-paste round-trip → honest pointer UNAVAILABLE**; sim window tool path; fixture scanners; metacharacter refusal |

## Capability matrix per environment (acceptance honesty)

| Environment | Backend | GREEN? | Notes |
|---|---|---|---|
| Linux / Wayland / Hyprland | `hyprland` | **GREEN** on this host | windows, capture, clipboard, keyboard, launch; pointer UNAVAILABLE |
| Linux / Wayland / Sway | `sway` | coded, not proven here | needs `swaymsg` host |
| Linux / X11 / i3 | `i3` | coded, not proven here | needs `i3-msg` + `xdotool` |
| Linux / X11 / bspwm | `bspwm` | coded, not proven here | needs `bspc` |
| Linux / KDE | `kde` | PARTIAL | `kdotool` absent → UNAVAILABLE rows |
| Linux / GNOME | `gnome` | PARTIAL | shell `Eval` mostly restricted → UNAVAILABLE/partial |
| Linux / Wayland, no compositor IPC | `wayland` | PARTIAL | capture/clipboard/keyboard only |
| Windows | `windows` | **BLOCKED** | unsafe FFI forbidden; architecture-ready surface only |
| Headless / CI | `sim` | GREEN (harness) | forced via `computer_backend: "sim"` |

## Reconciliation with earlier phases

- Phase 2 `screen.*`/`input.*`/`browser.*` tools and `ScreenBackend` remain
  owned by `openrt-computer` — **not rebuilt**.
- Phase 6 gate/kill/policy/audit are reused; no parallel authorities.
- `OWNERSHIP.md` Phase 7 table allocates crate `openrt-platform` and
  namespace `platform.profile` (ADR-007) before implementation.

## Files

- Crate: `crates/openrt-platform/`
- Integration tests: `crates/integration-tests/tests/p7_tool_fabric.rs`
- Gate record: `docs/architecture/phase7/PHASE7_ACCEPTANCE.md`
- Allocation: `docs/architecture/OWNERSHIP.md` (Phase 7 section)
- Roadmap: `docs/roadmap/ROADMAP.md` (v0.3, `## PHASE 7 — TOOL FABRIC`)
