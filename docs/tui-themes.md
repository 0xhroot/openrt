# OpenRT TUI themes

OpenRT ships six themes. Each is a complete `Palette` — 33 semantic slots — that
resolves into `Tokens` once per frame.

| Theme | Persisted name | Intent |
| --- | --- | --- |
| Pitch Black | `pitch-black` | Default. True-black background, violet accent, minimal non-essential color. |
| Obsidian | `obsidian` | Warm dark; softer contrast, amber-leaning accents for long sessions. |
| Graphite | `graphite` | Neutral, low-chroma. For terminals that re-tint or for people who find color loud. |
| Midnight | `midnight` | Deep blue-black; highest chroma accent, designed for low ambient light. |
| High Contrast | `high-contrast` | Maximum separation for low vision and bright rooms. Status tones are pushed apart deliberately. |
| Monochrome | `monochrome` | No hue at all. Status is carried by glyphs and weight as well as color. |

## How a theme is resolved

```text
ThemeKind  →  Palette  →  Tokens  →  components  →  views
```

* `ThemeKind::palette()` is a `const fn`. A palette is pure data — no
  allocation, no lookup table, no I/O.
* `Tokens` is the semantic expansion of a palette. This is the only thing
  components and views ever see.
* `App.theme` is the **single committed theme owner**, shared by the Agent
  renderer, the Workbench renderer, and every overlay. This is what makes a
  switch repaint the whole shell at once rather than one panel.

## Selecting a theme

**Settings.** `Settings` has two focus sections — `INFO` (read-only) and
`THEME`. `Tab` / `BackTab` move between them, `↑`/`↓` move the selection, `Enter`
commits **and persists**, and `Esc` cancels back to the previously committed
theme.

The picker marks each row's state in words, so the distinction survives
monochrome and color-blind viewing:

| Marker | Meaning |
| --- | --- |
| `LIVE · COMMITTED` | The theme that is actually active right now. |
| `LIVE (preview)` | Arrow-key preview, not yet saved. |
| (unmarked) | Selected but not live. |
| `unsaved preview` | Status line: a preview is active and Enter will persist it. |

Preview is deliberately **transient**. `AgentApp.theme_preview` exists only while
you are arrowing through the list; nothing else reads it. Committing or
cancelling clears it. A reload, a view switch, or quitting mid-preview cannot
leave the UI showing a theme that was never saved.

## Persistence

Resolved once at startup, in this order:

1. `OPENRT_TUI_THEME` (environment override)
2. `$XDG_CONFIG_HOME/openrt/tui.toml`
3. `$HOME/.config/openrt/tui.toml`

File format is a single `theme = "<name>"` line. Anything else — a missing file,
an unknown name, a non-string value, malformed TOML — falls back to Pitch Black
rather than erroring at startup. A broken theme file must never be able to stop
the app from opening.

`App.theme_path` records the resolved path. `App::save_theme_to(path)` writes
explicitly, which is why **no test can overwrite a developer's real
preferences** — tests point `theme_path` at a temp file. Writing goes through
`save_theme()`; a failed write is surfaced in the Settings status line rather
than swallowed.

## Measured contrast per theme

Contrast ratios are **computed** from each palette's literal sRGB values using the
WCAG 2.x relative-luminance definition, and printed by
`theme::tests::measured_contrast_across_all_six_themes`. Reproduce them with:

```
cargo test -p openrt-tui measured_contrast -- --nocapture
```

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

A theme is not WCAG-conformant by having good numbers here. The ratio is computed
from the palette, and the terminal remaps, dims, and clamps colors on the way to
the screen. See `docs/tui-accessibility.md` for what the measurement does and does
not establish, and for the border figures.

### Borders and selection

`border_focus` must clear 4.5:1 and `border_active` / `border_destructive` 3:1,
measured against the **worst** plane a panel can rest on. Phase 7 fixed two real
failures: `border_active` was 1.88–2.27:1 across the four normal themes and
Monochrome (now 3.10–3.14), and `border_focus` was 4.15:1 in Pitch Black (now
4.61). `border` and `border_subtle` are chrome, not state, so their values are
measured and printed rather than asserted.

A selected row paints `text_inverse` on `accent.primary`. In `HighContrast` that
matters more than anywhere: `selected` is bright yellow and `text_primary` is
white, so primary-on-selected measures 1.07:1 and the row's text vanishes. The
inversion is load-bearing and a test asserts it stays that way.

## Guarantees the tests enforce

* **Every theme is complete and distinct.** All 6 palettes yield 33 defined
  colors, and no two themes produce an identical `Tokens` (a theme that silently
  failed to load would be indistinguishable from one that loaded).
* **The active theme reaches the renderer.** `assert_eq!(a.tokens(), b.tokens())`
  after switching, in both Agent and Workbench modes and in every overlay.
* **Palette compliance.** For each theme, every view and every overlay renders
  only colors drawn from that theme's palette. Verified non-vacuous by injecting
  `Color::Magenta` and watching the test reject it.
* **Round-trip.** Save → reload → identical `ThemeKind`.
* **Cancellation restores.** Preview → `Esc` → tokens equal the committed theme.
* **Failure is visible.** An unwritable path produces a status, not a silent no-op.
* **No alias leakage.** No renderer may reference the frozen Pitch Black
  compatibility constants (`no_renderer_uses_the_pitch_black_compat_aliases`).

## Adding a theme

1. Add the variant to `ThemeKind` and its `name()` / `from_name()` arms.
2. Add its `const` palette arm — all 33 slots, deliberately.
3. It appears automatically in the Settings picker, which iterates `ThemeKind::ALL`.
4. Add a case to the persistence tests if its name needs special handling
   (hyphenation, digits).

There is no registry to update: the picker, the parser, and the compliance suite
are all driven by `ThemeKind::ALL`.


## Tests must not inherit the developer's saved theme

`App::new` calls `load_theme()`, which reads the operator's real preferences in
this order:

1. `OPENRT_TUI_THEME`
2. `$XDG_CONFIG_HOME/openrt/tui.toml`
3. `$HOME/.config/openrt/tui.toml`

A test that assumed the default theme therefore passed or failed depending on who
ran it. With `theme = "monochrome"` saved in a local `tui.toml`, three theme
tests failed with no code change and no failing commit to bisect — the only
difference was a file outside the repository.

The shared `app()` test helper now repoints `theme_path` at a temp file **before**
setting the baseline theme. Two properties follow:

* A local preference cannot change a test result.
* Nothing a test does can write to the operator's real `tui.toml`.

The second point is the reason the path is repointed rather than merely setting
the theme in memory: `set_theme` persists, so pinning the theme without
redirecting the path would let a test overwrite a real user preference.
