# OpenRT TUI interaction

This document covers how the keyboard, mouse, and overlays behave, and — more
importantly — the rules that keep a permission prompt from being answered by
accident.

## The rule that outranks everything else

**A pending permission owns the keyboard.** While `app.approvals` is non-empty:

* The composer is unfocused: no caret, default border.
* Every key is routed to the approval branch before any other handler runs.
* `Enter` **denies**. It never grants.

This routing happens at the top of the key handler, before view navigation, before
slash-command parsing, and before text input. There is no code path where a
stray keystroke reaches the composer while a permission is outstanding.

## Permission keys

| Key | Effect |
| --- | --- |
| `y` / `1` | Allow once |
| `2` | Allow for this session |
| `3` | Allow for this workspace |
| `d` | **Deny** |
| `n` / `4` | Deny |
| `Esc` | Deny |
| `Enter` | Deny |

The card renders exactly these labels, and `d` is genuinely wired — an earlier
build advertised `d Deny` while only `n`/`4` worked, which the test
`enter_and_every_deny_key_refuse` now pins down. A key shown to the operator but
not implemented is worse than an undocumented key.

The decision type is `openrt_permissions::PermissionDecision`, the same type the
runtime consumes. The TUI does not invent a permission model.

### What the card shows

Six fields, all required, all derived from the real `ApprovalRequest`:

| Field | Source |
| --- | --- |
| `ACTION` | `request.tool` |
| `TARGET` | Parsed from `request.args` (path, command, host, …) |
| `REASON` | `request.reason`, falling back to the policy's reason |
| `RISK` | Derived from tool + argument signals; drives the border tone |
| `SCOPE` | The grant's persistence scope |
| `CONSEQUENCES` | A plain-language statement of what allowing does |

Plus four graded grants and an explicit deny note. The dialog's height is derived
from the *wrapped* body, so it cannot clip a field it promised to show.

## Navigation

15 destinations in a fixed flat order (`View::ALL`), grouped for presentation
only:

| Group | Destinations |
| --- | --- |
| WORKSPACE | Chat, Files, Search, Tasks, Sessions, Agents |
| BUILD | Git, Terminal, Tools, Artifacts, Models |
| PLATFORM | Mcp, Extensions, Security |
| SYSTEM | Settings |

Grouping never changes `View::ALL` order, indices, or shortcut keys — it is
purely how the rail is drawn.

| Key | Action |
| --- | --- |
| `↑` / `↓` | Previous / next destination |
| `PgUp` / `PgDn` | Page through destinations |
| `Ctrl+W` | Toggle Workbench mode |
| `F1` or `?` | Help overlay (only when the composer is empty) |
| `Ctrl+K` | Command palette |
| `Ctrl+C` | Interrupt: resolve one layer (see below) |
| `Ctrl+Q` | Quit anyway — same teardown, unsent composer text is dropped |

Inside Settings, `Tab` / `BackTab` move between the `INFO` and `THEME` sections.
Settings input handling is matched *before* generic navigation, so arrowing
through the theme list moves the theme selection rather than the view.

## Overlays

Overlays are modal. The pattern is the same everywhere:

1. `Esc` closes.
2. The overlay captures keys; they do not fall through to the view beneath.
3. A click that lands inside the overlay but on no control is swallowed
   (`HitTarget::ModalScrim`) — it must never activate the button underneath.
4. Content that outgrows the terminal **pages** rather than clips. The help
   overlay's key table is larger than 80x24, so it scrolls.
5. Resizing with an overlay open keeps the overlay on top and re-clamps it.

An overlay never appears without a visible route out. `Esc`, and usually `q`,
always close it.

### Background results never take the keyboard

An overlay is not just a widget, it is a *keyboard claim*. Anything that lands
asynchronously must therefore know whether someone else is already holding it:

* A model switch stages its candidate, spawns a probe, and commits **only** if
  the probe still belongs to the staged switch. A cancelled probe clears the
  stage and says so instead of leaving "probing" on screen forever; a superseded
  probe can never commit its own candidate. See
  `a_cancelled_probe_does_not_strand_a_staged_switch`,
  `a_superseded_probe_cannot_commit_its_own_candidate`.
* Background data — a preview resolving, a model listing arriving — may open an
  overlay when the surface is empty. It may **not** replace one that is already
  open: a completed fetch is not more important than what the operator is
  halfway through reading. It reports that it is waiting instead.
  See `a_background_preview_does_not_steal_an_open_overlay`,
  `a_background_preview_still_opens_when_no_overlay_is_open`.
* A permission answer is fail-closed in both directions: an explicit allow
  releases the gate, a dropped resolver **denies**, and a reflexive `Enter` —
  the most likely accidental key — denies rather than granting. A one-shot allow
  does not widen to a request for a different path.
  See `a_reflexive_enter_denies_a_permission_instead_of_granting_it`,
  `an_allow_once_grant_does_not_widen_to_another_scope`.

The general principle: **no result may commit, replace, or open UI state
belonging to a request it does not belong to.** Staleness is checked before
commit, not after.

