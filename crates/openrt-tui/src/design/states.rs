//! The four screen states: Loading, Empty, Error, Info.
//!
//! Every list view, panel and overlay eventually hits one of these, and the
//! difference between a calm "nothing here yet" and a loud "this failed" is
//! most of what makes an interface feel finished. Getting them wrong is how a
//! TUI ends up showing an empty box where a tool crashed.
//!
//! All four share one shape — a title, an optional explanation, and optional
//! actions — so a view can hand its worst moment to this module instead of
//! hand-rolling an empty panel:
//!
//! ```text
//! ErrorState   something failed          -> never swallowed, always retryable
//! LoadingState work in progress          -> says what it is waiting for
//! EmptyState   the answer is "none"     -> says why that is not a failure
//! InfoState    a capability is missing  -> states the limitation, no apology
//! ```

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::layout::HitTargets;
use super::tokens::saturating_sub;
use super::tokens::Tokens;
use super::widgets::{ellipsize, sanitize_field, Badge, BadgeTone, ButtonGroup, ButtonSpec};

/// Which state a region is in. Views match on this to decide what to draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateKind {
    Loading,
    Empty,
    Error,
    Info,
}

impl StateKind {
    /// The marker glyph — always present so the state survives monochrome.
    fn marker(self) -> &'static str {
        match self {
            Self::Loading => crate::run::g("◌", "~"),
            Self::Empty => crate::run::g("○", "o"),
            Self::Error => crate::run::g("✗", "x"),
            Self::Info => crate::run::g("i", "i"),
        }
    }

    /// The word this state is named by.
    ///
    /// Public because a surface that summarises a state — a panel title, a header
    /// badge — must use these words and not invent its own. Two vocabularies for
    /// "the read failed" means an operator has to learn that `UNAVAILABLE` on a
    /// title and `FAILED` in the body are the same news.
    pub fn label(self) -> &'static str {
        match self {
            Self::Loading => "LOADING",
            Self::Empty => "NOTHING HERE",
            Self::Error => "FAILED",
            Self::Info => "INFO",
        }
    }

    fn tone(self) -> BadgeTone {
        match self {
            Self::Loading => BadgeTone::Running,
            Self::Empty => BadgeTone::Muted,
            Self::Error => BadgeTone::Danger,
            Self::Info => BadgeTone::Info,
        }
    }

    fn color(self, t: &Tokens) -> ratatui::style::Color {
        match self {
            Self::Loading => t.status.running,
            Self::Empty => t.text.muted,
            Self::Error => t.status.danger,
            Self::Info => t.status.info,
        }
    }
}

/// A screen state, ready to render.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    pub kind: StateKind,
    /// One line. What happened, in the operator's vocabulary.
    pub title: String,
    /// Optional second line: the cause, the next step, or the limitation.
    pub detail: Option<String>,
    /// Optional actions, e.g. Retry.
    pub actions: ButtonGroup,
}

impl State {
    pub fn new(kind: StateKind, title: impl Into<String>) -> Self {
        Self {
            kind,
            title: sanitize_field(title),
            detail: None,
            actions: ButtonGroup::default(),
        }
    }

    pub fn loading(title: impl Into<String>) -> Self {
        Self::new(StateKind::Loading, title)
    }

    pub fn empty(title: impl Into<String>) -> Self {
        Self::new(StateKind::Empty, title)
    }

    pub fn error(title: impl Into<String>) -> Self {
        Self::new(StateKind::Error, title)
    }

    /// An unavailable capability, stated plainly with its reason. The design
    /// rule: never fake a capability and never hide the absence.
    pub fn unavailable(capability: impl AsRef<str>, reason: impl Into<String>) -> Self {
        Self::new(
            StateKind::Info,
            format!("UNAVAILABLE: {}", sanitize_field(capability.as_ref())),
        )
        .with_detail(reason)
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        let d = sanitize_field(detail);
        if !d.trim().is_empty() {
            self.detail = Some(d);
        }
        self
    }

