# OpenRT TUI accessibility

An interface is accessible when it works for people whose terminal, vision,
locale, or hardware differ from the author's. This document lists what OpenRT
guarantees and, where relevant, what it deliberately does not.

## Color is never the only channel

Every state that matters is carried by **at least two** of: color, glyph, text,
weight, position.

The vocabulary is not re-invented per view. `design::semantics::Semantics` is the
single table of state identity, and the tests check the table itself for internal
collisions — which is how a shared glyph between two states gets caught once
instead of one screen at a time.

| State | Word | Marker (Unicode / ASCII) | Tone |
| --- | --- | --- | --- |
| `Success` | `SUCCESS` | `✓` / `[OK]` | accent |
| `Warning` | `WARNING` | `⚠` / `[!]` | warning |
| `Failed` | `FAILED` | `✗` / `[X]` | danger |
| `Blocked` | `BLOCKED` | `■` / `#` | blocked |
| `Running` | `RUNNING` | `▸` / `>` | running |
| `Pending` | `PENDING` | `○` / `o` | pending |
| `Selected` | *(gutter position)* | — | accent |
| `Focused` | *(weight + brackets)* | — | accent |
| `Disabled` | `DISABLED` | `–` / `-` | muted |
| `Unavailable` | `UNAVAILABLE` | `Ø` / `0` | muted |
| `ResourceTight` | `RESOURCE TIGHT` | `↑` / `^` | warning |
| `ResourceBlocked` | `RESOURCE BLOCKED` | `▓` / `=` | danger |
| `WaitingForResources` | `WAITING FOR RESOURCES` | `…` / `...` | pending |

Two decisions in that table are load-bearing:

* **`Blocked` and `Failed` no longer share a marker.** They did — both painted
  `✗` — so on a monochrome terminal a denied request and a missing one were
  indistinguishable. `Blocked` is `■`, `Failed` is `✗`.
* **The three resource states never collapse into one another.** `RESOURCE TIGHT`
  (proceed), `RESOURCE BLOCKED` (free something, or override), and
  `WAITING FOR RESOURCES` (wait) each have their own word *and* their own
  marker, because each one prescribes a different next action. `RESOURCE BLOCKED`
  and `UNAVAILABLE` are also deliberately different words: one is a capacity
  decision a retry can change, the other is a property of the configuration.

**Selection and focus are positional, not marked.** They describe where the
operator is, not what happened, so they carry no private glyph: selection is the
gutter column plus weight, focus is weight plus brackets. Giving them glyphs too
would add a fifth thing to read in every list row and break the invariant that
gutter marks are reserved for status.

Critical header states are spelled out in words — `AGENT READY`,
`RESOURCE TIGHT`, `RESOURCE BLOCKED`, `RUNNING`, `WAITING`, `FAILED`, `PAUSED` —
so they survive both monochrome and every form of color blindness. The `Monochrome`
theme removes all hue, and `borders.focus` remains the single strongest border in
the UI so keyboard focus is locatable without color.

## Monochrome and limited-color terminals

* **Monochrome theme.** All six token groups resolve to shades of one hue.
  `status.success`, `.warning`, `.danger`, and `.info` are still *distinct
  values*, so a state change is visible as a brightness change.
* **16-color / 8-color terminals.** The palette is truecolor, but ratatui
  degrades it. Degradation is acceptable because nothing depends on hue
  discrimination.
* **No-color at all.** Glyphs and words carry the state.

## ASCII mode

`run::g(unicode, ascii)` selects a glyph at runtime. `OPENRT_ASCII=1` (or any
non-`0` value) forces ASCII; `OPENRT_ASCII=0` forces Unicode. With neither set,
the locale decides via `LC_ALL` → `LC_CTYPE` → `LANG`.

**Fail closed on rendering, never on behavior:** no locale information at all
falls back to ASCII. A terminal that cannot render the glyph gets a working app,
not a broken one.

The glyph inventory is one table, `run::GLYPHS`, and it is **enforced rather than
conventional**. Two mechanisms used to disagree: `g(unicode, ascii)` at the call
site and a replacement chain inside `sanitize_text`. Glyphs that were never passed
through `g()` — `▾` (a collapsed branch), `⎇` (the Alt key), `⋯` (a running
ellipsis), and all four braille spinner phases — reached ASCII terminals raw. On
an ASCII terminal a collapsed branch looked identical to an expanded one and the
Alt key rendered as a box. That is not cosmetic: a glyph that carries meaning and
has no fallback silently removes a distinction the operator was relying on.

Now:

* every functional mark resolves through `GLYPHS`;
* `g()`'s inline ASCII argument is checked against the table, so the two
  mechanisms cannot drift;
