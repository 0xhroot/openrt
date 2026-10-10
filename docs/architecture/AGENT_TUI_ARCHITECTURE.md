# AGENT TUI — Architecture

**Status:** Accepted · **Date:** 2026-10-01 (production-harness revision)
**Parent:** [AGENT_TUI.md](./AGENT_TUI.md) · Acceptance:
[AGENT_TUI_ACCEPTANCE.md](./AGENT_TUI_ACCEPTANCE.md) · Keys:
[AGENT_TUI_KEYBINDINGS.md](./AGENT_TUI_KEYBINDINGS.md) ·
[KEYBOARD.md](./KEYBOARD.md) · Harness:
[PRODUCTION_HARNESS.md](./PRODUCTION_HARNESS.md)

## Design constraints

1. **One Runtime, two surfaces.** Agent Mode and the existing Workbench share
   one `Arc<Runtime>`, one event bus, one store, one gate. `Ctrl+W` swaps the
   renderer, never the process or the state.
2. **Backend frozen, one gate extension.** No second runtime/gate/store/
   policy authority anywhere. The only backend change of this revision is an
   **in-memory grant list on the one `GateExecutor`**
   (`grants: Mutex<Vec<GrantEntry>>`) consulted *inside* `resolve_ask` for
   the `AllowSession`/`AllowWorkspace` approval tiers — Deny still denies,
   first-match policy still wins; the TUI itself never evaluates policy.
   Everything else is consumption of existing APIs (`RuntimeBuilder::sink`
   / `::permission_resolver`, `spawn_task`, `run_workflow`,
   `list_sessions`, `store`, `modes`, `model_registry`, `registry`,
   `policy_document`, `recovery`, …).
3. **No fake activity.** No letter-by-letter streaming, no invented metrics,
   no synthetic tool cards. Every string on screen maps to runtime state or
   an event payload; unknown facts render `UNKNOWN` with the reason.
4. **Fail closed.** Approvals block input until answered; quitting answers
   Deny; `Enter` is deliberately **not** an approval grant; rendering is
   read-only (`&AgentApp`).
5. **80×24 responsive**, `unsafe_code = "forbid"`, warnings = 0.

## Module map (`crates/openrt-tui/src/agent/`)

