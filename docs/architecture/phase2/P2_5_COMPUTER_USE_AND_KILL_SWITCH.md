# Phase 2.5 — Computer-Use Loop, Screen Backend, and Kill Switch

**Status:** Implemented
**Date:** 2026-09-17
**Scope:** ROADMAP M2.5 — browser (window-level), deterministic screen
grounding, mouse/keyboard input, and the computer-use loop, plus the
ADR-008 computer-use kill switch over the event bus. Adds `openrt-safety`
and `openrt-computer` M2.5 tools, wires everything into `openrt-runtime`,
extends `openrt-config`. Baseline behavior unchanged (183 → 201 tests, all
green, stable ×3). P2.6 (checkpoints/rollback/dry-run) is NOT in this
milestone.

## 1. Verification (before changing anything)

- All milestones gated green: fmt / check / clippy `-D warnings` / build /
  `cargo test --workspace` (183 passed ×3 stable).
- The P2.5 harness question (Xvfb + xdotool vs deterministic screen) was
  resolved by host inspection: Xvfb, xdotool, `xmessage`, `xwd`, `xterm`,
  `wmctrl`, `xdpyinfo` are all ABSENT, and `sudo -n` requires a password
  (no install possible). Decision: a deterministic in-memory `SimScreen`
  backend is the primary test harness, and `X11Screen` (xdotool + ImageMagick
  `import`) is an optional real-display backend, active only when the tooling
  is detected at build time — the same skip-if-absent pattern as bwrap.
  This satisfies the M2.5 DoD: "scripted screen states drive grounding +
  input in headless test harness (no real display needed)."
- ADR-008 (common HALT signal over the event bus, per-subsystem scoped
  mechanism adapters, operator-only) was already accepted and prescriptive —
  no ADR amendment needed.

## 2. Design decisions

### 2.1 Screen backends — `openrt-computer`
- `ScreenBackend` trait (downcastable): `capture()`, `pointer_*`, `keyboard_*`,
  `window_*`. Two implementations:
  - `SimScreen`: deterministic 1280×720 DOM of `ScreenElement` (name/role/box),
    top-most-hit click targeting, window focus tracking, full interaction log
    (`pointer_clicks()`, `typed()`, `keys()`, `focused_windows()`). This is
    the roadmap's no-real-display harness backend and the configured default.
  - `X11Screen`: xdotool `search`/`getwindowgeometry`/`getwindowname` + `import`
    root capture; `try_new()` only resolves when `have_x11_tooling()` is true.
- `ScreenCapture.elements` carries the verdict-free DOM; agents ground targets
  either through the `screen.ground` tool or the loop's snapshot. Grounding is
  substring label matching — the same primitive a future model-assisted
  grounding adapter (P5) will implement over.

### 2.2 Computer-use loop — `ComputerUseLoop`
- `computer_loop<D>(max_steps, decider)`; decider maps a `ScreenCapture` to
  `Option<ToolCall>`. The loop checks the kill switch **before perceiving and
  after every action**; it is a thin, unsafe-free driver over the gated
  executor (permissions/resource checks still apply per action).

### 2.3 Kill switch — `openrt-safety` (ADR-008 mechanism adapter)
- `KillSwitch` (latches on first tripper-wins `trip(reason)`), `KillScope`
  (`computer` now; `trading`/`global` reserved for P6), `tool_scope` classifier
  (6 computer tools + `terminal.run` + `python.run`).
- **Gate integration:** a scoped, engaged switch turns every computer-scoped
  tool invocation into `ToolError::KillSwitchEngaged` and emits an audited
  `EventKind::SafetyHalt` (scope/reason/tool/reaction="denied") — the ADR-008
  signal bus.
- **Mid-action halting:** `Terminal` and `PythonSandbox` poll the switch with
  `tokio::select!` and kill the live process group / interpreter, surfacing
  `KillSwitchEngaged` to the caller.
- `Runtime::halt(reason)` (first-tripper-wins, emits operator `SafetyHalt`),
  `halted()`, `clear_halt()`.
- **Scoping discipline:** a computer HALT emits the signal from the computer
  adapter only; it is denied at the gate for computer tools and never touches
  other subsystems (`echo`, `system.info`, `filesystem.read` keep working).

## 3. Tools added (all deny-by-default; plain-tool-id allow rules)

| Tool | Description |
|---|---|
| `screen.capture` | Snapshot of the active backend (elements + optional image) |
| `screen.ground` | Resolve a target label to bounding box(es) in the snapshot |
| `input.mouse` | click / move / drag via the backend |
| `input.keyboard` | type / keys via the backend |
| `browser.list` | Window list (window-level browser, M2.5) |
| `browser.activate` | Focus a window for subsequent input |
| `terminal.run` / `python.run` | Existing M2.3/M2.4 tools, now kill-aware |

## 4. Config

- `RuntimeConfig.screen_backend: Option<String>` — `"sim"` (default) or
  `"x11"` (falls back to sim when tooling is absent); `Runtime.screen` and
  `Runtime.kill_switch` are public.

## 5. Files changed

- New crate `openrt-safety` (`kill.rs`, `lib.rs`), added to workspace.
- `openrt-computer`: new `screen.rs` (backends + 6 tools), `loop_.rs`
  (computer-use loop); `terminal.rs` kill-aware; 3-arg `computer_tools(...)`
  + `computer_tools_with_screen(...)` + `default_screen_backend(...)`.
- `openrt-sandbox`: kill-aware `collect_output`, `SandboxError::Halted`.
- `openrt-tools`: `ToolError::KillSwitchEngaged { tool, reason }`.
- `openrt-core`: `EventKind::SafetyHalt` (bus signal).
- `openrt-config`: `screen_backend` field.
- `openrt-runtime`: gate kill-check + `halt`/`halted`/`clear_halt`/
  `computer_loop`, wiring.
- `integration-tests`: `p2_5_computer_use.rs`, `openrt-computer` dev-dep.

## 6. Tests added

- `openrt-safety` 5: scope classifier (8 computer tools + 4 non-scoped),
  trip latch, reset, shared clones, halt poll.
- `openrt-computer` 9 (24 total): sim ground/click/type/focus,
  grounding accuracy 20/20 on a 20-element fixture (≥95% DoD),
  X11 tooling detection, tools drive a scripted harness, loop
  perceive-decide-act / budget exhaustion / kill-abort.
- `openrt-sandbox` 8, `openrt-tools` 16 (100% error-path suite) unchanged.
- `integration-tests` 4 (201 total): registration + deny-by-default;
  loop "fill-and-submit a form" on a seeded `SimScreen`; mid-action
  `terminal.run` (sleep 300) halted promptly + audited; scoped isolation
  (echo/filesystem still work, computer blocked, `clear_halt()` restores).

## 7. Quality gate (all green)

fmt `--check` · `cargo check --workspace` · clippy `-D warnings` ·
`cargo test --workspace` 201 passed / 0 failed (×3 stable) ·
`cargo build --workspace`.

## 8. Documented gaps (not silently accepted)

- Grounding is DOM-substring matching; model-assisted (image→DOM) grounding
  is deferred to P5, where the verdict-free `ScreenCapture` model applies.
- `browser.*` is window-level (list/focus), not webdriver automation.
- `X11Screen` is implemented but unexercised on this host (no xdotool/Xvfb);
  it self-skips. Its path is covered by unit-tested tooling detection only.
- Kill switch is scope-granular (computer-wide), operator-only; per-tool or
  per-process tripping is a future refinement. P6 adds `trading`/`global`.