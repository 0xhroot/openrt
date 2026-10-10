# OpenRT TUI design system

The design system is the only place OpenRT's visual language is defined. Views
compose it; they never name a color, a magic width, or a border configuration of
their own.

## Layers

```text
theme        Palette + ThemeKind + persistence        crate::theme
 └─ tokens   Surfaces / Borders / Text / Status /     design::tokens
             Interaction / Accent / Spacing /
             LayoutMetrics / Typography
   ├─ layout     Breakpoints + ShellLayout + HitTargets   design::layout
   ├─ widgets    Panel, Badge, Button, TabBar, scrim       design::widgets
   ├─ shell      Header, StatusBar, NavigationRail, AppShell design::shell
   ├─ collections ListView, TableView, TreeView            design::collections
   ├─ input      Field, Search, caret viewport             design::input
   ├─ feedback   Toast, Notification                       design::feedback
   ├─ modals     PermissionDialog, ConfirmDialog           design::modals
   └─ states     Loading / Empty / Error / Info            design::states
 └─ views     Chat, Files, Security, ...                   agent::render
```

## Invariants

These are enforced, not aspirational:

1. **A component never reads a `Palette`.** It takes `&Tokens` — the resolved
   semantic layer. This is what makes a theme switch repaint everything at once.
2. **No `Rgb` literal exists outside `theme`.** The palette is the only place raw
   color values are written.
3. **No view uses the compatibility aliases** (`theme::BG`, `theme::ACCENT`, …).
   They are frozen at Pitch Black and marked `#[deprecated]`; adopting one
   silently pins that surface to the default theme. A guard test
   (`no_renderer_uses_the_pitch_black_compat_aliases`) walks the renderer
   sources and fails if one reappears. The test is verified non-vacuous by
   injecting an alias and watching it fail.
4. **Every allocation of width or height comes from `LayoutMetrics` or a
   `Breakpoint` decision.**
5. **Nothing indexes a viewport dimension.** All arithmetic goes through
   `tokens::saturating_sub`, so a 1x1 terminal is survivable.
6. **Every allocation renders its overflow.** A dropped row, a collapsed pane, or
   a truncated meter reports itself (`↑N`/`↓N`, `…`, "none") rather than
   silently lying about completeness.
7. **Width is measured in display columns, in one place.** `chars().count()` and
   `str::len()` are both wrong answers to "how wide is this" — CJK is one char and
   two columns, an accented character is one char and two bytes, a combining mark
   is one char and zero columns. `design::text` is the only module allowed to
   answer, using `unicode-width`, the crate `ratatui-core` renders with. If the
   workspace and ratatui ever disagree about a width, every alignment guarantee
   in this document is false at once, so there is one answer and one crate.
   Content caps (transcript length, paste size) stay in characters on purpose —
   that is a storage contract, not a layout one.
8. **Every glyph that carries meaning has a table entry.** The functional-mark
   inventory is `run::GLYPHS`, enforced by a source scan rather than by
   convention. Punctuation is exempt and listed in `run::PASSTHROUGH_PUNCTUATION`;
   operator prose is never rewritten.
9. **Status markers come from `design::semantics::Semantics`,** not from a
   per-widget marker choice — `Badge::marker()` reads the table, so two states
   sharing a marker is caught by the table's uniqueness tests *and* by a widget
   test, instead of being discovered one screen at a time. Collection-state
   words (`StateKind::label()`) are still per surface; see the Phase 7 scope note
   in `docs/architecture/TUI_ACCEPTANCE.md`.

## Semantic tokens

Tokens name *roles*, not appearances. A view asks for `text.muted`, never for
"gray".

| Group | Slots |
| --- | --- |
| `surfaces` | `bg`, `surface`, `elevated`, `overlay`, `input`, `selected` |
| `borders` | `subtle`, `default`, `active`, `focus`, `destructive` |
| `text` | `primary`, `secondary`, `muted`, `disabled`, `inverse`, `link`, `code` |
| `status` | `success`, `warning`, `danger`, `info`, `running`, `blocked`, `pending` |
| `interaction` | `focus`, `hover`, `selected`, `pressed`, `disabled` |
| `accent` | `primary`, `secondary`, `muted` |
| `space` | `xs` 0, `sm` 1, `md` 2, `lg` 4, `xl` 8 |
| `layout` | see below |
| `type_scale` | `app_title`, `page_title`, … |

