# AGENT TUI — Keybindings & Commands

**Status:** Accepted · **Date:** 2026-10-01 (updated: approval tiers,
global shortcut set, mode overlay)
**Parent:** [AGENT_TUI.md](./AGENT_TUI.md) · **Quick map:** [KEYBOARD.md](./KEYBOARD.md)

Mirrors the in-app help (`F1` or `/help`). Keys are US-layout; `Ctrl` = the
control modifier.

## Global (Agent Mode)

| Key | Action |
|---|---|
| `Ctrl+C` | Quit (works everywhere, including mid-typing) |
| `Enter` | Context action — see per-view table below |
| `Esc` | Clear input → else cancel running task → else "nothing to cancel" |
| `F1` / `?` (input empty) | Help overlay (`Esc`/`Enter`/`F1`/`q` closes) |
| `Ctrl+W` | Toggle Agent ↔ Workbench (one terminal) |
| `Ctrl+K` | Command palette (fuzzy jump-to-view; type to filter, `↑`/`↓`, `Enter`) |
| `Ctrl+P` | Files view — opens with the filter ready (type to narrow) |
| `Ctrl+M` | Model overlay (per-request switch). *Some terminals deliver `Enter` for `Ctrl+M` — use `/model` or `Ctrl+K → model` as the fallback* |
| `Ctrl+T` | Terminal view |
| `Ctrl+G` | Git view |
| `Ctrl+,` | Settings view (read-only runtime configuration) |
| `Ctrl+O` | Operating mode overlay (operator-only; a switch always confirms) |
| `Ctrl+←` / `Ctrl+→` | Previous / next rail view |
| `Ctrl+R` | Refresh current view from its real source |
| `Ctrl+F` | Filter the current list (Files · Tools · Tasks · Sessions · MCP · Extensions · Security) |
| `↑` / `↓` | Chat: scroll stream ±3 lines · other views: move list selection |
| `PageUp` / `PageDown` | Lists: move selection ±10 · Chat: scroll history ±10 · Terminal: scroll log ±10 |
| `Home` / `End` | Lists: first / last row · Chat: bottom · Terminal: oldest / newest output |
| `Esc` | Close filter → close prompt (rule/operator/import) → clear input → cancel running task |
| `Backspace` | Delete last char; **while a filter is open it edits the filter first** (in Files on empty input: up one directory) |
| `Delete` | Clear the input line (while filtering: clear the filter text) |
| printable chars | Append to the input line (or overlay/filter/prompt buffer when one owns it) |
| mouse wheel | `↑`/`↓` equivalent in every view; clicks are ignored by design |

## While an approval is pending

The approval card owns the keyboard until answered
([PERMISSIONS_UX.md](./PERMISSIONS_UX.md)):

| Key | Action |
|---|---|
| `1` / `y` | Allow this call only (`PermissionDecision::Allow`) |
| `2` | Allow session — identical requests release without re-asking (`AllowSession`) |
| `3` | Allow workspace — same, across sessions in this workspace (`AllowWorkspace`) |
| `4` / `n` / `Esc` / `Enter` | Deny (fail closed — `Enter` is deliberately **not** a grant) |
| any other | Status hint: `1 allow once · 2 session · 3 workspace · 4/esc deny` |

Answering Deny at quit time is automatic (fail closed — quitting never
approves anything). The card also overlays every non-Chat view while a
request is pending.

## Confirmation boxes (`y` = proceed)

Destructive/operator actions arm a confirmation that owns the keyboard;
any other key cancels:

| Armed by | Effect of `y` |
|---|---|
| `d` in Sessions (or the session switcher) | `Runtime::delete_session` — persisted transcript removed; task history + audit kept |
| Security `1` (read-only preset) / `2` (full trust) | Append the printed allow-rule list to the versioned policy document (immutable revision + `policy.append` audit row) |
| Security `n` → valid rule | Append that one rule as a new policy revision |
| `Ctrl+O` → `Enter` (mode overlay) | Switch the operating mode through the one `ModeManager` (audited, idempotent); network posture follows the mode |

## Per-view `Enter`

| View | `Enter` does |
|---|---|
| Chat (and any view without a special action) | Send the input as a task prompt (or run the `/…` command) |
| Terminal | Run the input line as a command (gate: `terminal.run`, deny-by-default) |
| Search | Run the query (background walk — the status line shows progress) |
| Files | Open the selected row (dir → descend, file → preview) |
| Models | Typed line → apply as provider tag · empty line → apply the selected card's suggested tag (a chat draft is stashed on entry and restored on exit) · search mode → registry search (then apply a hit); in the `[Hugging Face]` section → the hub query / the selected hub row · `i` import prompt → import the typed local path |
| Sessions | Switch to the selected session (refused while a task runs) |
| Tasks | Preview the selected task (id/status/prompt/result/error) |
| Tools | Preview the selected tool spec as JSON |
| MCP | Inspect the selected server (JSON in Preview) |
| Extensions | Show the selected package manifest (JSON in Preview) |
| Security | Show the versioned policy document + revision list (Preview) |

