# Phase 1 TUI redesign — acceptance record

Scope: the design-system migration of the OpenRT agent TUI (six themes,
responsive layout, reusable components, permission presentation, accessibility),
plus the production artifact cleanup that closes the phase.

This document records **what was verified and how**. Anything not verified is
listed as such rather than implied to be done.

## Verification summary

| Suite | Result |
| --- | --- |
| `cargo check --workspace --all-targets --offline` | clean, zero warnings |
| `cargo fmt --all --check` | clean |
| `cargo test -p openrt-tui --offline --lib` | **678 passed, 0 failed, 1 ignored** |
| `cargo test --workspace --offline` | **2362 passed, 0 failed, 1 ignored** (189 test binaries, exit 0) |
| `openrt doctor` | unchanged — 0 fail / 1 warn (pre-existing, unrelated) |
| live PTY run | `exit 0`, full terminal restore (see Phase 10) |

These numbers were taken **immediately before** the artifact cleanup, which
removes `target/` and is not followed by a rebuild. Re-running any suite
afterwards requires a full rebuild, by design — that is what makes the
"no rebuild" claim about the measurement meaningful.

## Accepted requirements

### Six themes, resolved through a single owner

* `ThemeKind` (6) → `Palette` (33 slots) → `Tokens` → components → views.
* `App.theme` is the only committed theme owner, shared by the Agent renderer,
  the Workbench renderer, and all seven overlays.
* Verified: `assert_eq!(a.tokens(), b.tokens())` after switching, in both modes
  and every overlay.
* Verified: all 6 palettes are complete and produce distinct `Tokens`.
* Verified: save → reload round-trip; unwritable path surfaces a status rather
  than failing silently; `Esc` from preview restores the committed theme.
* Verified: a broken/unknown/malformed theme file falls back to Pitch Black
  instead of blocking startup.

### No hard-coded color outside the palette

* Verified by `no_renderer_uses_the_pitch_black_compat_aliases`, a source-level
  guard over `agent/render.rs` and `ui.rs`. The frozen compat constants
  (`theme::BG`, `theme::ACCENT`, …) are `#[deprecated]` and unused.
* **Non-vacuity checked**: injecting `crate::theme::BG` into a renderer made the
  guard fail with the exact file and line. Verified separately for the palette
  suite by injecting `Color::Magenta`.

### Permission presentation

* Six required fields (`ACTION`, `TARGET`, `REASON`, `RISK`, `SCOPE`,
  `CONSEQUENCES`) and four graded grants, derived from the real
  `openrt_permissions::ApprovalRequest`.
* Height derived from the **wrapped** body, so a promised field cannot be
  clipped.
* Every deny key verified: `d`, `n`, `N`, `4`, `Esc`, and `Enter` all deny —
  `Enter` never grants.
* Verified: the card is reachable in **all 15 views**, not just Chat.
* Verified: survives `80x24` by becoming a modal rather than showing a partial
  card.
* Two real defects were found and fixed during this work: the overlay was sized
  in *percentages* through a legacy `centered()` helper (so it rendered 6 rows
  regardless of content), and `d` was advertised on the card but not wired.

### Responsive layout

* Verified across `80x24`, `100x30`, `120x40`, `160x50`, `180x50`, plus the
  degenerate `1x1`, `1x40`, `2x2`, `3x1`, `40x2`, `200x60` — no panics.
* Rail: drops headings before destinations, always keeps the active
  destination, reports `↑N`/`↓N`, keeps WORKBENCH reachable.
* Meters: collapse to labels rather than clipping a number mid-digits.
* Fields: caret always visible; character-based truncation; `…` on scroll.
* Nothing indexes a viewport dimension; all arithmetic uses
  `tokens::saturating_sub`.

### Accessibility

* Status is never carried by color alone — each state has a glyph and a word.
* ASCII fallback via `OPENRT_ASCII` / locale detection, failing closed to ASCII
  with no locale info. Every glyph has a meaningful ASCII form; the toast
  markers are asserted mutually distinct in both modes.
* Control bytes are stripped before rendering; asserted by feeding
  `a\u{1b}[31mb\u{07}` through a field.
* **Two real palette collisions were found by the acceptance test and fixed**:
  `High Contrast` had `danger` and `blocked` both `rgb(255,0,0)`, and
  `Monochrome` had `success` and `blocked` both 240-gray. Either made "the host
  refused this" indistinguishable from "this failed".

### Resource and security posture preserved

* Resource admission behavior is unchanged: real numbers, bounded retry,
  `ResourceBlocked`, gate-bound override, structured checks, fail-closed audit,
  doctor rows, actionable TUI states, retained prompts.
* `openrt doctor` output is unchanged.

## Phase 4 — design-system adoption

Follow-on to the phase above, recorded here because it changed how the Agent
renderer draws rather than what it can do.

### Verification summary

| Suite | Result |
| --- | --- |
| `cargo fmt --all --check` | clean |
| `cargo check --workspace --all-targets --offline` | clean, zero warnings |
| `cargo test -p openrt-tui --offline --lib` | **540 passed, 0 failed** |
| `cargo test --workspace --offline` | **2213 passed, 0 failed** |
| `openrt doctor` | unchanged — 0 fail / 1 warn (pre-existing: no whisper on PATH) |

### What changed

* **One frame.** `render_boxed` was the last framed surface in Agent Mode
  building its own `Block` from a raw border `Color`, which put the context
  column on `surfaces.surface` while every other main panel sat on
  `surfaces.bg`. It now delegates to `widgets::render_panel`, so a page reads as
  one plane.
* **One status bar, one rail, one split.** Workbench drew its own header,
  footer, and vertical split; both modes now draw through `AppShell`. The
  Agent/Workbench difference is *content*, and that stayed in the caller — only
  the drawing was unified.
* **Semantics instead of colours.** `panel_with_footer` and the local `pill`
  were removed rather than migrated: a `&str` footer cannot express key
  bindings, and the dashboard strip's badge contract now belongs to the shared
  `Badge`. Callers take `BadgeTone`, never a raw `Color`.
* **A fitting policy that is stated, not assumed.** `render_badges` used to
  render *nothing* when the badge set did not fit. The Chat dashboard strip
  depends on the opposite — it is ordered safety-first, so a narrow terminal
  must drop the session label and never the halt reason. Fit, anchor, and
  emphasis are now `BadgeRow`, with a test at 40 columns.
* **Lost information recovered.** The context panel's `path` row clipped from
  the right and lost the filename (`~/Projects/openrt/crates/openrt-`); it now
  truncates from the front (`…crates/openrt-tui`).
* **Primitive inventory.** Every shared primitive has an explicit
  `ADOPT`/`EXTEND`/`KEEP AS INFRASTRUCTURE`/`REMOVE` decision with a reason —
  see `docs/tui-design-system.md`. Two primitives were removed for having no
  caller and no honest future caller (`section_header`, `panel_with_footer`);
  `TreeView` was kept, unused, with the reason recorded rather than being
  deleted or forced onto `Files`.

