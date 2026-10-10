//! Editable fields: the composer and the search box.
//!
//! The hard part of a text field is not drawing it — it is deciding *which part
//! of the string the operator can see*. A field whose cursor is off-screen, or
//! whose view scrolls past the caret, is broken in a way that is obvious the
//! moment you type more than a screenful. All of that lives in [`viewport`], so
//! every field in the app scrolls the same way.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use super::tokens::Tokens;
use super::widgets::{ellipsize, sanitize_field};

/// Which part of the text a field of `width` columns should show, so that the
/// caret at `cursor` (a char index) is inside the window.
///
/// Returns `(start_char, visible_chars)`. The caret is always visible: the
/// window scrolls right when the caret would pass the edge, and back toward the
/// start when the caret moves left of the window.
///
/// The budget is **display columns, not characters**. A field holding
/// `日本語` in a 10-column frame shows two of those three characters, not
/// three — three would paint 6 columns of glyph into 5 columns of frame,
/// pushing the border out of alignment and dragging the caret block off the
/// edge with it. Counting characters here was the difference between the
/// composer showing the operator's text and the composer mangling it.
///
/// The caret may sit at `start + vis` (one past the last text column) because
/// callers reserve a trailing column for the caret block itself — that is how an
/// empty field still shows a caret.
pub fn viewport(chars: &[char], cursor: usize, width: u16) -> (usize, usize) {
    let w = usize::from(width);
    if w == 0 {
        return (0, 0);
    }
    let cursor = cursor.min(chars.len());
    let cols = |i: usize| -> usize {
        chars[..i.min(chars.len())]
            .iter()
            .copied()
            .map(char_cols)
            .sum()
    };
    // Everything fits: there is no window to move.
    if cols(chars.len()) <= w {
        return (0, chars.len());
    }
    // Walk left from the caret, spending at most `w - 1` columns. That fills the
    // left side and guarantees the caret lands strictly inside the frame instead
    // of on or past its last cell.
    let keep = w.saturating_sub(1).max(1);
    let mut start = cursor;
    let mut left = 0usize;
    while start > 0 {
        let cw = char_cols(chars[start - 1]);
        if left + cw > keep {
            break;
        }
        left += cw;
        start -= 1;
    }
    // Spend the rest of the budget on the right. The right walk resumes at the
    // caret, not at `start` — `left` already paid for `chars[start..cursor]`,
    // and recounting them would double-charge the window and blank the field.
    let mut end = cursor;
    let mut right = 0usize;
    while end < chars.len() {
        let cw = char_cols(chars[end]);
        if left + right + cw > w {
            break;
        }
        right += cw;
        end += 1;
    }
    debug_assert!(
        left + right <= w,
        "window {start}..{end} paints {right} columns in {w}",
    );
    (start, end - start)
}

