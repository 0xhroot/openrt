# OPENRT AGENT TUI

**Status:** Implemented (Agent Mode is the default surface of `openrt`)
**Date:** 2026-09-29
**Companion docs:** [AGENT_TUI_ARCHITECTURE.md](./AGENT_TUI_ARCHITECTURE.md) ·
[AGENT_TUI_ACCEPTANCE.md](./AGENT_TUI_ACCEPTANCE.md) ·
[AGENT_TUI_KEYBINDINGS.md](./AGENT_TUI_KEYBINDINGS.md) ·
[REAL_AGENT_HARNESS.md](./REAL_AGENT_HARNESS.md) ·
[AGENT_TOOL_CALLING.md](./AGENT_TOOL_CALLING.md)

## What it is

A pitch-black, keyboard-first, agent-first terminal UI over the existing
Runtime — an operator console where a chat stream, a real permission gate and
the full Workbench live in **one terminal, one process, one Runtime**.

```console
$ openrt            # Agent Mode is the first screen
$ openrt agent      # same, explicit
```

`Ctrl+W` toggles Agent ↔ Workbench at any time; the chat input never
disappears in Agent Mode; approvals can only be answered with an explicit
`y`/`n`.

## Screen anatomy

```
┌ header ────────────────────────────────────────────────────────────────┐
│ OPENRT agent │ session  ⎇ branch  ~/path   SOVEREIGN NET-OFF ctx ≈0% 08:40 │
└────────────────────────────────────────────────────────────────────────┘
┌ rail ─────┐┌ chat ────────────────────────────────┐┌ right column ──────┐
│ › CHAT    ││ You 08:42                            ││ project / task      │
│   FILES   ││ list three files here                ││ git status          │
│   …       ││ OpenRT 08:42                         ││ system              │
│   SECURITY││  … answer …                          ││                     │
│ ────────  │├──────────────────────────────────────┤│                     │
│ ›WORKBENCH││ Ask OpenRT anything…                 ││                     │
│ v0.1.0    │└ Enter send · /help · Esc clear/cancel┘│                     │
└───────────┘                                        └─────────────────────┘
 ○ status line          F1 help · Ctrl+K jump · Ctrl+W workbench · Ctrl+R refresh
```

- **Header** — session label, git branch, working path, and right-aligned
  pills read from real runtime state: operating mode, network policy, honest
  `ctx ≈n%` estimate (or `ctx n/a`), wall clock. On narrow terminals pills
  degrade one at a time instead of disappearing.
- **Left rail** — Chat · Files · Search · Tasks · Sessions · Agents · Git ·
  Terminal · Tools · Artifacts · Models · MCP · Extensions · Security, plus a
  Workbench jumper and the version. Active item is violet.
- **Chat stream** — `You` / `OpenRT` turns, per-session status notes
  (`session created`, errors), tool cards showing the gate's own
  `PermissionRequested` args (redacted by the gate) with duration and
  outcome (Done / Denied), and **Plan entries** (goal + compiler step
  labels, live ✓/▸/✗ marks) streamed into the transcript — plan output is
  visible in chat without opening another view.
- **Dashboard** — chat is the home screen. At ≥110 columns the right
  column shows Project / Current Task / Git Status / System boxes; below
  that (down to the 80×24 minimum) a **one-row compact dashboard strip**
  above the input line keeps the same facts visible (kill switch first,
  then session, model, task, plan progress, findings) — items drop
  right-to-left as width shrinks, never overlapping.
- **Input line + hotkey hints** — permanent; context-dependent `Enter`
  (send, run, open, apply, switch).

## Operator surfaces (product polish)

The list views are one consistent contract, and the operator-facing
workflows run through the existing authorities only:

- **First-run setup wizard** (`/setup`, auto-open when the provider has no
  default model) — real host specs from `SystemInfo::collect`, honest
  provider probe, recommended models from installed bytes + curated
  catalog + Hugging Face metadata (network leg runs in the background), and
  an apply step through the same per-request switcher.