### Interaction contracts re-verified, not assumed

The existing suites remain the authority and were not weakened: `Enter`/`Esc`
deny a permission, pointer press arms and release activates over the same
target, resource grants flow through `Runtime::grant_resource_override`, and
the Workbench panel-scoped hit-target model is unchanged. New coverage this
phase: the halt badge survives a 40-column strip, the migrated badge and card
surfaces still render their own content at all five acceptance sizes, and the
inline approval card reserves its buttons and deny note so `CONSEQUENCES` and
"Enter never grants" cannot be truncated.

### Not validated

Mouse handling and terminal restore on panic remain unit-tested and reasoned
about, **NOT VALIDATED IN THIS ENVIRONMENT**. Resize-with-overlay was later
validated on a real PTY in Phase 10. Contrast is still asserted for distinctness,
not measured against a WCAG ratio.

## Phase 5 — Workbench as a first-class surface

Workbench stopped being the mode with its own hand-drawn chrome and became the
first consumer of the whole shared surface set: one shell, one panel, one list,
one state, one tone vocabulary, one hit-target model.

### Verification summary

| Suite | Result |
| --- | --- |
| `cargo fmt --all --check` | clean |
| `cargo check --workspace --all-targets --offline` | clean, zero warnings |
| `cargo test -p openrt-tui --offline --lib` | **562 passed, 0 failed** |
| `cargo test --workspace --offline` | **2235 passed, 0 failed** |
| `openrt doctor` | unchanged — 0 fail / 1 warn (pre-existing: no whisper on PATH) |

### What changed

* **The shell owns the whole row.** Workbench asked `AppShell` for a rail it
  never drew, which reserved a 16-column gutter and pushed the right-hand panel
  off-screen at 80 columns. `ShellLayout::build` now takes `has_rail`, so a
  surface without a rail is given the width it is actually using.
* **Four panels, one implementation.** `layout::panel_grid` balances columns and
  rows for the panel *count* rather than for a fixed 2×2, so 80×24 gets the 2×2
  the acceptance matrix asks for without stretching a panel to consume slack.
* **Rows are shared rows.** `draw_list_rows_targeted` is the same renderer as
  `draw_list_rows` with a per-row callback, so a Workbench row and an Agent list
  row cannot drift apart. Hit registration order is deliberate: the panel claims
  its region first, then each painted row claims its own cell, so the more
  specific target wins without a second lookup.
* **Empty, loading, and failed are three states.** `PanelLoad` records the
  outcome of every panel's load separately. `refresh()` still refreshes every
  panel and still returns the first error, but a failed panel now says `FAILED`
  in its own title instead of being indistinguishable from a panel with no rows.
  The state label comes from the shared `StateKind::label()`, so the vocabulary
  cannot drift per surface.
* **Selection cannot be stale.** Selections are `Option<usize>` rather than a
  bare index. `selectable_rows` reports rows that exist *and* can be opened —
  read-only panels sink into a selection of `None` instead of pretending — and
  `reconcile_selection` runs after every load, so a shrinking list cannot leave
  the cursor pointing past the end.
* **Focus is four panels, and it is visible.** `Focus::Status` is gone; the ring
  is Workspaces → Sessions → Tasks → Ops with `Shift+Tab` reversing it. The
  focused panel shows a `▸` caret in its title, so focus is not carried by color
  alone.
* **Mode is stated once.** The header carried `mode=sovereign` *and* a
  `! SOVEREIGN` badge — one fact, two renderings, and the duplication was what
  overflowed the 80-column header onto a second line and cut the badge to
  `SOVERE`/`IGN`. The badge is now the only mode rendering, and
  `App::header_line_for` picks the spelled-out or compact subtitle by width so
  the `Ctrl+W agent` route out is never the thing that gets cropped.
* **Truncation keeps the informative end.** The shared row renderer truncates
  from the right because rows usually end in a path. A task prompt and a room id
  are the opposite — the head identifies them — so those go through
  `head_excerpt`/`head_text`, and a task's status moved to the right-aligned
  detail column after a narrow panel was observed dropping the status prefix
  first.
* **Returning to Workbench is a transition, not an event.** Palette and
  `/workbench` used to flip the mode and leave the other surface's pressed state
  and hit targets in place. `AgentApp::enter_workbench`/`enter_agent` now own
  the whole switch: clear interaction state, clear stale hits, refresh, set the
  status. Both routes go through it.

### Interaction contracts re-verified, not assumed

The permission, pointer, and resource suites were not weakened and remain the
authority: `Enter`/`Esc` deny, press arms and release activates over the same
target only, movement and non-left clicks do not activate, and the resource grant
still flows `Runtime::grant_resource_override(reason, operator)` → policy → audit
→ one-shot arm. New P5 coverage (18 tests): a row click resolves to the row and
not the panel, no panel registers a row it does not have, an empty panel
registers no rows, a selection survives a shrinking list and never lands on a
read-only row, panel geometry never overlaps or collapses at any acceptance size,
every panel state renders in all six themes, every key map is reachable, and
focus/selection/wheel behave per panel.

### Not validated

Unchanged from the phases above *at the time this was written*: real mouse
delivery and terminal restore after an actual panic in a live terminal remain
**NOT VALIDATED IN THIS ENVIRONMENT**. Physical resize with an overlay open **was**
later validated in Phase 10 on a real PTY. See Phase 10 for what a live terminal
did and did not prove.

## Phase 6 — responsive geometry acceptance

Phase 1 proved that every surface *renders* at every size. Phase 6 asks the
harder question: is the geometry it produced actually usable — do the clickable
rectangles exist, do they contain what they claim to contain, and do they stay
inside the window after a resize.

### Verification summary

| Suite | Result |
| --- | --- |
| `cargo fmt --all --check` | clean |
| `cargo check --workspace --all-targets --offline` | clean, zero warnings |
| `cargo test -p openrt-tui --offline --lib` | **578 passed, 0 failed, 1 ignored** |
| `cargo test --workspace --offline` | **2261 passed, 0 failed, 1 ignored** |
| `openrt doctor` | unchanged behaviour — 0 fail / 2 warn, both environmental (Provisioning is not Sovereign; no whisper on PATH) |

### The method

A test that hard-codes coordinates rots the first time a layout changes, and a
test that re-implements the layout proves nothing about the layout. So the Phase
6 harness reads the geometry the renderer *actually* used:

* `Frame::render` draws through the real `crate::agent::render::draw` (or
  `crate::ui::draw` for Workbench) into a `TestBackend`, then reads back the
  buffer **and** the `HitTargets` registry that the same draw call filled. The
  rect and the pixels under it are therefore audited together, and there is no
  second "test layout" anywhere.
* Shell rectangles come from the same `ShellLayout::build(area, tokens,
  wants_context_panel(), true)` call the renderer makes.

### What the invariants are, and why each one earns its place