Extra: `Ctrl+D` in **Files** opens the git diff of the selected file;
`Ctrl+E` in **Chat** expands the latest tool card's full args.

## List views — filter + navigation

| Key | Action |
|---|---|
| `Ctrl+F` | Open the filter line (Files / Tools / Tasks / Sessions / MCP / Extensions / Security) |
| typing | Narrows rows live — substring on the row's real fields; **Files** filters by file name and keeps the selection on a visible row (`Enter` can never open a hidden file) |
| `Enter` | Keep the filter and return focus to the list (in Files: open the selected **visible** row) |
| `Esc` | Close the filter (list text cleared; a second `Esc` may leave the view) |
| `PageUp`/`PageDown` | Move selection ±10 rows |
| `Home`/`End` | First / last visible row |

## Models view — sections, search mode & import

| Key | Action |
|---|---|
| `Tab` / `BackTab` | Cycle the sections `[Local]` → `[Hugging Face]` → `[Installed]` → `[Recommended]` (cursor resets; status line reports the section) |
| `Ctrl+F` | Toggle search mode (the input line becomes a `search models` query field) |
| `Enter` (search mode, query typed) | Registry section → the real registry search (installed + curated + Hugging Face when the mode allows) · `[Hugging Face]` section → the hub query (empty query = recent listing; blocked modes stage nothing — zero rows and a plain `not contacted` note) |
| `Enter` (search mode, a hit selected) | Apply the selected hit's suggested provider tag |
| `Enter` (empty line) | `[Local]`/`[Installed]`/`[Recommended]` → apply the selected card's suggested tag · `[Hugging Face]` → installed row applies its tag; otherwise the **metadata-only** status (remote install does not exist; `i` imports a local copy) — on an empty hub it re-runs the query |
| `Esc` (search mode) | Leave search mode; the input line returns to the model-tag field |
| `i` | Import the selected artifact from a **local file**: type the path, `Enter` runs it in the background (real sha256 + size from the bytes read; refuses files over half of `MemAvailable`; extension allowlist checked first), `Esc` cancels — failures render a `MODEL IMPORT FAILED` card with the exact reason (cleared on success / next import) |
| `s` | Drop-in scan of `models/<category>/` (`safetensors · gguf · bin · pt · pth · onnx · npz`, dirs auto-created on first entry) — each file imported through the one registry with real bytes + digest (+ signature check when `OPENRT_SCAN_BUNDLE` is set); per-file refusals reported in the status line; an empty walk reports `NO MODELS FOUND` |
| `p` (`[Hugging Face]`) | Cycle the pipeline facet (All → each loaded `pipeline_tag` → All) |
| `f` (`[Hugging Face]`) | Open the filter overlay (Task · Provider · Parameters · Quantization · Context · Vision · Tool calling · Format · Runtime): `↑/↓` dimension · `←/→` value · `c` clear · `Enter`/`Esc` close — only real row metadata; dims with no data say so |
| `[` / `]` (`[Hugging Face]`) | Page the hub listing by 10 rows (`limit`/`skip` on the hub API; blocked modes advance the offset but fetch nothing) |
| `d` (`[Hugging Face]`) | Detail facts for the selected row in a Preview overlay (wide terminals also show them in the right-hand pane); every missing field prints `UNKNOWN` with why |

The Models view also shows an honest **native tools** line for the selected
model (`yes` / `no` / `unknown` / `unavailable(probe …)`) — probed through
the runtime provider, never name-guessed.

## MCP connectivity manager (MCP view)

| Key | Action |
|---|---|
| `Enter` | Inspect the selected server (JSON in Preview) |
| `e` | Enable / disable the selected server |
| `t` | Cycle trust (`trusted` → `review` → `blocked`) |
| `s` | Security scan |
| `h` | Health check |
| `o` | Edit the operator id used for these actions (pre-filled; `Enter` saves, `Esc` cancels) |

All actions run in the background through the one MCP registry and report
through the status line.

## Extension browser (Extensions view)