Two separations are deliberate and load-bearing:

* **`status` vs `interaction`.** An element can be *focused* and simultaneously
  *blocked*. Collapsing these is what makes an agent feel unsafe.
* **`status.blocked` vs `status.danger`.** "The host will not let us proceed" is
  not a failure. `RESOURCE BLOCKED` must not be painted like a crash.

`Tokens` derives `PartialEq + Eq`, so `assert_eq!(a.tokens(), b.tokens())` is a
whole-theme assertion rather than a single-color one. `Tokens::colors()` returns
every distinct color for palette-compliance checks.

## Layout metrics

Measured, not guessed (`design::tokens::LayoutMetrics`):

| Metric | Value | Meaning |
| --- | --- | --- |
| `rail_width` | 16 | constant at every width — the rail collapses, never reflows |
| `header_height` | 3 | masthead |
| `status_height` | 1 | footer |
| `gutter` | 2 | between panels |
| `panel_pad` | 1 | panel interior |
| `min_chat_width` | 40 | below this, chat is a ribbon, so the context column is dropped |
| `comfortable_chat_width` | 96 | chat beyond this earns a side panel |
| `context_width` | 34 | side panel |
| `min_modal_width` / `max_modal_width` | 48 / 120 | dialogs are never stretched across a wide screen |
| `min_modal_height` | 8 | |
| `modal_margin` | 2 | rows kept clear above and below a modal |

## Space allocation precedence

When rows are scarce, `ShellLayout::build` spends them in this order:

1. **body** — content is never fully displaced by chrome
2. **header** — capped at half of what remains, so an 80x4 window shows chat
   rather than a three-row masthead
3. **status** — the last thing dropped, because it carries the resource state

Inside the header itself, rows are spent: identity → meters → divider. A
two-row header shows the meters *instead of* the rule, because pushing the rule
there would hide the numbers behind the crop.

## Component notes

**Navigation rail.** Grouping is presentation only; `View::ALL` order and every
key are unchanged. Under pressure the rail drops *headings* before
destinations, never the active destination, and reports what it hid with `↑N` /
`↓N`. See `design::shell::rail_window`.

**Permission dialog.** Six required fields — `ACTION`, `TARGET`, `REASON`,
`RISK`, `SCOPE`, `CONSEQUENCES` — plus four graded grants. Its height is derived
from the *wrapped* body, so it cannot clip a field it promised. On a terminal too
short to show it inline, it becomes a centered modal rather than a partial card:
a card whose fields show but whose grants are cut off is worse than a modal.

**Fields.** `design::input::viewport` keeps the caret visible from either
direction, counts characters rather than bytes (CJK and emoji stay intact), and
sanitizes control bytes so an escape sequence in a tool result cannot reach the
screen. A scrolled field prefixes `…` so the operator never mistakes the window
for the beginning of the text.

**Toasts vs notifications.** Nothing important may live only in a toast. Toasts
are transient and lossy — when the region is full, the *newest* win the rows and
the overflow count is returned. `Toast::sticky` marks messages (a denied
permission, a resource block) that are never auto-dismissed and never evicted to
make room for chatter.

**Scroll windows.** `design::collections::window` derives `(start, visible)` from
`(total, cursor, height)` and is invariant-tested to always contain the cursor,
never over-read, and clamp a stale cursor to the last row. It is the *only* scroll
rule in the crate: cursor-following surfaces use `window`/`list_window`,
offset-scrolled surfaces use `window_at`, and callers that pre-slice use the same
entry points, so a caller cannot measure a window the painter will not agree with.
`list_window` additionally takes `show_displacement`, because `↑N`/`↓N` are painted
as rows and have to be subtracted before the window is measured.

**Rows are built by the painter, not handed to it.** `draw_list`,
`draw_list_rows_lazy_targeted`, `render_windowed_list`, `render_windowed_filtered_list`
and `render_table_filtered` all take a *row factory* plus the collection's length,
so a surface with 50,000 rows builds the ~40 it paints. The eager `ListView` path
exists for callers that genuinely hold a slice, and it shares one implementation
with the lazy path rather than duplicating the paint loop.

Two consequences follow, and both are load-bearing:

* A caller may pass `total` larger than what it holds, which is what makes
  `↓N` report the true hidden count rather than the slice's.
