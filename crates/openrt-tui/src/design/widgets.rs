//! Shared visual primitives.
//!
//! Every component here is theme-aware, width-safe and allocation-frugal. The
//! rules they all follow:
//!
//! * **No color literals.** A component reads [`Tokens`] and nothing else.
//! * **No fixed widths.** Every component measures its own area and degrades;
//!   `render_*` functions are safe to call with a zero-area rect.
//! * **State is never carried by color alone.** A badge, a status line or a
//!   blocked indicator always includes a text marker, so the UI survives a
//!   monochrome terminal and a color-blind operator.
//! * **Focus is a role, not an ad-hoc background swap.** Use [`Tokens::focus_ring`]
//!   or [`PanelTone::Focus`] so focus reads identically everywhere.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear};
use ratatui::Frame;

use super::layout::{HitTarget, HitTargets};
use super::semantics::Semantics;
use super::tokens::{saturating_sub, Tokens};

/// Visual emphasis of a panel. Maps to a border role, never a background
/// swap, so panels never "flicker" when focus moves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PanelTone {
    /// Resting on the app background.
    #[default]
    Default,
    /// Hovered, or selected without focus.
    Active,
    /// The focused element.
    Focus,
    /// Destructive: confirmation, denial, refused admission.
    Destructive,
    /// Floating chrome.
    Overlay,
    /// An editable field.
    Input,
}

impl PanelTone {
    /// Human-readable name, used in diagnostics so a rendered panel can be
    /// identified in a test without depending on colors.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Active => "active",
            Self::Focus => "focus",
            Self::Destructive => "destructive",
            Self::Overlay => "overlay",
            Self::Input => "input",
        }
    }
}

/// Normalize untrusted text destined for a single terminal row.
///
/// Model output, tool arguments and file names are all attacker-influenced.
/// They reach the screen through chat rows, card fields and table cells, where
/// a raw newline or tab would break the one-row-per-entry contract that every
/// list view depends on, and where a Unicode mark would corrupt a terminal
/// that cannot render it. Returns an owned `String`; the input is untouched.
pub fn sanitize_field(s: impl Into<String>) -> String {
    let s = s.into();
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\n' | '\r' | '\t' => out.push(' '),
            // Control characters are dropped rather than mapped: they have no
            // visual meaning and can confuse the terminal emulator.
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    crate::run::sanitize_text(&out)
}

/// Truncate to `n` display columns, appending an ellipsis when shortened.
/// Never panics on a short width, never splits a wide cell, and measures the
/// way the terminal does — see [`crate::design::text`].
pub fn ellipsize(s: &str, n: u16) -> String {
    crate::design::text::truncate_width(s, n, crate::design::text::Truncation::Head)
}

/// The border a tone resolves to.
pub fn tone_border(t: &Tokens, tone: PanelTone) -> ratatui::style::Color {
    match tone {
        PanelTone::Default => t.borders.default,
        PanelTone::Active => t.borders.active,
        PanelTone::Focus => t.borders.focus,
        PanelTone::Destructive => t.borders.destructive,
        PanelTone::Overlay => t.borders.active,
        PanelTone::Input => t.borders.active,
    }
}

/// A titled surface.
///
/// Titles are *text*, never the border color: a thin decorative color must not
/// double as low-contrast body text.
pub fn panel(t: &Tokens, title: &str, tone: PanelTone) -> Block<'static> {
    let bg = match tone {
        PanelTone::Overlay => t.surfaces.overlay,
        PanelTone::Input => t.surfaces.input,
        // A resting panel is part of the page, so it sits on the app background
        // rather than lifting off it. Lifting is reserved for surfaces that are
        // genuinely above the page — a focused or selected panel, a floating
        // overlay, an editable field. When every panel lifted, the whole screen
        // read as floating cards and the hierarchy disappeared.
        PanelTone::Default => t.surfaces.bg,
        PanelTone::Active | PanelTone::Focus | PanelTone::Destructive => t.surfaces.surface,
    };
    let mut b = Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_style(Style::default().fg(tone_border(t, tone)))
        .style(Style::default().bg(bg));
    if !title.trim().is_empty() {
        let style = if tone == PanelTone::Focus {
            t.type_scale.section_title.add_modifier(Modifier::BOLD)
        } else {
            t.type_scale.metadata
        };
        // The panel owns its title padding, so every frame reads the same:
        // `┌ chat ─┐`, never `┌chat──┐`. Callers that pass an already-padded
        // title get the same result, which is what let Agent's `" chat "` and
        // Workbench's `"Workspaces"` converge on one call.
        b = b.title(Line::styled(format!(" {} ", title.trim()), style));
    }
    b
}

/// Render a bordered panel and hand its interior to `body`.
///
/// `body` receives the *padded* interior, so a component never has to know the
/// border width or the gutter. Passing a degenerate area skips the call rather
/// than rendering into nothing.
pub fn render_panel<F>(
    frame: &mut Frame,
    area: Rect,
    t: &Tokens,
    title: &str,
    tone: PanelTone,
    body: F,
) where
    F: FnOnce(&mut Frame, Rect),
{
    let block = panel(t, title, tone);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width > 0 && inner.height > 0 {
        body(frame, inner);
    }
}

