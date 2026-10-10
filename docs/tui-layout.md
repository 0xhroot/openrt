# OpenRT TUI layout

All measurement flows through `design::layout` and `design::tokens`. Renderers
call `ShellLayout::build` and draw into the rectangles it returns; they do not
compute chrome sizes themselves.

## Breakpoints

| Name | Width | Behavior |
| --- | --- | --- |
| Compact | `< 90` | Rail keeps its fixed 16 columns; context panel dropped; chat is full width. |
| Normal | `90–119` | As above, with more room for the transcript. |
| Wide | `120–149` | Contextual right panel appears when chat stays readable. |
| XWide | `150–179` | Comfortable chat plus the side panel. |
| UltraWide | `>= 180` | Extra space goes to the transcript, never to wider chrome. |

The rail never reflows — it is 16 columns at every width. On a narrow terminal
it *collapses* by dropping content, because a rail that changed width would make
the transcript unreadable exactly when space is scarce.

## Shell regions

`ShellLayout::build(area, tokens, context)` returns:

```rust
ShellLayout {
    area, breakpoint,
    header,   // masthead
    status,   // footer
    rail,     // navigation, fixed width
    body,     // chat or the active list view
    context,  // side panel; zero-size when absent
}
```

`context: bool` is a *request*, not a command. The layout grants the panel only
if the width budget allows, so a caller can never accidentally produce a
zero-width panel it then renders into.

## Vertical space precedence

When rows are scarce, they are spent in this order:

1. **body** — chrome never fully displaces content
2. **header** — capped at half of what remains after the status bar, so an 80x4
   window shows chat rather than a three-row masthead
3. **status** — dropped last

The status bar carries `AGENT READY`, `RESOURCE TIGHT`, `RESOURCE BLOCKED`, and
the active mode. Those are the strings an operator acts on, so they are the last
thing to lose a row.

When the status bar has no room, `AppShell::draw` moves the key hints into the
header's final row rather than discarding them.

## Modal sizing

`ShellLayout::modal_rect(tokens, want_w, want_h)` is the single dialog-positioning
path. Clamp order is deliberate: **shrink to the available space first, then
center.** Centering first and clamping after pushes a dialog's title and buttons
off-screen — the impossible-to-reach dialog.

Dialogs pass a *pixel* size derived from their own wrapped content:

```rust
let want_h = wrapped_height(&lines, want_w - 2) + 2; // +2 for the frame
let popup  = ShellLayout::build(area, t, false).modal_rect(t, want_w, want_h);
```

Note `ShellLayout::build`, not `new` — the measure step is what produces a
`layout` to call `modal_rect` on. (There is also a private percent-based
`centered` helper in the Agent renderer; it is not for dialogs.)

## The responsive fallbacks that matter

These are behaviors, not aspirations, and each has a test:

* **Permission card.** Inline in Chat when the whole card fits; otherwise a
  centered modal. A partial card — fields visible, grants cut off — is worse
  than a modal, because the operator cannot see what they are agreeing to.
* **Rail.** Drops headings before destinations, always keeps the active
  destination, reports `↑N` / `↓N` for what it hid, and keeps the WORKBENCH
  toggle reachable at every height.
* **Meters.** When meter text overflows the header, values collapse to labels
  rather than clipping mid-number. A half-written `1GB/1` is a lie about a real
  number.
* **Fields.** The caret is always visible; a scrolled field prefixes `…`.
* **Tables.** The header row and rule are spent from the vertical budget first.
  Flexible columns share the remainder; fixed columns keep their width.
* **Toasts.** Newest messages win the available rows and the overflow count is
  returned to the caller.

## Validation sizes

`80x24`, `100x30`, `120x40`, `160x50`, `180x50` are the exercised matrix, plus
degenerate `1x1`, `1x40`, `2x2`, `3x1`, and `200x60`. Every renderer is required
to survive all of them without panicking — the size matrix is asserted, not
eyeballed.

No layout arithmetic subtracts from a viewport dimension directly. All of it
goes through `tokens::saturating_sub`, which is why `1x1` is survivable rather
than an underflow panic.

## Panel reflow

A row of equally-weighted panels is arranged by
`design::layout::panel_grid`, not by percentage constraints.

Four fixed `Percentage(25)` columns are the obvious way to lay out a dashboard
and the wrong one. At 80 columns each panel is 19 wide, which cannot hold a
border, a title, and a value — and no amount of shrinking helps, because the
constraint never asks for fewer columns. Panels are therefore *reflowed*: the
column count drops as the width does, and only then is the space divided evenly.

| Width | Columns | Notes |
| --- | --- | --- |
| ≥ 91 | 4 abreast | 4 × `PANEL_MIN_WIDTH` (22) + 3 gaps |
| 46–90 | 3 abreast, 4th wraps | Workbench's 80x24 case |
| 23–45 | 2 abreast | |
| ≤ 22 | 1 column | A single column of the width available beats nothing |

`panel_grid` is invariant-tested across the acceptance matrix plus degenerate
geometries (`0x0`, `1x1`, `80x0`, `21x24`): it always returns one rect per panel,
never returns a rect outside its area, keeps every panel at or above
`PANEL_MIN_WIDTH` whenever that many panels genuinely fit abreast, and has its
last column reach the right edge so no dead strip is left over.

The last column is measured from its own origin to the right edge rather than by
adding a division remainder. A remainder leaves a dead strip whenever the
division is uneven, which is most widths.