| Invariant | Failure it catches |
| --- | --- |
| Every registered rect lies inside the window | A control the operator can see in the model but cannot click, because its rect ends past the edge. |
| Every registered rect (except `ModalScrim`, `Header`) has non-blank cells under it | Ghost controls — a hit region with nothing painted in it. |
| Two distinct buttons never share a cell | `HitTargets` is topmost-wins, so an overlap does not fail loudly; the first click silently activates the *wrong* button. On a permission dialog that is a security failure. |
| The five shell regions are pairwise disjoint and inside the window | Two surfaces painting over each other, which no "does it contain" assertion can see. |
| A resize never increases the region count, and every region fits the new window | The stale-registry bug: a control painted in a 200-column layout stays armed in an 80-column one, so clicking empty space fires something. |
| `WorkbenchRow` rects lie inside a `WorkbenchPanel` rect | A row clickable where nothing is drawn, resolving against a panel the operator never touched — exactly what the `PanelId` split exists to make impossible. |
| All four `GRANT_BUTTONS` exist, are painted, and are mutually disjoint, from every view at every size | A grant action collapsing or merging. `Enter` denies, so a lost button is the difference between a safe default and an accidental grant. |
| The blocked-admission headline and its branch's recovery affordances survive 80×24 | The shell's precedence is body > header > status, so a cramped terminal is precisely where a resource card gets squeezed to a headline with no way out. |
| A path keeps its tail, a message keeps its head, and a cut is marked | `…/file.rs` is identifiable; `file.rs` cut from the right is not. An unmarked cut reads as the whole name. |
| An empty collection still says which state it is in | "no sessions yet" and "sessions could not be loaded" call for opposite responses. |

### Two real defects the matrix found

1. **An absent context panel was anchored outside the frame.**
   `ShellLayout::build` computed `context.x = body.x + body_w + gutter`
   unconditionally, so at 80×24 — where the compact layout refuses the context
   column — the returned rect was `x: 82` in an 80-column window. It is
   zero-area and never painted, so no amount of "does the text render" testing
   would have caught it; but `context` is public, and any caller measuring
   `x + width` on it reads past the edge. An absent panel is now anchored at the
   body's right edge, with the gutter applied only between two real columns.

2. **The bounded-memory guard was measuring the test harness.**
   `registering_a_large_artifact_costs_bounded_memory` compares peak-RSS growth
   against the artifact size, and `VmHWM` is a *process-wide monotonic* mark:
   under the parallel test harness any concurrently rendering test moves it, so
   the guard reported the harness rather than the code under test. The matrix
   above made that latent flakiness fire. Two changes: the memory tests and every
   test that renders frames in bulk now share one `heavy_frames()` gate, and
   `peak_rss_bytes` carries a comment saying why. The cost is honest wall clock
   — the TUI library suite went from ~33s to ~85s — which is the price of a
   memory bound that is actually about the artifact. A counting global allocator
   would be a better mechanism and is not available: the workspace sets
   `unsafe_code = "forbid"`.

### Not validated

Unchanged: real mouse delivery and terminal restore after an actual panic in a
live terminal remain **NOT VALIDATED IN THIS ENVIRONMENT**. Physical resize while
an overlay is open was **later validated** on a real PTY in Phase 10 — a full
repaint at 80×24 and back with the help overlay open, no panic. Resize *behaviour* is covered
by re-rendering at a new size and auditing the new frame's registry, which is
what the stale-target tests do; it is not a physical resize event.

## Phase 7 — accessibility hardening

Phase 6 asked whether the geometry is usable. Phase 7 asks whether the *content*
is: whether a state is identifiable without color, whether a string is measured in
the units a terminal actually uses, and whether the six themes hold numeric
contrast figures or merely pleasant-looking ones.

### Verification summary

| Suite | Result |
| --- | --- |
| `cargo fmt --all --check` | clean |
| `cargo check --workspace --all-targets --offline` | clean, zero warnings |
| `cargo test -p openrt-tui --offline --lib` | **678 passed, 0 failed, 1 ignored** (current; see Phases 8–10) |
| `cargo test --workspace --offline` | **2317 passed, 0 failed, 1 ignored** |
| `cargo test -p openrt-tui --offline measured_contrast -- --nocapture` | 6 themes × 9 pairs measured and printed |

### Defects found and fixed

1. **Width was counted in the wrong unit, in both directions.** Bytes and
   characters were both in use. CJK is 1 char / 2 columns / 3 bytes; an accented
   character is 1 / 1 / 2; a combining mark is 1 / 0 / 2. Six columns of `日本語`
   went into a 5-column frame and pushed panel borders out of alignment, and the
   composer's caret drifted off its field. `design::text` is now the only module
   that answers "how wide is this", via `unicode-width` — the crate ratatui
   renders with. Truncation is asserted on display columns for every sample at
   every width in `[0, 23]` in both glyph modes.

2. **The glyph inventory and the sanitizer disagreed, so four glyphs reached
   ASCII terminals raw.** `▾` (a collapsed branch), `⎇` (the Alt key), `⋯`, and
   all four braille spinner phases were painted without an entry — a collapsed
   branch looked identical to an expanded one, and Alt rendered as a box. The
   inventory is now one table with three guards: `g()`'s inline argument is
   checked against it, every entry is proven to degrade, and a source scan
   rejects any functional glyph painted in production that has no entry.
   Punctuation is explicitly passed through and operator prose is never
   rewritten.

3. **`Blocked` and `Failed` shared a marker.** `BadgeTone::Danger` and
   `BadgeTone::Blocked` both resolved to `glyph("✗", "x")`, so a denied request
   and a missing one were identical on a monochrome terminal. Badges now take
   their markers from `design::semantics::Semantics` (`Failed` → `✗`/`[X]`,
   `Blocked` → `■`/`#`), and a test in `widgets` pins the distinction at the
   widget rather than only in the table, so a future tone that re-merges them
   fails there. The three resource states also stay distinct from each other,
   because each prescribes a different next action.

   *Scope note, stated so this is not read as more than it is:* this fixed the
   shared **marker**. `Semantics` is the canonical table for badges, but
   collection-state labels (`StateKind::label()`: `LOADING`, `NOTHING HERE`,
   `FAILED`, `INFO`) are still spelled per surface. That is defensible —
   an empty collection wants an affordance, not a status word — but the words are
   not yet drawn from one table, and this document does not claim they are.

4. **`border_active` failed 3:1 in all four normal themes and in Monochrome**
   (worst case 1.88:1, Midnight). It is the hover and
   selected-but-not-focused indicator, so it carries state and cannot be treated
   as chrome. Raised, preserving hue ratio, to 3.10–3.14.

5. **`border_focus` measured 4.15:1 in Pitch Black**, under the 4.5:1 floor for
   the one element that must be findable by eye. Lightened to 4.61:1.

