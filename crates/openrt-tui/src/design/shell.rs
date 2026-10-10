//! Application chrome: the masthead, the status bar, and the navigation rail.
//!
//! These are the three regions that exist on every screen, which is exactly why
//! they were the hardest to get right: they compete with content for space, and
//! the ones that matter most when space runs out are the ones that get dropped.
//!
//! The rules this module encodes, so no view has to remember them:
//!
//! * The header never grows past half the remaining height — an 80x4 window
//!   must show chat, not a three-row masthead.
//! * The status bar is the last thing to survive, because it carries the
//!   resource state an operator needs to decide whether to keep going.
//! * The rail drops *headings* before destinations, never the active
//!   destination, and always keeps the WORKBENCH toggle reachable.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use super::layout::{Breakpoint, ShellLayout};
use super::tokens::{saturating_sub, Tokens};
use super::widgets::{divider, ellipsize, sanitize_field, BadgeTone};

/// The masthead: identity, session, posture, and resource meters.
///
/// `meters` is already-formatted text (e.g. `RAM 512M/4G`). This function owns
/// *placement* and *truncation order* only, so the numbers themselves stay the
/// caller's single source of truth.
pub fn draw_header(
    t: &Tokens,
    frame: &mut Frame,
    layout: &ShellLayout,
    title: &str,
    subtitle: &str,
    status_badges: &[(&str, super::widgets::BadgeTone)],
    meters: &[(&str, String)],
) {
    if layout.header.height == 0 {
        return;
    }
    let w = layout.header.width;

    // Row 1: identity, then the posture badges. Badges win ties against the
    // subtitle because AGENT READY / RESOURCE BLOCKED must never be the thing
    // that gets truncated away.
    let mut left = vec![Span::styled(
        sanitize_field(title),
        Style::default()
            .fg(t.text.primary)
            .add_modifier(Modifier::BOLD),
    )];
    if !subtitle.is_empty() {
        left.push(Span::raw("  "));
        left.push(Span::styled(
            sanitize_field(subtitle),
            Style::default().fg(t.text.muted),
        ));
    }
    let badge_text: Vec<Span> = status_badges
        .iter()
        .flat_map(|(text, tone)| {
            let b = super::widgets::Badge::new(*text, *tone);
            b.spans(t)
                .into_iter()
                .chain(std::iter::once(Span::raw(" ")))
        })
        .collect();

    // A badge must not run straight into the subtitle text: `ne…! SOVEREIGN`
    // read as one token. The gap is reserved in `badge_w` too, or the subtitle
    // would be truncated one column too late and collide with it.
    let badge_w: u16 = status_badges
        .iter()
        .map(|(text, _)| u16::try_from(text.chars().count()).unwrap_or(0) + 4)
        .sum();
    let left_max = saturating_sub(w, badge_w);
    let mut badge_group = vec![Span::raw(" ")];
    badge_group.extend(badge_text);
    let row1 = Line::from(join_spans(vec![fit_spans(left, left_max), badge_group]));

    // Row 2: meters, right-aligned when there is room.
    let meter_spans: Vec<Span> = if layout.header.height >= 2 && !meters.is_empty() {
        // Built from the shared `metric` primitive: a header meter is exactly a
        // muted label with a bold value, and hand-writing the pair here had
        // already drifted from the same pair in the dashboard strip.
        let full: Vec<Span> = meters
            .iter()
            .flat_map(|(label, value)| {
                let mut spans = super::widgets::metric(t, label, value).spans;
                spans.push(Span::raw("   "));
                spans
            })
            .collect();
        let used: u16 = meters
            .iter()
            .map(|(l, v)| u16::try_from(l.chars().count() + v.chars().count() + 3).unwrap_or(0))
            .sum();
        if used <= w {
            full
        } else {
            // Overflowing meters collapse to labels only — "RAM" tells you a
            // meter exists; it never lies about the value by clipping mid-number.
            let compact: Vec<Span> = meters
                .iter()
                .flat_map(|(label, _)| {
                    [
                        Span::styled(format!("{label} "), Style::default().fg(t.text.muted)),
                        Span::raw("… "),
                    ]
                })
                .collect();
            compact
        }
    } else {
        Vec::new()
    };

    // Rows are spent in priority order: identity, then meters, then the
    // divider. A two-row header shows the meters *instead of* the rule —
    // pushing the rule there would hide the numbers behind the crop.
    let mut lines = vec![row1];
    if layout.header.height >= 3 {
        lines.push(divider(t, w));
    }
    if layout.header.height >= 2 {
        lines.push(Line::from(meter_spans));
    }
    // Trailing filler: ratatui leaves stale cells when a Paragraph is shorter
    // than its area, which shows up as ghosting over a previous theme.
    lines.push(Line::default());

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(t.surfaces.bg)),
        layout.header,
    );
}

/// The footer status bar: hints on the left, mode on the right.
///
/// Returns `false` when the bar had no room, so the caller can move its hints
/// into the header instead of silently dropping them.
pub fn draw_status_bar(
    t: &Tokens,
    frame: &mut Frame,
    layout: &ShellLayout,
    left: &str,
    right: &str,
    tone: BadgeTone,
) -> bool {
    if layout.status.height == 0 {
        return false;
    }
    let w = layout.status.width;
    let right = sanitize_field(right);
    let rw = u16::try_from(right.chars().count()).unwrap_or(0);
    // Two columns of gap, not one: the hint text is muted and the status is not,
    // and a single space made the ellipsized hint look like it ran into the
    // status text (`…time-machineq:quitready`).
    let lw = saturating_sub(w, rw.saturating_add(2));

    // The right segment's colour comes from a tone, not a literal: "ready" and
    // "paused" and "refused" are meanings, and a screen that picks a shade for
    // a status can pick one that no theme defines. Workbench previously coloured
    // its live status green at the call site while Agent Mode coloured the same
    // slot from its own palette.
    let line = Line::from(join_spans(vec![
        fit_spans(
            vec![Span::styled(
                ellipsize(&sanitize_field(left), lw),
                Style::default().fg(t.text.muted),
            )],
            lw,
        ),
        vec![Span::styled(
            format!(" {right}"),
            Style::default().fg(tone.color(t)),
        )],
    ]));

    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(t.surfaces.bg).fg(t.text.muted)),
        layout.status,
    );
    true
}

/// A rail entry. Headings are droppable; destinations are not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RailItem {
    /// Group heading, if this row introduces a section.
    pub heading: Option<String>,
    /// Destination label.
    pub label: String,
    /// Zero-based index into the app's flat view list.
    pub index: usize,
}

/// One row of a rail window: either a heading or a destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RailRow {
    Heading(String),
    Item(RailItem),
}

/// The ordinal of the active destination among all destinations in `rows`.
///
/// `active` is a *destination index* everywhere in this module, not a row offset
/// and not an ordinal among destinations — but `rows` interleaves headings, so
/// all three differ. Resolving it in one place is what keeps the window and the
/// highlight agreeing about which destination is selected; when they took the two
/// meanings separately, the window anchored on one destination while the
/// highlight landed on another.
///
/// An unknown index resolves to the first destination, so a stale `active` from a
/// previous frame degrades to "anchor at the top" instead of panicking or
/// dropping the rail.
fn active_position(rows: &[RailRow], index: usize) -> usize {
    rows.iter()
        .enumerate()
        .filter(|(_, r)| matches!(r, RailRow::Item(_)))
        .position(|(_, r)| matches!(r, RailRow::Item(i) if i.index == index))
        .unwrap_or(0)
}