/// Semantic badge tone. A tone is a *meaning*, so the text marker that comes
/// with it is what actually communicates state on a monochrome terminal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BadgeTone {
    #[default]
    Neutral,
    Muted,
    Accent,
    Success,
    Warning,
    Danger,
    Info,
    Running,
    Blocked,
    Pending,
}

/// The text marker for a tone — the accessibility half of every status color.
///
/// `unicode`/`ascii` pairs mirror the app-wide glyph policy: a terminal that
/// cannot render the Unicode mark gets an ASCII stand-in rather than mojibake.
/// Pick the glyph pair for the current terminal's capabilities.
fn glyph(unicode: &'static str, ascii: &'static str) -> &'static str {
    crate::run::g(unicode, ascii)
}

impl BadgeTone {
    /// Status color.
    pub fn color(self, t: &Tokens) -> ratatui::style::Color {
        match self {
            Self::Neutral => t.text.secondary,
            Self::Muted => t.text.muted,
            Self::Accent => t.accent.primary,
            Self::Success => t.status.success,
            Self::Warning => t.status.warning,
            Self::Danger => t.status.danger,
            Self::Info => t.status.info,
            Self::Running => t.status.running,
            Self::Blocked => t.status.blocked,
            Self::Pending => t.status.pending,
        }
    }
}

/// One badge: marker + label, both present so state survives color loss.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Badge {
    pub text: String,
    pub tone: BadgeTone,
}

impl Badge {
    pub fn new(text: impl Into<String>, tone: BadgeTone) -> Self {
        Self {
            text: text.into(),
            tone,
        }
    }

    /// The status marker for this badge's tone, on its own.
    ///
    /// A row that puts something *between* the marker and the label — a tool
    /// name, say — uses this instead of [`Badge::spans`], so the marker and the
    /// label stay bound to the same tone without being welded together.
    pub fn marker(&self) -> &'static str {
        // Status tones read their marker from the shared state table rather than
        // picking one here. `Danger` and `Blocked` used to share a glyph, which
        // made a denied request and a missing one identical on a monochrome
        // terminal; the table keeps them apart and its uniqueness tests fail if
        // they ever merge again.
        use Semantics as S;
        // `marker()` is `None` only for the two positional states, which no
        // status tone maps to; the fallback is unreachable, not a silent blank.
        fn m(s: S) -> &'static str {
            s.marker().unwrap_or(" ")
        }
        match self.tone {
            BadgeTone::Success => m(S::Success),
            BadgeTone::Danger => m(S::Failed),
            BadgeTone::Blocked => m(S::Blocked),
            BadgeTone::Warning => m(S::Warning),
            BadgeTone::Running => m(S::Running),
            BadgeTone::Pending => m(S::Pending),
            // Not status states: a badge's `Info`/`Accent`/`Muted`/`Neutral` is
            // emphasis, so it has no word in the state vocabulary.
            BadgeTone::Info => glyph("i", "i"),
            BadgeTone::Accent => glyph("●", "*"),
            BadgeTone::Muted => glyph("·", "."),
            BadgeTone::Neutral => glyph(" ", " "),
        }
    }

    /// Spans for inline use inside a larger line.
    pub fn spans(&self, t: &Tokens) -> Vec<Span<'static>> {
        let c = self.tone.color(t);
        vec![
            Span::styled(format!("{} ", self.marker()), Style::default().fg(c)),
            Span::styled(self.text.clone(), Style::default().fg(c)),
        ]
    }

    /// The same badge, emphasised.
    ///
    /// Reserved for a row where the badges *are* the content — the Chat
    /// dashboard strip, where nothing else carries emphasis. Panels and cards
    /// use [`Badge::spans`]: bolding every badge in a panel turns the whole
    /// surface into one weight and gives the reader nothing to separate.
    pub fn spans_bold(&self, t: &Tokens) -> Vec<Span<'static>> {
        let c = self.tone.color(t);
        let bold = Style::default().fg(c).add_modifier(Modifier::BOLD);
        vec![
            Span::styled(format!("{} ", self.marker()), bold),
            Span::styled(self.text.clone(), bold),
        ]
    }

    /// Rendered width, so callers can lay out around it without measuring
    /// twice.
    /// Columns this badge actually paints: marker, one space, label.
    ///
    /// The marker has to be *measured*, not assumed. It used to add a flat 2,
    /// which is right only for a one-column glyph like `✓` — and the ASCII
    /// replacements are wider (`[OK]`, `[X]`, `[!]`). Under-counting clipped the
    /// right-aligned badge to its marker and silently dropped the label, which
    /// is the one part of a badge that carries the state in words.
    pub fn width(&self) -> u16 {
        let marker = crate::design::text::display_width(self.marker()) as u16;
        let text = crate::design::text::display_width(&self.text) as u16;
        marker.saturating_add(1).saturating_add(text)
    }
}

/// A full-width rule.
///
/// Lives in the shared header because a horizontal rule inside a *bordered*
/// panel is redundant — the border already separates. This is for the places
/// that divide content without a frame, like the header's second row.
pub fn divider(t: &Tokens, width: u16) -> Line<'static> {
    let c = crate::run::g("\u{2500}", "-");
    Line::from(Span::styled(
        c.repeat(width as usize),
        Style::default().fg(t.borders.subtle),
    ))
}