6. **The border test checked the wrong plane.** It measured against `bg` only, so
   a border that cleared the ratio on the root and vanished on a raised card
   passed. Now measured against the worst of `bg`/`surface`/`elevated`.

7. **`HighContrast`'s selected row was nearly invisible, and that was by design.**
   `text_primary` on `surfaces.selected` measures 1.07:1. It survives only because
   the list inverts to `text_inverse` on `accent.primary`. A test now reads the
   painted cells back out of the buffer and checks each text cell against its
   theme's floor, and a second test asserts the inversion is still load-bearing —
   so a future palette change cannot quietly break the selected row.

7. **The wider ASCII markers silently clipped every badge's label.**
   `Badge::width()` added a flat 2 columns for "marker + space", which is only
   right for a one-column glyph like `✓`. The ASCII replacements are wider —
   `[OK]`, `[X]`, `[!]` — so a badge reserved 4 columns and painted 7, and
   ratatui clipped the overflow. A right-aligned badge rendered as `[OK] ` with
   its word gone: the marker without the label, which is the opposite of
   accessibility. The marker is now measured, and a test asserts
   `width() == the sum of the spans it actually paints` for every tone in both
   modes, so this cannot come back quietly.

   It is recorded here rather than quietly fixed because it is the reason the
   marker change needed a re-run of the whole suite: **widening a glyph is a
   layout change**, and the glyph audit that verified the markers were
   renderable had no reason on its own to notice that the label went missing.

### What is now measured rather than asserted

* Every text pair in all six themes, against a 4.5:1 floor (3:1 for disabled
  labels, which WCAG exempts; 7:1 throughout for High Contrast). Full tables in
  `docs/tui-accessibility.md` and `docs/tui-themes.md`.
* Every painted text cell against its theme's floor, read back from the buffer
  rather than predicted.
* Display-column budgets for truncation and padding across the full width range.
* Glyph inventory completeness, degradation, and mutual distinctness in both
  modes.

### P6 reconciliation

All nine overlay surfaces are accounted for across the P6 and P7 record: `Help`,
`Palette`, `Model`, `Sessions`, `Preview`, `Setup`, `Mode`, `Permission`, and
`Confirm`. Two decisions from that pass are recorded here because they look like
defects otherwise:

* **Confirmations are single-affirmative-key, not yes/no buttons.** The dialog
  shows the action key and a cancel path; `y` confirms, anything else cancels.
  `a_destructive_confirmation_states_both_outcomes_at_every_size` pins both
  outcomes at all five acceptance sizes, from the Agent, Settings, and Security
  views.
* **The permission scrim is tested by what `hit_test` resolves**, not by what was
  registered, because registration succeeds even when the scrim shadows the
  control behind it.

### One deliberate deviation from the Phase 7 brief

The brief listed `✗ -> [FAIL]` among its required ASCII examples. The inventory
ships `✗ -> [X]`, confirmed with the author. `[X]` is one column wide against
`[FAIL]`'s six, which matters directly: the marker change in defect 7 above
widened ASCII markers and silently clipped badge labels, and a six-column
replacement would widen every failed badge by five more columns in the mode that
has the least room. The pairing stays symmetric with `✓ -> [OK]`, and a failure
adjacent to a path still reads as a mark rather than a filename character.

### Not validated

Unchanged from Phase 6, minus what Phase 10 later covered: physical resize with
an overlay open **was** validated on a real PTY. Still **NOT VALIDATED IN THIS
ENVIRONMENT**: real mouse delivery, terminal restore after an actual panic, and
real terminal glyph/font rendering. The measured contrast figures are palette computations —
a terminal that remaps, dims, or clamps colors can render a lower ratio than the
palette holds, and on a 16-color terminal it can render something else entirely.
This is not a WCAG conformance claim.

## Phase 8 — bounded rendering

### The claim

Per-frame rendering work is bounded by the **viewport**, not by the size of the
backing collection. That is a falsifiable claim, and it needs two different tests
because one can hold while the other fails:

* **Bounded work** — how much was actually laid out. Output equality cannot
  show this: a window can be correct and still be rebuilt from 10,000 items on
  every repaint. Two `#[cfg(test)]` counters (`MESSAGES_MATERIALISED`,
  `ROWS_MATERIALISED`, `MODEL_ROWS_BUILT`) make it measurable, and are compiled
  out of non-test builds.
* **Correct windowing** — a large collection must render byte-identically to the
  small collection holding the same tail, at every scroll position. This is what
  catches an off-by-one in the backwards walk.

### Defects found and fixed

1. **`draw_stream` was O(history) per frame.** It materialised every message in
   the transcript, converted every line, wrapped every paragraph, and *then*
   scrolled the result. At 50k messages that is tens of thousands of allocations
   per frame for output entirely off screen. It now walks backwards and stops
   once the viewport is covered (`push_message_lines` owns per-message layout).
2. **`View::Sessions` was O(archive) per frame.** Ten thousand sessions built
   ten thousand `ListItem`s — label and detail `String`s included — for a
   twenty-row viewport. Sessions, Files, Tasks and Tools now go through
   `render_windowed_list`, which owns the window and calls the row builder at
   most once per visible row.
3. **The model search browser was O(hits) per frame.** One row per hit, five
   `format!` allocations each, on the surface a broad query produces most of.
4. **`View::Terminal` was O(buffer) per frame, twice over.** It cloned every log
   entry, split every entry into lines, and built the whole `Vec<Line>` before
   scrolling. A 10,000-line scrollback did ~30,000 allocations per frame to paint
   forty lines. It now walks the log backwards from the newest entry and stops
   once the window is covered.
5. **`View::Agents` de-duplicated with `Vec::contains`.** That is quadratic in
   the number of distinct agents, so 10,000 tasks with distinct agent ids meant
   ~50M string comparisons per frame. Deduplication is now a `HashSet` and row
   order is still first-seen order, which is the order the old code produced.
6. **`View::Git` built all four sections whole** — status, log and diffstat —
   before scrolling. Each section is now indexed, and only the rows inside the
   window are built.
7. **The model catalog sections were unbounded.** `Local`, `Installed` and
   `Recommended` all drew from `app.model_cards`, a scan of every weight file
   under `models/`, and each built a `Line` for every qualifying card per frame.
   The section header interleaves one category row per group, so a card's row
   position depends on every category before it; the row total and the cursor row
   are now computed with an allocation-free counting pass, and only the window's
   cards are materialised. The hub browser and the `Installed`/`Recommended`
   empty states are windowed on the same rule.
8. **The sessions overlay sized its dialog from `rows.len()`.** It therefore
   built a row — two `String`s and a `format!` — for every stored session on
   every frame, including while the operator only read one. The row count comes
   from the collection, so the dialog is now sized first and the window second.
9. **`View::Artifacts` formatted the whole report per frame.** A `format!` per
   JSON line, for a scan report whose size is set by the scan. The report is
   still pretty-printed in one piece — that allocation is not avoidable without a
   streaming serializer — but the per-line allocation is now paid only for the
   window.
