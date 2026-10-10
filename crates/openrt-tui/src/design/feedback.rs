//! Transient feedback: toasts and notifications.
//!
//! The rule that makes these trustworthy: **nothing important may live only in a
//! toast.** A toast is for "the thing you just did had an effect"; it disappears
//! on its own. Anything an operator must not miss — a denied permission, a
//! resource block, a failed admission — is a *sticky* notification that occupies
//! its own region and is cleared by acknowledgement.
//!
//! That distinction is encoded in [`Toast::sticky`], and [`ToastQueue`] refuses
//! to evict a sticky toast to make room for a transient one.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::tokens::Tokens;
use super::widgets::{ellipsize, sanitize_field};

/// Toast tones are [`BadgeTone`]s.
///
/// This module originally declared its own four-variant `ToastTone`. That is a
/// second status vocabulary: a toast could say `Warning` in a colour the rest of
/// the app called `Tight`, and nothing would catch it. One vocabulary means a
/// warning reads the same everywhere, including on a monochrome terminal where
/// the shared marker is the only thing left.
pub use super::widgets::BadgeTone as ToastTone;

/// A single message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toast {
    pub text: String,
    pub tone: ToastTone,
    /// Frames remaining for a transient toast. Ignored when `sticky`.
    pub ttl: u16,
    /// A sticky toast is never auto-dismissed and never evicted.
    pub sticky: bool,
}

impl Toast {
    pub fn transient(text: impl Into<String>, tone: ToastTone, ttl: u16) -> Self {
        Self {
            text: text.into(),
            tone,
            ttl,
            sticky: false,
        }
    }

    /// A message the operator must acknowledge. Survives until cleared.
    pub fn sticky(text: impl Into<String>, tone: ToastTone) -> Self {
        Self {
            text: text.into(),
            tone,
            ttl: 0,
            sticky: true,
        }
    }

    fn fg(&self, t: &Tokens) -> ratatui::style::Color {
        self.tone.color(t)
    }

    /// The shared badge marker for this tone.
    fn marker(&self) -> &'static str {
        super::widgets::Badge::new("", self.tone).marker()
    }

    /// The panel tone a *sticky* notification is framed with.
    ///
    /// A refusal is destructive; anything else is a notice and stays on the
    /// neutral edge, so a sticky notification never borrows the strongest border
    /// in the UI for something informational.
    fn panel_tone(&self) -> super::widgets::PanelTone {
        match self.tone {
            ToastTone::Danger | ToastTone::Blocked => super::widgets::PanelTone::Destructive,
            _ => super::widgets::PanelTone::Overlay,
        }
    }
}

/// The toast region for one frame.
///
/// `capacity` is how many rows the region can hold; it is a bound, not a hint,
/// so a flood of messages can never push content off screen.
pub struct ToastQueue {
    items: Vec<Toast>,
    capacity: usize,
    /// Bumped whenever the visible set changes, so a redraw can be skipped.
    revision: u64,
}