/// A key hint chip: `r retry`, with the key emphasized and the label muted.
///
/// The key is a `&str`, not a `char`, because the bindings this has to name are
/// not all single keys: `Ctrl+K`, `F1`, `PgUp` and `Esc` are one binding each
/// and must read as one chip. Taking a `char` forced those callers back into
/// hand-written text, which is how hint rows drifted apart in the first place.
pub fn key_hint(t: &Tokens, key: &str, label: &str) -> Vec<Span<'static>> {
    let key = sanitize_field(key);
    let label = sanitize_field(label);
    vec![
        Span::styled(
            format!(" {key} "),
            Style::default().fg(t.text.inverse).bg(t.accent.muted),
        ),
        Span::styled(format!(" {label} "), t.type_scale.metadata),
    ]
}

/// A row of key hint chips joined by a separator: `r retry · Esc dismiss`.
///
/// One function for the footer row every card, overlay and field ends with, so
/// a new screen inherits the spacing and chip styling instead of re-deriving it.
pub fn key_hints(t: &Tokens, hints: &[(&str, &str)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, (key, label)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("· ", t.type_scale.metadata));
        }
        spans.extend(key_hint(t, key, label));
    }
    spans
}

/// A label/value metric. `value` is bold; the label is metadata.
pub fn metric(t: &Tokens, label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label} "), t.type_scale.metadata),
        Span::styled(value.to_string(), t.type_scale.metric),
    ])
}

/// A compact meter. `frac` is clamped to `[0, 1]`; a non-finite value renders
/// empty rather than panicking or drawing nonsense.
pub fn progress_bar(frac: f64, width: u16) -> String {
    if width == 0 {
        return String::new();
    }
    let f = if frac.is_finite() {
        frac.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let filled = ((f * width as f64).round() as u16).min(width);
    let bar = crate::run::g("█", "#");
    let rest = crate::run::g("░", ".");
    format!(
        "{}{}",
        bar.repeat(filled as usize),
        rest.repeat(saturating_sub(width, filled) as usize)
    )
}

/// Frame-cycled spinner glyph. Index wraps, so any frame count is safe.
pub fn spinner(frame: usize) -> &'static str {
    const FRAMES: [&str; 4] = ["⠋", "⠙", "⠹", "⠸"];
    const ASCII: [&str; 4] = ["|", "/", "-", "\\"];
    if crate::run::ascii_mode() {
        ASCII[frame % ASCII.len()]
    } else {
        FRAMES[frame % FRAMES.len()]
    }
}

/// A horizontal tab strip. Overflow is truncated from the right, and the
/// active tab is always kept visible by the caller passing a scrolled window.
pub fn tab_bar(t: &Tokens, labels: &[&str], active: usize) -> Line<'static> {
    let mut spans = Vec::with_capacity(labels.len() * 2);
    for (i, label) in labels.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(
                crate::run::g(" │ ", " | "),
                Style::default().fg(t.borders.subtle),
            ));
        }
        let style = if i == active {
            t.type_scale.page_title
        } else {
            t.type_scale.metadata
        };
        spans.push(Span::styled(
            format!(" {} ", label),
            if i == active {
                style.bg(t.interaction.selected)
            } else {
                style
            },
        ));
    }
    if spans.is_empty() {
        spans.push(Span::styled(" ", t.type_scale.metadata));
    }
    Line::from(spans)
}

/// One interactive control.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ButtonSpec {
    /// Stable identity for mouse hit-testing and for action dispatch.
    pub id: &'static str,
    pub label: String,
    /// Direct key that activates it, if any.
    pub hint: Option<char>,
    /// Disabled controls render in the disabled role and are not clickable.
    pub enabled: bool,
}

impl ButtonSpec {
    pub fn new(id: &'static str, label: impl Into<String>, hint: Option<char>) -> Self {
        Self {
            id,
            label: label.into(),
            hint,
            enabled: true,
        }
    }

    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }

    /// Minimum columns this control occupies, so a group can be measured
    /// before it is drawn.
    pub fn min_width(&self) -> u16 {
        let label =
            u16::try_from(crate::design::text::display_width(&self.label)).unwrap_or(u16::MAX);
        let key = match self.hint {
            Some(_) => 3, // " k "
            None => 0,
        };
        4 + label + key
    }
}

/// A row of buttons with one selected.
///
/// The selected control is marked with `[ ]`/`[x]` plus the focus ring, so the
/// selection is legible in a monochrome terminal and does not rely on a fill.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ButtonGroup {
    pub specs: Vec<ButtonSpec>,
    pub selected: usize,
}

impl ButtonGroup {
    pub fn new(specs: Vec<ButtonSpec>) -> Self {
        Self { specs, selected: 0 }
    }

    /// Move the selection, clamped. Out-of-range input is a no-op, never a
    /// panic — a stale index from a previous, longer dialog is expected.
    pub fn select(&mut self, i: usize) {
        if i < self.specs.len() {
            self.selected = i;
        }
    }

    pub fn next(&mut self) {
        if !self.specs.is_empty() {
            self.selected = (self.selected + 1) % self.specs.len();
        }
    }