10. **A row-budget bug on the Models panel, found while wiring it.** The section
    head, the offline "hub unavailable" card and the rows are three claims on one
    rectangle, and the rows were built against the *full* height; the shared tail
    then clipped the overflow. The clipped end is the *top* of the window, which is
    exactly where the cursor sits when the selection is at the start of a long
    section — so the selected card silently vanished. The three budgets are now
    settled before any row is built. `a_selected_card_survives_a_section_header_and_an_offline_card`
    pins it.
11. **A second windowing implementation existed, and it had drifted.** `list_offset`
    duplicated the shared `window()`. It is now **deleted**; offset-scrolled
    surfaces use `design::collections::window_at`, and cursor-following surfaces use
    `window`/`list_window`. There is one scroll rule in the crate.
12. **The instrumentation itself was wrong.** The bounded-work counters were
    process-global atomics, so two tests sharing a counter clobbered each other
    under the default parallel test runner and failed intermittently depending on
    scheduling. They are now `thread_local`, which is what a per-frame counter
    actually wants: the frame is rendered on the thread that measures it.
13. **A real clipping bug, found while wiring the window.** The displacement
   markers (`↑N`/`↓N`) are painted as *rows*, but the window was measured before
   they were appended. A scrolled list therefore produced `height + 2` lines for
   a `height` row area, and both the last visible item and the `↓N` marker were
   silently clipped. The one existing test only asserted `↑`, so it passed.
   `design::collections::list_window` now reserves those rows before measuring,
   and is shared by the painter and by every caller that pre-slices — so the two
   cannot disagree about where the window starts.
14. **The four `TableView` surfaces built every matching row per frame.**
    `View::Mcp`, `View::Extensions`, `View::Security` and `View::Settings` each
    collected a `Vec<Vec<String>>` of every *matching* row and handed it to
    `render_table`, which then took its window. Filtering had therefore been made
    cheaper without ever having made it windowed. `TableView` gained a lazy row
    source (`TableRows::lazy`) and `render_table_filtered` takes the window before
    the rows exist, so a filter that matches one row in 10,000 now builds one row.
    `TableView` also gained an `offset`: a filtered table's cursor and its
    `HitTarget::ListRow` are in filtered coordinates, so without an offset a click
    on the third visible row resolved to the third surviving row's *source* index.
15. **`View::Security` built a finding/recovery row for every entry, then indexed
    both placeholders unconditionally.** Two separate defects in one surface: the
    row count came from materialising every row, and an empty findings section
    still evaluated `security_rows[0]`, which is how an empty section reads as a
    panic report or a blank row. Row count and row construction are separate
    accessors now, and both placeholder sections guard on emptiness.
16. **`Preview` sized its dialog from the whole document.** The overlay is
    windowed like every other scroller now.
17. **`View::Terminal` split one enormous entry at the viewport budget and
    re-measured whole chunks.** A single 50,000-line entry produced a
    `height + 1` line vector to paint `height` rows, and a full buffer scrolled
    the "still running" footer out of existence — so the one indicator that tells
    the operator the command has not exited was exactly the indicator lost under
    load. Entries are split against the remaining budget, chunks are built once and
    measured once, and the footer is composed first so it survives a full buffer.
18. **The Workbench panels built every row for all four collections.** Workspaces,
    Sessions, Tasks, and all eight `OpsTab` views built a `Vec<ListItem>` for the
    whole collection and passed it to `draw_list`, which then windowed it. The
    shared list painter now takes a *row factory* plus the row count, so the eager
    `ListView` path and the lazy path are the same code with two sources.
19. **`list_header` counted filter survivors by materialising an index vector.**
    The Tasks/Sessions/Tools header printed `({n} shown)` by calling
    `filtered_tasks()`, which allocates a `Vec<usize>` of every match — the same
    defect as #14, in a string. `AgentApp::count_matching` walks the predicate and
    allocates nothing.
20. **A window computed one way and painted another, found by the shared painter's
    first reuse.** `FilteredWindow` computed its window with `window()` while the
    list painter computed it with `list_window(..., show_displacement)`, and the
    two disagree by two reserved rows. The symptom was not a cosmetic miscount: the
    selected row of a filtered 10,000-row session list was *off screen*, because
    the painter's window started two rows later than the one that had been mapped
    onto the slice. `FilteredWindow::new` now takes the same `show_displacement`
    flag the painter will use, and cannot be called without it.

### Verification

| Suite | Result |
| --- | --- |
| `a_large_chat_transcript_lays_out_no_more_messages_than_the_viewport_holds` | 100 / 1,000 / 10,000 / 50,000 messages × 5 acceptance sizes |
| `a_large_chat_transcript_renders_exactly_what_the_same_tail_renders` | byte-identical to the same 400-message tail at 4 scroll offsets |
| `a_new_message_does_not_disturb_a_scrolled_chat` | offset preserved, no yank to bottom |
| `a_large_session_list_builds_only_the_rows_it_paints` | 100 / 1,000 / 10,000 / 50,000 × 5 sizes |
| `a_large_session_list_keeps_the_selected_row_on_screen` | selection visible at 4 positions |
| `a_broad_model_search_builds_only_the_rows_it_shows` | 100 / 1,000 / 10,000 / 50,000 hits × 3 selections |
| `a_broad_model_search_can_scroll_to_the_last_hit` | window reaches the last hit |
| `a_pre_sliced_window_reports_a_known_total` | `↓N` derived from the true total, not the slice |
| `a_large_search_result_set_renders_and_targets_only_visible_rows` | 100 / 1,000 / 10,000 × 5 sizes; rows *and* `HitTarget`s bounded |
| `a_large_search_reaches_its_end_and_admits_truncation` | last hit reachable, displacement still reported |
| `a_large_terminal_buffer_renders_only_its_visible_tail` | 100 / 1,000 / 10,000 × 5 sizes; newest output visible |
| `a_terminal_buffer_stays_correct_while_output_appends` | append at the bottom stays visible; append while scrolled away does not yank |
| `a_large_task_list_does_not_make_the_agents_view_quadratic` | 100 / 1,000 / 10,000; no `Vec::contains` scan |
| `a_large_agents_list_scrolls` | window moves across four offsets |
| `a_large_git_report_renders_only_its_visible_window` | 100 / 1,000 / 10,000 commits; header preserved |
| `a_large_git_history_scrolls_to_its_end` | oldest commit reachable |
| `a_large_model_catalog_renders_only_its_visible_cards` | all 3 card sections × 100 / 1,000 / 10,000 × 5 sizes |
| `a_large_local_catalog_keeps_the_selection_visible_across_categories` | category headers do not push the cursor off screen |
| `a_selected_card_survives_a_section_header_and_an_offline_card` | the Models row-budget bug (defect 10) |
| `an_empty_model_catalog_says_so` | empty state per section, and not shown when a qualifying card exists |
| `a_large_hub_page_renders_only_its_visible_rows` | 10,000 hub rows, 4 selections |
| `a_large_session_history_overlay_builds_only_visible_rows` | overlay dialog sized before rows are built; hits bounded |
| `a_large_artifacts_report_builds_only_visible_rows` | 100 / 1,000 / 10,000 entry report |
| `a_large_preview_overlay_builds_only_the_lines_it_paints` | 100 / 1,000 / 10,000 line document |
| `one_enormous_terminal_entry_builds_only_what_it_paints` | one 50,000-line entry |
| `a_running_command_keeps_its_footer_when_the_buffer_is_full` | footer survives a full buffer |
| `workbench_panels_register_only_their_visible_rows` | 100 / 1,000 / 10,000 / 50,000 × 3 collections |
| `growing_tables_build_only_the_rows_they_paint_under_a_filter` | 100 / 1,000 / 10,000 / 50,000 rows, 1 match |
| `a_filtered_window_selects_and_hits_the_same_row` | selection and click agree under a filter |
| `a_filtered_table_hits_the_row_it_painted` | `TableView.offset` keeps hit indices in filtered coordinates |
| `a_growing_security_view_builds_only_what_it_paints` | 100 / 1,000 / 10,000 / 50,000 findings |
| `settings_stays_a_fixed_width_live_readout` | 5,000 permission rules, row builds unchanged |
| `fixed_overlays_stay_bounded_when_their_inputs_grow` | Help / Palette / Model / Mode |
| `a_mixed_transcript_of_every_message_kind_stays_bounded` | user, assistant, tool, plan, rejected plan, note, error × 5 sizes |
| `a_denied_tool_call_is_readable_in_a_long_transcript` | `denied · <reason>` survives a 4,000-message window |
| `chat_under_every_overlay_holds_both_bounds` | 20,000 messages × 7 overlays × 5 sizes |
| `a_chat_resize_keeps_the_message_it_was_showing` | 120x40 → 80x24 keeps the newest message |
| `filtered_list_headers_count_survivors_without_losing_the_selection` | 50,000 rows, 1 survivor, header still correct |