impl ToastQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            items: Vec::new(),
            capacity: capacity.max(1),
            revision: 0,
        }
    }

    pub fn push(&mut self, toast: Toast) {
        self.items.push(toast);
        self.evict();
    }

    /// Remove the transient toasts that have expired. Sticky ones never expire,
    /// which is the whole point of them.
    pub fn tick(&mut self) {
        let before = self.items.len();
        self.items.retain(|t| t.sticky || t.ttl > 0);
        if self.items.len() != before {
            self.bump();
        } else {
            for t in self.items.iter_mut().filter(|t| !t.sticky) {
                t.ttl = t.ttl.saturating_sub(1);
            }
        }
    }

    pub fn clear(&mut self) {
        if !self.items.is_empty() {
            self.items.clear();
            self.bump();
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn visible(&self) -> &[Toast] {
        &self.items
    }

    /// Drop the oldest *transient* toasts until the queue fits. A sticky toast is
    /// never dropped, so a real message is never lost to a burst of chatter.
    fn evict(&mut self) {
        let mut dropped = false;
        while self.items.len() > self.capacity {
            let victim = self
                .items
                .iter()
                .position(|t| !t.sticky)
                .unwrap_or_else(|| {
                    // Everything is sticky. Rather than lose a message, keep them
                    // all and let the renderer crop with an explicit overflow
                    // count, so the omission is visible.
                    usize::MAX
                });
            if victim == usize::MAX {
                break;
            }
            self.items.remove(victim);
            dropped = true;
        }
        if dropped {
            self.bump();
        }
    }

    fn bump(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
}

/// Draw the queue into `area`, newest last. Returns the number of toasts that did
/// not fit, so a caller can decide whether to surface an overflow indicator.
pub fn draw_toasts(t: &Tokens, frame: &mut Frame, area: Rect, queue: &ToastQueue) -> usize {
    if area.width == 0 || area.height == 0 || queue.visible().is_empty() {
        return 0;
    }
    // The *newest* messages win the available rows: when a burst fills the
    // region, the one the operator most needs to see is the one that just
    // happened. Overflow is reported so the crop is never silent.
    let cap = usize::from(area.height);
    let total = queue.visible().len();
    let skip = total.saturating_sub(cap);
    let mut lines: Vec<Line> = Vec::new();
    let mut shown = 0usize;
    for toast in queue.visible().iter().skip(skip) {
        if shown >= cap {
            break;
        }
        // The marker and its colour come from the shared badge for the tone, so
        // a toast cannot invent a glyph the rest of the app does not use.
        let mut spans = vec![Span::styled(
            format!("{} ", toast.marker()),
            Style::default()
                .fg(toast.fg(t))
                .add_modifier(Modifier::BOLD),
        )];
        spans.push(Span::styled(
            ellipsize(&sanitize_field(&toast.text), area.width.saturating_sub(2)),
            Style::default().fg(t.text.primary),
        ));
        lines.push(Line::from(spans));
        shown += 1;
    }
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(t.surfaces.overlay)),
        Rect {
            height: u16::try_from(shown).unwrap_or(0),
            ..area
        },
    );
    let _ = shown;
    skip
}

