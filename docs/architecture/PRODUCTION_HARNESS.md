# Production Harness — terminal safety, visual identity, input surface

**Status:** Accepted · **Date:** 2026-10-01
**Parent:** [AGENT_TUI.md](./AGENT_TUI.md) · **Companion docs:** [KEYBOARD.md](./KEYBOARD.md),
[AGENT_RECOVERY.md](./AGENT_RECOVERY.md), [PERMISSIONS_UX.md](./PERMISSIONS_UX.md)

This is the master contract for the harness layer that surrounds the Agent
TUI: what the terminal is allowed to do to the operator, what the operator
can do to the terminal, and what the screen is allowed to look like.

## 1. Terminal safety (raw mode + alternate screen)

Setup/teardown lives in **one place** (`openrt-tui/src/run.rs`,
`enter_terminal` / `leave_terminal`) and both TUI modes (Agent and
Workbench) use it.

| Guarantee | Mechanism |
|---|---|
| Every normal exit path restores the terminal | `leave_terminal`: raw mode off, alternate screen left, mouse capture disabled, cursor shown — restore errors are swallowed so the original error surfaces |
| A **panic** never leaves the terminal broken | `install_panic_restore` chains a hook (installed once per process) that performs the same restore **before** the previous hook prints the panic — the message lands on the restored screen, not inside the alternate buffer |
| A key-handler error never kills the TUI | structured in-session error (`record_error`); only an explicit quit or a truly fatal draw/IO failure stops the loop |
| Unanswered approvals never auto-grant | `app.shutdown()` fails closed on every exit path |

The hook is idempotent (`std::sync::Once`) — double installation cannot
double-chain.

## 2. Mouse support

* Mouse capture is enabled together with the alternate screen and disabled
  on every exit path (including the panic hook).
* **Scroll wheel → arrow keys** (`mouse_wheel_to_key`): `ScrollUp` = `↑`,
  `ScrollDown` = `↓`. Every view's existing scroll semantics therefore
  apply unchanged — chat history, list selection, terminal scrollback,
  overlay lists.
* **Clicks are ignored by design.** No view has click affordances, and a
  click must never fire a default action (like `Enter` = send).
* Both event loops (Agent `agent/run.rs`, Workbench `run.rs`) handle
  `Event::Mouse`; other event kinds are ignored.

## 3. ASCII fallback

Semantic glyphs (`▸ › ✓ ✗ ● ○ ⚠ … → │ ┌ …`) do not exist on every
terminal. Resolution runs **once per process** at terminal setup:

1. `OPENRT_ASCII=0` → Unicode forced; any other non-empty value → ASCII forced.
2. Otherwise locale: `LC_ALL` → `LC_CTYPE` → `LANG`. A locale that does not
   contain `utf` (case-insensitive) → ASCII.
3. **No locale info at all → ASCII** (fail closed on rendering, never on
   behavior).

Two mechanisms:

* `g("✓", "+")` — static UI marks in `agent/render.rs` swap at the source.
* `sanitize_text` / `ascii_line` — applied at the text sinks (chat stream,
  status line, list rows, preview, approval card, boxed panels), so
  **model output and status strings** carrying arbitrary Unicode degrade
  safely. Latin-1 separators (`·`, `—`) pass through — every code page has
  them.

Test: `ascii_glyph_pairs_map_and_restore` exercises both branches via
`set_ascii_mode`.

## 4. Visual identity (one palette, both modes)

`openrt-tui/src/theme.rs` is the single source of truth; Agent
(`agent/render.rs`) and Workbench (`ui.rs`) import it.

| Token | Value | Role |
|---|---|---|
| `BG` | `#000000` | **Painted explicitly** by both renderers — the app never trusts the terminal's background theme |
| `SURFACE` | `#0E0E12` | Lifted surfaces: input bar, approval/confirm/overlay cards, right column |
| `ACCENT` | `#7C5CFF` | Violet: focus borders, selection, primary actions |
| `BORDER` | `#343440` | **Borders only — never text** (enforced by test) |
| `DIM` | `#848490` | Secondary text: titles, hints, timestamps |
| `OK` / `BAD` / `WARN` / `INFO` | green / red / amber / cyan | Semantic status |

Contract details:

* Main panels are pitch black (`BG`); interactive/floating surfaces lift to
  `SURFACE`.
* Panel titles are explicit text (never inherit border color).
* The 47 former `fg(BORDER)` text sites moved to `DIM`; the test
  `theme_paints_black_surfaces_and_thin_borders` fails if `BORDER`
  reappears on a non-border cell.
* Focus semantics are shared: focused borders/selections use `ACCENT`
  (Workbench panel focus ring included); restricted posture (Sovereign)
  is amber in **both** headers, open posture (Provisioning) is green.

## 5. Recovery surface

Boot recovery is surfaced **in-session** — see
[AGENT_RECOVERY.md](./AGENT_RECOVERY.md). Summary: a non-clean boot record
renders a `SESSION INTERRUPTED` block in the chat stream at startup with
real findings (never invented), while the recovery rows stay browsable in
the SECURITY view.

## 6. Resilience contracts

* View refreshes are background joins — the UI never blocks; results land
  via `pump()`.
* `files`/`git`/`models` loads show honest `not loaded — <action>` empty
  states with the **real** key for the action (`Ctrl+R`, not a stale `r`).
* Filter lists always keep the selection on a **visible** row (Files maps
  through the filtered index set; `Enter` cannot open a hidden file).
* Empty states carry an action (`Esc clears`, `send a message or /plan`,
  `open GIT view`).

## 7. Doc map

| Doc | Covers |
|---|---|
| [KEYBOARD.md](./KEYBOARD.md) | Every key, every view, overlays, approvals, mouse |
| [PERMISSIONS_UX.md](./PERMISSIONS_UX.md) | Approval card, tier semantics, gate integration |
| [AGENT_RECOVERY.md](./AGENT_RECOVERY.md) | Boot scan, interrupted session card, resume paths |
| [SECURITY_CONTROL_CENTER.md](./SECURITY_CONTROL_CENTER.md) | SECURITY view, presets, shield, policy appends |
| [MODEL_LIBRARY.md](./MODEL_LIBRARY.md) | Models view end-to-end |
| [MODEL_BROWSER.md](./MODEL_BROWSER.md) | Hugging Face hub browser detail |
