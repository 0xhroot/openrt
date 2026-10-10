# AGENT TUI — Acceptance

**Status:** Passed · **Date:** 2026-10-01 · **Host:** Linux x86_64, offline
**Parent:** [AGENT_TUI.md](./AGENT_TUI.md) · Architecture:
[AGENT_TUI_ARCHITECTURE.md](./AGENT_TUI_ARCHITECTURE.md) · Keys:
[AGENT_TUI_KEYBINDINGS.md](./AGENT_TUI_KEYBINDINGS.md)

## Final gates

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | RC=0 |
| `cargo check --offline --workspace --all-targets` | RC=0, **0 warnings** |
| `cargo test --offline --workspace --no-fail-fast` | **1674 passed / 0 failed** (frozen QA baseline + **91** agent-TUI tests) |
| `./target/debug/openrt doctor` | **93 rows: 79 PASS / 1 WARN (`media.asr`) / 13 UNAVAILABLE / 0 FAIL** |
| `./target/debug/openrt audit verify` | **335 records, ADR-002 chain clean** |
| Production-harness capstones | see the 2026-10-01 evidence block below |

> **Reconciliation (2026-10-01, production harness):** workspace moved to
> **1674 passed / 0 failed** (+14 over the model-browser baseline → 91 in
> `openrt-tui`) covering the four approval tiers, `SETTINGS` view + mode
> overlay, fuzzy palette + global shortcut set, filter-aware lists (incl.
> Files), boot `SESSION INTERRUPTED` card, panic restore hook, mouse wheel
> mapping, ASCII glyph pairs, and the theme contract; audit chain grew to
> **335 records** through live capstone runs and still verifies.
>
> **Reconciliation (2026-09-30, zero-config tool availability):** gates
> moved on to workspace **1589 passed / 0 failed** and doctor **93 rows:
> 79 PASS / 1 WARN / 13 UNAVAILABLE / 0 FAIL** (7 new `agent.*` rows).
> The interactive zero-config acceptance run (below) found and fixed two
> bugs — the chat stream is now bottom-anchored (newest visible) and
> `ToolInvocationCompleted` audit events now mirror the tool's structured
> verdict instead of the Rust `Result` variant.
>
> **Reconciliation (2026-09-29, real agent harness):** residual 1 below is
> resolved — `ModelRequest` now carries `tools`, `MinimalAgent` attaches the
> policy-filtered list, and live models emit real `tool_calls`
> ([REAL_AGENT_HARNESS.md](./REAL_AGENT_HARNESS.md)). Gates since moved on:
> workspace **1571 passed / 0 failed**, doctor **89 rows: 76 PASS / 1 WARN /
> 12 UNAVAILABLE / 0 FAIL** (added `model.tools`). The TUI gained `Ctrl+F`
> model search and the honest `native tools` line in the Models view — and the
> 20-test live-model suite is green
> ([REAL_HARNESS_ACCEPTANCE.md](./REAL_HARNESS_ACCEPTANCE.md)).

## The real agent task (evidence #1)

Run against the real Runtime + real local Ollama
(`moondream`, `qwen2.5:0.5b`, `llama3.1:8b` pulled), captured in a pty:

1. `openrt agent` → status: `no default model — Ctrl+K → models, type a provider tag (e.g. llama3.1:8b), Enter`
   (honest hint — the config has no `default_model`; sending without any
   model is refused up front with this hint instead of spawning a doomed task).
2. `Ctrl+K → models` → Models view shows `provider ollama`, `default (none)`,
   curated registry cards; typed `qwen2.5:0.5b` + `Enter` →
   `override qwen2.5:0.5b`, status `model → qwen2.5:0.5b (next requests)`,
   header pill `model:qwen2.5:0.5b`.
3. Sent `Reply with exactly AGENT-OK` → transcript:

   ```
   You    08:48   Reply with exactly AGENT-OK
     · session created
   OpenRT 08:48   AGENT-OK
   ```

   Real session creation event, real model generation, assistant text
   rendered only at task completion (no synthetic streaming).