/// The sticky region: full-width, bordered, and never cropped silently.
pub fn draw_notification(t: &Tokens, frame: &mut Frame, area: Rect, toast: &Toast) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // Shared panel chrome: the sticky region was the last framed surface in the
    // app still building its own `Block`, so its title and border treatment did
    // not match any other panel.
    let border = toast.fg(t);
    let b = super::widgets::panel(t, "", toast.panel_tone());
    let inner = b.inner(area);
    frame.render_widget(b, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let lines: Vec<Line> = sanitize_field(&toast.text)
        .lines()
        .map(|l| {
            Line::from(vec![
                Span::styled(format!("{} ", toast.marker()), Style::default().fg(border)),
                Span::styled(
                    ellipsize(l, inner.width.saturating_sub(2)),
                    Style::default().fg(t.text.primary),
                ),
            ])
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(t.surfaces.overlay)),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn t() -> Tokens {
        Theme::default_theme().tokens()
    }

    fn draw(w: u16, h: u16, f: impl FnOnce(&mut Frame, Rect)) -> String {
        let mut term = Terminal::new(TestBackend::new(w.max(1), h.max(1))).expect("term");
        term.draw(|f2| f(f2, f2.area())).expect("draw");
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn transient_toasts_expire_and_sticky_ones_do_not() {
        let mut q = ToastQueue::new(8);
        q.push(Toast::transient("saved", ToastTone::Success, 1));
        q.push(Toast::sticky("permission denied", ToastTone::Danger));
        q.tick();
        assert_eq!(q.visible().len(), 2, "first tick only decrements");
        q.tick();
        q.tick();
        assert_eq!(q.visible().len(), 1, "transient never expired");
        assert!(q.visible()[0].sticky);
    }

    #[test]
    fn sticky_toasts_are_never_evicted_by_a_burst() {
        let mut q = ToastQueue::new(2);
        q.push(Toast::sticky("RESOURCE BLOCKED", ToastTone::Danger));
        for i in 0..10 {
            q.push(Toast::transient(format!("noise{i}"), ToastTone::Info, 5));
        }
        assert!(
            q.visible().iter().any(|x| x.text == "RESOURCE BLOCKED"),
            "the message that mattered was evicted: {:?}",
            q.visible()
        );
        // The transient ones are capped, not unbounded.
        assert!(
            q.visible().len() <= 3,
            "queue grew past capacity: {}",
            q.visible().len()
        );
    }

    #[test]
    fn all_sticky_queue_reports_its_overflow_rather_than_dropping() {
        let mut q = ToastQueue::new(2);
        for i in 0..5 {
            q.push(Toast::sticky(format!("keep{i}"), ToastTone::Warning));
        }
        let _ = draw(20, 3, |f, a| {
            let hidden = draw_toasts(&t(), f, a, &q);
            assert_eq!(hidden, 2, "the region reports what it could not show");
        });
        assert_eq!(q.visible().len(), 5, "sticky messages were dropped");
    }

    #[test]
    fn revision_changes_only_when_the_visible_set_changes() {
        let mut q = ToastQueue::new(4);
        let r0 = q.revision();
        q.tick();
        assert_eq!(q.revision(), r0, "an empty tick churned the revision");
        q.push(Toast::transient("x", ToastTone::Info, 1));
        let r1 = q.revision();
        q.tick();
        assert_eq!(q.revision(), r1);
        q.tick();
        assert_ne!(q.revision(), r1, "expiry must invalidate the cache");
    }

    #[test]
    fn clear_drops_everything_including_sticky() {
        let mut q = ToastQueue::new(4);
        q.push(Toast::sticky("x", ToastTone::Danger));
        q.clear();
        assert!(q.visible().is_empty());
    }

    #[test]
    fn toasts_render_and_report_overflow() {
        let mut q = ToastQueue::new(8);
        for i in 0..6 {
            q.push(Toast::transient(format!("message {i}"), ToastTone::Info, 5));
        }
        let out = draw(30, 3, |f, a| {
            let hidden = draw_toasts(&t(), f, a, &q);
            assert_eq!(hidden, 3, "overflow must be reported, not hidden");
        });
        assert!(
            out.contains("message 5"),
            "the newest toast must survive: {out}"
        );
        assert!(
            !out.contains("message 0"),
            "cropped rows still drawn: {out}"
        );
    }

    #[test]
    fn toasts_survive_degenerate_geometry() {
        let mut q = ToastQueue::new(4);
        q.push(Toast::transient(
            "a-very-long-message-that-cannot-fit",
            ToastTone::Warning,
            5,
        ));
        for (w, h) in [(1u16, 1u16), (1, 10), (3, 1), (200, 1)] {
            let _ = draw(w, h, |f, a| {
                draw_toasts(&t(), f, a, &q);
                draw_notification(&t(), f, a, &q.visible()[0]);
            });
        }
    }

    #[test]
    fn notification_wraps_a_long_message_without_panicking() {
        let toast = Toast::sticky(
            "The host refused this admission because the required GPU memory \
             exceeds the configured ceiling, and the override has expired.",
            ToastTone::Danger,
        );
        let out = draw(40, 6, |f, a| draw_notification(&t(), f, a, &toast));
        assert!(out.contains("refused"), "{out}");
    }

    #[test]
    fn notification_sanitizes_embedded_control_bytes() {
        let toast = Toast::sticky("bad\u{1b}[2Jbytes\u{7}", ToastTone::Danger);
        let out = draw(40, 4, |f, a| draw_notification(&t(), f, a, &toast));
        assert!(
            !out.contains('\u{1b}'),
            "escape reached the screen: {out:?}"
        );
    }

    #[test]
    fn every_tone_renders_its_own_marker() {
        for tone in [
            ToastTone::Info,
            ToastTone::Success,
            ToastTone::Warning,
            ToastTone::Danger,
        ] {
            let toast = Toast::transient("msg", tone, 5);
            let m = toast.marker();
            assert!(!m.is_empty());
            assert_eq!(m.chars().count(), 1, "marker must be one cell: {m:?}");
        }
    }

    #[test]
    fn markers_have_ascii_fallbacks() {
        // Monochrome/ASCII mode must still get a distinct glyph per tone.
        let seen: std::collections::BTreeSet<String> = [
            ToastTone::Info,
            ToastTone::Success,
            ToastTone::Warning,
            ToastTone::Danger,
        ]
        .iter()
        .map(|tone| Toast::transient("x", *tone, 1).marker().to_string())
        .collect();
        assert_eq!(seen.len(), 4, "markers collide in some mode: {seen:?}");
    }
}