| Key | Action |
|---|---|
| `Enter` | Show the selected package manifest (JSON in Preview) |
| `v` / `s` | Validate / security-scan the selected extension |
| `a` / `d` | Activate / deactivate |
| `x` | Invalidate |
| `i` | Install from a local package JSON path or an https (GitHub raw) URL — the URL leg passes the operating-mode + netpol egress gate **before** any socket; 32 MiB package cap |
| `o` | Edit the operator id (same prompt as MCP) |

## Security control center (Security view)

| Key | Action |
|---|---|
| `Enter` | Show the versioned policy document + revision list (Preview) |
| `1` | Arm the **read-only preset** confirm — allow rules derived from the read-shaped capabilities/tools actually registered (stated substring heuristic); `y` appends |
| `2` | Arm the **full-trust preset** confirm — every registered tool id + capability; `y` appends |
| `n` | New rule prompt: type `<pattern> <allow\|deny\|ask> [scope]`, `Enter` reviews in a confirm (`y` appends), `Esc` cancels; invalid syntax keeps the prompt open |
| `l` | Recent audit-chain rows (actor/action/subject/result) in Preview |
| `Ctrl+F` | Filter the status/rule/finding/recovery rows |

Every append creates an **immutable policy revision** through
`Runtime::policy_append_rule` and a `policy.append` audit row. The view
states honestly that the live gate keeps its current policy until the next
runtime build (the TUI never claims a live policy swap).

## Setup wizard (Overlay::Setup)

| Key | Action |
|---|---|
| `Enter` / `→` | Advance the step / apply the selected recommendation |
| `←` | Back one step |
| `↑`/`↓` | Move the selection |
| `f` (models step) | Focus the Hugging Face query field (`Enter` runs the search in the background, `Esc` unfocuses) |
| `Esc` | Close the wizard (auto-opens on first run with no default model; `/setup` reopens) |

## Overlay keys

| Overlay | Keys |
|---|---|
| Help | `Esc` `Enter` `F1` `q` close · `↑`/`↓` ±1 · `PageUp`/`PageDown` ±10 (content-sized + scrollable at 80×24) |
| Palette | typing filters **fuzzily** (subsequence, best score first) · `↑`/`↓` select · `Enter` jump (view / command / mode overlay) · `Backspace` edit · `Esc` close |
| Model | typing edits the model id · `Enter` apply · `Backspace` delete · `Esc` close |
| Sessions | `↑`/`↓` select · `Enter` switch · `d` delete (confirms) · `Esc` close |
| Mode | `Enter` proposes switching to the other operating mode (confirm: `y`) · `Esc` close |
| Preview | `↑`/`↓` ±1 · `PageUp`/`PageDown` ±10 · `Esc` `Enter` `q` close |
| Setup | see the Setup wizard table above |

## Workbench mode

All existing Workbench keys work unchanged (`workbench_key` delegation).
Two keys always return to Agent Mode first: `Ctrl+W` (toggle back) and
`Ctrl+K` (returns and opens the palette).

## Slash commands (input line)

| Command | Action |
|---|---|
| `/help`, `/?` | Help overlay |
| `/clear` | Clear the visible stream (view only — stored sessions are untouched) |
| `/new`, `/session` | New session |
| `/sessions`, `/switch` | Session switcher overlay |
| `/plan <goal>` | Compile a real workflow plan (openrt-orch compiler) |
| `/run` | Execute the compiled plan |
| `/cancel` | Cancel the running task/plan |
| `/model` | Model overlay (per-request switch) |
| `/setup` | Setup wizard (host / provider / models) |
| `/status` | `session … · task … · mode …` in the status line |
| `/refresh`, `/r` | Reload the current view |
| `/workbench`, `/wb` | Switch to Workbench mode |
| `/quit`, `/exit` | Exit |
| anything else | `unknown command /… — /help lists commands` |

## Status line

The bottom line shows the current status message on the left and the hotkey
hints on the right:

```
 ○ ready — ask OpenRT anything (F1 for help)   F1 help · Ctrl+K jump · Ctrl+P files · Ctrl+M model · Ctrl+T term · Ctrl+O mode
```

While an approval is pending the hint swaps to
`approval keys: 1 allow once · 2 session · 3 workspace · 4/Esc/Enter deny`.

Notable status messages: `no default model — Ctrl+K → models, …` (honest
startup hint when the provider has no `default_model`), `no model — Ctrl+K →
models, …` (send refused up front when no model would resolve — input is
kept),
`model → <id> (next requests)`, `approved <tool>` / `denied <tool>`,
`stream cleared`, `nothing to cancel`,
`session interrupted — boot recovery found work left behind` (see
[AGENT_RECOVERY.md](./AGENT_RECOVERY.md)).