## Scrollback

`↑` / `↓` scroll one line, `PgUp` / `PgDn` a page, in the chat transcript, in
every list view, and in the help overlay. Scroll windows are derived from
`(total, cursor, height)` by `design::collections::window`, which is
invariant-tested to always contain the cursor, never over-read the collection,
and clamp a cursor left stale by a shrinking collection to the last row.

A filtered view puts both the cursor and the click target in *filtered*
coordinates, so a filter change can leave the cursor pointing past the end. That is
clamped on the way into the window rather than at paint time, and
`a_filtered_window_selects_and_hits_the_same_row` pins the two halves together: a
row the operator can select must be the same row a click on it resolves to.

## Slash commands

`/help` `/clear` `/new` `/sessions` `/plan <goal>` `/run` `/cancel` `/model`
`/setup` `/status` `/debug model-response` `/refresh` `/workbench` `/quit`

Commands are parsed only when the composer is focused. `Esc` clears or cancels.

## Mouse

Hit regions are registered **during render** into a `HitTargets` map and consumed
by the next input event. A region is honoured only when it lies inside its owning
element's `Rect`, so a stale region from a larger viewport can never trigger a
control that is no longer visible. A zero-area rect is never registered: a
control that was clipped away has no hit target, which is the correct behavior.

### A click is press, then release — not one activation

Activation happens on **release**, matching every desktop toolkit:

| Event | Effect |
| --- | --- |
| `Down` | Arm the target, record it, move focus to it. **Never activates.** |
| `Moved` | Update hover only. Never activates. |
| `Drag` | If the pointer leaves the armed target, disarm it. |
| `Up` | If the target is still armed, activate it — once. |

This is a correctness rule, not a style preference. The dispatcher once returned
the `Down` target into the shared activation match, so a single click fired
twice. For a permission grant that is not cosmetic: the press consumed the
approval, and the release then dispatched `2` against an app that no longer had
anything to approve. Both halves are pinned by
`a_press_alone_must_not_activate_the_target`,
`a_press_and_release_on_the_same_row_activates_exactly_once`,
`dragging_off_a_row_cancels_the_click`, and
`a_click_activates_a_permission_action_exactly_once`.

Selection and activation stay separate acts. A click on a row *selects* it;
running it still needs `Enter`.

### A mode gate comes before any hit test

`handle_click` returns early when `app.mode` is `Workbench`. The Agent's hit
registry is only rewritten when the Agent draws, so after `Ctrl+W` it still holds
the last Agent frame — including the rectangles of a permission card that is no
longer on screen. Dispatching against it would let a click answer an approval the
operator cannot see, which is exactly what a permission prompt exists to prevent.
Any pending press is cancelled on the switch so a release cannot fire it later.
Pinned by `mouse_input_must_not_drive_agent_state_while_in_workbench_mode`.

### What is clickable

Registration order is load-bearing. `HitTargets::hit` scans **newest-first**, so
a scrim registered after the buttons it is meant to sit behind will shadow every
one of them. The permission dialog once did exactly that: it drew its four
grants, then registered the scrim over the same rect, and because the click path
resolved the scrim first the entire modal was unclickable — three grants and a
Deny button, all dead, while looking completely normal on screen. The rule is
therefore: **register the scrim first, then the controls on top of it**, and
register the scrim over the whole region behind the dialog rather than over the
dialog rect. Pinned by
`a_permission_modal_resolves_clicks_to_its_buttons_not_the_scrim` and
`a_permission_modal_swallows_clicks_outside_its_buttons`.

| Surface | Hit target |
| --- | --- |
| Rail destination | `HitTarget::NavItem` |
| Collection row | `HitTarget::ListRow` |
| Permission grant / confirm | `HitTarget::Button` via the shared `ButtonGroup` |
| Palette row | `HitTarget::PaletteItem` |
| Header, chat hint chip | registered, activation is a no-op |
| Anywhere else under a dialog | `HitTarget::ModalScrim` — swallowed |

Palette rows are addressed by their position **in the match list**, which is what
was painted, so a click can never name a destination the current query does not
contain. Rows are registered only while they fall inside the dialog frame: the
dialog is content-sized, and a short terminal can show fewer matches than the
query has. Registering the invisible remainder would let a click below the frame
act on an entry nobody can see.

## Terminal lifecycle

Raw mode and the alternate screen are entered on start and **restored on exit,
including on panic**. Mouse capture and bracketed paste are enabled on entry and
disabled on every exit path — the normal one in `leave_terminal`, and the panic
hook in `install_panic_restore`, which restores *before* delegating to the
previous hook so the panic message still prints on a restored terminal.

`OPENRT_ASCII=1` (and the detected-monochrome case) select ASCII glyph fallbacks
through `run::g` rather than dropping glyphs.

## Paste

Bracketed paste reaches the composer as **one** event carrying the whole payload,
not as a keystroke per character.