- **Model browser** — four sections (`Tab`/`BackTab`): `[Local]` (category
  sections over real tags), `[Hugging Face]` (hub marketplace metadata —
  mode-gated before anything is staged, zero rows + `not contacted` note
  under Sovereign),
  `[Installed]` (bytes on disk) and `[Recommended]` (curated catalog), all
  derived from the ONE registry list. Enter applies the selected card's
  suggested provider tag (display name ≠ tag; the tag drives the request) —
  a hub row is **metadata only** (remote install does not exist; `i`
  imports a local copy with a real sha256 + size computed from the bytes
  read, size guard refuses anything over half of live `MemAvailable`).
  Wide terminals (≥ 100 cols) render a right-hand detail pane of declared
  hub facts (license, params, context, gated, sha, format, quant, real
  size, real `nvidia-smi` VRAM, a factual fit line when both are known) —
  see [MODEL_BROWSER.md](./MODEL_BROWSER.md). `Ctrl+F` runs the real
  registry search (installed + curated + HF when the mode allows) with
  honest per-row sizes from hub metadata; `s` drop-in-scans
  `models/<category>/` through the same import path.
- **Session lifecycle** — `d` arms a confirmation; only `y` deletes, through
  `Runtime::delete_session` (persisted transcript removed; task history and
  audit rows kept; cancelling is any other key).
- **Terminal** — commands run through the gated `terminal.run` tool in a
  background join; approvals still own the keyboard; output appends when
  the command finishes.
- **Universal list contract** — `Ctrl+F` filters (Tools / Tasks / Sessions /
  MCP / Extensions / Security), `PageUp`/`PageDown` move by 10,
  `Home`/`End` jump to the first/last row (Chat scrolls history, Terminal
  scrolls its log), `Esc` closes the filter before it leaves the view.
- **MCP connectivity manager** — real registry rows (trust, enabled,
  security status, discovered tool count, declarer) with `e` enable/disable,
  `t` trust cycle, `s` scan, `h` health, `o` operator-id edit; every action
  is spawned through the runtime and reports through the status line.
- **Extension browser** — inventory from `extension_states()` (version,
  activation, security) with validate/scan/activate/deactivate/invalidate,
  plus `i` install from a local package path or an egress-gated GitHub raw
  URL (mode + netpol `ensure_egress("extension", …)` before any socket,
  32 MiB package cap, honest read/egress errors).
- **Security control center** — the Security view is editable: `1` applies
  a *read-only* preset and `2` a *full-trust* preset (rules derived from the
  tools/capabilities ACTUALLY registered in this runtime — full = every
  tool id + capability, read-only = read-shaped names via a stated substring
  heuristic), `n` appends one custom rule (`pattern <allow|deny|ask>
  [scope]`), `Enter` shows the versioned policy document + revision list,
  `l` opens recent audit-chain rows. Every append creates a new immutable
  policy revision through `Runtime::policy_append_rule` (audited,
  `policy.append`), and the view states honestly that the live gate keeps
  its current policy until the next runtime build.

## Async contract

The draw → pump → key loop never waits on IO:

- Directory walks, `git` subprocesses, registry/model probes, HF searches
  (including the wizard's), previews/diffs, terminal commands, imports,
  operator actions (MCP/extensions/policy) and view refreshes all run as
  background joins (`refresh_join` / `term_join` / `op_join` / `hf_size_join`
  / `import_join`) whose results land in `finalize_joins` — the status line
  narrates progress (`loading files …`, `searching “q” …`) and errors
  surface as themselves. Only a single-flight refresh runs at a time
  (latest request wins).
- The one deliberate startup exception: `AgentApp::new` drains the initial
  git read before the first frame so the home screen ships real data.

## What is real

Everything the TUI shows comes from the Runtime — no synthetic activity:

- **Tasks** are `Runtime::spawn_task` runs; statuses come from the event bus
  and `Runtime::store`; results are persisted and reloaded from disk.
- **Approvals** are the actual `PermissionResolver` handshake: queued
  `ApprovalRequest`s block the keyboard until answered; the reply is the
  `PermissionDecision` the gate consumes. Quit answers **Deny** to anything
  still pending (fail closed).
- **Model switching** is a per-request `model_id` override
  (`SwitchableProvider`) — no second router, no re-registration.
- **No fake streaming** — the provider is non-streaming, so assistant text
  renders when the task completes, never letter-by-letter; `ctx` is a labeled
  approximation (`≈`), or `n/a`.
- **Plans** (`/plan`, `/run`) are the real `openrt-orch` compiler and
  workflow runner (`WorkflowStep*` events update the plan checklist).
  Compiler/policy rejections are structured, session-alive results — a
  `PLAN FAILED` block with the real diagnostic code, reason, and fix, plus
  `r` retry / `e` edit / `Esc` dismiss; a failed plan always clears the
  `plan`/`compiled` state so `/run` can never fire a stale plan
  ([PLAN_ERROR_HANDLING.md](./PLAN_ERROR_HANDLING.md)).
- **Tool calling** is native: the agent attaches the policy-filtered tool
  schemas to every model request, live calls run through the one gate, and
  denials/failures come back to the model as structured tool results (see
  [AGENT_TOOL_CALLING.md](./AGENT_TOOL_CALLING.md)).

## Honest behaviors

- Provider has no `default_model`? The status line says so at startup and
  the Models view (`Ctrl+K → models`) is the way to pick one.
- Git with zero commits renders `⎇ master` / `No commits yet` (via
  `symbolic-ref` fallback) instead of failing.
- A model that does not call tools answers from its own weights — the TUI
  never fabricates tool cards. Whether the selected model even *supports*
  tools is probed live: the Models view shows an honest
  `native tools yes/no/unknown/unavailable` line (and `openrt doctor` has a
  `model.tools` row) — never a name-guessed `yes`. Tool-card behavior is
  exercised end-to-end through scripted provider tests (acceptance S12) and
  real-model runs ([REAL_AGENT_HARNESS.md](./REAL_AGENT_HARNESS.md)).
- **Models view search** (`Ctrl+F`) runs the real registry search; `Esc`
  leaves search mode and the input line returns to the model-tag field.
- **Security presets are named honestly** — the read-only preset says it
  uses a substring heuristic over registered capability/tool names, and the
  confirm box prints the exact rules it will append before `y`.
- **Counts are never fabricated** — MCP/extension headers show their real
  error (or `not loaded — Ctrl+R`) instead of zero counters when the
  registry read fails; empty lists distinguish "none registered" from
  "doesn't match the filter" from "unavailable".

## Launch matrix

| Command | Surface |
|---|---|
| `openrt` | Agent Mode (first screen) |
| `openrt agent` | Agent Mode |
| `openrt tui` | Workbench (unchanged) |
| `openrt --memory … agent` | Agent Mode, in-memory store (tests/CI) |
| `openrt --mock … agent` | Agent Mode with the scripted mock provider |
| `OPENRT_MOCK=1 openrt` | same, via environment |

## Status

`cargo fmt` · `cargo check --workspace --all-targets` (0 warnings) ·
`cargo test --workspace` **1674 passed / 0 failed** ·
`openrt doctor` **93 rows: 79 PASS / 1 WARN / 13 UNAVAILABLE / 0 FAIL** ·
`openrt audit verify` **335 records, clean** — the frozen QA baseline is
intact; the Agent TUI adds **91 tests** of its own on top of it (the
product-polish suite: wizard, model cards, plan-in-chat, session delete,
non-blocking terminal, list filter/paging, HF import, MCP manager,
extension browser, Security control center, dashboard strip, background
loads; the `/plan` failure contract
([PLAN_ERROR_HANDLING.md](./PLAN_ERROR_HANDLING.md)); the model-browser
sections + the OPENRT HF-surface battery
([MODEL_BROWSER.md](./MODEL_BROWSER.md)); and the production-harness
suite: 4-tier approvals, `SETTINGS` view + mode overlay, fuzzy palette,
global shortcuts, filter-aware lists, boot recovery card, panic restore,
mouse wheel, ASCII glyph mode, theme contract
([PRODUCTION_HARNESS.md](./PRODUCTION_HARNESS.md) ·
[KEYBOARD.md](./KEYBOARD.md) ·
[PERMISSIONS_UX.md](./PERMISSIONS_UX.md))).