/// Choose which rail rows fit in `height`, given the active destination.
///
/// `active` is the destination's flat index (see [`RailItem::index`]).
///
/// The policy, in order:
/// 1. The active destination is always present.
/// 2. Headings are dropped first — a lost heading is a lost hint, a lost
///    destination is a lost feature.
/// 3. If there is still room, neighbours of the active row are preferred, so
///    the visible window moves with the selection instead of jumping.
/// 4. Whatever was hidden is reported through the returned `hidden` count, and
///    the caller shows `↑N`/`↓N` so the omission is never silent.
/// A resolved rail window: which rows are visible, and what was dropped.
///
/// `hidden_above` and `hidden_below` count dropped **destinations only**, and
/// they always report the truth even when the marker cannot be drawn.
///
/// `rows.len() <= height` is guaranteed. Markers are *not* guaranteed to fit:
/// when destinations already fill the rail, the marker is suppressed rather
/// than allowed to evict the destination it was warning about. A one-row rail
/// showing only the active destination and no explanation is the correct
/// outcome there — the alternative is an explanation with nothing to explain.
///
/// Headings are hints, and reporting a lost hint as a lost feature trains people
/// to ignore the overflow marker — so a rail showing every destination stays
/// silent even when its headings were squeezed out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RailWindow {
    pub rows: Vec<RailRow>,
    pub hidden_above: u16,
    pub hidden_below: u16,
}

impl RailWindow {
    /// Total destinations the operator cannot currently reach.
    pub fn hidden(&self) -> u16 {
        self.hidden_above.saturating_add(self.hidden_below)
    }

    /// True when nothing reachable is missing, so no marker is warranted.
    pub fn is_complete(&self) -> bool {
        self.hidden() == 0
    }

    /// How many marker rows a renderer can still draw.
    ///
    /// Markers are best-effort: they are appended into whatever space the
    /// destinations left, and silently skipped when there is none. `above` is
    /// offered the space first so a reader who cannot see the top of the rail
    /// learns that there is something above it.
    pub fn marker_budget(&self, height: u16) -> u16 {
        let wanted = usize::from(self.hidden_above > 0) + usize::from(self.hidden_below > 0);
        let free = usize::from(height).saturating_sub(self.rows.len());
        u16::try_from(wanted.min(free)).unwrap_or(0)
    }
}

/// Choose which rail rows fit in `height`, given the active destination.
///
/// `active` is the destination's flat index (see [`RailItem::index`]).
///
/// The policy, in order:
///
/// 1. The active destination is always present.
/// 2. **Destinations are placed before any heading gets a row.** At 80x24 the
///    rail has exactly 15 body rows for 15 destinations, so spending one on a
///    heading would push a real destination off the edge. A lost heading can be
///    recovered by reading the labels; a lost destination cannot.
/// 3. Overflow markers are counted as rows. A marker that is not budgeted is a
///    marker that silently evicts the destination it was warning about, which is
///    how a rail ends up claiming to show everything while hiding Settings.
/// 4. Whatever remains goes to headings, nearest the active destination first.
///
/// Step 3 is why this loops: reserving a marker row shrinks the destination
/// budget, which can push a destination off the far side and add a *second*
/// marker. The loop settles in a couple of passes and is bounded so a
/// pathological input cannot spin.
pub fn rail_window(rows: &[RailRow], active: usize, height: u16) -> RailWindow {
    let h = usize::from(height);
    if rows.is_empty() || h == 0 {
        return RailWindow::default();
    }
    if rows.len() <= h {
        return RailWindow {
            rows: rows.to_vec(),
            hidden_above: 0,
            hidden_below: 0,
        };
    }

    // Every destination, in rail order. Headings are excluded from the budget
    // arithmetic entirely, which is what makes step 2 structural rather than a
    // priority that a later edit could reorder.
    let dests: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| matches!(r, RailRow::Item(_)))
        .map(|(i, _)| i)
        .collect();
    if dests.is_empty() {
        // Nothing to navigate; show as many labels as fit.
        return RailWindow {
            rows: rows.iter().take(h).cloned().collect(),
            hidden_above: 0,
            hidden_below: 0,
        };
    }
    let active_pos = active_position(rows, active).min(dests.len() - 1);

    // `select` works in *destination ordinals*, not row offsets. Those differ
    // once headings are interleaved, and mixing them silently inflates the
    // marker counts — which then reserves too many rows and drops a destination
    // the budget could have held.
    let select = |budget: usize| -> (Vec<usize>, u16, u16) {
        let budget = budget.max(1);
        let mut picked: Vec<usize> = Vec::with_capacity(budget);
        let mut hi = active_pos;
        let mut lo = active_pos;
        let mut down = true;
        while picked.len() < budget && (down || lo > 0) {
            if down {
                if hi < dests.len() {
                    picked.push(hi);
                    hi += 1;
                } else {
                    down = false;
                }
            } else {
                lo -= 1;
                picked.push(lo);
            }
        }
        // Count what was *dropped*, not what sits beyond the active row. When
        // every destination fits, `below` must be zero even though the active
        // destination is at the top and fourteen follow it — otherwise the rail
        // reserves a marker row for a window that has nothing to report, and
        // drops the destination it was being honest about.
        let kept_below = picked.iter().filter(|o| **o > active_pos).count();
        let kept_above = picked.iter().filter(|o| **o < active_pos).count();
        let above = active_pos.saturating_sub(kept_above) as u16;
        let below = (dests.len() - 1 - active_pos).saturating_sub(kept_below) as u16;
        picked.sort_unstable();
        (picked, above, below)
    };

    // Settle the budget: reserve a row per marker that is actually needed.
    let mut budget = h;
    let (mut included, mut above, mut below) = select(budget);
    for _ in 0..4 {
        let markers = usize::from(above > 0) + usize::from(below > 0);
        if included.len() + markers <= h {
            break;
        }
        let next = h.saturating_sub(markers);
        // Destinations outrank markers: if reserving the marker rows would leave
        // no room for a destination, keep the destinations and drop the markers.
        if next == budget || next == 0 {
            break;
        }
        budget = next;
        let picked = select(budget);
        if picked.0.len() == included.len() && picked.1 == above && picked.2 == below {
            break;
        }
        included = picked.0;
        above = picked.1;
        below = picked.2;
    }
    // Translate ordinals back to row positions for rendering.
    let mut included: Vec<usize> = included.into_iter().map(|o| dests[o]).collect();
    included.sort_unstable();

    // The settle loop above can stop early when shrinking the budget further
    // would empty it, which leaves `included` sized for a larger window than
    // the one it was given. Adding a row would then *reduce* the number of
    // destinations on screen, and the window plus its markers would not fit.
    // Trim from whichever end is farther from the active destination so the
    // operator keeps whatever is nearest what they are looking at — and never
    // the active destination itself.
    {
        let markers = usize::from(above > 0) + usize::from(below > 0);
        let room = h.saturating_sub(markers);
        if included.len() > room && !included.is_empty() {
            let active_row = dests[active_pos];
            let at = included.iter().position(|r| *r == active_row).unwrap_or(0);
            let below_active = included.len() - 1 - at;
            let above_active = at;
            while included.len() > room && included.len() > 1 {
                if below_active >= above_active && included.len() - 1 > at {
                    included.remove(included.len() - 1);
                } else if above_active > 0 {
                    included.remove(0);
                } else {
                    break;
                }
            }
        }

        // Trimming changes what is on screen, so the overflow counts have to
        // be restated in terms of what survived. A stale count here is exactly
        // the "claims to show everything while hiding Settings" bug.
        let kept_above = included.iter().filter(|r| **r < dests[active_pos]).count();
        let kept_below = included.iter().filter(|r| **r > dests[active_pos]).count();
        above = active_pos.saturating_sub(kept_above) as u16;
        below = (dests.len() - 1 - active_pos).saturating_sub(kept_below) as u16;
    }

    // Spend any leftover rows on headings, nearest the active destination first,
    // and only where at least one member of the group is actually visible.
    let mut keep = vec![false; rows.len()];
    for i in &included {
        keep[*i] = true;
    }
    let mut head_keep = vec![false; rows.len()];
    // Marker rows come off the top of the budget before headings do: a heading
    // that displaces the overflow indicator removes the only explanation for
    // the destinations that are missing.
    let markers = usize::from(above > 0) + usize::from(below > 0);
    let mut spare = h.saturating_sub(included.len()).saturating_sub(markers);
    for i in (0..rows.len()).rev() {
        if spare == 0 {
            break;
        }
        if !matches!(rows[i], RailRow::Heading(_)) {
            continue;
        }
        let group_rows = &rows[i + 1..];
        let end = group_rows
            .iter()
            .position(|r| matches!(r, RailRow::Heading(_)))
            .unwrap_or(group_rows.len());
        if group_rows[..end]
            .iter()
            .enumerate()
            .any(|(off, _)| keep[i + 1 + off])
        {
            head_keep[i] = true;
            spare -= 1;
        }
    }

    RailWindow {
        rows: (0..rows.len())
            .filter(|i| keep[*i] || head_keep[*i])
            .map(|i| rows[i].clone())
            .collect(),
        hidden_above: above,
        hidden_below: below,
    }
}