| File | Responsibility |
|---|---|
| `mod.rs` | Module wiring, re-exports (`UiSink`, `TuiResolver`, …) |
| `sink.rs` | `UiSink` (bus → unbounded mpsc bridge), `TuiResolver` (`PermissionResolver` impl: inbox + `Notify` + oneshot reply; dropped UI ⇒ `NoResolver` deny) |
| `provider.rs` | `SwitchableProvider` — wraps `Arc<dyn ModelProvider>`, `set_model` fills `ModelRequest.model_id` per request; `is_switched()`/`selected()` feed the header; `probe_tool_support` forwards to the wrapped provider (honest yes/no/unknown) |
| `state.rs` | `AgentApp` — the single state object: stream/plan/tool cards, selections, overlays, `View::Settings`, `Overlay::Mode`, confirm boxes, `SESSION INTERRUPTED` boot card, `pump()` (event drain + approvals + join finalization + clock + mode cache), `apply_event()`, `send()/cancel()/command()`, `refresh_view()`, `shutdown()`; fuzzy `palette_matches`/`fuzzy_score`; Files `filtered_files()`/`filter_sel_reset()` |
| `views.rs` | Per-view loaders against real sources: files/preview/diff/search/git/terminal/artifacts/models/mcp/extensions/security/**settings**, bounded (`SKIP_DIRS`, item caps); Models view: `Ctrl+F` registry search + `native tools` capability probe |
| `input.rs` | Key handling precedence: `Ctrl+C` → pending approval (`1/2/3/4` + `y`/`n`/`Esc`/`Enter`) → workbench delegation → global shortcuts (`Ctrl+P/M/T/G/,/O`) → overlays → filter-aware view keys; `on_view_enter()` loads data on rail jumps |
| `render.rs` | Draw only (`&AgentApp`): header pills (mode/net/model always on), rail, chat + tool cards, right context column, **15 views** (incl. Settings), overlays (help/palette/model/sessions/preview/mode), approval cards; delegates Workbench to `crate::ui::draw` |
| `run.rs` | `run_agent()` event loop: `enter_terminal` → draw/pump/poll(100ms)/key **+ mouse wheel** → `shutdown()` → `leave_terminal` |
| `tests.rs` | **91 headless tests** (real Runtime + MemoryStore + mock provider) |

Supporting changes outside `agent/`:

- `crates/openrt-tui/src/run.rs` — `enter_terminal` / `leave_terminal` /
  `workbench_key` extracted as `pub(crate)` so both surfaces share terminal
  ownership and key dispatch; also: `EnableMouseCapture`/`DisableMouseCapture`
  on every exit path, `install_panic_restore` (chained hook: leave raw mode,
  restore screen/mouse, then the previous hook), `init_glyph_mode` +
  `g()`/`sanitize_text()` ASCII fallback, `mouse_wheel_to_key`.
- `crates/openrt-tui/src/theme.rs` — **the single palette** (BG/SURFACE/
  ACCENT/DIM/OK/BAD/WARN/INFO/BORDER/BORDER_ON) consumed by both renderers;
  the render contract is test-enforced (`theme_paints_black_surfaces_and_
  thin_borders`).
- `crates/openrt-tui/src/lib.rs` — exports `agent`, `run_agent`, `AgentApp`,
  `SwitchableProvider`, `theme`.
- `crates/openrt-cli/src/main.rs` — `Cli.cmd`, `Cmd::Agent`, `agent_builder()`,
  `cmd_agent()`; `cmd_recovery` (`scan`/`list`/`inspect`), `WorkflowCmd::Resume`.
- `crates/openrt-runtime/src/gate.rs` — the one extension: `GrantEntry` /
  `grants` consulted in `resolve_ask` for session/workspace grants.

## Data flow

```
Runtime event bus ──(UiSink: mpsc)──► rx ──┐
Gate (Ask) ──(TuiResolver inbox+Notify)──►  ├─► AgentApp::pump() every 100ms
clock / mode cache (30 ticks) ─────────────┘        │
                                                    ▼
                                            apply_event()  → stream/plan/
                                                             tool cards/tasks
input.rs handle_key() ────────────────────► AgentApp (mut)
                                                    │
                                                    ▼
                                    render::draw(frame, &AgentApp)
                                     (sync — no await in render)
```

### Event → view mapping (the honesty contract)

| Event | Stream effect |
|---|---|
| `PermissionRequested` (emitted unconditionally, pre-policy, args already redacted) | Opens a tool card with the gate's exact args |
| `PermissionDenied` / approval answered `n` | Closes card as **Denied** |
| `ToolInvocationStarted` | Fallback open (correlated sequentially by open-tool index) |
| `ToolInvocationCompleted` (no id/args in payload) | Closes card **Done {ok, error}** + duration |
| `TaskCompleted` | Renders the assistant text (only place real assistant output appears) |
| `TaskFailed` | Renders the error verbatim (e.g. model/provider failures) |
| `WorkflowStep*` / `WorkflowCompleted` | Plan checklist marks; completion note |
| `SafetyHalt` / `ResourceCheck` / `ErrorOccurred` | Error lines |
| `SessionCreated` + fold | `· session created` note; store round-trip |

### Approvals

`gate → PermissionResolver::resolve → TuiResolver inbox → drain() in pump →
approvals queue → render card → input.rs tier keys → oneshot reply → gate
proceeds/aborts`. `shutdown()` answers **Deny** to everything left — the TUI
can never leave the gate hanging.

Four tiers (the card's one language, see
[PERMISSIONS_UX.md](./PERMISSIONS_UX.md)):

| Key | Decision | Effect |
|---|---|---|
| `1` / `y` | `Allow` | this invocation only |
| `2` | `AllowSession` | `GateExecutor.grants` += exact (workspace, session, tool, scope); identical asks release without a card |
| `3` | `AllowWorkspace` | same grant keyed by workspace, across sessions |
| `4` / `n` / `Esc` / `Enter` | `Deny` | fail closed (`Enter` intentionally not a grant) |

Grants are **in-memory, process-lifetime, exact tool+scope matches**,
consulted only when the policy says `Ask` — they are not policy appends
(appended allow rules cannot beat an earlier first-match `Ask`), a `Deny`
rule still denies, and scope escalation is impossible because the grant
stores the exact scope the gate asked for. Cards also overlay every
non-Chat view while a request is pending.

### Permissions seen by the TUI

The default policy is deny-by-default (8 allow rules: `echo`, `system.info*`,
`filesystem.read*`, `fs.read`, `tool.run`, `tool.other`). The card shows the
Ask chain (`Ask` rule on the tool id, or a `required_caps` check after an
approved tool-id — cap denial still kills the call even when the tool-id was
approved). The TUI never evaluates policy itself; it only answers the gate.

### Model switching

`SwitchableProvider` is attached **pre-build** via
`RuntimeBuilder::with_provider`, so `runtime.provider` *is* the switcher —
one router, one registry, one provider instance. `set_model(Some(id))`
overrides `ModelRequest.model_id` per request; `default`/empty resets to the
provider default. `is_switched()` + `selected()` render the header pill.

### Terminal & workbench

`enter_terminal`/`leave_terminal` are the single raw-mode authority (RAII-ish
pair around the loop) and also own the alternate screen, **mouse capture**,
and **glyph mode** (ASCII decided once at entry: `OPENRT_ASCII` override →
locale probe → fail closed to ASCII). Workbench keys route through
`workbench_key`, and its `should_quit` propagates. Agent Mode never mutates
workbench state except via that shared handler and `workbench.refresh()` on
toggle. `install_panic_restore` chains a panic hook that unwinds terminal
state (raw mode, alternate screen, mouse) **before** any previous hook, so a
panic never leaves the operator's terminal dead. Mouse input maps to
synthetic arrow keys only (`mouse_wheel_to_key`); clicks are ignored by
design — no affordance exists that a click could fire by accident.

## Honesty decisions (do not regress)

- **No token streaming.** Providers are non-streaming (`generate` only);
  text appears at `TaskCompleted`.
- **`ctx ≈n%`** = `chars/4 ÷ capabilities.context_window`, labeled `≈`;
  `ctx n/a` when the window is unknown.
- **Render is sync.** Mode/network pills are cached fields refreshed in
  `pump()` (every 30 ticks, plus startup and view refresh) because
  `render::draw` cannot await.
- **Tool-card args** come from `PermissionRequested` (pre-policy,
  redacted by the gate) — the only event that carries args.
- **`native tools` line** (Models view) reflects only real
  `probe_tool_support` answers — `yes` / `no` / `unknown` /
  `unavailable(probe …)` — capability is never inferred from a model name.
  `Ctrl+F` in the Models view runs the real registry search (mode-gated);
  `Esc` restores the model-tag input line.
- **Single-task operator model** — `send()` while a task runs is refused;
  session switching while running is refused; `Esc` cancels.
- **Input line is one buffer**, context-dependent on `Enter` (Terminal runs
  the command, Search queries, Files opens, Models applies, Sessions
  switches, Tasks/Tools preview, otherwise sends). List navigation is
  filter-aware (selection is mapped through `filtered_files()`, so `Enter`
  can never open a hidden row); `Backspace` edits an open filter before it
  edits text; in Files on an empty line it goes up a directory.
- **Boot recovery is a note, not an action.** Non-clean `RecoveryRecord`s
  render a `SESSION INTERRUPTED` card naming the real task ids/status —
  nothing is auto-resumed; resuming is an explicit `openrt workflow resume`.
- **ASCII is decided once and fails closed.** No locale info ⇒ ASCII; the
  renderer draws through `g()`/`ascii_line` pairs and sanitizes text sinks
  (`OPENRT_ASCII` forces either way). Unknown state never becomes a fake
  glyph.
- **Mode changes go through the one `ModeManager`.** `Ctrl+O` opens the
  operator-only overlay; `Enter` proposes, `y` confirms;
  `modes.transition(actor, mode, reason)` audits it (idempotent). The TUI
  never flips network state itself — `ExecutionMode::network_allowed()` is
  the single source, and only Provisioning allows egress.

## Testing strategy

`agent/tests.rs` (**91 tests**) drives `AgentApp` exactly like the event
loop (same `pump`/`handle_key` paths) against a real `RuntimeBuilder` +
`MemoryStore` + `MockProvider`: transcript round-trips, gate tool-card
lifecycle, approval tiers (allow once/session/workspace/deny + shutdown
deny), plan compile/run through the real compiler, model switch visibility,
session switching, key/palette flows, no-fake-activity on a fresh app,
boot-recovery card, filter-aware lists, theme contract, ASCII glyph pairs,
mouse-wheel mapping, and a headless render sweep of every view/overlay at
80×24 and 120×40 (`ratatui` `TestBackend`). Interactive evidence (pty
smoke) is recorded in the acceptance doc.