    pub fn prev(&mut self) {
        if !self.specs.is_empty() {
            self.selected = if self.selected == 0 {
                self.specs.len() - 1
            } else {
                self.selected - 1
            };
        }
    }

    /// Activate by direct key. Returns the pressed control's id.
    pub fn press_key(&mut self, key: char) -> Option<&'static str> {
        let hit = self
            .specs
            .iter()
            .position(|s| s.enabled && s.hint == Some(key))?;
        self.selected = hit;
        Some(self.specs[hit].id)
    }

    pub fn press_mouse(&mut self, hits: &HitTargets, x: u16, y: u16) -> Option<&'static str> {
        if let HitTarget::Button { id } = hits.hit(x, y) {
            if let Some(i) = self.specs.iter().position(|s| s.id == id && s.enabled) {
                self.selected = i;
                return Some(id);
            }
        }
        None
    }

    /// Activate the selected control. Returns `None` on an empty group or a
    /// disabled selection — an unreachable action must not fire.
    pub fn press_selected(&self) -> Option<&'static str> {
        self.specs
            .get(self.selected)
            .filter(|s| s.enabled)
            .map(|s| s.id)
    }

    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    /// Total width including separators and the selection brackets.
    pub fn width(&self) -> u16 {
        if self.specs.is_empty() {
            return 0;
        }
        let sum: u16 = self.specs.iter().map(|s| s.min_width()).sum();
        sum + self.specs.len() as u16 - 1 + 2 // separators + selection brackets
    }

    /// How many rows the group needs to show every button at `width`.
    ///
    /// A caller reserving a fixed single row for its actions will silently drop
    /// the trailing buttons on a narrow terminal. That is harmless for a pair of
    /// dialog buttons and *not* harmless for a permission card, where the button
    /// that gets dropped is "Deny" — leaving three clickable grants and no
    /// clickable way to refuse.
    pub fn rows_needed(&self, width: u16) -> u16 {
        if width == 0 || self.specs.is_empty() {
            return 0;
        }
        let mut rows = 1u16;
        let mut used = 0u16;
        for spec in &self.specs {
            let w = spec.min_width();
            let need = if used == 0 { w } else { used + 1 + w };
            if need > width && used != 0 {
                rows = rows.saturating_add(1);
                used = w;
            } else {
                used = need;
            }
        }
        rows
    }

    /// Render every button, wrapping onto further rows rather than dropping the
    /// ones that do not fit.
    ///
    /// Returns the number of rows actually used, which is
    /// `min(rows_needed(area.width), area.height)`. A button that would need a
    /// row the area does not have is still not drawn — wrapping must not become a
    /// way to paint outside the reserved region — so a caller that cares should
    /// reserve [`ButtonGroup::rows_needed`] rows.
    pub fn render_wrapped(
        &self,
        frame: &mut Frame,
        area: Rect,
        t: &Tokens,
        hits: &mut HitTargets,
    ) -> u16 {
        if area.width == 0 || area.height == 0 || self.specs.is_empty() {
            return 0;
        }
        let mut x = area.x;
        let mut y = area.y;
        let mut used = 0u16;
        let mut rows_used = 1u16;
        for (i, spec) in self.specs.iter().enumerate() {
            let selected = i == self.selected;
            let mark = if selected {
                crate::run::g("▸", ">")
            } else {
                " "
            };
            let hint = match spec.hint {
                Some(k) => format!(" {k}"),
                None => String::new(),
            };
            let label = format!(" {mark} {}{hint} ", spec.label);
            let w = u16::try_from(crate::design::text::display_width(&label)).unwrap_or(u16::MAX);
            let need = if used == 0 { w } else { used + 1 + w };
            if need > area.width {
                if used == 0 {
                    // A single button wider than the whole row: drawing it would
                    // overflow, so it is skipped rather than clipped.
                    continue;
                }
                if y.saturating_add(1) >= area.y.saturating_add(area.height) {
                    break; // no row left; do not paint outside the area
                }
                y += 1;
                rows_used += 1;
                x = area.x;
                used = 0;
            }
            if used != 0 {
                x += 1;
                used += 1;
            }
            let style = if !spec.enabled {
                Style::default().fg(t.interaction.disabled)
            } else if selected {
                t.type_scale
                    .body
                    .bg(t.interaction.selected)
                    .fg(t.text.inverse)
                    .add_modifier(Modifier::BOLD)
            } else {
                t.type_scale.secondary_body
            };
            let rect = Rect {
                x,
                y,
                width: w.min(saturating_sub(area.x.saturating_add(area.width), x)),
                height: 1,
            };
            if rect.width == 0 {
                break;
            }
            hits.register(rect, HitTarget::Button { id: spec.id });
            frame.render_widget(Line::from(Span::styled(label, style)), rect);
            x += w;
            used += w;
        }
        rows_used
    }

    /// Render the row. Returns the width actually used, which may be less than
    /// [`ButtonGroup::width`] on a narrow terminal.
    pub fn render(&self, frame: &mut Frame, area: Rect, t: &Tokens, hits: &mut HitTargets) -> u16 {
        if area.width == 0 || area.height == 0 || self.specs.is_empty() {
            return 0;
        }
        let mut x = area.x;
        let mut used = 0u16;
        for (i, spec) in self.specs.iter().enumerate() {
            let selected = i == self.selected;
            let mark = if selected {
                crate::run::g("▸", ">")
            } else {
                " "
            };
            let hint = match spec.hint {
                Some(k) => format!(" {k}"),
                None => String::new(),
            };
            let label = format!(" {mark} {}{hint} ", spec.label);
            let w = u16::try_from(crate::design::text::display_width(&label)).unwrap_or(u16::MAX);
            if used + w > area.width {
                // No room for this control: stop rather than draw a clipped
                // label the operator cannot read.
                break;
            }
            let style = if !spec.enabled {
                Style::default().fg(t.interaction.disabled)
            } else if selected {
                t.type_scale
                    .body
                    .bg(t.interaction.selected)
                    .fg(t.text.inverse)
                    .add_modifier(Modifier::BOLD)
            } else {
                t.type_scale.secondary_body
            };
            let rect = Rect {
                x,
                y: area.y,
                width: w,
                height: 1,
            };
            hits.register(rect, HitTarget::Button { id: spec.id });
            frame.render_widget(Line::from(Span::styled(label, style)), rect);
            x += w + 1;
            used += w + 1;
        }
        saturating_sub(used, 1)
    }
}