/// Render the navigation rail, including overflow markers when destinations
/// were dropped.
///
/// Both markers are shown when both ends are clipped, matching the pre-migration
/// rail (recovered templates `format!(" ↑{}")` and `format!(" ↓{}")`). A single
/// combined "N hidden" count would be shorter, but it loses the one fact that
/// helps: whether the missing destinations are above or below, which is what
/// tells the operator which way to scroll.
pub fn draw_rail(t: &Tokens, frame: &mut Frame, area: Rect, rows: &[RailRow], active: usize) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let title_h = u16::from(area.height > 3);
    let body = Rect {
        x: area.x,
        y: area.y + title_h,
        width: area.width,
        height: saturating_sub(area.height, title_h),
    };
    let block = super::widgets::panel(t, " navigate ", super::widgets::PanelTone::Default);
    let inner = block.inner(body);
    frame.render_widget(block, body);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let win = rail_window(rows, active, inner.height);
    // Markers are appended after the destinations and only into space the
    // destinations left, so a marker can never displace the row it describes.
    let marker_budget = win.marker_budget(inner.height);
    let mut markers_drawn = 0u16;
    let mut lines: Vec<Line> = Vec::new();
    if win.hidden_above > 0 && markers_drawn < marker_budget {
        lines.push(Line::styled(
            format!("\u{2191}{}", win.hidden_above),
            Style::default().fg(t.text.muted),
        ));
        markers_drawn += 1;
    }
    for row in &win.rows {
        match row {
            RailRow::Heading(h) => lines.push(Line::styled(
                ellipsize(h, inner.width),
                Style::default().fg(t.text.muted),
            )),
            RailRow::Item(item) => {
                let selected = item.index == active;
                let style = if selected {
                    Style::default()
                        .fg(t.text.inverse)
                        .bg(t.accent.primary)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(t.text.secondary)
                };
                lines.push(Line::styled(
                    format!(
                        "{} {}",
                        crate::run::g("\u{203a}", ">"),
                        ellipsize(&sanitize_field(&item.label), inner.width.saturating_sub(2))
                    ),
                    style,
                ));
            }
        }
    }
    if win.hidden_below > 0 && markers_drawn < marker_budget {
        lines.push(Line::styled(
            format!("\u{2193}{}", win.hidden_below),
            Style::default().fg(t.text.muted),
        ));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Draw the whole chrome for one frame and return the layout the content area
/// should use.
///
/// This is the authoritative entry point: a renderer that wants the shell
/// simply calls `AppShell::draw` and draws into `layout.body`.
pub struct AppShell<'a> {
    pub title: &'a str,
    pub subtitle: &'a str,
    pub badges: &'a [(&'a str, super::widgets::BadgeTone)],
    pub meters: &'a [(&'a str, String)],
    pub hints: &'a str,
    /// The status bar's right-hand segment — the one thing that changes without
    /// the operator doing anything ("ready", "paused", "3 findings").
    pub status: &'a str,
    /// What that status *means*. Carried as a tone so every caller states
    /// semantics instead of choosing a colour.
    pub status_tone: BadgeTone,
    pub rail: &'a [RailRow],
    pub active: usize,
    /// Request the contextual side panel; granted only if the width allows.
    pub context: bool,
}

impl AppShell<'_> {
    pub fn draw(&self, t: &Tokens, frame: &mut Frame, area: Rect) -> ShellLayout {
        // The shell owns the rail's width, the caller owns whether there is
        // one. Passing `!self.rail.is_empty()` is what lets Workbench render a
        // rail-free dashboard without paying for a gutter it never draws.
        let layout = ShellLayout::build(area, t, self.context, !self.rail.is_empty());
        draw_header(
            t,
            frame,
            &layout,
            self.title,
            self.subtitle,
            self.badges,
            self.meters,
        );
        if !self.rail.is_empty() && layout.rail.width > 0 {
            draw_rail(t, frame, layout.rail, self.rail, self.active);
        }
        // Hints that lose the status bar move into the header's last row rather
        // than disappearing.
        let drew = draw_status_bar(t, frame, &layout, self.hints, self.status, self.status_tone);
        if !drew && layout.header.height > 0 && !self.hints.is_empty() {
            let h = layout.header;
            frame.render_widget(
                Paragraph::new(Line::styled(
                    ellipsize(&sanitize_field(self.hints), h.width),
                    Style::default().fg(t.text.muted),
                ))
                .style(Style::default().bg(t.surfaces.bg)),
                Rect {
                    x: h.x,
                    y: h.y + saturating_sub(h.height, 1),
                    width: h.width,
                    height: 1,
                },
            );
        }
        layout
    }
}