Mutation-checked: reverting `render_windowed_list` to build every row
(`for i in 0..total`) makes `a_large_session_list_builds_only_the_rows_it_paints`
fail. A windowing test that cannot fail when the windowing is removed is not a
test.

The shared list painter carries its own counter (`LIST_ROWS_MATERIALISED`) for the
same reason `ROWS_MATERIALISED` exists: a hit-target count is an *upper bound* on
materialisation, so only a counter at the factory call can show the work did not
happen. `workbench_panels_register_only_their_visible_rows` asserts on it and
measures a constant 102 rows at 100 and at 50,000 rows per collection — the
identical number at every size is the evidence that the cost is a function of the
viewport.

### Performance posture

What is claimed is what was measured: **rows materialised per frame are bounded
by the viewport** on the chat, session, session-overlay, file, task, tool,
agent, git, terminal, artifacts, model-search, model-catalog and model-hub paths.
`HitTarget::ListRow` registrations are asserted to be bounded on the same paths,
because a row that is off screen but still clickable reads as a selection and acts
on a different item than the operator aimed at.

This is a row-counting claim, not an allocation-counting one. No claim is made
about zero allocations per frame, about wall-clock frame time, or about
allocation counts — none of those were measured, and inventing them would be
worse than leaving them out.

Three bounds are honest exceptions, all stated rather than hidden:

* The artifacts report is pretty-printed in one allocation before the window is
  taken. Eliminating that needs a streaming serializer; the per-line cost is
  bounded, the string is not.
* **Settings is eager, by documented exemption.** Its rows are fixed by the code
  (a field per setting) plus the user's own permission rules, and the only
  iteration is the fixed six-entry theme catalogue. It is *live*: it re-reads
  config on every activation rather than rendering a cached snapshot, which is the
  property the exemption must not cost.
  `settings_stays_a_fixed_width_live_readout` builds 5,000 permission rules and
  asserts the row-build count does not move, so the exemption is measured rather
  than argued.
* Filter *matching* is O(n) per frame. See limitation 2. What is bounded is the
  per-row `String` churn, which is the expensive half.

## Phase 9 — crash and error hardening

### Classification

The `unwrap`/`expect`/`panic` surface was classified rather than blanket-banned.
The policy is deliberately asymmetric: blanket-banning `unwrap` pushes code
toward `unwrap_or_default()`, which converts a loud failure into a silently wrong
value. `unwrap`/`expect` are reviewed per site; the macros that make a *whole
subsystem* die are policed mechanically.

### Defects found and fixed

1. **Two `unreachable!()` arms in extension dispatch** (`agent/views.rs`). These
   run inside `tokio::spawn`, so a panic would *not* take down the process — it
   would kill only the task and leave the status line reading
   `extension scan …` forever. That is a silent hang with no error, which is
   worse than a crash because nothing looks wrong. Both now return an
   operator-visible error.
2. **`.expect("model override poisoned")` on the model-override lock**
   (`agent/provider.rs`). The guarded value is an `Option<String>` with no
   cross-field invariant, so poisoning cannot have left it inconsistent — yet one
   unrelated panic permanently killed every later `selected()` call, including
   the render that shows which model is active. Poisoning is now recovered, and a
   test pins the reasoning.
3. **A policy guard so the first class cannot return.** `no_production_code_can_panic_the_ui`
   scans non-test source for `unreachable!`, `todo!`, `panic!` and
   `unimplemented!`. Absence bugs persist precisely because no behavioural test
   notices them, and this is the only thing that notices.

### Also covered by tests

* `no_view_or_overlay_paints_at_a_terminal_smaller_than_its_chrome` — all 15
  views and all 7 overlays at `(1,1) (2,1) (1,2) (2,2) (3,3) (5,3) (3,5)`.
  `TestBackend` cannot express a zero dimension, so 1 is the floor; it is also the
  size at which every `u16` subtraction the layout performs is still in range.
* `incoherent_state_renders_instead_of_panicking` — selection past the end of
  every list, a filter matching nothing with a pre-filter selection, a modal over
  an empty collection, an empty model search, and the grapheme clusters from
  Phase 7 driven through the composer and the lists together.

### Not done (superseded)

This section originally said the async/cancellation and security-invariant
sweeps were incomplete. That was true at the end of the panic pass. It is no
longer true, and is kept only so the reader can see what changed.

---

## Phase 9b — async, cancellation, and security-invariant sweeps

Phase 9 above closed the panic/crash surface and said plainly that this sweep was
left undone. This section is that sweep, done — hence the `9b` rather than a
second Phase 9.

### The claim

Every background task that can land after it stopped mattering must not change
the UI; every security path must fail closed under test, not in production.

### Defects found and fixed

21. **A cancelled model probe stranded a staged switch.** `Ctrl+M` stages a
   candidate model, spawns a probe, and only commits on a match. A cancelled
   probe returned before `finalize_joins` cleaned up, leaving
   `pending_switch` set and the status line reading "probing" forever — a switch
   that could never be accepted or abandoned by any key. The cancel path now
   clears the staged switch and reports the cancellation.