    /// Attach a retry affordance. Errors are always offered one; a failure
    /// with no way forward is a dead end.
    pub fn with_retry(self, id: &'static str, label: &'static str, hint: char) -> Self {
        let mut actions = self.actions;
        if actions.press_selected().is_some() || !actions.is_empty() {
            actions.specs.push(ButtonSpec::new(id, label, Some(hint)));
        } else {
            actions = ButtonGroup::new(vec![ButtonSpec::new(id, label, Some(hint))]);
        }
        Self { actions, ..self }
    }

    fn tone(&self, t: &Tokens) -> Style {
        t.type_scale.page_title.fg(self.kind.color(t))
    }

    /// Render centered in `area`. Centering is relative to the available
    /// space, so it degrades correctly from a 180-column terminal down to an
    /// 80x24 one, and from a full-screen panel down to a 3-row strip.
    pub fn render(&self, frame: &mut Frame, area: Rect, t: &Tokens, hits: &mut HitTargets) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let rows_needed =
            1 + usize::from(self.detail.is_some()) + usize::from(!self.actions.is_empty());
        // Not enough room for the full state: show the marker and title only,
        // never a half-drawn explanation that reads as a rendering bug.
        let show_detail = area.height >= 2 && self.detail.is_some();
        let show_actions = area.height >= rows_needed as u16 && !self.actions.is_empty();

        // The state is named in words (`FAILED`, `LOADING`, `UNAVAILABLE`) so
        // it survives a monochrome terminal; the marker and color are
        // reinforcement, not the message.
        let mut lines: Vec<Line> = Vec::new();
        let lead = format!("{} {}", self.kind.marker(), self.kind.label());
        let lead_len = lead.chars().count() as u16;
        let mut spans = vec![Span::styled(
            format!("{lead}  "),
            self.tone(t).add_modifier(ratatui::style::Modifier::BOLD),
        )];
        let budget = area.width.saturating_sub(lead_len + 2).max(1);
        // A statement squeezed into the last few columns beside the label reads
        // as a rendering bug, not as a message: at 21 columns an empty panel
        // showed "NOTHING HERE  no se…", which tells the operator less than the
        // label alone. When the title cannot sit beside the label, it gets its
        // own line — one more row is worth more than a legible sentence.
        //
        // Wide layouts are untouched: the title still shares the label's line
        // whenever it fits with room to be a sentence.
        const MIN_INLINE_TITLE: u16 = 10;
        if budget >= MIN_INLINE_TITLE {
            spans.push(Span::styled(ellipsize(&self.title, budget), self.tone(t)));
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::from(spans));
            if area.height >= 2 {
                lines.push(Line::from(Span::styled(
                    ellipsize(&self.title, area.width),
                    self.tone(t),
                )));
            }
        }
        if show_detail {
            if let Some(d) = &self.detail {
                lines.push(Line::from(Span::styled(
                    ellipsize(d, area.width.max(1)),
                    t.type_scale.metadata,
                )));
            }
        }
        frame.render_widget(Paragraph::new(lines), area);

        if show_actions {
            let row = Rect {
                x: area.x,
                y: area.y + saturating_sub(area.height, 1),
                width: area.width,
                height: 1,
            };
            self.actions.render(frame, row, t, hits);
        }
    }
}

/// Build the correct state for an arbitrary collection.
///
/// The rule this encodes: an empty list is only `Empty` when the source
/// succeeded. A failed load is `Error`, and the distinction must survive all
/// the way to the screen — a silently-empty list reads as "there is nothing",
/// which is a lie about a broken capability.
pub fn for_collection(loaded: bool, failed: Option<&str>, count: usize, noun: &str) -> State {
    if let Some(e) = failed {
        return State::error(format!("could not load {noun}"))
            .with_detail(e)
            .with_retry("state.retry", "Retry", 'r');
    }
    if !loaded {
        return State::loading(format!("loading {noun}…"));
    }
    if count == 0 {
        return State::empty(format!("no {noun} yet"));
    }
    // Non-empty: there is nothing to state; the caller draws the list.
    State::new(StateKind::Info, "")
}