* A caller must pass `show_displacement` (or the table equivalent) to any helper
  that measures a window, because a window computed one way and painted another
  puts the cursor two rows off screen with no error anywhere.

`FilteredWindow` is the filtering counterpart: it walks a predicate to count
survivors, allocates no index vector, and hands back only the visible indices. The
scan is O(n) per frame by design — nothing caches a row's text, so there is no
index to consult — while row *materialisation* is bounded by the window.

**Workbench panels.** Four panels come from `layout::panel_grid`, which balances
columns and rows for the panel *count* rather than assuming 2×2, and every panel
draws through `widgets::panel` so a focused panel reads as the same object as a
focused panel in any other mode. Focus is a `▸` in the title as well as a tone,
and the title's second half is the shared `StateKind::label()` — `LOADING`,
`FAILED`, or the real count — so no surface invents its own vocabulary. Panel
rows go through `collections::draw_list_rows_lazy_targeted`: the same row renderer
as an Agent list, plus a per-row hit callback, and the same row *factory* contract
— a 50,000-workspace panel builds the rows it paints. Registration is ordered, panel first
then rows, so the specific target wins without a second lookup at resolve time.

**Truncation direction is a caller decision.** The shared rule keeps a row's
*tail*, because rows usually end in a file path. Two Workbench fields are the
exception and say so: a task prompt and a room id are identified by their head,
so they truncate through `ui::head_excerpt`/`app::head_text` instead. Pushing a
status prefix into a row *label* is not safe in a narrow panel — the shared
renderer drops the label's head first, so status belongs in the right-aligned
detail column, where nothing is cropped.

## Primitive inventory

Every shared primitive carries an explicit decision, so "unused" is never left to
imply "forgotten". Production reference counts were measured, not estimated.