22. **A crashed probe was silent.** A `JoinError` from the probe task fell
   through to a status line the operator had no reason to read. Probe failures
   now surface through `native_tools_err`/`provider_status`.
23. **A stale probe could commit a different model.** `finalize_joins` committed
   on completion without checking the probe still belonged to the staged switch.
   It now matches `pending_switch.tag` before committing, so a probe that was
   superseded can never commit its own candidate.
24. **Background results could steal an open overlay.** A preview resolving while
   Help/Palette/Model/Mode was open would replace the overlay the operator was
   mid-way through reading. Background data now never opens over an existing
   overlay, and reports that it is waiting instead; with no overlay open it opens
   as before.
25. **A poisoned resolver inbox killed the UI.** `TuiResolver` read the inbox
   behind a poisoned lock with `expect`, so one panic on a pending approval made
   every later gate request panic too. It now recovers the value from the poison.
   A dropped approval still **denies** rather than blocking forever.

### Coverage added

* Staged-switch lifecycle: `a_cancelled_probe_does_not_strand_a_staged_switch`,
  `a_landed_probe_still_commits_its_staged_switch`.
* Staleness: `a_superseded_probe_cannot_commit_its_own_candidate`,
  `a_matching_probe_still_commits_after_a_superseded_one`.
* Overlay ownership: `a_background_preview_does_not_steal_an_open_overlay`,
  `a_background_preview_still_opens_when_no_overlay_is_open`.
* Failure surface: `a_poisoned_resolver_inbox_recovers_instead_of_killing_the_ui`,
  `a_dropped_approval_denies_rather_than_blocks`.
* Resource gate: `the_tui_cannot_arm_a_resource_override_policy_forbids`,
  `the_tui_reports_an_audited_resource_override_when_policy_allows_it`.
  Both force a **real** denial (`min_memory_headroom_percent = 100`) rather than
  asserting on a machine where nothing was blocked — the first drafts of these
  tests passed on their assumptions and proved nothing.
* Permission scope: `a_reflexive_enter_denies_a_permission_instead_of_granting_it`,
  `an_explicit_allow_reaches_the_gate`,
  `an_allow_once_grant_does_not_widen_to_another_scope`.

### How a scope was checked, honestly

The scoped-grant test asserts that a second request for a *different* path raises
a new question. It does **not** claim the gate is sound: that belongs to
`openrt-permissions`, whose own tests are authoritative. What is pinned here is
the TUI-side wiring — that an allow-once answer does not silently answer a
request for a different resource.

### Channel closure

`pump` drains the receiver with `try_recv` and stops on `TryRecvError::Disconnected`,
so a closed channel exits the loop instead of blocking it. This is *not* claimed
as validated by a dedicated regression; it is a property of the drain loop itself.
No UI hang was observed with any combination of probes, joins, and overlays.

---

## Phase 10 — live TTY validation

### The claim

The layout is correct on a `TestBackend`. Phase 10 asks whether the *rest* of a
real terminal — alternate screen, raw mode, mouse tracking, live key delivery,
mid-session resize, and teardown — behaves.

### Method

A `pty.fork()` harness (disposable, kept out of the tree — it is a measurement
instrument, not a test that belongs in CI) runs the real `openrt`
binary on a real PTY at 120×40, writes whole key tokens, resizes to 80×24 and
back, then sends the quit key. A minimal ANSI screen reconstruction turns the
byte stream back into a screen dump so claims are made about what was **painted**
rather than what was requested.

> **Side effect worth disclosing.** The first two PTY runs used the real `HOME`,
> so they touched the real store: `history.ledger`, `checksums.json`, and
> `recovery.records` were written by ordinary boot bookkeeping (the session is
> reported as interrupted and *nothing is auto-resumed*, fail-closed). The two
> registry files that must never be written — `registry.lifecycle/…` and
> `registry.manifests/…` — are unchanged from 2026-10-05 00:11. Every later run
> used an isolated `HOME` (`/tmp/opencode/p10/home`) because the store root is
> derived from `$HOME`.

> The harness bug that mattered: a `KEY_TOKEN`, sent a byte at a time, turned one
> arrow keypress into `Esc` + `[` + `B` — three real keystrokes. A terminal never
> does that. Fixing the harness before blaming the app matters.

### Measured on a real terminal

| Observation | Result |
| --- | --- |
| Alternate screen entered | `?1049h` |
| Cursor hidden, then shown | `?25l` → `?25h` |
| Mouse reporting + bracketed paste armed | `?1006h`, `?1000h`, `?2004h` |
| Each key painted a fresh frame | 190–10 061 bytes per keypress |
| Resize 120×40 → 80×24 → 120×40 | full repaint both ways, no panic |
| Clean exit | `exit 0`, terminal restored |
| Teardown order | paste off → mouse off → alt screen left → cursor last |
| Panics in output | none |

Screen dumps of the workbench, the help overlay, and the first-run setup wizard
were reconstructed from the live byte stream.

### Defect the live terminal found

26. **`Ctrl+Q` did not exist.** The composer guard in
`AgentApp::handle_interrupt` refuses to quit over unsent text and prints
*"interrupted — Esc discards the composer, Ctrl+Q quits anyway"*, while the help
card claimed plain `Ctrl+C` simply "quits". No `Ctrl+Q` handler existed in any
layer of `input.rs`.

A/B on the same PTY, same sequence (type text, `Ctrl+C`):

| Run | Result |
| --- | --- |
| without `Ctrl+Q` | hangs, `SIGKILL`, `?1049l` **never sent** — terminal left in alt screen with raw mode on |
| with `Ctrl+Q` | `exit 0`, full restore |

The operator's only way out was `Esc` — discarding exactly the work the guard was
trying to protect. Fixed: `Ctrl+Q` runs `AgentApp::handle_quit_now`, the same
teardown as `Ctrl+C` except that unsent composer text is disposable. Pinned by
`the_quit_hint_the_composer_guard_prints_is_actually_honoured`.

The help card was also wrong about `Ctrl+C`, which is deliberate: a layered
interrupt that dismisses a modal rather than quitting. It now reads
*interrupt: deny / cancel / close, then quit*.

### Not validated in this environment

* **No real terminal emulator.** A PTY is not a terminal emulator: no DEC-era
  *not validated here* claims about particular emulators, truecolor fidelity,
  DA/DSR query-response, or kitty keyboard protocol negotiation.
* **The kitty keyboard protocol is not armed.** `?5749h` / CSI `>` were not
  observed in the stream; the app runs on legacy `?2004h` bracketed paste alone.
* **No real provider round trip.** Every run reports `NET-OFF`; no LLM was called.
* **No genuine signal storm.** `SIGWINCH` was delivered by the harness, not by a
  display manager.