/// What a badge row does when the badges are wider than the row.
///
/// This is a policy, not an implementation detail: "show nothing" and "show
/// what fits" are different answers with very different safety consequences,
/// so the caller states which one it wants instead of each call site inventing
/// its own overflow behaviour.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BadgeFit {
    /// Render nothing unless the whole set fits.
    ///
    /// Right for a badge set that only means something as a complete unit.
    #[default]
    All,
    /// Render the leftmost badges that fit, dropping from the right.
    ///
    /// Right for ordered rows whose safety-critical items come first: the
    /// caller puts the kill switch at the left, so a narrow terminal loses the
    /// session label rather than the halt reason.
    Prefix,
}

/// How loudly a badge row is drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BadgeEmphasis {
    /// Share the weight of the surrounding text.
    #[default]
    Plain,
    /// Bold. For a row where the badges *are* the content.
    Bold,
}

/// How a badge row is anchored, and what it does when it does not fit.
///
/// These are caller decisions rather than implementation details: "show
/// nothing", "show what fits", and "start from the right" have very different
/// consequences depending on what the row is for, and a shared primitive that
/// picks one for every screen is how a kill-switch badge ends up dropped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BadgeRow {
    /// Anchor to the right edge instead of the left.
    pub right_align: bool,
    pub fit: BadgeFit,
    pub emphasis: BadgeEmphasis,
}

impl BadgeRow {
    /// The common case: a panel or header row that shows nothing rather than
    /// showing a partial set.
    pub fn plain() -> Self {
        Self::default()
    }

    /// A dense one-row strip ordered safety-first: left-aligned, drops from the
    /// right, and bold because nothing else on the row carries emphasis.
    pub fn strip() -> Self {
        Self {
            right_align: false,
            fit: BadgeFit::Prefix,
            emphasis: BadgeEmphasis::Bold,
        }
    }
}

/// Render a set of badges on one row, truncating rather than wrapping.
///
/// Returns the width consumed, so a caller that reserved space can reuse the
/// remainder.
pub fn render_badges(
    frame: &mut Frame,
    area: Rect,
    t: &Tokens,
    badges: &[Badge],
    row: BadgeRow,
) -> u16 {
    if area.width == 0 || area.height == 0 || badges.is_empty() {
        return 0;
    }
    let width_of = |b: &Badge| b.width() + 1;
    let shown: Vec<&Badge> = match row.fit {
        BadgeFit::All => {
            let total: u16 = badges.iter().map(|b| width_of(b)).sum();
            if total > area.width {
                return 0;
            }
            badges.iter().collect()
        }
        BadgeFit::Prefix => badges
            .iter()
            .scan(0u16, |used, b| {
                let next = *used + width_of(b);
                if next <= area.width {
                    *used = next;
                    Some(b)
                } else {
                    None
                }
            })
            .collect(),
    };
    if shown.is_empty() {
        return 0;
    }
    let total: u16 = shown.iter().map(|b| width_of(b)).sum();
    let start = if row.right_align {
        area.x + saturating_sub(area.width, total)
    } else {
        area.x
    };
    let mut x = start;
    for b in shown {
        let w = b.width();
        let spans = match row.emphasis {
            BadgeEmphasis::Plain => b.spans(t),
            BadgeEmphasis::Bold => b.spans_bold(t),
        };
        frame.render_widget(
            Line::from(spans),
            Rect {
                x,
                y: area.y,
                width: w,
                height: 1,
            },
        );
        x += w + 1;
    }
    saturating_sub(total, 1)
}