* a source scan rejects any functional glyph painted in production code that has
  no table entry, so the *next* person to write `let marker = "▶"` fails a test
  rather than a user's terminal;
* a test proves each entry degrades, and that ASCII mode never turns two states
  into the same mark.

`✓` → `[OK]` and `✗` → `[X]` rather than `+` and `x`: next to a path a bare `x`
reads as part of a filename, and a `+` reads as a diff.

**Punctuation is deliberately not substituted.** `·`, `—`, `–`, and curly quotes
carry no state, every code page this app targets has them, and rewriting them
would corrupt text the operator wrote — an em dash inside a model response is
content, not a marker. `run::PASSTHROUGH_PUNCTUATION` records that decision so
`g()` can prove every non-ASCII glyph it is handed is either in the inventory or
deliberately passed through.

**Operator prose is never rewritten.** `café`, `日本語`, `“quoted”`, and `🎉` pass
through ASCII mode untouched.

## Text handling

* **Truncation counts display columns — not bytes, and not characters.** This
  was the single worst defect class found in Phase 7. A terminal cell grid has
  three widths for every string: bytes, chars, and *columns*. They differ for
  exactly the content an operator is most likely to paste in. CJK is 1 char and
  2 columns; an accented character is 1 char, 1 column, 2 bytes; a combining mark
  is 1 char and 0 columns. Counting chars put 6 columns of `日本語` into a
  5-column frame and pushed the panel border out of alignment; counting bytes
  produced mojibake.

  `design::text` is now the only place that measures width, using `unicode-width`
  — the same crate `ratatui-core` renders with, so a width this workspace
  computes is the width ratatui paints. If the two ever disagreed, the guarantee
  this phase exists for would be false in the one place it matters.

  What it guarantees, each asserted:

  * truncation never exceeds its column budget, for every sample and every width
    from 0 to 23, in both glyph modes;
  * it never splits a **grapheme cluster**. This is stronger than "never splits a
    wide cell": a base letter plus its combining mark is one user-perceived
    character but two Rust `char`s, so a `char`-based cut leaves the mark behind
    and it renders as a stray accent next to unrelated text — indistinguishable
    from a font bug in the terminal. Truncation walks
    `unicode-segmentation`'s extended grapheme clusters, the same boundaries
    `ratatui-core` honours, so ZWJ emoji, tag sequences and regional-indicator
    pairs survive whole. The composer prefix and the caret-column inverse are
    cluster-aligned for the same reason, and no caret position inside a cluster
    is ever handed back;
  * the marker it inserts fits the column too — `…` is one cell and `...` is
    three, and a marker wider than its column is the one outcome truncation must
    never produce;
  * paths and identifiers keep their **tail**, sentences and errors keep their
    **head** (`…normalize_records.rs` is one file; `normalize_recor….rs` could be
    any);
  * padding never lengthens a row past its column and never shortens content.

  **Content limits are not widths.** Caps like "the transcript is 600 characters"
  remain character counts on purpose: that is a storage and paste-size contract,
  and making it column-dependent would make the limit depend on the operator's
  script.
* **Long values are never clipped mid-number.** When header meters overflow, they
  collapse to labels (`RAM …`) rather than rendering a half-written `1GB/1`. A
  clipped number is a wrong number.
* **The caret lands where the glyph is.** The composer's viewport is width-aware:
  `design::input::viewport` takes the characters and spends a *column* budget,
  walking left from the caret and then right with whatever is left. Counting
  characters there meant a field holding `日本語` in a 10-column frame showed three
  characters of 6 columns into 5 columns of field, and dragged the caret block off
  the edge. The caret may sit at `start + vis` because callers reserve a trailing
  column for the caret block — that is how an empty field still shows a caret.
* **Wide and zero-width characters.** Emoji and CJK render by column count;
  combining marks are not split, and neither is any extended grapheme cluster —
  not by truncation, not by the composer prefix, and not by the caret's
  column-to-character inverse.
* **Control bytes are stripped** before anything reaches the screen. An escape
  sequence in a tool result cannot move the cursor, recolor the display, or emit
  a bell. Asserted by feeding `\u{1b}[31m` and `\u{7}` through a field and
  checking neither byte appears in the buffer.
* **Malformed optional data** renders as a value, not a panic: a missing git
  branch, an unparseable address, an absent model id.

## Keyboard

The application is fully operable from the keyboard. Focus is always visible as
`borders.focus` — the strongest border in the UI. There is no mouse-only path:
every action reachable by click is reachable by key, and the permission flow is
keyboard-first by design.

While a permission is pending the keyboard belongs to the dialog. This is not a
convenience; it is the reason an approval cannot be granted by a stray keystroke
sent to a different window.

## Small and unusual terminals