/// Render the state appropriate to a collection, or nothing when there is a
/// list to draw. Keeps every list view honest with one call.
pub fn render_collection(
    frame: &mut Frame,
    area: Rect,
    t: &Tokens,
    hits: &mut HitTargets,
    loaded: bool,
    failed: Option<&str>,
    count: usize,
    noun: &str,
) -> bool {
    if loaded && failed.is_none() && count > 0 {
        return false; // nothing to say; draw the list
    }
    for_collection(loaded, failed, count, noun).render(frame, area, t, hits);
    true
}

/// A compact badge for a state, for use inside a header or a card header.
pub fn state_badge(kind: StateKind, text: impl Into<String>) -> Badge {
    Badge::new(text, kind.tone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{Theme, ThemeKind};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn t() -> Tokens {
        Theme::default_theme().tokens()
    }

    fn draw(width: u16, height: u16, f: impl FnOnce(&mut Frame, Rect, &mut HitTargets)) -> String {
        let mut term = Terminal::new(TestBackend::new(width.max(1), height.max(1))).expect("term");
        term.draw(|frame| {
            let area = frame.area();
            let mut hits = HitTargets::new();
            f(frame, area, &mut hits);
        })
        .expect("draw");
        let buf = term.backend().buffer().clone();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn each_state_states_its_kind_in_words() {
        let cases = [
            (StateKind::Loading, "LOADING"),
            (StateKind::Empty, "NOTHING HERE"),
            (StateKind::Error, "FAILED"),
            (StateKind::Info, "INFO"),
        ];
        for (kind, word) in cases {
            assert_eq!(kind.label(), word);
            assert!(!kind.marker().is_empty(), "{kind:?} has no marker");
        }
    }

    #[test]
    fn an_unavailable_capability_is_stated_with_its_reason() {
        let s = State::unavailable("audio transcription", "no whisper on PATH");
        assert!(s.title.starts_with("UNAVAILABLE:"), "{}", s.title);
        assert!(s.title.contains("audio transcription"));
        assert_eq!(s.detail.as_deref(), Some("no whisper on PATH"));
    }

    #[test]
    fn a_failed_load_is_never_reported_as_empty() {
        // The whole point of for_collection: a broken source must not read as
        // "there is nothing here".
        let s = for_collection(true, Some("store unavailable"), 0, "sessions");
        assert_eq!(s.kind, StateKind::Error);
        assert!(!s.actions.is_empty(), "a failure must offer a way forward");
    }

    #[test]
    fn a_pending_load_is_not_an_error_and_not_empty() {
        let s = for_collection(false, None, 0, "models");
        assert_eq!(s.kind, StateKind::Loading);
        let e = for_collection(true, None, 0, "models");
        assert_eq!(e.kind, StateKind::Empty);
    }

    #[test]
    fn render_collection_draws_nothing_when_there_is_a_real_list() {
        let mut drawn = false;
        let out = draw(40, 3, |f, a, hits| {
            drawn = render_collection(f, a, &t(), hits, true, None, 7, "sessions");
        });
        assert!(!drawn, "a populated collection must not draw a state");
        assert!(out.trim().is_empty(), "{out:?}");
    }

    #[test]
    fn render_collection_reports_each_failure_mode() {
        for (loaded, failed, count, expect) in [
            (false, None, 0, "LOADING"),
            (true, Some("io error"), 0, "FAILED"),
            (true, None, 0, "NOTHING HERE"),
        ] {
            let out = draw(60, 3, |f, a, hits| {
                assert!(render_collection(
                    f,
                    a,
                    &t(),
                    hits,
                    loaded,
                    failed,
                    count,
                    "sessions"
                ));
            });
            assert!(out.contains(expect), "expected {expect} in:\n{out}");
        }
    }

    #[test]
    fn the_error_detail_survives_in_monochrome() {
        let s = State::error("model probe failed").with_detail("connection refused");
        let out = draw(60, 3, |f, a, hits| {
            s.render(f, a, &Theme::new(ThemeKind::Monochrome).tokens(), hits);
        });
        assert!(out.contains("FAILED"));
        assert!(out.contains("connection refused"), "{out}");
    }

    #[test]
    fn a_state_degrades_instead_of_drawing_half_of_itself() {
        let s = State::error("boom")
            .with_detail("a very long explanation that will not fit")
            .with_retry("state.retry", "Retry", 'r');
        // One row: title only, no clipped detail.
        let out = draw(40, 1, |f, a, hits| s.render(f, a, &t(), hits));
        assert!(out.contains("FAILED"));
        assert!(!out.contains("very long"), "clipped detail leaked: {out}");
        // Zero rows: nothing at all.
        draw(40, 1, |f, _a, hits| {
            s.render(f, Rect::default(), &t(), hits)
        });
    }

    #[test]
    fn untrusted_text_cannot_break_the_row_contract() {
        let s = State::error("bad\nvalue\twith\u{7}controls");
        assert!(!s.title.contains('\n'));
        assert!(!s.title.contains('\t'));
        assert!(!s.title.contains('\u{7}'));
    }

    #[test]
    fn a_long_title_is_ellipsized_to_the_region() {
        let s = State::error("x".repeat(500));
        let out = draw(30, 1, |f, a, hits| s.render(f, a, &t(), hits));
        let row = out.lines().next().unwrap_or("");
        assert!(
            row.chars().count() <= 30,
            "row too long: {}",
            row.chars().count()
        );
    }

    #[test]
    fn every_state_renders_in_every_theme_at_every_reference_size() {
        let states = vec![
            State::loading("probing models"),
            State::empty("no artifacts"),
            State::error("probe failed").with_detail("timeout after 30s"),
            State::unavailable("asr", "whisper missing"),
        ];
        for kind in ThemeKind::ALL {
            let tok = Theme::new(kind).tokens();
            for s in &states {
                for (w, h) in [
                    (80u16, 24u16),
                    (100, 30),
                    (120, 40),
                    (160, 50),
                    (180, 50),
                    (1, 1),
                ] {
                    let mut term = Terminal::new(TestBackend::new(w, h)).expect("term");
                    let mut hits = HitTargets::new();
                    let area = term.backend().buffer().area;
                    term.draw(|f| s.render(f, area, &tok, &mut hits))
                        .expect("draw");
                }
            }
        }
    }

    #[test]
    fn retry_registers_a_clickable_control() {
        let s = State::error("transient").with_retry("state.retry", "Retry", 'r');
        let mut hits = HitTargets::new();
        let mut term = Terminal::new(TestBackend::new(40, 3)).expect("term");
        let area = term.backend().buffer().area;
        term.draw(|f| s.render(f, area, &t(), &mut hits))
            .expect("draw");
        let buf = term.backend().buffer().clone();
        let out: String = (0..3)
            .map(|y| (0..40).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(out.contains("Retry"), "{out}");
        assert!(
            hits.region_of("state.retry").is_some(),
            "the retry control must be clickable"
        );
    }

    #[test]
    fn a_destructive_dialog_states_its_meaning_in_words() {
        // Monochrome cannot distinguish the destructive border by color, so
        // the meaning must live in the title. This is the accessibility floor.
        let dlg = crate::design::modals::ConfirmDialog::new(
            "PERMISSION: DENY THIS ACTION",
            "Refusing keeps the workspace unchanged.",
            "Deny",
        );
        let out = draw(60, 8, |f, a, hits| {
            crate::design::modals::render_confirm_dialog(
                f,
                a,
                &Theme::new(ThemeKind::Monochrome).tokens(),
                &dlg,
                hits,
            );
        });
        assert!(
            out.contains("PERMISSION"),
            "the meaning is not in words:\n{out}"
        );
        assert!(out.contains("Deny"), "{out}");
    }
}