/// Clear an area to the given surface — the prerequisite for floating chrome,
/// so a modal never shows terminal content through its own background.
pub fn scrim(frame: &mut Frame, area: Rect, color: ratatui::style::Color) {
    if area.width > 0 && area.height > 0 {
        frame.render_widget(Clear, area);
        frame.render_widget(Block::default().style(Style::default().bg(color)), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{Theme, ThemeKind};

    /// `BadgeTone::Danger` and `BadgeTone::Blocked` once shared `glyph("✗", "x")`,
    /// so a denied request and a missing one were the same mark on a terminal
    /// with no hue. Badges now take their markers from `Semantics`, so pin the
    /// consequence where the regression would happen: the widget, not the table.
    #[test]
    fn a_failed_badge_and_a_blocked_badge_are_never_the_same_mark() {
        let _guard = crate::run::ascii_mode_lock();
        for ascii in [false, true] {
            crate::run::set_ascii_mode(ascii);
            let failed = Badge::new("FAILED", BadgeTone::Danger).marker();
            let blocked = Badge::new("BLOCKED", BadgeTone::Blocked).marker();
            assert_ne!(
                failed, blocked,
                "ascii={ascii}: danger and blocked badges share {failed:?}",
            );
            assert_eq!(failed, Semantics::Failed.marker().unwrap());
            assert_eq!(blocked, Semantics::Blocked.marker().unwrap());
            // And every status badge still carries a mark rather than a blank.
            for tone in [
                BadgeTone::Success,
                BadgeTone::Danger,
                BadgeTone::Blocked,
                BadgeTone::Warning,
                BadgeTone::Running,
                BadgeTone::Pending,
            ] {
                let m = Badge::new("X", tone).marker();
                assert!(
                    !m.is_empty() && m != " ",
                    "ascii={ascii} {tone:?} has no mark"
                );
            }
        }
    }

    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn tokens() -> Tokens {
        Theme::default_theme().tokens()
    }

    fn r(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    /// Render into a `TestBackend` and return the visible text, row by row.
    fn draw(width: u16, height: u16, f: impl FnOnce(&mut Frame, Rect)) -> String {
        let mut term =
            Terminal::new(TestBackend::new(width.max(1), height.max(1))).expect("test terminal");
        term.draw(|frame| f(frame, frame.area())).expect("draw");
        let buf = term.backend().buffer().clone();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn every_theme_draws_every_badge_tone_without_panicking() {
        for kind in ThemeKind::ALL {
            let t = Theme::new(kind).tokens();
            for tone in [
                BadgeTone::Neutral,
                BadgeTone::Muted,
                BadgeTone::Accent,
                BadgeTone::Success,
                BadgeTone::Warning,
                BadgeTone::Danger,
                BadgeTone::Info,
                BadgeTone::Running,
                BadgeTone::Blocked,
                BadgeTone::Pending,
            ] {
                let b = Badge::new("STATE", tone);
                let out = draw(24, 1, |f, a| f.render_widget(Line::from(b.spans(&t)), a));
                assert!(
                    out.contains("STATE"),
                    "{}/{tone:?} lost its label",
                    kind.name()
                );
            }
        }
    }

    #[test]
    fn a_blocked_badge_keeps_its_label_in_monochrome() {
        let t = Theme::new(ThemeKind::Monochrome).tokens();
        let b = Badge::new("RESOURCE BLOCKED", BadgeTone::Blocked);
        let out = draw(30, 1, |f, a| f.render_widget(Line::from(b.spans(&t)), a));
        assert!(out.contains("RESOURCE BLOCKED"));
    }

    #[test]
    fn badges_render_nothing_rather_than_overflowing() {
        let t = tokens();
        let badges: Vec<Badge> = (0..10)
            .map(|i| Badge::new(format!("B{i}"), BadgeTone::Info))
            .collect();
        let mut used = 0;
        draw(12, 1, |f, _| {
            used = render_badges(f, r(0, 0, 12, 1), &t, &badges, BadgeRow::plain());
        });
        assert_eq!(
            used, 0,
            "an over-wide badge row must render nothing, not overflow"
        );
    }

    #[test]
    fn a_prefix_fitted_badge_row_keeps_the_leftmost_badges() {
        // This is the safety property the Chat dashboard strip depends on: the
        // kill-switch badge is pushed first, so narrowing the row must drop the
        // trailing badges, never the leading one.
        let t = tokens();
        let badges = vec![
            Badge::new("HALT: disk", BadgeTone::Danger),
            Badge::new("session-7", BadgeTone::Accent),
            Badge::new("model qwen3", BadgeTone::Muted),
        ];
        let out = draw(16, 1, |f, _| {
            render_badges(f, r(0, 0, 16, 1), &t, &badges, BadgeRow::strip());
        });
        assert!(out.contains("HALT"), "{out:?}");
        assert!(!out.contains("qwen3"), "{out:?}");
    }

    /// `Badge::width()` under-counting does not fail loudly: the row simply
    /// clips. Assert the two numbers are the same number, in both modes, so a
    /// wider ASCII marker can never quietly cost a badge its label again.
    #[test]
    fn a_badge_reserves_exactly_the_columns_it_paints_in_both_modes() {
        let _guard = crate::run::ascii_mode_lock();
        let t = tokens();
        for ascii in [false, true] {
            crate::run::set_ascii_mode(ascii);
            for tone in [
                BadgeTone::Success,
                BadgeTone::Danger,
                BadgeTone::Blocked,
                BadgeTone::Warning,
                BadgeTone::Running,
                BadgeTone::Pending,
                BadgeTone::Info,
            ] {
                let b = Badge::new("BLOCKED", tone);
                let painted: u16 = b
                    .spans(&t)
                    .iter()
                    .map(|s| crate::design::text::display_width(s.content.as_ref()) as u16)
                    .sum();
                assert_eq!(
                    b.width(),
                    painted,
                    "ascii={ascii} {tone:?}: reserves {} but paints {painted:?}",
                    b.width(),
                );
                assert!(
                    b.width() <= 40,
                    "ascii={ascii} {tone:?} marker {:?} is absurdly wide",
                    b.marker(),
                );
            }
        }
    }

    #[test]
    fn right_aligned_badges_sit_at_the_far_edge() {
        let t = tokens();
        let badges = vec![Badge::new("OK", BadgeTone::Success)];
        let out = draw(20, 1, |f, _| {
            render_badges(
                f,
                r(0, 0, 20, 1),
                &t,
                &badges,
                BadgeRow {
                    right_align: true,
                    ..BadgeRow::plain()
                },
            );
        });
        let line = out.lines().next().unwrap_or("");
        assert!(
            line.trim_end().ends_with("OK"),
            "not right aligned: {line:?}"
        );
    }

    #[test]
    fn a_divider_fills_its_width() {
        let t = tokens();
        let w: usize = divider(&t, 20)
            .spans
            .iter()
            .map(|s| s.content.chars().count())
            .sum();
        assert_eq!(w, 20);
        assert_eq!(
            divider(&t, 0)
                .spans
                .iter()
                .map(|s| crate::design::text::display_width(s.content.as_ref()))
                .sum::<usize>(),
            0
        );
    }

    #[test]
    fn a_metric_shows_both_label_and_value() {
        let t = tokens();
        let line = metric(&t, "RAM", "42%");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("RAM") && text.contains("42%"), "{text:?}");
    }

    #[test]
    fn progress_bar_is_bounded_and_handles_nonsense_input() {
        assert_eq!(progress_bar(0.5, 0).len(), 0);
        assert_eq!(progress_bar(-3.0, 10).chars().count(), 10);
        assert_eq!(progress_bar(9.0, 10).chars().count(), 10);
        assert_eq!(progress_bar(f64::NAN, 10).chars().count(), 10);
        assert_eq!(progress_bar(f64::INFINITY, 10).chars().count(), 10);
        let full = crate::run::g("█", "#");
        let empty = crate::run::g("░", ".");
        assert_eq!(progress_bar(1.0, 10).matches(full).count(), 10);
        assert_eq!(progress_bar(0.0, 10).matches(full).count(), 0);
        assert_eq!(progress_bar(0.0, 10).matches(empty).count(), 10);
        // A half-filled meter is exactly half of each glyph.
        let half = progress_bar(0.5, 8);
        assert_eq!(half.matches(full).count(), 4);
        assert_eq!(half.matches(empty).count(), 4);
    }

    #[test]
    fn the_spinner_cycles_for_any_frame_index() {
        for i in 0..1000usize {
            assert!(!spinner(i).is_empty());
        }
        assert_eq!(spinner(0), spinner(4));
    }

    #[test]
    fn a_tab_bar_marks_the_active_tab_and_survives_an_empty_slice() {
        let t = tokens();
        let out = draw(40, 1, |f, a| {
            f.render_widget(tab_bar(&t, &["A", "B", "C"], 1), a)
        });
        assert!(out.contains('B'), "active tab label missing: {out:?}");
        assert!(!tab_bar(&t, &[], 0).spans.is_empty());
    }

    // -- button group ---------------------------------------------------

    fn group() -> ButtonGroup {
        ButtonGroup::new(vec![
            ButtonSpec::new("a", "Allow", Some('y')),
            ButtonSpec::new("b", "Deny", Some('n')),
            ButtonSpec::new("c", "Retry", Some('r')).disabled(),
        ])
    }

    #[test]
    fn a_button_group_selects_moves_and_clamps() {
        let mut g = group();
        assert_eq!(g.press_selected(), Some("a"));
        g.next();
        assert_eq!(g.selected, 1);
        g.next();
        assert_eq!(g.selected, 2);
        g.next();
        assert_eq!(g.selected, 0, "must wrap");
        g.prev();
        assert_eq!(g.selected, 2);
        // A stale index from a previous, longer dialog must be inert.
        g.select(99);
        assert_eq!(g.selected, 2);
    }

    #[test]
    fn a_disabled_control_cannot_be_activated() {
        let mut g = group();
        assert_eq!(g.press_key('r'), None, "disabled key must not fire");
        assert_eq!(g.press_key('y'), Some("a"));
        g.select(2);
        assert_eq!(g.press_selected(), None, "disabled selection must not fire");
    }

    #[test]
    fn an_empty_button_group_is_inert() {
        let mut g = ButtonGroup::default();
        g.next();
        g.prev();
        g.select(3);
        assert_eq!(g.press_selected(), None);
        assert_eq!(g.press_key('y'), None);
        assert_eq!(g.width(), 0);
        assert!(g.is_empty());
    }

    #[test]
    fn key_hints_activate_their_button() {
        let mut g = ButtonGroup::new(vec![
            ButtonSpec::new("allow", "Allow Once", Some('a')),
            ButtonSpec::new("deny", "Deny", Some('d')),
        ]);
        assert_eq!(g.press_key('d'), Some("deny"));
        assert_eq!(g.selected, 1);
        assert_eq!(g.press_key('z'), None);
    }

    #[test]
    fn buttons_render_and_register_hit_targets() {
        let t = tokens();
        let mut g = ButtonGroup::new(vec![
            ButtonSpec::new("retry", "Retry", Some('r')),
            ButtonSpec::new("wait", "Wait", Some('w')),
        ]);
        let mut hits = HitTargets::new();
        let out = draw(40, 1, |f, a| {
            g.render(f, a, &t, &mut hits);
        });
        assert!(out.contains("Retry") && out.contains("Wait"), "{out:?}");
        let wait = hits.region_of("wait").expect("Wait must be clickable");
        assert_eq!(
            g.press_mouse(&hits, wait.x + 1, wait.y),
            Some("wait"),
            "clicking the Wait control must activate it"
        );
        assert_eq!(g.selected, 1);
        // A click in empty space to the right must do nothing.
        assert_eq!(g.press_mouse(&hits, 39, 0), None);
        assert_eq!(g.selected, 1);
    }

    #[test]
    fn buttons_stop_rendering_rather_than_clipping() {
        let t = tokens();
        let g = ButtonGroup::new(vec![
            ButtonSpec::new("a", "Allow Once", Some('a')),
            ButtonSpec::new("b", "Allow Session", Some('s')),
            ButtonSpec::new("c", "Deny", Some('d')),
        ]);
        let mut hits = HitTargets::new();
        let out = draw(14, 1, |f, a| {
            g.render(f, a, &t, &mut hits);
        });
        assert!(
            !out.contains("Deny"),
            "a clipped control must be dropped, not drawn: {out:?}"
        );
    }

    #[test]
    fn rendering_into_a_zero_area_rect_is_a_no_op() {
        let t = tokens();
        let g = ButtonGroup::new(vec![ButtonSpec::new("a", "Allow", Some('a'))]);
        let mut hits = HitTargets::new();
        draw(20, 1, |f, _| {
            g.render(f, r(0, 0, 0, 0), &t, &mut hits);
        });
        assert!(hits.is_empty(), "a clipped control must not stay clickable");
    }

    #[test]
    fn panel_tones_map_to_distinct_borders_where_the_theme_has_hue() {
        for kind in ThemeKind::ALL {
            let t = Theme::new(kind).tokens();
            assert_ne!(
                tone_border(&t, PanelTone::Default),
                tone_border(&t, PanelTone::Active),
                "{}: resting and active borders are identical",
                kind.name()
            );
            // Monochrome has one family by construction: a destructive dialog
            // is distinguished there by its title and its risk label, which
            // `a_destructive_dialog_states_its_meaning_in_words` covers. Every
            // other theme must separate focus from destructive by color.
            if !kind.is_monochrome() {
                assert_ne!(
                    tone_border(&t, PanelTone::Focus),
                    tone_border(&t, PanelTone::Destructive),
                    "{}: focus and destructive borders are identical",
                    kind.name()
                );
            }
        }
    }

    #[test]
    fn a_panel_footer_row_can_be_built_from_key_hints() {
        // `panel_with_footer` was removed: every footer in the app is either a
        // static note or a set of key bindings, and a `&str` footer could only
        // express the first. Panels now compose `panel` with a `key_hints` row,
        // which is what the two live call sites do.
        let t = tokens();
        let out = draw(40, 3, |f, a| {
            let block = panel(&t, "Files", PanelTone::Default);
            let inner = block.inner(a);
            f.render_widget(block, a);
            let mut spans = vec![Span::raw(" ")];
            spans.extend(key_hints(&t, &[("i", "import"), ("s", "scan")]));
            f.render_widget(Line::from(spans), inner);
        });
        assert!(out.contains("Files"), "{out:?}");
        assert!(out.contains("import"), "{out:?}");
        assert!(out.contains("scan"), "{out:?}");
    }

    #[test]
    fn render_panel_hands_the_body_a_padded_interior() {
        let t = tokens();
        let mut seen: Option<Rect> = None;
        draw(40, 6, |f, a| {
            render_panel(f, a, &t, "X", PanelTone::Focus, |_f, inner| {
                seen = Some(inner);
            });
        });
        let inner = seen.expect("body was never called");
        assert!(inner.width > 0 && inner.height > 0, "{inner:?}");
        // Padded past the border: interior starts inside the frame.
        assert!(inner.x >= 1 && inner.y >= 1, "{inner:?}");
        assert!(inner.width <= 40 - 2, "{inner:?}");
    }

    #[test]
    fn render_panel_skips_the_body_when_there_is_no_interior() {
        let t = tokens();
        let mut called = false;
        draw(10, 1, |f, _| {
            render_panel(
                f,
                r(0, 0, 2, 1),
                &t,
                "X",
                PanelTone::Default,
                |_f, _inner| {
                    called = true;
                },
            );
        });
        assert!(
            !called,
            "body must not be called with a degenerate interior"
        );
    }
}