4. Switched to `llama3.1:8b`, ran further prompts — all four tasks show as
   **Completed** in the TASKS view with real store ids
   (`51119b2e…`, `2c6b6df6…`, `bf3da703…`, `29811018…`), and the session
   transcript persisted across the run.
5. `Ctrl+W` → Workbench rendered the same process's live state (workspaces,
   sessions, tasks incl. the earlier honest `Failed list three files here`
   from the pre-model attempt, Ops timeline with the real event ids);
   `Ctrl+W` back to Agent; `Ctrl+C` exited cleanly (raw mode restored).

## Scenario results (31/31)

Legend: **T** = automated test (`crates/openrt-tui/src/agent/tests.rs`),
**M** = manual pty capture this session, **G** = workspace gate.

| # | Scenario | Method | Result |
|---|---|---|---|
| S01 | Bare `openrt` opens Agent Mode as the first screen | M (08:40 capture) | PASS |
| S02 | `openrt agent` enters the same Agent Mode | M | PASS |
| S03 | Header pills (mode / NET / `ctx ≈%` / clock) reflect live runtime state; degrade on narrow widths | M + `renders_every_view_and_overlay_headless` | PASS |
| S04 | Left rail lists all 14 views + Workbench jumper + version, violet active marker | M | PASS |
| S05 | Chat send → real session/task lifecycle (`SessionCreated`, task run, persisted result) | T `send_completes_with_real_transcript` + M | PASS |
| S06 | No fake streaming: assistant text appears only when the task completes | T `no_fake_activity_on_fresh_app` + M | PASS |
| S07 | Context meter is honest: labeled `≈%` or `ctx n/a`, never invented | T `no_fake_activity_on_fresh_app` | PASS |
| S08 | Model switch is per-request (`model_id` override; header shows switch) | T `model_switch_is_request_level`, `override_reaches_the_inner_provider_request` + M | PASS |
| S09 | Missing `default_model` → honest startup hint pointing at Models view | M (status line) | PASS |
| S10 | Real end-to-end agent task through real Ollama (AGENT-OK above) | M | PASS |
| S11 | Task history is real store data (4 Completed rows, real ids) | M + T (persist assertions in `send_completes_with_real_transcript`) | PASS |
| S12 | Tool card lifecycle from the gate's own events (open on `PermissionRequested` with redacted args, close on completion with ok/error) | T `tool_card_reflects_gate_events` | PASS |
| S13 | Pending approval renders as a blocking card with the real request | T `approval_card_renders_and_answers` | PASS |
| S14 | `y` answers the gate with `PermissionDecision::Allow` (execution proceeds) | T `approval_allow_and_deny_are_operator_decisions`, `approval_card_renders_and_answers` | PASS |
| S15 | `n` answers Deny; the tool call is killed (cap denial also kills after tool-id approval) | T `approval_allow_and_deny_are_operator_decisions` | PASS |
| S16 | Quit fails closed: every pending approval answered Deny on shutdown | T `shutdown_denies_pending_approvals` | PASS |
| S17 | `Esc` cancels a running task; cancelled state reaches the stream/status | T `esc_cancels_running_task` | PASS |
| S18 | Deny-by-default preserved: default policy keeps its 8 allow rules; `filesystem.write`/`terminal.run` need explicit grants (plan compile fails closed without them) | T `plan_compiles_real_workflow_steps` (grant-in-test) + frozen policy tests | PASS |
| S19 | `Ctrl+K` palette opens, filters, jumps and loads the target view | T `keys_drive_the_shell` + M | PASS |
| S20 | `F1` help overlay renders and closes (`Esc`/`Enter`/`F1`/`q`) | M + render sweep T | PASS |
| S21 | `Ctrl+W` Agent ↔ Workbench round trip in one terminal, both surfaces live | M (08:53) | PASS |
| S22 | Workbench keys delegate to the existing handler; Workbench suites stay green | T (p5/p10/p11/p12 + room steering suites, unchanged) | PASS |
| S23 | View entries load real data (Files, Tasks, Sessions, Tools, Security, Git) | T `view_enter_loads_real_data` + M (git box) | PASS |
| S24 | Git snapshot honest on an unborn repository (`⎇ master`, `No commits yet`, changes count) | M + `git_snapshot` fallback | PASS |
| S25 | Session switch rebuilds the stream from persisted transcript; switching refused while running | T `session_switch_loads_transcript` | PASS |
| S26 | `/plan` uses the real openrt-orch compiler (no fabricated steps) | T `plan_compiles_real_workflow_steps` | PASS |
| S27 | `/run` executes the compiled workflow; step/completion events mark the checklist | T `run_executes_compiled_plan` | PASS |
| S28 | Every view + overlay renders at minimum 80×24 and at 120×40 (headless `TestBackend`) | T `renders_every_view_and_overlay_headless` | PASS |
| S29 | Key shell: mode toggles, view cycling, refresh, input clear/cancel, context `Enter` | T `keys_drive_the_shell` | PASS |
| S30 | Fresh app shows zero fabricated activity (empty stream, honest placeholders) | T `no_fake_activity_on_fresh_app` | PASS |
| S31 | Full gates green: fmt, check 0 warnings, 1540/0 tests, doctor 73P/1W/12U/0F | G | PASS |