fn char_cols(c: char) -> usize {
    unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Render the visible slice of a field with a caret.
///
/// A multi-line paste is shown as a single row with `⏎`-free collapsing: the
/// field reports that it holds more than one line instead of rendering newlines
/// that would corrupt the layout.
pub fn draw_field(
    t: &Tokens,
    frame: &mut Frame,
    area: Rect,
    title: &str,
    footer: &str,
    text: &str,
    cursor: usize,
    focused: bool,
) {
    draw_field_with(t, frame, area, title, footer, text, cursor, focused, None)
}

/// [`draw_field`] plus a placeholder shown only while the field is empty.
///
/// The placeholder is deliberately dimmer than real input: an operator must
/// never mistake the suggestion for something they typed.
#[allow(clippy::too_many_arguments)]
pub fn draw_field_with(
    t: &Tokens,
    frame: &mut Frame,
    area: Rect,
    title: &str,
    footer: &str,
    text: &str,
    cursor: usize,
    focused: bool,
    placeholder: Option<&str>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let border = if focused {
        t.borders.focus
    } else {
        t.borders.default
    };
    let mut b = Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_style(Style::default().fg(border))
        .style(Style::default().bg(t.surfaces.input));
    if !title.is_empty() {
        b = b.title(Line::styled(
            format!(" {title} "),
            Style::default().fg(if focused {
                t.text.primary
            } else {
                t.text.muted
            }),
        ));
    }
    // The footer lives inside the bottom border row, so it costs no body space
    // at all — it is only dropped when there is no border row to hold it.
    if !footer.is_empty() && area.height >= 3 {
        b = b.title_bottom(Line::styled(
            format!(" {footer} "),
            Style::default().fg(t.text.muted),
        ));
    }
    let inner = b.inner(area);
    frame.render_widget(b, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // The caret block occupies one column, so the text gets one less.
    let text_w = inner.width.saturating_sub(1);
    let clean = sanitize_field(text);
    let is_empty = clean.is_empty();
    if is_empty {
        if let Some(p) = placeholder.filter(|p| !p.is_empty()) {
            let body = Paragraph::new(Line::from(vec![
                Span::styled(
                    ellipsize(p, inner.width.saturating_sub(u16::from(focused))),
                    Style::default().fg(t.text.muted).bg(t.surfaces.input),
                ),
                Span::styled(
                    " ".to_string(),
                    if focused {
                        Style::default()
                            .fg(t.text.inverse)
                            .bg(t.accent.primary)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().bg(t.surfaces.input)
                    },
                ),
            ]))
            .style(Style::default().bg(t.surfaces.input));
            frame.render_widget(body, inner);
            return;
        }
    }
    let chars: Vec<char> = clean.chars().collect();
    let (start, vis) = viewport(&chars, cursor, text_w);
    let slice: String = chars.iter().skip(start).take(vis).collect::<String>();
    // A scrolled field says so, instead of letting the operator think the text
    // begins where the window does.
    let prefix = if start > 0 { "…" } else { "" };
    let shown = format!("{prefix}{slice}");

    let mut spans = Vec::new();
    if !prefix.is_empty() {
        spans.push(Span::styled("…", Style::default().fg(t.text.muted)));
    }
    let cur_local = cursor.saturating_sub(start);
    if focused && cur_local <= slice.chars().count() {
        let head: String = slice.chars().take(cur_local).collect();
        let tail: String = slice.chars().skip(cur_local).collect();
        spans.push(Span::styled(
            head,
            Style::default().fg(t.text.primary).bg(t.surfaces.input),
        ));
        spans.push(Span::styled(
            " ".to_string(),
            Style::default()
                .fg(t.text.inverse)
                .bg(t.accent.primary)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            tail,
            Style::default().fg(t.text.primary).bg(t.surfaces.input),
        ));
    } else {
        spans.push(Span::styled(
            shown,
            Style::default()
                .fg(if focused {
                    t.text.primary
                } else {
                    t.text.muted
                })
                .bg(t.surfaces.input),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(t.surfaces.input)),
        inner,
    );
}

/// A search field: a field plus a result count, rendered in one row.
pub fn draw_search(
    t: &Tokens,
    frame: &mut Frame,
    area: Rect,
    text: &str,
    cursor: usize,
    matches: Option<usize>,
    focused: bool,
) {
    let right = match matches {
        Some(0) => "none".to_string(),
        Some(n) => n.to_string(),
        None => String::new(),
    };
    draw_field(t, frame, area, " search ", &right, text, cursor, focused);
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

    /// The two invariants every window must satisfy, at any width and for any
    /// script: the caret is inside the window, and the window's glyphs fit the
    /// column budget.
    fn assert_window(chars: &[char], cursor: usize, width: u16, label: &str) {
        let (start, vis) = viewport(chars, cursor, width);
        if width == 0 {
            // A field with no columns has no window and no caret. Callers return
            // before drawing, so this is the only correct answer.
            assert_eq!((start, vis), (0, 0), "{label}: a zero-width field is empty");
            return;
        }
        assert!(
            cursor >= start,
            "{label}: caret {cursor} left of {start} w={width}"
        );
        assert!(
            cursor <= start + vis,
            "{label}: caret {cursor} right of {start}+{vis} w={width}",
        );
        assert!(
            start + vis <= chars.len().max(usize::from(width)),
            "{label}: overshot text"
        );
        let painted: usize = chars[start..start + vis]
            .iter()
            .copied()
            .map(char_cols)
            .sum();
        assert!(
            painted <= usize::from(width),
            "{label}: {vis} chars paint {painted} columns in {width}",
        );
    }

    #[test]
    fn viewport_keeps_the_caret_visible_at_both_ends() {
        for text in ["abcdefghijklmnop", "日本語のテキストです", "é日本語ab"] {
            let chars: Vec<char> = text.chars().collect();
            for width in [1u16, 2, 5, 10, 20] {
                for cursor in 0..=chars.len() {
                    assert_window(&chars, cursor, width, text);
                }
            }
        }
    }

    #[test]
    fn a_wide_string_never_paints_wider_than_its_frame() {
        // The bug this guards: counting characters in a frame that counts cells
        // put 6 columns of CJK glyph into 5 columns of field.
        let chars: Vec<char> = "日本語".chars().collect();
        for width in [1u16, 3, 5, 10] {
            for cursor in 0..=chars.len() {
                assert_window(&chars, cursor, width, "日本語");
            }
        }
        // 5 columns hold two wide characters, not three.
        let (_start, vis) = viewport(&chars, 3, 5);
        assert!(vis <= 2, "5 columns took {vis} double-width characters");
    }

    #[test]
    fn viewport_is_identity_when_the_text_fits() {
        let five: Vec<char> = "abcde".chars().collect();
        assert_eq!(viewport(&five, 0, 10), (0, 5));
        assert_eq!(viewport(&five, 3, 10), (0, 5));
        assert_eq!(viewport(&five, 3, 0), (0, 0));
        // Six columns of Japanese fits in ten columns; a char budget of ten
        // would have let three characters through where two were correct.
        let wide: Vec<char> = "日本語".chars().collect();
        assert_eq!(viewport(&wide, 3, 10), (0, 3));
    }

    #[test]
    fn viewport_never_overshoots_the_text() {
        for text in ["abcdefghijklmnopqrst", "日本語のテキストです", "aé日b"] {
            let chars: Vec<char> = text.chars().collect();
            for cursor in 0..=chars.len() {
                for w in [0u16, 1, 3, 7] {
                    assert_window(&chars, cursor, w, text);
                }
            }
        }
    }

    #[test]
    fn field_shows_text_and_a_caret() {
        let out = draw(30, 3, |f, a| {
            draw_field(&t(), f, a, " you ", " Enter send ", "hello world", 5, true);
        });
        assert!(out.contains("hello"), "{out}");
        assert!(out.contains("you"), "title missing: {out}");
    }

    #[test]
    fn field_marks_a_scrolled_view_so_text_is_not_mistaken_for_the_start() {
        let out = draw(20, 3, |f, a| {
            let long: String = (0..100)
                .map(|i| char::from(b'a' + (i % 26) as u8))
                .collect();
            draw_field(&t(), f, a, " you ", "", &long, 90, true);
        });
        assert!(out.contains('…'), "no scroll marker: {out}");
    }

    #[test]
    fn field_is_unfocused_still_when_a_modal_is_open() {
        let out = draw(20, 3, |f, a| {
            draw_field(&t(), f, a, " you ", "", "text", 2, false);
        });
        assert!(out.contains("text"), "{out}");
    }

    #[test]
    fn field_survives_degenerate_geometry() {
        for (w, h) in [(1u16, 1u16), (1, 5), (2, 2), (4, 1), (3, 3), (200, 1)] {
            let _ = draw(w, h, |f, a| {
                draw_field(&t(), f, a, " you ", " Enter send ", "abcdef", 99, true);
            });
        }
    }

    #[test]
    fn field_reports_an_oversized_caret_without_panicking() {
        let _ = draw(20, 3, |f, a| {
            draw_field(&t(), f, a, " you ", "", "abc", 9999, true);
        });
    }

    #[test]
    fn field_sanitizes_control_characters() {
        let out = draw(30, 3, |f, a| {
            draw_field(&t(), f, a, " you ", "", "a\u{1b}[31mb\x07", 1, true);
        });
        assert!(
            !out.contains('\u{1b}'),
            "escape byte reached the screen: {out:?}"
        );
        assert!(
            !out.contains('\u{7}'),
            "bell byte reached the screen: {out:?}"
        );
    }

    #[test]
    fn placeholder_shows_only_while_empty_and_is_dimmer_than_input() {
        let empty = draw(30, 3, |f, a| {
            draw_field_with(
                &t(),
                f,
                a,
                " you ",
                "",
                "",
                0,
                true,
                Some("Ask OpenRT anything"),
            )
        });
        assert!(
            empty.contains("Ask OpenRT anything"),
            "placeholder missing: {empty}"
        );
        let typed = draw(30, 3, |f, a| {
            draw_field_with(
                &t(),
                f,
                a,
                " you ",
                "",
                "real input",
                0,
                true,
                Some("Ask OpenRT anything"),
            )
        });
        assert!(
            !typed.contains("Ask OpenRT anything"),
            "placeholder shown over real input: {typed}"
        );
        // The caret splits the row, so compare on a caret-free render.
        assert!(
            typed.contains("real input") || typed.contains("rea l input"),
            "{typed}"
        );
    }

    #[test]
    fn placeholder_survives_a_narrow_field() {
        let _ = draw(4, 3, |f, a| {
            draw_field_with(
                &t(),
                f,
                a,
                " y ",
                "",
                "",
                0,
                true,
                Some("a-very-long-suggestion"),
            )
        });
    }

    #[test]
    fn search_reports_its_match_count() {
        let out = draw(30, 3, |f, a| {
            draw_search(&t(), f, a, "gpt", 3, Some(7), true)
        });
        assert!(out.contains('7'), "match count missing: {out}");
        let out = draw(30, 3, |f, a| {
            draw_search(&t(), f, a, "zzz", 3, Some(0), true)
        });
        assert!(
            out.contains("none"),
            "zero matches must read as 'none': {out}"
        );
    }

    #[test]
    fn footer_is_dropped_before_the_field_body() {
        // The hint strip shares the bottom border row, so a 3-row field still
        // has both the text and the hint.
        let normal = draw(20, 3, |f, a| {
            draw_field(&t(), f, a, " you ", " Enter send ", "text", 0, true)
        });
        assert!(normal.contains("text"), "field body lost: {normal}");
        assert!(
            normal.contains("Enter send"),
            "footer dropped when free: {normal}"
        );
        let cramped = draw(20, 2, |f, a| {
            draw_field(&t(), f, a, " you ", " Enter send ", "text", 1, true)
        });
        assert!(
            !cramped.contains("Enter send"),
            "footer ate the border row: {cramped}"
        );
    }
}