| Primitive | Decision | Why |
| --- | --- | --- |
| `panel`, `render_panel`, `PanelTone` | ADOPT | Every main surface draws through them. `PanelTone::Default` sits on `surfaces.bg` so a page reads as one plane; active/focused/destructive are the only elevated surfaces. |
| `Badge`, `BadgeTone`, `badge.marker()`, `Badge::spans_bold()` | EXTEND, ADOPT | Adopted for transcript headers, tool states, task and resource state, dashboards, and permission/resource/model cards. `marker()` lets a row bind marker and label to one tone without welding them together; `spans_bold()` is reserved for a row whose badges *are* the content. |
| `render_badges`, `BadgeRow`, `BadgeFit`, `BadgeEmphasis` | EXTEND, ADOPT | Was a single fixed policy: render nothing when the set does not fit. That silently blanked a row whose leftmost item was a halt reason. Fit, anchor, and emphasis are now caller-stated, and the Chat dashboard strip uses `BadgeRow::strip()` (prefix fit, bold, left-aligned) with a test that the halt badge survives at 40 columns. |
| `key_hint`, `key_hints` | EXTEND, ADOPT | `key_hint` took a `char`, which excluded `Ctrl+K`, `F1`, `Esc` and `PgUp` — one binding each. Now `&str`, with `key_hints` for rows. Used by Chat, Models, and Workbench. |
| `metric` | ADOPT | The header meters are a muted label with a bold value. The pair was hand-written in the shell and had already drifted from the same pair in the dashboard strip. |
| `tab_bar` | ADOPT | Models' section tabs and the Models mode tabs. |
| `progress_bar` | ADOPT | The RAM meter shows a bar and a percentage, not just a number. |
| `spinner` | ADOPT | The Models import-in-flight line. Bounded by a join handle and paired with a note naming the bound, so it never reads as an open-ended spinner. |
| `divider` | KEEP AS INFRASTRUCTURE | Used by the header's second row, which divides content with no frame. Inside a bordered panel a rule would be redundant — that is why no panel uses it. |
| `section_header` | REMOVE | Zero callers. Its one plausible home was the navigation rail heading, which is a 16-column muted label where a full-width rule is noise. |
| `panel_with_footer` | REMOVE | Its `&str` footer could only express a static note, but every real footer here is a set of key bindings. Panels now compose `panel` with a `key_hints` row. |
| `sanitize_field`, `truncate_tail`, `ellipsize` | KEEP AS INFRASTRUCTURE | Reached transitively — `collections`, `shell`, `feedback`, and the agent renderers all sanitize and truncate through them. Truncation goes through `design::text` and is asserted on display columns, never bytes and never characters. |
| `TreeView`, `draw_tree`, `flatten_tree` | KEEP AS INFRASTRUCTURE | No production caller. `Files` is the intended consumer — it already walks the directory hierarchy — but switching it from a flat list to an expanding tree changes selection, scrolling, and hit targets, which is an interaction change rather than a design-system migration. Recorded as follow-on work instead of being deleted or forced. |
| `feedback` (`Toast`, `Notification`) | KEEP AS INFRASTRUCTURE | Still the only transient channel, and it now shares `BadgeTone` and the panel chrome. It is not the home for anything durable: a denied permission, a resource block, and a model-import failure each already have a dedicated sticky card, and the one status line that cannot be missed is `app.status`. Nothing was moved here to manufacture usage. |
| `status bar` | ADOPT | One `AppShell` status row for both modes, with a caller-supplied `status_tone`. The Agent/Workbench split is about *content*, and that stayed in the caller; only the drawing was unified. |
| `panel_grid` | EXTEND, ADOPT | Was a fixed 2×2. It now balances columns and rows for the panel *count*, so a 1-, 3-, or 4-panel surface gets even cells instead of a stretched layout. Adopted by Workbench's four panels. |
| `ShellLayout::build(area, tokens, context, has_rail)` | EXTEND, ADOPT | A surface with no rail was still charged a 16-column gutter, which is how Workbench's right-hand panel used to fall off an 80-column terminal. The rail is now a stated fact of the surface, not an assumption. |
| `draw_list_rows_targeted` | EXTEND, ADOPT | `draw_list_rows` with a per-row callback. One row renderer means an Agent list row and a Workbench row cannot drift; the callback is what lets a surface own hit registration without copying the layout. |
| `StateKind::label()`, `render_collection` | EXTEND, ADOPT | `label()` is public so every surface spells a collection state the same way, and `render_collection` gives empty/loading/failed one implementation. A title too narrow to share its line with the state gets its own line rather than an unreadable inline fragment. |
| `design::text` (`display_width`, `truncate_width`, `Truncation`, `head_width`, `caret_column`, `chars_in_width`, `pad_right`) | NEW, ADOPT | The single place that answers how wide a string is. Phase 7 replaced char- and byte-counting across widget sizing, table spans, list labels, composer viewport, and renderer padding. `Truncation::Head` keeps the start (sentences, errors), `Truncation::Tail` keeps the end (paths, ids), and the marker itself has to fit the column — a marker wider than its column is the one outcome truncation must never produce. |
| `design::semantics` (`Semantics`) | NEW | The state vocabulary: word, marker, tone, and whether the state is positional. `Blocked` and `Failed` used to share `✗` in `Badge::marker()`, so on a monochrome terminal a denied request and a missing one were identical; badges now read this table and the collision is a test failure at both the table and the widget. The three resource states (`ResourceTight`, `ResourceBlocked`, `WaitingForResources`) each keep a distinct word *and* marker because each prescribes a different next action. |
| `design::contrast` (`contrast_ratio`) | NEW | WCAG sRGB relative luminance, so "meets 4.5:1" is a computation rather than an adjective. Used by the theme table, the painted-cell checks, and the border floors. |
| `head_excerpt`, `head_text` | EXTEND, KEEP IN CALLER | Workbench-local, on purpose: they exist only where the head is the informative end (task prompt, room id, status bar). Folding them into the shared module would have implied the rule changed globally, which it did not. |

## Testing

The design system is tested by contract, not by golden files:

* Structural invariants (sizes, no panics) across a size matrix including 1x1.
* Workbench panel geometry at every acceptance size: no overlap, no collapse,
  and every panel's state/title rendered.
* Truncation asserted on **display columns** for every sample at every width in
  `[0, 23]`, in both glyph modes — never bytes, and never characters.
* Truncation asserted never to split a **grapheme cluster**, in either truncation
  direction, in both modes, across combining marks, ZWJ emoji, tag sequences and
  regional-indicator pairs. Verified non-vacuous by reverting the implementation
  to `char` iteration in an isolated copy and watching all three cluster tests
  fail.
* Every `assert!(out.contains(...))` accompanied by a test that proves it can
  fail — several were checked by deliberately breaking the renderer.

Run them:

```sh
cargo test -p openrt-tui --offline design::
```