* **No headless/desktop E2E.** No frame-by-frame screenshot diffing, no other
  terminals (kitty/foot/alacritty/wezterm) were driven.

These are honest gaps, not accepted risks. The keyboard protocol item in
particular is an implementation fact that can be checked by reading, not by
guessing.

## Known limitations (not accepted, not fixed)

Stated plainly so they are not mistaken for done:

> A former item here — *"No interactive terminal run"* — is **no longer a
> limitation**: Phase 10 drove the real binary on a real PTY. What remains
> unvalidated is listed under Phase 10 rather than here, because "we did some of
> it" is the kind of phrase that turns into "we did it" on the next reading.

1. **`ShellLayout` is not the sole authority in Chat.** The rail, header, and
   status bar now draw through `AppShell`/`ShellLayout` in both Chat and
   Workbench, so the split, the badge gap, and the status row have one
   implementation. Chat still lays its *body* out itself — stream, side panel,
   and composer — because Chat's body is a function of its permission/resource
   state rather than a fixed column set. That is a documented deviation, not a
   missing wiring.
2. **Filtering is an O(n) scan, and that is not fixed.** Every filtered surface now
   bounds *materialisation* by the viewport: `FilteredWindow` walks the predicate
   to count survivors and allocates nothing, and the second walk builds only the
   rows inside the window. A 10,000-row filter with one match builds one row.

   What is still O(n) is deciding *which* rows match, on every repaint. That is a
   deliberate floor, not an oversight: the predicate needs the row's text, and
   nothing in the current state caches it. A filter with one match on 10,000 rows
   costs 10,000 predicate calls and zero row allocations per frame. No claim of
   O(1) filtering is made anywhere in this document, and none would be honest
   without an index the operator's edits invalidate.

   `filtered_tasks()`/`filtered_sessions()`/`filtered_tools()` and friends still
   return a `Vec<usize>`, and they are still called — on *keypresses*, which is
   where a movement needs "how many survive" to clamp against. Per-frame
   rendering does not use them.
3. **`list_offset` is gone.** It was deleted, not just bypassed: offset-scrolled
   surfaces use `design::collections::window_at` and cursor-following surfaces use
   `window`/`list_window`, so there is exactly one scroll rule in the crate. This
   limitation is closed.
4. **`ListView`/`TableView` adoption is partial, and `TreeView` is unused.**
   All nine collection call sites draw their panel chrome and rows through the
   shared `render_list`/`render_table` path, which registers a real
   `HitTarget::ListRow` per painted row. Models' provider hub was deliberately
   *not* forced into `TableView`: its rows carry a three-column layout plus
   detail subrows and a live filter, and the generic row cursor cannot express
   that without changing what the view does. `TreeView` still has no production
   caller; see the primitive inventory for the decision.
5. **The Models preamble outgrew a short terminal.** It is *closed* as of Phase 6.
   The preamble used to claim the whole panel at 80x24 — scope, resource fit, exec
   need, ctx window, provider status, key hints — leaving a legitimate row budget
   of zero and no reachable card. `MIN_SECTION_ROWS = 6` makes the section headers
   and the collapsed diagnostics yield first, and the remaining diagnostics become
   a collapsible disclosure. `model_rows_stay_reachable_on_an_80x24_terminal` and
   `the_models_preamble_collapses_only_when_rows_need_the_space` pin both halves:
   rows stay reachable, and the preamble still shows everything it has room for.

   This was not a windowing regression. The old code built the card rows and the
   shared tail then clipped every one of them, so no card was visible at 80x24
   either way.
6. **Contrast is not measured.** Themes are asserted for *distinctness* (no two
   semantic status slots collide) but not against a WCAG ratio.
7. **Screen-reader support** is out of reach for a cell-grid TUI; glyph and word
   redundancy mitigates rather than solves it.
8. **Open security follow-ups**, narrowed but not closed by Phases 9b/10:
   `the_tui_reports_an_audited_resource_override_when_policy_allows_it` now
   proves that a permitted override arms the gate, writes an audit row at grant
   time, is reported as audited to the operator, admits exactly one preflight,
   and does not become standing policy. **Still open and not claimed:**
   whether *consumption* of a one-shot override writes its own audit row (the
   TUI sees the grant, not the moment it is spent); the authorization behind
   the public string-valued `grant_override()`; and the default-on
   `allow_operator_override` policy itself, which is a policy question rather
   than a code defect.

## Artifact cleanup

Measured first, cleaned second, **never rebuilt afterwards** — `target/` is
absent as of this writing, and no Cargo command has been run since the removal.

| | Bytes | |
| --- | --- | --- |
| `target/` before | 47,846,385,163 | 44.56 GiB |
| `target/` after | 0 | **removed** |
| Reclaimed | 47,846,385,163 | **44.56 GiB** |
| Repository after (all, excl. `.git`) | 16,584,129,882 | 15.45 GiB |
| Files after (excl. `.git`) | 4,586 | |

The footprint grew between cleanups: the previous pass recorded 18.28 GiB, this
one 44.56 GiB, because Phases 8–10 rebuilt `openrt` (a 338 MB debug binary) plus
its dependencies. It is a cache directory, and it grows back — which is exactly
why it is removable.

Removed: `target/` (9,806 files at measurement time) — gitignored
(`.gitignore:1`, `/target`), carrying cargo's `CACHEDIR.TAG`
(`Signature: 8a477f597d28d172789f06886806bc55`), verified by `git status --porcelain target`
returning empty, and verified to contain nothing but Cargo output. Nothing was
tracked by git, so removal could not destroy work in progress.

After cleanup, by `du -sb`:

| Preserved | Bytes |
| --- | --- |
| `models/` | 16,493,694,528 (15.36 GiB — model weights, never touched) |
| `.opencode/` | 81,286,099 (77.5 MiB) |
| `crates/` | 7,723,965 (7.4 MiB, 388 `.rs` files) |
| `docs/` | 1,002,797 (1.0 MiB, 86 `.md` files) |

Also present and verified: `Cargo.toml`, `Cargo.lock`, `README.md`,
`.gitignore`, `project-complete-plan.txt`. There are 1,520 `.json` files inside
`target/` (regenerable test output) and zero `.md`/`.csv`/`.yaml`/`.txt` —
no authored file lived under `target/`.

## Documentation delivered

| Document | Contents |
| --- | --- |
| `docs/tui-design-system.md` | Layers, enforced invariants, token roles, layout metrics, component contracts |
| `docs/tui-themes.md` | The six themes, resolution order, preview vs commit, persistence, adding a theme |
| `docs/tui-layout.md` | Breakpoints, shell regions, space precedence, modal sizing, fallbacks |
| `docs/tui-interaction.md` | Permission routing (the rule that outranks the rest), keys, overlays, mouse, lifecycle |
| `docs/tui-accessibility.md` | Color redundancy, monochrome/ASCII, text handling, keyboard, small terminals, limitations |
| this file | Acceptance record |