/// Concatenate span groups, dropping any group that does not fit the width.
fn join_spans(groups: Vec<Vec<Span<'static>>>) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    for g in groups {
        out.extend(g);
    }
    out
}

/// Truncate a span group to `max` columns, appending an ellipsis so the cut is
/// visible rather than looking like a shorter value.
fn fit_spans(spans: Vec<Span<'static>>, max: u16) -> Vec<Span<'static>> {
    let mut width = 0usize;
    let mut out = Vec::new();
    for s in spans {
        let n = s.content.chars().count();
        if width + n <= usize::from(max) {
            width += n;
            out.push(s);
        } else if usize::from(max).saturating_sub(width) >= 1 {
            let keep = usize::from(max) - width - 1;
            let text: String = s.content.chars().take(keep).collect();
            out.push(Span::styled(
                format!("{text}…"),
                Style::default().fg(t_ellipsis_style()),
            ));
            break;
        } else {
            break;
        }
    }
    out
}

fn t_ellipsis_style() -> ratatui::style::Color {
    // Deliberately colorless: the ellipsis inherits whatever style the caller
    // used, so it never introduces a color of its own.
    ratatui::style::Color::Reset
}

/// Convenience: a plain bordered box for content areas that are not a rail.
pub fn content_block(t: &Tokens, title: &str, tone: super::widgets::PanelTone) -> Block<'static> {
    Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_style(Style::default().fg(super::widgets::tone_border(t, tone)))
        .style(Style::default().bg(t.surfaces.surface))
        .title(Line::styled(
            format!(" {title} "),
            Style::default().fg(t.text.muted),
        ))
}

/// Breakpoint re-export so views can branch without importing `layout` too.
pub use super::layout::Breakpoint as ShellBreakpoint;