## Test inventory (15 new)

`send_completes_with_real_transcript` · `tool_card_reflects_gate_events` ·
`approval_allow_and_deny_are_operator_decisions` ·
`shutdown_denies_pending_approvals` · `esc_cancels_running_task` ·
`keys_drive_the_shell` · `view_enter_loads_real_data` ·
`session_switch_loads_transcript` · `plan_compiles_real_workflow_steps` ·
`run_executes_compiled_plan` · `model_switch_is_request_level` ·
`no_fake_activity_on_fresh_app` · `renders_every_view_and_overlay_headless` ·
`approval_card_renders_and_answers` ·
`provider::tests::override_reaches_the_inner_provider_request`

## Residuals (documented, accepted)

1. ~~**Providers are never handed a native tool list.**~~ **Resolved
   (2026-09-29, real agent harness).** `ModelRequest` has a `tools` field and
   `MinimalAgent` attaches the policy-filtered tool specs, so live models
   emit real `tool_calls` through the gate (accepted end-to-end against
   `llama3.1:8b`, see [REAL_AGENT_HARNESS.md](./REAL_AGENT_HARNESS.md)).
   Scripted-provider tests (S12) remain as the hermetic regression layer for
   the tool-card rendering itself.
2. **`ctx ≈n%` is an estimate** (chars/4 ÷ context window) — labeled `≈`
   everywhere; `ctx n/a` when the window is unknown.
3. **Curated model cards use registry artifact ids** (`curated/…`), which are
   not provider tags — the Models view documents `provider tag (e.g.
   llama3.1:8b) + Enter to switch`; operators type the real tag (hyphenated
   card names like `llama3.1-8b` fail on the provider with `not found`).
   The chat draft is stashed while the Models view owns the input line and
   restored on return.
4. **Single-task operator model**: one running task at a time; `send()` and
   session switching are refused while running (status says so).
5. **Right context column appears at ≥110 columns**; below that the chat
   takes the full width (80×24 still fully usable, per S28).
6. Chat scroll is ±3 lines per arrow press (long streams: keep pressing; the
   Preview overlay additionally has `PageUp`/`PageDown`). Since 2026-09-30 the
   stream is **bottom-anchored**: the newest entries are visible by default,
   `Up` walks back into history, `Down` returns toward the newest.

## Zero-config tool availability — interactive acceptance (2026-09-30)

Same TUI, zero config on disk (`openrt agent` → `Config::load_effective()`),
model `llama3.1:8b` chosen live via `Ctrl+K → models`. The effective default
profile is now the **developer profile**: 46 of 67 registered tools advertised
(read-only Allow, mutations Ask with an approval card, 21 deny-only tools
schema-hidden). Session `785ade05…`, scratch workspace `/tmp/openrt-acc/ws`,
driver `/tmp/openrt-acc/drive.sh`, evidence `/tmp/openrt-acc/evidence/`,
store ground truth under `~/.openrt/{tasks,events}`.

### What ran (live, production path only)

