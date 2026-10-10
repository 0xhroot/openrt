# KEYBOARD — canonical key map

**Status:** Accepted · **Date:** 2026-10-01
**Parent:** [AGENT_TUI.md](./AGENT_TUI.md) · **Deep reference:**
[AGENT_TUI_KEYBINDINGS.md](./AGENT_TUI_KEYBINDINGS.md) (per-view `Enter`,
Models/MCP/Extensions/Security tables, slash commands)

US layout. `Ctrl` = control modifier. Everything below is also in the
in-app help (`F1` / `/help`).

## At a glance

```
 navigation        views                  surfaces             approvals
 ─────────         ─────                  ───────              ────────
 ↑/↓  move         Ctrl+P  Files          Ctrl+K  palette      1 / y  allow once
 PgUp/PgDn ±10     Ctrl+T  Terminal       Ctrl+M  model        2      allow session
 Home/End          Ctrl+G  Git            Ctrl+,  settings     3      allow workspace
 Ctrl+←/→  views   Ctrl+O  mode           F1      help         4/n/Esc/Enter  deny
 Ctrl+R  refresh   Ctrl+W  workbench      Ctrl+E  expand card  (Enter NEVER grants)
```

## Global

| Key | Action |
|---|---|
| `Ctrl+C` | Quit — works everywhere, including mid-typing |
| `Enter` | Context action (per view — see the deep reference) |
| `Esc` | Filter → prompt → clear input → cancel task (innermost first) |
| `F1` / `?` (empty input) | Help overlay |
| `Ctrl+K` | Command palette — **fuzzy** jump to any view, Workbench, slash command, or the mode overlay |
| `Ctrl+W` | Toggle Agent ↔ Workbench |
| `Ctrl+←` / `Ctrl+→` | Previous / next view (rail order) |
| `Ctrl+R` | Refresh the current view from its real source |
| `Ctrl+P` | Files — file picker with the filter ready |
| `Ctrl+T` | Terminal |
| `Ctrl+G` | Git |
| `Ctrl+,` | Settings (read-only runtime configuration: mode, limits, model, workspace) |
| `Ctrl+M` | Model overlay — *some terminals send `Enter` for `Ctrl+M`; fallback: `/model` or `Ctrl+K → model`* |
| `Ctrl+O` | Operating mode overlay (operator-only; `Enter` proposes, `y` confirms) |
| `Ctrl+F` | Filter the current list (Files, Tools, Tasks, Sessions, MCP, Extensions, Security) |
| `Ctrl+D` | Files: git diff of the selected file |
| `Ctrl+E` | Chat: expand the latest tool card's full args |

## List navigation & filters

| Key | Action |
|---|---|
| `↑` / `↓` | Move the selection (Chat: scroll the stream) |
| `PageUp` / `PageDown` | Move ±10 (Chat/Terminal: scroll ±10) |
| `Home` / `End` | First / last **visible** row (Chat: bottom; Terminal: oldest/newest) |
| `Ctrl+F` / typing | Open filter, type to narrow (Files: by name, selection stays visible) |
| `Backspace` | Edits the **filter** while it is open; otherwise deletes input (Files, empty input: up one directory) |
| `Esc` (filter) | Close the filter, full list back |

## Approvals (pending permission)

| Key | Decision |
|---|---|
| `1` / `y` | Allow once |
| `2` | Allow for the session (identical requests release without re-asking) |
| `3` | Allow for the workspace (across sessions) |
| `4` / `n` / `Esc` / `Enter` | **Deny** — fail closed; Enter is never a grant |
| wheel scroll | Works normally; the card also overlays non-Chat views |

Unanswered approvals are denied at quit. Details:
[PERMISSIONS_UX.md](./PERMISSIONS_UX.md).

## Confirmations

Destructive/operator actions: `y` proceeds, **any other key cancels** —
session delete, policy presets, new rule append, operating-mode switch.

## Per-view `Enter` (summary)

| View | Enter |
|---|---|
| Chat / default | Send (or run `/…`) |
| Terminal | Run the input as a command (`terminal.run`, deny-by-default) |
| Files | Open the selected row (dir descends / file previews) |
| Sessions | Switch session (refused while a task runs) |
| Tasks / Tools / MCP / Extensions / Security | Preview / inspect the selected row |
| Models | Apply selected/typed model tag, or run the active search |
| Search | Run the query |

## Mouse

| Input | Action |
|---|---|
| Wheel up / down | `↑` / `↓` — same semantics as the keys in every view |
| Click / drag | Ignored (no click affordances exist; a click must never fire a default action) |

Capture is enabled with the alternate screen and disabled on every exit
path, including the panic restore hook.

## Operating mode overlay (`Ctrl+O`)

`Enter` proposes the other mode → confirm `y` → the one `ModeManager`
performs the audited, idempotent transition; network posture follows
(`sovereign` = net blocked, amber in the header; `provisioning` = net
policy-governed, green). `Esc` closes.

## Environment

| Variable | Effect |
|---|---|
| `OPENRT_ASCII=0` | Force Unicode glyphs |
| `OPENRT_ASCII` (other non-empty) | Force ASCII glyphs |
| *(unset)* | Locale decides (`LC_ALL` → `LC_CTYPE` → `LANG`); no UTF-8 info ⇒ ASCII |

See [PRODUCTION_HARNESS.md](./PRODUCTION_HARNESS.md) §3 for the fallback
rules.