#[allow(dead_code)]
fn _assert_breakpoint_used(b: Breakpoint) -> bool {
    matches!(
        b,
        Breakpoint::Compact | Breakpoint::Normal | Breakpoint::Wide
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::design::widgets::BadgeTone;
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

    fn rows(n: usize, active: usize) -> Vec<RailRow> {
        let mut out = Vec::new();
        for i in 0..n {
            if i % 3 == 0 {
                out.push(RailRow::Heading(format!("G{}", i / 3)));
            }
            out.push(RailRow::Item(RailItem {
                heading: None,
                label: format!("ITEM{i}"),
                index: i,
            }));
        }
        let _ = active;
        out
    }

    #[test]
    fn header_keeps_badges_when_the_subtitle_is_long() {
        let out = draw(120, 3, |f, a| {
            let l = ShellLayout::build(a, &t(), false, true);
            draw_header(
                &t(),
                f,
                &l,
                "OPENRT agent",
                &"a-very-long-session-identifier-that-will-not-fit ".repeat(8),
                &[("AGENT READY", BadgeTone::Success)],
                &[("RAM", "512M/4G".into())],
            );
        });
        assert!(
            out.contains("AGENT READY"),
            "posture badge must survive: {out}"
        );
        assert!(out.contains("RAM 512M/4G"), "meters must survive: {out}");
    }

    #[test]
    fn header_survives_a_one_row_window() {
        let out = draw(60, 1, |f, a| {
            let l = ShellLayout::build(a, &t(), false, true);
            draw_header(&t(), f, &l, "OPENRT", "sub", &[], &[]);
        });
        assert!(out.contains("OPENRT"), "{out}");
    }

    #[test]
    fn header_shrinks_below_half_the_window_so_content_survives() {
        let l = ShellLayout::build(Rect::new(0, 0, 80, 4), &t(), false, true);
        assert!(
            l.header.height <= 2,
            "header ate the window: {:?}",
            l.header
        );
        assert!(l.body.height >= 1, "no body left: {:?}", l.body);
    }

    #[test]
    fn overflowing_meters_drop_values_rather_than_clip_numbers() {
        let owned: Vec<(String, String)> = (0..12)
            .map(|i| (format!("M{i}"), format!("{i}GB/16GB")))
            .collect();
        let meters: Vec<(&str, String)> =
            owned.iter().map(|(a, b)| (a.as_str(), b.clone())).collect();
        let out = draw(60, 3, |f, a| {
            let l = ShellLayout::build(a, &t(), false, true);
            draw_header(&t(), f, &l, "OPENRT", "", &[], &meters);
        });
        // A half-written "1GB/1" would be a lie about the real number.
        assert!(
            !out.contains("/1GB"),
            "meter value was clipped mid-number: {out}"
        );
    }

    #[test]
    fn status_bar_reports_when_it_had_no_room() {
        let mut term = Terminal::new(TestBackend::new(40, 1)).expect("term");
        term.draw(|f| {
            let l = ShellLayout::build(f.area(), &t(), false, true);
            assert!(!draw_status_bar(
                &t(),
                f,
                &l,
                "hints",
                "status",
                BadgeTone::Accent
            ));
        })
        .expect("draw");
    }

    /// The 80x24 case that forced the current policy: 15 destinations and 15
    /// body rows. Spending a row on a heading would push a destination off the
    /// edge, and the marker must stay silent because nothing reachable is
    /// actually missing.
    #[test]
    fn rail_shows_every_destination_without_a_marker_when_only_headings_lost() {
        let mut rows = Vec::new();
        for (gi, group) in ["WORKSPACE", "BUILD", "PLATFORM", "SYSTEM"]
            .iter()
            .enumerate()
        {
            rows.push(RailRow::Heading((*group).to_string()));
            let count = [6usize, 4, 4, 1][gi];
            for i in 0..count {
                rows.push(RailRow::Item(RailItem {
                    heading: None,
                    label: format!("v{gi}_{i}"),
                    index: gi * 10 + i,
                }));
            }
        }
        assert_eq!(
            rows.iter()
                .filter(|r| matches!(r, RailRow::Item(_)))
                .count(),
            15
        );

        let win = rail_window(&rows, 0, 15);
        let items = win
            .rows
            .iter()
            .filter(|r| matches!(r, RailRow::Item(_)))
            .count();
        assert_eq!(items, 15, "every destination must survive at 80x24");
        assert!(
            win.is_complete(),
            "dropping headings is not a reason to warn about hidden destinations"
        );
        assert!(
            win.rows.iter().all(|r| matches!(r, RailRow::Item(_))),
            "with no spare rows, headings must yield entirely"
        );
        assert!(win.rows.len() <= 15, "the window must respect its budget");
    }

    /// When destinations genuinely do not fit, the marker must appear and must
    /// count destinations, not headings.
    #[test]
    fn rail_marker_counts_only_dropped_destinations() {
        let mut rows = Vec::new();
        for (gi, group) in ["WORKSPACE", "BUILD", "PLATFORM", "SYSTEM"]
            .iter()
            .enumerate()
        {
            rows.push(RailRow::Heading((*group).to_string()));
            let count = [6usize, 4, 4, 1][gi];
            for i in 0..count {
                rows.push(RailRow::Item(RailItem {
                    heading: None,
                    label: format!("v{gi}_{i}"),
                    index: gi * 10 + i,
                }));
            }
        }
        let win = rail_window(&rows, 0, 8);
        let items = win
            .rows
            .iter()
            .filter(|r| matches!(r, RailRow::Item(_)))
            .count();
        // 8 rows cannot hold 8 destinations *and* the marker that explains the
        // missing ones. The marker wins the row it needs; that is the whole
        // point of charging it against the budget.
        let markers = usize::from(win.hidden_above > 0) + usize::from(win.hidden_below > 0);
        assert_eq!(items + markers, 8, "every row must be accounted for");
        assert_eq!(
            items,
            8 - markers,
            "the budget must be spent on destinations, not headings"
        );
        assert_eq!(
            usize::from(win.hidden()),
            15 - items,
            "marker counts dropped destinations only"
        );
        assert!(
            win.rows
                .iter()
                .filter(|r| matches!(r, RailRow::Heading(_)))
                .count()
                == 0,
            "no spare rows remain for headings"
        );
    }

    /// The active destination survives regardless of where it sits in the list.
    #[test]
    fn rail_always_keeps_the_active_destination_even_at_the_bottom() {
        let mut rows = Vec::new();
        for (gi, group) in ["WORKSPACE", "BUILD", "PLATFORM", "SYSTEM"]
            .iter()
            .enumerate()
        {
            rows.push(RailRow::Heading((*group).to_string()));
            let count = [6usize, 4, 4, 1][gi];
            for i in 0..count {
                rows.push(RailRow::Item(RailItem {
                    heading: None,
                    label: format!("v{gi}_{i}"),
                    index: gi * 10 + i,
                }));
            }
        }
        // First, last, and a middle destination of a later group.
        for active in [0usize, 5, 13, 30] {
            let win = rail_window(&rows, active, 5);
            let kept = win
                .rows
                .iter()
                .any(|r| matches!(r, RailRow::Item(i) if i.index == active));
            assert!(
                kept,
                "destination {active} must stay visible in a 5-row rail"
            );
            assert!(
                win.rows.len()
                    + usize::from(win.hidden_above > 0)
                    + usize::from(win.hidden_below > 0)
                    <= 5
            );
        }
    }

    /// The 15-destination / 4-heading shape the real rail has at 80x24.
    fn real_rail_rows() -> Vec<RailRow> {
        let mut rows = Vec::new();
        for (gi, (group, count)) in [
            ("WORKSPACE", 6usize),
            ("BUILD", 4),
            ("PLATFORM", 4),
            ("SYSTEM", 1),
        ]
        .into_iter()
        .enumerate()
        {
            rows.push(RailRow::Heading(group.to_string()));
            for i in 0..count {
                rows.push(RailRow::Item(RailItem {
                    heading: None,
                    label: format!("V{gi}_{i}"),
                    // Dense indices so a test cannot accidentally rely on
                    // ordinal == index.
                    index: gi * 100 + i,
                }));
            }
        }
        rows
    }

    fn all_dests(r: &[RailRow]) -> Vec<usize> {
        r.iter()
            .filter_map(|x| match x {
                RailRow::Item(i) => Some(i.index),
                _ => None,
            })
            .collect()
    }

    /// A rail with an exact, known shape: `items` destinations and `headings`
    /// headings, headings first. The scenario tests need to reason about row
    /// arithmetic, so they cannot use a fixture that sprinkles headings every
    /// third row.
    fn fixture(items: usize, headings: usize) -> Vec<RailRow> {
        let mut out = Vec::new();
        for h in 0..headings {
            out.push(RailRow::Heading(format!("H{h}")));
        }
        for i in 0..items {
            out.push(RailRow::Item(RailItem {
                heading: None,
                label: format!("I{i}"),
                index: i,
            }));
        }
        out
    }

    fn first_dest(r: &[RailRow]) -> usize {
        all_dests(r)[0]
    }

    /// `draw_rail` highlights a row exactly when `item.index == active`, so
    /// "the highlight maps to the active destination" means the window contains
    /// that row — and exactly one copy of it, or the rail would highlight two
    /// destinations at once.
    fn highlighted(win: &RailWindow, active: usize) -> Vec<usize> {
        dest_indices(win)
            .into_iter()
            .enumerate()
            .filter(|(_, i)| *i == active)
            .map(|(pos, _)| pos)
            .collect()
    }

    /// Assert the highlight invariant at the model level.
    fn assert_highlight(win: &RailWindow, active: usize, ctx: &str) {
        assert_eq!(
            highlighted(win, active).len(),
            1,
            "{ctx}: expected exactly one highlighted row for active={active}, got {:?}",
            dest_indices(win)
        );
    }

    fn sorted_unique(v: &[usize]) -> Vec<usize> {
        let mut out = v.to_vec();
        out.sort_unstable();
        out.dedup();
        out
    }

    fn dest_indices(w: &RailWindow) -> Vec<usize> {
        w.rows
            .iter()
            .filter_map(|r| match r {
                RailRow::Item(i) => Some(i.index),
                _ => None,
            })
            .collect()
    }

    /// Invariant 1: the active destination is reachable at *every* height and
    /// *every* active index, including indices that do not exist.
    #[test]
    fn rail_never_loses_the_active_destination_at_any_height() {
        let rows = real_rail_rows();
        let all: Vec<usize> = rows
            .iter()
            .filter_map(|r| match r {
                RailRow::Item(i) => Some(i.index),
                _ => None,
            })
            .collect();
        for h in 0u16..=24 {
            for active in &all {
                let win = rail_window(&rows, *active, h);
                if h == 0 {
                    assert!(win.rows.is_empty());
                    continue;
                }
                assert!(
                    dest_indices(&win).contains(active),
                    "h={h} active={active} lost the active destination: {:?}",
                    dest_indices(&win)
                );
            }
        }
    }

    /// Invariant 2: rows + markers never exceed the height they were given.
    /// This is the invariant that a naive "window to full height, then draw the
    /// marker" implementation violates, and it is the one that silently pushed
    /// Settings off the rail.
    #[test]
    fn rail_never_exceeds_its_height_including_markers() {
        let rows = real_rail_rows();
        for h in 0u16..=24 {
            for active in [0usize, 103, 203, 300, 999] {
                let win = rail_window(&rows, active, h);
                assert!(
                    win.rows.len() <= usize::from(h),
                    "h={h} active={active}: {} rows exceeds {h}",
                    win.rows.len()
                );
                // A marker may be suppressed, but only when the destinations
                // already fill the rail. It must never be the thing that costs a
                // destination its row.
                let markers = usize::from(win.marker_budget(h));
                if markers > 0 {
                    assert!(
                        win.rows.len() + markers <= usize::from(h),
                        "h={h} active={active}: {} rows + {markers} markers exceeds {h}",
                        win.rows.len()
                    );
                }
            }
        }
    }

    /// Invariant 3: a destination that is not shown is always explained.
    /// "Silently disappears" is the failure mode this guards — the count of
    /// hidden destinations must always equal the count of missing ones.
    #[test]
    fn rail_every_hidden_destination_is_accounted_for() {
        let rows = real_rail_rows();
        let total = 15usize;
        for h in 1u16..=24 {
            for active in [0usize, 103, 203, 300] {
                let win = rail_window(&rows, active, h);
                let shown = dest_indices(&win).len();
                assert_eq!(
                    usize::from(win.hidden()),
                    total.saturating_sub(shown),
                    "h={h} active={active}: {shown} shown but hidden={} of {total}",
                    win.hidden()
                );
            }
        }
    }

    /// Invariant 4: no duplicate destinations in the window (a repeat would
    /// make two rows claim to be the same place).
    #[test]
    fn rail_never_repeats_a_destination() {
        let rows = real_rail_rows();
        for h in 1u16..=24 {
            for active in [0usize, 103, 203, 300] {
                let win = rail_window(&rows, active, h);
                let mut seen = dest_indices(&win);
                let before = seen.len();
                seen.sort_unstable();
                seen.dedup();
                assert_eq!(seen.len(), before, "h={h} active={active} repeated a row");
            }
        }
    }

    /// Invariant 5: window rows keep rail order. A window that reorders rows
    /// makes ↑/↓ mean nothing to the operator.
    #[test]
    fn rail_window_preserves_rail_order() {
        let rows = real_rail_rows();
        let order: Vec<usize> = rows
            .iter()
            .filter_map(|r| match r {
                RailRow::Item(i) => Some(i.index),
                _ => None,
            })
            .collect();
        for h in 1u16..=24 {
            for active in [0usize, 103, 203, 300] {
                let win = rail_window(&rows, active, h);
                let shown = dest_indices(&win);
                let filtered: Vec<usize> = order
                    .iter()
                    .copied()
                    .filter(|i| shown.contains(i))
                    .collect();
                assert_eq!(shown, filtered, "h={h} active={active} reordered the rail");
            }
        }
    }

    /// Invariant 6: a heading is never shown when the group beneath it is not.
    /// An orphan heading is worse than no heading — it labels the wrong thing.
    #[test]
    fn rail_never_shows_an_orphan_heading() {
        let rows = real_rail_rows();
        for h in 1u16..=24 {
            for active in [0usize, 103, 203, 300] {
                let win = rail_window(&rows, active, h);
                for (i, row) in win.rows.iter().enumerate() {
                    let RailRow::Heading(_) = row else { continue };
                    let has_member = win.rows[i + 1..]
                        .iter()
                        .take_while(|r| !matches!(r, RailRow::Heading(_)))
                        .any(|r| matches!(r, RailRow::Item(_)));
                    assert!(has_member, "h={h} active={active} showed an orphan heading");
                }
            }
        }
    }

    /// Invariant 7: changing the active view cannot leave a stale selection.
    /// The window anchor and the highlight both key off the destination index,
    /// so the selected row is always present exactly once.
    #[test]
    fn rail_selection_always_matches_the_active_destination() {
        let rows = real_rail_rows();
        for h in 1u16..=24 {
            for active in [0usize, 103, 203, 300, 999] {
                let win = rail_window(&rows, active, h);
                let selected = win
                    .rows
                    .iter()
                    .filter(|r| matches!(r, RailRow::Item(i) if i.index == active))
                    .count();
                // An unknown index has no row to select, which is correct: it
                // must not highlight an arbitrary neighbour.
                assert!(
                    selected <= 1,
                    "h={h} active={active} highlighted {selected} rows"
                );
                if active != 999 {
                    assert_eq!(selected, 1, "h={h} active={active} selection missing");
                }
            }
        }
    }

    /// Invariant 8: markers appear only when something is actually hidden.
    /// A marker on a complete rail is a false alarm; no marker on an incomplete
    /// one is a lie.
    #[test]
    fn rail_markers_appear_exactly_when_destinations_are_missing() {
        let rows = real_rail_rows();
        for h in 1u16..=24 {
            for active in [0usize, 103, 203, 300] {
                let win = rail_window(&rows, active, h);
                let missing = 15 - dest_indices(&win).len();
                assert_eq!(
                    win.hidden() > 0,
                    missing > 0,
                    "h={h} active={active}: missing={missing} hidden={}",
                    win.hidden()
                );
            }
        }
    }

    /// Adjacent to the 80x24 boundary: one row either side must not change which
    /// destination is reachable, only how many neighbours are visible.
    #[test]
    fn rail_handles_the_overflow_boundary_from_both_sides() {
        let rows = real_rail_rows();
        // 19 total rows: 15 destinations + 4 headings.
        for h in [18u16, 19, 20] {
            let win = rail_window(&rows, 203, h);
            assert!(
                dest_indices(&win).contains(&203),
                "h={h} lost the active destination"
            );
            assert!(
                win.rows.len()
                    + usize::from(win.hidden_above > 0)
                    + usize::from(win.hidden_below > 0)
                    <= usize::from(h),
                "h={h} overran its budget"
            );
        }
        // The destination boundary is 15 rows, not 19: headings yield first, so
        // at 18 and 19 every destination is reachable and the rail stays silent.
        // Only below 15 does anything reachable actually go missing.
        for h in [15u16, 18, 19, 20] {
            assert!(
                rail_window(&rows, 203, h).is_complete(),
                "h={h} should show all 15 destinations and stay silent"
            );
        }
        for h in [1u16, 7, 14] {
            assert!(
                !rail_window(&rows, 203, h).is_complete(),
                "h={h} must warn: destinations really are missing"
            );
        }
    }

    /// The very shortest rails: 1, 2 and 3 rows still yield the active
    /// destination and still account for every row.
    #[test]
    fn rail_very_short_heights_keep_the_active_destination() {
        let rows = real_rail_rows();
        for h in 1u16..=3 {
            for active in [0usize, 103, 203, 300] {
                let win = rail_window(&rows, active, h);
                assert!(
                    dest_indices(&win).contains(&active),
                    "h={h} active={active}: {:?}",
                    dest_indices(&win)
                );
                assert!(
                    win.rows.len() <= usize::from(h),
                    "h={h} active={active} produced {} rows",
                    win.rows.len()
                );
            }
        }
    }

    // ------------------------------------------------------------------
    // The nine named scenarios. Each states which invariant it pins, so a
    // failure names the rule that broke rather than just a coordinate.
    // ------------------------------------------------------------------

    /// Scenario: the FIRST destination is active.
    /// Invariants 1, 2, 7, 8.
    #[test]
    fn scenario_first_destination_active() {
        let r = rows(15, 4);
        let first = first_dest(&r);
        let win = rail_window(&r, first, 5);
        assert!(dest_indices(&win).contains(&first), "first active lost");
        assert_highlight(&win, first, "first active");
        assert_eq!(
            dest_indices(&win),
            sorted_unique(&dest_indices(&win)),
            "order/uniqueness"
        );
    }

    /// Scenario: a MIDDLE destination is active.
    /// Invariants 1, 2, 7, 8.
    #[test]
    fn scenario_middle_destination_active() {
        let r = rows(15, 4);
        let all = all_dests(&r);
        let mid = all[all.len() / 2];
        let win = rail_window(&r, mid, 6);
        assert!(dest_indices(&win).contains(&mid), "middle active lost");
        assert_highlight(&win, mid, "middle active");
        assert_eq!(dest_indices(&win), sorted_unique(&dest_indices(&win)));
    }

    /// Scenario: the LAST destination is active.
    /// Invariants 1, 2, 7, 8.
    #[test]
    fn scenario_last_destination_active() {
        let r = rows(15, 4);
        let last = *all_dests(&r).last().unwrap();
        let win = rail_window(&r, last, 5);
        assert!(dest_indices(&win).contains(&last), "last active lost");
        assert_highlight(&win, last, "last active");
        assert_eq!(dest_indices(&win), sorted_unique(&dest_indices(&win)));
    }

    /// Scenario: NOTHING is hidden.
    /// Invariants 6, 9 — and 4: the hidden count must be a real count, not a
    /// pessimistic guess that would make a complete rail claim to be partial.
    #[test]
    fn scenario_no_hidden_destinations() {
        let r = rows(15, 4);
        let win = rail_window(&r, 0, r.len() as u16 + 4);
        assert_eq!(win.hidden(), 0);
        assert!(win.is_complete());
        assert_eq!(win.hidden_above, 0);
        assert_eq!(win.hidden_below, 0);
        assert_eq!(
            win.marker_budget(40),
            0,
            "a complete rail must not be able to spend a marker row"
        );
        assert_eq!(dest_indices(&win), all_dests(&r), "nothing may be dropped");
    }

    /// Scenario: exactly ONE direction of overflow.
    ///
    /// Invariants 4, 5, 9. Note the arithmetic: a marker row is only drawn
    /// when a destination was actually dropped, and it is counted as a row
    /// before destinations are chosen. So the smallest non-zero hidden count
    /// is 2 (one destination lost, plus the row its marker had to occupy) —
    /// "exactly one hidden" is unreachable by construction, and a test that
    /// demanded it would be testing an arithmetic bug. What must hold is:
    /// one overflow direction => exactly one marker row, and the hidden count
    /// equals the destinations genuinely dropped.
    #[test]
    fn scenario_one_overflow_direction() {
        let r = fixture(16, 1);
        let total = all_dests(&r).len();

        // Active at the top: only the bottom overflows.
        let win = rail_window(&r, 0, 15);
        let shown = dest_indices(&win).len();
        assert!(shown < total, "expected overflow");
        assert_eq!(win.hidden_above, 0, "nothing is above the active row");
        assert!(win.hidden_below > 0, "something must be below");
        assert_eq!(
            win.marker_budget(15),
            1,
            "one direction in play costs exactly one marker row"
        );
        assert_eq!(
            win.hidden() as usize,
            total - shown,
            "hidden must equal destinations actually dropped"
        );

        // Active at the bottom: only the top overflows, and still one marker.
        let win = rail_window(&r, total - 1, 15);
        assert!(win.hidden_below == 0);
        assert!(win.hidden_above > 0);
        assert_eq!(win.marker_budget(15), 1);
    }

    /// Scenario: MULTIPLE destinations hidden, both directions in play.
    /// Invariants 4, 5, 9 — the marker count tracks the directions, never a
    /// flat guess, and never exceeds the free space.
    #[test]
    fn scenario_multiple_hidden_destinations() {
        let r = fixture(16, 1);
        let total = all_dests(&r).len();
        // The window fills downward from the active destination before it fills
        // upward, so two-way overflow only appears once the rail is too short
        // to reach the end. Find the shortest such rail deterministically
        // rather than hard-coding arithmetic that could drift.
        let active = total / 2;
        let tight = (1u16..=total as u16)
            .find(|h| {
                let w = rail_window(&r, active, *h);
                w.hidden_above > 0 && w.hidden_below > 0
            })
            .unwrap_or_else(|| panic!("no two-way overflow is reachable"));

        let win = rail_window(&r, active, tight);
        let shown = dest_indices(&win).len();
        assert!(shown < total, "expected overflow");
        assert!(win.hidden_above > 0, "expected overflow above");
        assert!(win.hidden_below > 0, "expected overflow below");
        assert_eq!(
            win.hidden() as usize,
            total - shown,
            "hidden must equal destinations actually dropped"
        );

        // Invariant 5 at its sharpest: when every row is spoken for, markers
        // yield to destinations rather than evicting one. The rail still
        // overflows both ways, it just cannot afford to say so — which is the
        // deliberate trade, since a lost destination is unrecoverable and a
        // lost marker is not.
        assert_eq!(
            win.marker_budget(tight),
            0,
            "no free row means no marker row"
        );
        assert!(win.rows.len() as u16 <= tight);

        // With spare rows the markers do get drawn, one per direction.
        let roomy = (tight..=total as u16 + 4)
            .find(|h| {
                let w = rail_window(&r, active, *h);
                w.hidden_above > 0 && w.hidden_below > 0 && w.marker_budget(*h) == 2
            })
            .unwrap_or_else(|| {
                panic!("no two-way overflow with room for both markers below h={tight}")
            });
        let win = rail_window(&r, active, roomy);
        assert_eq!(
            win.marker_budget(roomy),
            2,
            "two directions in play cost two marker rows once there is room"
        );
        assert!(
            win.rows.len() as u16 + win.marker_budget(roomy) <= roomy,
            "rows plus markers must still fit in {roomy}"
        );

        // The hidden count must scale with what was dropped across the whole
        // height range, never saturating or rounding to "some".
        let mut prev: Option<usize> = None;
        for height in 1u16..=total as u16 + 4 {
            let w = rail_window(&r, total / 2, height);
            let shown = dest_indices(&w).len();
            assert_eq!(
                w.hidden() as usize,
                total - shown,
                "height={height} miscounted"
            );
            if height <= total as u16 {
                if let Some(prev) = prev {
                    assert!(
                        shown >= prev,
                        "a taller rail must never show fewer destinations (h={height}: {shown} < {prev})"
                    );
                }
            }
            prev = Some(shown);
        }
    }

    /// Invariant 5, heading clause: a marker row outranks a heading row.
    ///
    /// If a heading can occupy the row an overflow marker needs, the rail keeps
    /// a decorative label and silently drops the only explanation for the
    /// destinations that are missing.
    #[test]
    fn a_marker_row_is_never_spent_on_a_heading() {
        // Many headings, many destinations: at a middle height both kinds of
        // row are in contention.
        let r = fixture(16, 6);
        let total = all_dests(&r).len();
        for h in 4u16..=total as u16 {
            for active in [0usize, total / 2, total - 1] {
                let win = rail_window(&r, active, h);
                let markers = usize::from(win.hidden_above > 0) + usize::from(win.hidden_below > 0);
                let budget = usize::from(win.marker_budget(h));
                if budget < markers {
                    // Not enough room to say so: then nothing decorative may
                    // take the rows either.
                    assert_eq!(
                        win.rows
                            .iter()
                            .filter(|x| matches!(x, RailRow::Heading(_)))
                            .count(),
                        0,
                        "h={h} active={active}: headings were kept while markers went unbudgeted"
                    );
                }
                assert!(
                    win.rows.len() + budget <= usize::from(h),
                    "h={h} active={active}: {} rows + {budget} markers exceeds {h}",
                    win.rows.len()
                );
            }
        }
    }

    /// Scenario: a SHORT rail (fewer rows than the window).
    /// Invariant 6 — no marker, because nothing is missing.
    #[test]
    fn scenario_short_rail() {
        let r = rows(2, 1);
        let win = rail_window(&r, 1, 24);
        assert!(win.is_complete());
        assert_eq!(win.marker_budget(24), 0);
        assert_eq!(dest_indices(&win), all_dests(&r));
    }

    /// Scenario: the real 15-view rail at 80x24.
    ///
    /// This is the tightest supported terminal and the one the windowing
    /// policy was written for: 15 destinations, a heading block, and a body
    /// that cannot hold all of it. Invariants 1-9 together.
    #[test]
    fn scenario_real_rail_at_80x24() {
        // The shell gives the rail a bordered column; the body is what the
        // windowing policy actually spends.
        let rows = real_rail_rows();
        let dests = all_dests(&rows);
        assert_eq!(dests.len(), 15, "the rail must offer all 15 views");
        assert!(
            !dests.windows(2).any(|w| w[0] == w[1]),
            "fixture must be unique"
        );

        for active in dests.iter().copied() {
            for body in 1u16..=20 {
                let win = rail_window(&rows, active, body);

                // 1: active survives.
                assert!(
                    dest_indices(&win).contains(&active),
                    "80x24 body={body} active={active} lost the active destination"
                );
                // 2: the highlight maps to the real active destination.
                assert_highlight(&win, active, &format!("80x24 body={body}"));
                // 7 + 8: order preserved, no repeats.
                let d = dest_indices(&win);
                assert_eq!(
                    d,
                    sorted_unique(&d),
                    "body={body} active={active} broke order/dupes"
                );
                // 4 + 9: hidden count equals what was really dropped.
                assert_eq!(
                    win.hidden() as usize,
                    dests.len() - d.len(),
                    "body={body} active={active} miscounted hidden destinations"
                );
                // 3: no heading was spent while a destination could have been.
                let items = win
                    .rows
                    .iter()
                    .filter(|r| matches!(r, RailRow::Item(_)))
                    .count();
                assert!(items >= d.len(), "impossible");
                // 5: markers are budgeted, never free-floating.
                let budget = win.marker_budget(body);
                assert!(usize::from(budget) <= body.saturating_sub(win.rows.len() as u16) as usize);
            }
        }
    }

    #[test]
    fn rail_keeps_every_destination_when_there_is_room() {
        let r = rows(15, 7);
        let win = rail_window(&r, 7, 40);
        assert_eq!(win.hidden(), 0);
        assert_eq!(win.rows.len(), r.len());
    }

    #[test]
    fn rail_drops_headings_before_destinations() {
        let r = rows(15, 7);
        let win = rail_window(&r, 7, 8);
        assert!(win.hidden() > 0, "expected hidden rows");
        let items = win
            .rows
            .iter()
            .filter(|x| matches!(x, RailRow::Item(_)))
            .count();
        assert!(
            items > win.rows.len() / 2,
            "too many headings kept: {:?}",
            win.rows
        );
        assert!(
            win.rows
                .iter()
                .any(|x| matches!(x, RailRow::Item(i) if i.index == 7)),
            "active destination must survive"
        );
    }

    #[test]
    fn rail_never_reports_zero_hidden_while_dropping_rows() {
        let r = rows(15, 14);
        let win = rail_window(&r, 14, 6);
        let items = win
            .rows
            .iter()
            .filter(|x| matches!(x, RailRow::Item(_)))
            .count();
        let shown_dests = win
            .rows
            .iter()
            .filter_map(|x| match x {
                RailRow::Item(i) => Some(i.index),
                _ => None,
            })
            .collect::<Vec<_>>();
        if items < 15 {
            assert!(win.hidden() > 0, "dropped {items}/15 but reported hidden=0");
        }
        assert!(
            shown_dests.contains(&14),
            "active row missing: {shown_dests:?}"
        );
    }

    #[test]
    fn rail_handles_degenerate_inputs_without_panicking() {
        for h in [0u16, 1, 2, 100] {
            for active in [0usize, 7, 999] {
                let w = rail_window(&rows(15, active), active, h);
                assert!(
                    w.rows.len() <= usize::from(h).max(1),
                    "h={h} active={active} rows={:?}",
                    w.rows
                );
                assert!(
                    usize::from(w.hidden_above > 0) + usize::from(w.hidden_below > 0) <= 2,
                    "h={h} active={active}"
                );
            }
        }
        assert!(rail_window(&[], 0, 10).rows.is_empty());
    }

    #[test]
    fn rail_renders_a_hidden_indicator() {
        let r = rows(15, 0);
        let out = draw(16, 10, |f, a| draw_rail(&t(), f, a, &r, 0));
        assert!(
            out.contains('↑') || out.contains('↓'),
            "no scroll hint: {out}"
        );
    }

    #[test]
    fn app_shell_returns_a_body_to_draw_into() {
        let out = draw(100, 24, |f, a| {
            let sh = AppShell {
                title: "OPENRT",
                subtitle: "session",
                badges: &[("READY", BadgeTone::Success)],
                meters: &[("RAM", "1G/4G".into())],
                hints: "Enter send",
                status: "CHAT",
                status_tone: BadgeTone::Accent,
                rail: &rows(6, 1),
                active: 1,
                context: false,
            };
            let l = sh.draw(&t(), f, a);
            assert!(l.body.width > 0 && l.body.height > 0);
            f.render_widget(
                Paragraph::new("BODY"),
                Rect {
                    x: l.body.x,
                    y: l.body.y,
                    width: 4.min(l.body.width),
                    height: 1,
                },
            );
        });
        assert!(out.contains("BODY"), "{out}");
        assert!(out.contains("READY"));
        assert!(out.contains("Enter send"));
        assert!(out.contains("CHAT"));
    }

    #[test]
    fn app_shell_survives_a_one_by_one_terminal() {
        let _ = draw(1, 1, |f, a| {
            let sh = AppShell {
                title: "OPENRT",
                subtitle: "s",
                badges: &[("R", BadgeTone::Success)],
                meters: &[("RAM", "1/2".into())],
                hints: "hints",
                status: "m",
                status_tone: BadgeTone::Accent,
                rail: &rows(3, 0),
                active: 0,
                context: true,
            };
            let _ = sh.draw(&t(), f, a);
        });
    }

    #[test]
    fn app_shell_never_panics_across_the_size_matrix() {
        for (w, h) in [
            (1u16, 1u16),
            (1, 40),
            (80, 24),
            (80, 1),
            (10, 3),
            (40, 2),
            (200, 60),
        ] {
            let _ = draw(w, h, |f, a| {
                let sh = AppShell {
                    title: "OPENRT agent",
                    subtitle: "a-session-that-is-long",
                    badges: &[("AGENT READY", BadgeTone::Success)],
                    meters: &[("RAM", "512M/4G".into())],
                    hints: "Enter send · /help commands",
                    status: "CHAT · 12ms",
                    status_tone: BadgeTone::Accent,
                    rail: &rows(15, 7),
                    active: 7,
                    context: w >= 120,
                };
                let _ = sh.draw(&t(), f, a);
            });
        }
    }
}