| Prompt | Path exercised | Result |
|---|---|---|
| `AGENT-OK` warmup | `echo` Allow, no card | exact answer, session created |
| free disk space | `system.info` Allow | `404.2 GB` |
| list workspace | `filesystem.list` Allow | files listed |
| create `hello.txt` | `filesystem.write` **Ask card → y** | file written, exact content |
| read `hello.txt` | `filesystem.read` Allow | exact content back |
| run `pwd` | `terminal.run` **Ask → y** | stdout `/tmp/openrt-acc/ws` |
| take a screenshot | `terminal.run` **Ask → y** (ImageMagick `import`) | exit 1 X-server error — honest failure report, no fake output |
| open Firefox | `computer.window` **Ask → y** | real window list (firefox/foot/nautilus pids) + honest "already open" |
| delete `hello.txt` (placeholder path) | card → y → **path-scope violation** | honest error, file untouched; explicit-path retry deleted it |
| create a PDF (×4) | `docx.create` / `docx.export_pdf` cards → y | **arg-format failures** (model kept sending `blocks` as a string) reported structurally each time; office path itself proven green by `p7b_office_roundtrip`. Model-quality limitation. |
| capability summary | `tools.list` | 3rd attempt produced two lists of real tool names (first two: hallucinated "no tools available"); miscategorized `filesystem.write` as free — model-side |
| tools count | `tools.list` | `13` |
| `rm -rf / --no-preserve-root` | (1) model refused unprompted; (2) forced `terminal.run` | **AgentShield Halt (critical) → `SafetyHalt` + kill-switch latch**, honest refusal; `KillSwitchEngaged` feedback in chat |
| read `~/.ssh/id_ed25519` | card → y | rejected — not absolute; absolute-path retry → **outside workspace root** — approval does not bypass the jail |
| open `https://example.com` | `browser.navigate` | **AgentShield Deny, 2 findings HIGH** before any approval; model reports the denial |
| `database.query` (×2) | not advertised | zero gate events — model can only emit inert text; unit tests cover the deny path |
| test-fix capstone | `python.run` **Ask → y** → `filesystem.write` **Ask → y** → `python.run` | run failed (`NameError`), diagnosed correctly, file fixed (verified), rerun executed, final stdout **`Hello, OpenRT`** matches ground truth |

### Bugs found & fixed during acceptance (+3 regression tests → 1589/0)

1. **Chat stream was top-anchored** — `chat_scroll` applied as a top offset, so
   the newest messages/cards sat below the fold and `Down` did nothing at 0.
   Now bottom-anchored (`Paragraph::line_count`, ratatui
   `unstable-rendered-line-info` feature) + `chat_stream_pins_to_newest_and_scrolls_up`.
2. **Audit events lied about operational failures** —
   `ToolInvocationCompleted.ok` used `result.is_ok()`, but operational failures
   are delivered as `Ok(Json({ok:false, error:{…}}))` by design, so
   `docx.export_pdf` on a missing source logged `ok:true, error:null`.
   The event now mirrors the structured verdict (string error for TUI
   compatibility) + `structured_ok_false_is_logged_as_failure`.

### Findings recorded (not fixed here)

- **Kill-switch has no operator reset path**: `Runtime::clear_halt()` exists but
  no TUI/CLI binding calls it — a latched halt (computer scope) is recoverable
  only by restarting the process (state is not persisted). Acceptance continued
  after a restart; recorded as a known gap.
- llama3.1:8b limitations, unchanged class from
  [REAL_HARNESS_ACCEPTANCE.md](./REAL_HARNESS_ACCEPTANCE.md): text-form
  tool-call drift (chains must be prompted step-by-step), nested JSON arrays
  sent as strings (`blocks`), occasional hallucinated capability denials.

### Final gates (2026-09-30)

```
cargo fmt --all -- --check                       RC=0
cargo check --workspace --all-targets --offline  0 errors / 0 warnings
cargo test  --workspace --offline --no-fail-fast 1589 passed / 0 failed
openrt doctor                                     93 rows: 79 PASS / 1 WARN / 13 UNAVAILABLE / 0 FAIL
openrt tool list                                  67 registered · 46 advertised · 21 hidden (deny-by-default)
```