| Rule | Why |
| --- | --- |
| Control bytes are dropped | The composer is a single line. A pasted `\r` would submit; a pasted escape could move the cursor or repaint the screen. |
| Only printable text is kept | Dropping the `ESC` byte is what neutralizes `\u{1b}[31m`; the leftover `[31m` is inert lettering, not a colour change. |
| A paste never submits | There is no code path from a paste to "send". |
| A paste is refused while a permission is outstanding | Text must not reach the composer behind a prompt — the same reason the keyboard routes to the approval branch first. |

Replaying a paste through the key handler would let a pasted newline send a
message the operator never finished typing. Pinned by
`a_paste_fills_the_composer_without_submitting`,
`a_paste_cannot_smuggle_a_submission_through_a_newline`,
`a_paste_cannot_carry_escape_sequences_into_the_composer`,
`a_paste_is_refused_while_a_permission_is_outstanding`, and
`a_paste_of_only_control_bytes_is_inert`.

Workbench enables bracketed paste too, so the escape sequences stay balanced on
enter and exit, but it has no text field: a paste arriving there is dropped
rather than redirected somewhere the operator did not target.

### Control bytes become separators

A control byte inside a paste is replaced with a space, not deleted. Deleting
it would fuse the words on either side (`"first line\r\nsecond line"` becoming
`"first linesecond line"`), which changes the operator's text without changing
its meaning visibly. Replacing it keeps the words separate and still guarantees
no control byte reaches the composer or the key handler.

Pinned by `pasted_control_bytes_are_filtered`,
`a_paste_cannot_carry_escape_sequences_as_keypresses`, and
`a_pasted_carriage_return_cannot_submit`.

### Pasting into a modal

A paste is dropped outright whenever `modal_open()` is true — a permission
outstanding, a confirmation, or any of the nine overlays. This is deliberately
stricter than routing the text into the overlay's own field: a paste must never
become an answer to a question the operator did not read, and the blanket rule
is easier to verify than a per-overlay exception. A future overlay that needs
paste-into-field must opt in explicitly.

### Ctrl+C is one layered decision

`Ctrl+C` resolves exactly one layer, from the inside out, and stops:

1. a pending permission is **denied** (never granted, never abandoned),
2. a destructive confirmation is **cancelled** and the mode is unchanged,
3. an open overlay or confirmation is **closed**,
4. a running task is **cancelled** and the app stays open,
5. a resource wait is **stopped** and the card is left in an inspectable state,
6. unsent composer text **blocks the quit** and the status bar says how to quit
   anyway,
7. otherwise the app quits.

The policy lives in `AgentApp::handle_interrupt`, and it clears armed pointer
state so a held mouse button cannot survive the interrupt.

### `Ctrl+Q` is the escape hatch the status bar promises

Step 6 blocks the quit and prints *"interrupted — Esc discards the composer,
Ctrl+Q quits anyway"*. That instruction is part of the contract: an operator
with unsent work must be able to keep it and still leave.

`Ctrl+Q` therefore runs the **same** teardown as `Ctrl+C` — a pending permission
is still denied, a confirmation still cancelled, a running task still
cancelled, a resource wait still stopped — and differs in exactly one respect:
unsent composer text is treated as disposable. That is the only layer that
cannot leave a task hung or a gate open; it is the operator's own typing, and
pressing an explicit "quit anyway" key is the consent to lose it. It is handled
before the modal layers so the advertised key cannot be swallowed by whichever
overlay happens to be open.

> **This was broken, and a real terminal is what proved it.** Step 6 named
> `Ctrl+Q`, no `Ctrl+Q` was implemented in any layer, and the help card claimed
> plain `Ctrl+C` "quits". A live PTY run showed: type text, `Ctrl+C`,
> `Ctrl+Q` → nothing happens; the only exit was `SIGKILL`, which left the
> terminal in the alternate screen with raw mode still on. Fixed in P10, with
> `the_quit_hint_the_composer_guard_prints_is_actually_honoured` pinning the
> promise. The help card now documents both keys accurately.

`Ctrl+Q` is verified on a real terminal as well as in tests — an A/B PTY run
hangs without it (`exit -9`, no `?1049l`) and exits cleanly with it
(`exit 0`, full terminal restore).

Pinned by `ctrl_c_denies_a_pending_permission_and_never_grants`,
`ctrl_c_cancels_a_destructive_confirmation`,
`ctrl_c_closes_a_modal_rather_than_quitting`,
`ctrl_c_refuses_to_quit_while_the_composer_holds_unsent_text`,
`ctrl_c_quits_when_there_is_nothing_to_stop`,
`ctrl_c_leaves_no_interaction_state_behind`,
`ctrl_c_in_the_workbench_is_handled_by_its_own_surface`, and
`the_quit_hint_the_composer_guard_prints_is_actually_honoured`.

## Sanitization

Everything that can carry untrusted bytes — tool output, model responses, file
paths, session ids, error text, permission arguments — passes through
`sanitize_field` before rendering. Control bytes are stripped so an escape
sequence in a tool result cannot move the cursor, recolor the screen, or emit a
bell. This is asserted directly: a test feeds `a\u{1b}[31mb\u{07}` through a
field and asserts neither byte reaches the screen.