`1x1`, `1x40`, `2x2`, `3x1`, `80x24`, `80x16`, `100x30`, `120x40`, `160x50`,
`180x50`, and `200x60` are all exercised. No layout arithmetic subtracts from a
viewport dimension directly; everything goes through `tokens::saturating_sub`.

Components degrade rather than disappear:

| Component | Behavior when space runs out |
| --- | --- |
| Rail | Drops headings, keeps the active destination, reports `↑N`/`↓N` |
| Header | Caps at half the remaining rows; drops the divider before the meters |
| Status bar | Last to go; hints relocate into the header |
| Context panel | Withdrawn entirely (zero-width) rather than squeezed |
| Permission card | Becomes a modal if it cannot be shown whole |
| Fields | Scroll to keep the caret visible, prefix `…` |
| Tables | Header row spent first, then flexible columns |
| Toasts | Newest win; overflow count returned to the caller |

Nothing is ever silently dropped: every reduction is either recoverable by
scrolling or explicitly reported.

## Reduced motion and attention

There is no animation, no flashing, and no color cycling. The only time-varying
element is the loading spinner, which cycles through a small set of glyphs rather
than animating color. Progress is shown as a static bar and literal counts.

## Measured contrast

Contrast ratios are **computed**, not asserted by adjective, from the palette's
literal sRGB values using the WCAG 2.x definition: relative luminance from
linearized sRGB, then `(L1 + 0.05) / (L2 + 0.05)`. `design::contrast` implements
it and `theme::tests::measured_contrast_across_all_six_themes` prints every
measurement, so the numbers below are reproducible rather than remembered.

| Pair | Pitch Black | Obsidian | Graphite | Midnight | High Contrast | Monochrome | Floor (normal) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| body prose / bg | 17.49 | 16.43 | 17.37 | 16.41 | 21.00 | 18.43 | 4.5 |
| body prose / panel | 16.04 | 15.56 | 16.44 | 15.58 | 21.00 | 16.70 | 4.5 |
| body prose / card | 15.01 | 14.32 | 15.11 | 14.52 | 17.76 | 14.95 | 4.5 |
| hints / bg | 9.96 | 9.31 | 9.87 | 9.37 | 15.91 | 11.06 | 4.5 |
| metadata / bg | 5.68 | 5.48 | 5.73 | 5.38 | 9.04 | 6.75 | 4.5 |
| disabled label / bg | 3.29 | 3.21 | 3.35 | 3.26 | 5.32 | 3.77 | 3.0 |
| dialog body / overlay | 14.41 | 13.54 | 14.30 | 13.63 | 16.29 | 13.62 | 4.5 |
| composer / input | 15.80 | 15.29 | 16.17 | 15.20 | 21.00 | 16.17 | 4.5 |
| selected row / accent | 4.55 | 8.11 | 14.72 | 7.47 | 19.56 | 17.26 | 4.5 |

The floor column applies to the four normal themes plus Monochrome, which are held
to 4.5:1 for text (3:1 for disabled labels). **`HighContrast` holds every
non-disabled pair at 7:1 or better** and disabled text at 4.5:1.

Two floors exist rather than one because WCAG 2.x deliberately exempts **disabled
controls** from the contrast minimum: dimming is how a control says "not
actionable", and holding disabled text to the body figure would make disabled and
enabled look identical. 3:1 is the figure WCAG uses for UI components and
graphical objects, which is what a disabled label is.

One margin is thin enough to name: **Pitch Black's selected row measures 4.55:1
against its 4.5:1 floor.** It passes by 0.05, because Pitch Black's accent is a
dim violet where Obsidian's is a bright cyan. Any future darkening of
`accent_primary` in that theme turns a passing selected row into an unreadable one,
and that test is what will say so.

`HighContrast` holds every text pair at **7:1 or better** (the WCAG large-text and
UI-component figure), and `Monochrome` holds every pair at 4.5:1 or better while
carrying no hue at all.

### Borders

Measured against the **worst** plane a panel can rest on (`bg`, `surface`, or
`elevated`), because a border that clears the ratio on the root and disappears on
a raised card has not met it:

| Token | Meaning | Normal themes | High Contrast | Floor |
| --- | --- | --- | --- | --- |
| `border_focus` | the focused element | 4.61–14.69 | 16.54 | 4.5 |
| `border_active` | hovered / selected-not-focused | **was 1.88–2.27 → now 3.10–3.14** | 17.76 | 3.0 |
| `border_destructive` | destructive confirmations | 4.81–5.03 | 4.44 | 3.0 |
| `border` | default panel edge | 1.41–1.51 | 10.61 | *decorative* |
| `border_subtle` | dividers | 1.05–1.14 | 4.02 | *decorative* |

Two real defects were found and fixed here:

* **`border_active` failed 3:1 in all four normal themes and in Monochrome**
  (worst case 1.88:1 in Midnight). It is the hover and
  selected-but-not-focused indicator, so it carries state and is not decorative —
  a state cannot rely on a subtle border. Values were raised, preserving each
  theme's hue ratio, until every one clears 3:1 on its worst plane.
* **`border_focus` measured 4.15:1 in Pitch Black**, just under the 4.5 floor for
  the one element that has to be findable by eye. Lightened from `(124, 92, 255)`
  to `(137, 101, 255)`.

`border` and `border_subtle` are treated as **chrome**: they separate regions, they
do not carry state, and no control is identified by them. WCAG 1.4.11 exempts
purely decorative elements, so their values are measured and printed rather than
asserted, and holding them to 3:1 would push a divider to be louder than the panel
it divides. The consequence is stated plainly: **in the four normal themes a panel
edge sits near 1.3–1.7:1, so region separation there is carried by layout and
text, not by the border.** An operator who needs visible edges switches to
`HighContrast`, where the edge measures 10.6:1 or better.

### What a measured ratio does not tell you

* It is **not** a WCAG conformance claim. Conformance is a property of a document;
  this crate renders a terminal grid.
* It **cannot** see how a terminal renders a color. Terminal themes remap, dim,
  and clamp colors, so a palette ratio is an *upper bound* on what an operator
  sees — and on a 16-color or low-color-depth terminal the number is fiction.
  That is why `HighContrast` and `Monochrome` are specified by separation between
  tokens and by non-color channels, not by ratio alone.
* It measures the **palette**, not the screen. Which token lands where is the
  renderer's decision, and that matters: in `HighContrast`, `text_primary` on
  `surfaces.selected` measures **1.07:1** and the selected row's text disappears.
  The list survives only because it inverts to `text_inverse` on
  `accent.primary`. A test therefore reads the painted cells back out of the
  buffer and checks each text cell against its theme's floor, and another test
  asserts the inversion convention is still load-bearing (primary-on-selected
  must *stay* unreadable, or the palette changed without revisiting the
  inversion).

## Known limitations

These are real and not yet addressed:

* **Screen readers.** A TUI is a grid of cells with no accessible-name tree.
  Glyph and word redundancy mitigates this but does not solve it.
* **Contrast ratios are measured, but only for the palette.** They are computed
  per theme against defined floors (see above) and the painted cells are checked
  back from the buffer. What remains unverified is the terminal in between: a
  remapping, dimming, or low-color-depth terminal can still reduce any of these
  numbers below what the operator sees. That gap cannot be closed headlessly.
* **Sticky notifications have no expiry.** By design — they are cleared by
  acknowledgement — but a user who never dismisses one keeps seeing it.
* **Color-blind simulation** is reasoned about, not tested against a simulator.

## Auditing

The accessibility properties above are enforced by tests, not by review:

```sh
cargo test -p openrt-tui --offline design::
```

Specifically: the truncation tests assert display-column budgets, the sanitizer tests
assert absent bytes, the degenerate-geometry tests assert no panic across the
size matrix, and the glyph tests assert that ASCII fallbacks remain mutually
distinct.


## Every grant stays reachable at every size

A permission card reserves as many rows as its four grants need at the current
width, rather than assuming one row fits.

This is not a cosmetic concern. The action row used to be a fixed single row and
`ButtonGroup::render` stopped drawing once it ran out of width, so on an 80x24
terminal the trailing buttons were dropped — and the trailing button is **Deny**.
The card was left showing three clickable grants and no clickable way to refuse
them. The keyboard still worked, so the bug was invisible to a keyboard test and
to anyone reading the source; only rendering at a narrow size and asking whether
the button had a hit region found it.

`ButtonGroup` therefore has two renderers:

| Method | Behaviour |
| --- | --- |
| `render` | One row. Stops at the first button that does not fit. Correct for a status bar or a two-button dialog. |
| `render_wrapped` | Wraps onto further rows so every button is drawn and registered. Used wherever an action must not disappear. |

`render_wrapped` still refuses to paint outside the region it was handed, so
wrapping cannot become a way to draw over neighbouring content — a caller that
wants every button must reserve `rows_needed(width)` rows, which is what both the
inline Chat card and the modal now do. `ButtonGroup::rows_needed` is the single
place that answers "how many rows do my actions need at this width".

Pinned by `the_permission_grants_stay_visible_and_clickable_at_every_size`.

## Hit regions are tested by resolution, not by registration

A test that only checks a button was *registered* passes even when the scrim
shadows it, because registration succeeded — the button was simply unreachable.
The dialog tests therefore assert what `hit_test` **resolves** at the button's
own coordinates, which is the only thing a click actually sees.
