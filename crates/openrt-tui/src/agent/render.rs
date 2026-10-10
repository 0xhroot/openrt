//! Agent Mode rendering: pitch-black surfaces, thin borders, violet accent.
//! Layout mirrors the reference screens: header pills, left rail, chat stream
//! with tool cards, always-visible input, right context column.

use ratatui::layout::{Constraint, Direction, Layout as TuiLayout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::agent::state::{
    AgentApp, Msg, MsgKind, NavGroup, Overlay, ResourceCard, ToolState, UiMode, View,
};
use crate::design::modals::PermissionRequest;
use crate::design::tokens::{saturating_sub, Tokens};
use crate::design::widgets::BadgeTone;
use crate::design::{
    self,
    layout::{HitTarget, ShellLayout},
    FocusStop, FocusTarget,
};
use crate::run::{ascii_mode, g, sanitize_text};

/// ASCII-mode pass over one rendered line: styles and structure preserved,
/// only the glyphs a non-UTF-8 terminal cannot draw are mapped (status text,
/// chat messages, tool args and model output all carry arbitrary Unicode).
fn ascii_line<'a>(mut l: Line<'a>) -> Line<'a> {
    if !ascii_mode() {
        return l;
    }
    for s in &mut l.spans {
        let cleaned = sanitize_text(&s.content);
        if cleaned != s.content {
            s.content = cleaned.into();
        }
    }
    l
}

pub fn draw(frame: &mut Frame, app: &AgentApp) {
    // One token set for the whole frame, resolved once. Every renderer below
    // takes `&Tokens`, so a theme switch is a single value change and no two
    // parts of one frame can disagree about the palette.
    let tokens = app.tokens();
    let t = &tokens;
    if app.mode == UiMode::Workbench {
        crate::ui::draw(frame, &app.workbench);
        return;
    }

    // Open the interaction frame *before* anything is painted and close it after.
    // Components register their hit regions and focus stops as they draw, so the
    // registry cannot describe a screen that is not on screen. Every early
    // return above this point leaves the previous frame's registry untouched,
    // which is correct: Workbench mode does not use it.
    app.begin_frame_ui();
    // Explicit root surface (the app never trusts the terminal's own
    // background theme); panels lift to `surfaces.surface` on top of it.
    frame.render_widget(
        Block::default().style(Style::default().bg(t.surfaces.bg)),
        frame.area(),
    );
    let area = frame.area();

    // THE layout authority. Every rectangle below — header, rail, body, side
    // panel, status bar, and every modal — comes from this one computation.
    // There are deliberately no renderer-local width or height constants here:
    // a second source of shell geometry is how a rail ends up overlapping the
    // transcript at 100 columns and nowhere else.
    // Agent always owns a rail, so the shell reserves one.
    let layout = ShellLayout::build(area, t, app.wants_context_panel(), true);

    draw_header(t, frame, app, &layout);
    draw_status(t, frame, app, layout.status);
    if layout.chrome.rail {
        draw_rail(t, frame, app, layout.rail);
    }

    match app.view {
        View::Chat => draw_chat(t, frame, app, layout.body),
        _ => draw_view(t, frame, app, layout.body),
    }
    // The side panel is granted by the layout, never by the renderer deciding on
    // its own that "the terminal is wide enough".
    if layout.chrome.context {
        draw_context(t, frame, app, layout.context);
    }

    // A modal owns the whole screen, so it must swallow clicks over the whole
    // screen — not merely paint a dim rectangle over the part it occupies.
    //
    // Only the permission overlay registered a scrim hit region before this, so
    // for the other eight modals a click beside the dialog fell through to the
    // rail or the view underneath and activated it: the operator answered a
    // dialog and the app ran a command behind it. Registration happens *before*
    // the modal draws because `HitTargets::hit` is newest-first — the controls
    // the overlay registers afterwards correctly win over the scrim.
    //
    // The inline approval card in Chat is deliberately not modal: it sits in the
    // transcript so the operator can read it in context, and the chat behind it
    // stays live. Only the non-Chat approval overlay gets a scrim.
    let needs_scrim = app.overlay.is_some()
        || app.confirm.is_some()
        || (!app.approvals.is_empty() && app.view != View::Chat);
    if needs_scrim {
        app.hits.borrow_mut().register(area, HitTarget::ModalScrim);
    }

    draw_overlays(t, frame, app, &layout);
    // Destructive confirmation sits above everything (only `y` proceeds).
    if app.confirm.is_some() {
        draw_confirm(t, frame, app, &layout);
    }
    // A pending permission is visible in EVERY view (Chat renders its inline
    // card; every other view gets this overlay — approvals are never hidden).
    if !app.approvals.is_empty() && app.view != View::Chat {
        draw_approval_overlay(t, frame, app, layout.body);
    }

    // The modal's controls are the only focusable things while it is open, so
    // they are pushed last and are the whole ring. Pushing them here rather
    // than inside each of the nine draw functions is what keeps the trap a
    // property of the scope instead of nine separate implementations of it.
    if let Some(scope) = app.modal_scope() {
        let mut focus = app.focus.borrow_mut();
        for stop in scope.stops {
            focus.push(stop);
        }
    }

    // Re-home focus now that every stop for this frame is known.
    app.end_frame_ui();
}

/// Pad or ellipsize `s` to exactly `w` columns.
///
/// Session titles, paths, and addresses are padded so their following columns
/// line up; a title longer than the budget is ellipsized rather than allowed to
/// push the next column off-screen.
fn pad_to(s: &str, w: u16) -> String {
    if w == 0 {
        return String::new();
    }
    design::text::pad_right(&design::widgets::ellipsize(s, w), w)
}

/// Geometry for a floating surface, from the layout authority.
///
/// `want_w` is a *pixel* width and `content_rows` the number of body rows the
/// dialog will draw. The frame adds its own two rows, and the layout clamps the
/// result to the viewport. This is the only place a dialog is positioned — the
/// old percent-based helper silently produced a 6-row permission dialog because
/// it read 14 as "14 percent of the terminal".
fn dialog(layout: &ShellLayout, t: &Tokens, want_w: u16, content_rows: u16) -> Rect {
    let w = want_w.min(saturating_sub(layout.area.width, 2 * t.layout.modal_margin));
    // Leave the modal margin clear above and below so a full-height dialog can
    // never butt against the terminal edge.
    let max_h = saturating_sub(layout.area.height, 2 * t.layout.modal_margin);
    layout.modal_rect(t, w, content_rows.saturating_add(2).min(max_h))
}

/// Every floating surface, in stacking order, positioned by the layout.
fn draw_overlays(t: &Tokens, frame: &mut Frame, app: &AgentApp, layout: &ShellLayout) {
    match app.overlay {
        Some(Overlay::Help) => draw_help(t, frame, app, layout),
        Some(Overlay::Palette) => draw_palette(t, frame, app, layout),
        Some(Overlay::Model) => draw_model(t, frame, app, layout),
        Some(Overlay::Sessions) => draw_sessions(t, frame, app, layout),
        Some(Overlay::Preview) => draw_preview(t, frame, app, layout),
        Some(Overlay::Setup) => draw_setup(t, frame, app, layout),
        Some(Overlay::Mode) => draw_mode_overlay(t, frame, app, layout),
        None => {}
    }
}

// ----------------------------------------------------------------------
// header / rail / status
// ----------------------------------------------------------------------

/// The session identity shown in the header and the chat dashboard strip.
///
/// Recovered from the pre-edit build (old line 282; locals `id` @284 and
/// `title` @285; `.rodata` `"new session"`; template `04 20 c2 b7 20 c0 00`
/// = `"{title} · {id}"`). Two behaviors that are easy to get wrong, and that an
/// earlier reconstruction here got wrong:
///
/// * `title` is `.clone()`d with **no** `trim` and **no** emptiness filter, so a
///   session present in state but absent from `sessions` renders as `" · <id>"`
///   — a leading separator, not the word "new session".
/// * The id is the **full** UUID via `Display`; there is no abbreviating
///   helper anywhere in the crate. Only `current_session == None` produces
///   "new session".
fn session_label(app: &AgentApp) -> String {
    let Some(id) = app.current_session else {
        return "new session".to_string();
    };
    let title = app
        .sessions
        .iter()
        .find(|s| s.id == id)
        .map(|s| s.title.clone())
        .unwrap_or_default();
    format!("{title} · {id}")
}

/// The current branch, prefixed with the branch glyph.
///
/// Recovered from the pre-edit build (old line 303; the `None` literal is the
/// 7-byte `"⎇ —"`). Both arms return the glyph, so the header always states the
/// git slot even when there is nothing to report; `"no repo"` and `"detached"`
/// were invented by an earlier reconstruction and appear **nowhere** in the
/// pre-edit artifact. `docs/architecture/AGENT_TUI.md:30` documents the header as
/// `session  ⎇ branch  ~/path`, which matches this exactly.
fn git_branch(app: &AgentApp) -> String {
    match app.git.as_ref() {
        Some(g) => format!("\u{2387} {}", g.branch),
        None => "\u{2387} \u{2014}".to_string(),
    }
}

/// The workspace path with the home directory collapsed to `~`.
///
/// Recovered from the pre-edit build (old line 297; locals `home` @298, `p`
/// @299). The original used `env::var("HOME").unwrap_or_default()` and
/// `str::strip_prefix`, which differ from the obvious implementation in two
/// ways worth keeping:
///
/// * When `HOME` is unset or non-UTF-8 it is `""`, and `strip_prefix("")`
///   matches everything — so the path is still shown, just prefixed with `~`.
/// * `strip_prefix` matches whole path *components* in the sense that it is a
///   plain string prefix, so `HOME=/home/u` will also shorten `/home/u2/...`
///   to `~/2/...`. That is the original behavior; tightening it would change
///   output for existing users, so it is documented rather than "fixed".
fn short_path(app: &AgentApp) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let p = app.files_root.display().to_string();
    p.strip_prefix(&home)
        .map(|rest| format!("~{rest}"))
        .unwrap_or(p)
}

/// The left navigation rail: group headings, every destination, and the
/// Workbench toggle.
///
/// The windowing policy — what to drop first when the rail is shorter than its
/// contents — belongs to [`design::shell::rail_window`]. It is duplicated policy
/// that had already drifted from the shared copy once, and drift here is not
/// cosmetic: dropping a *heading* costs a hint, dropping a *destination* costs a
/// feature, and only one of those is recoverable by the operator.
fn draw_rail(t: &Tokens, frame: &mut Frame, app: &AgentApp, area: Rect) {
    use design::shell::{rail_window, RailItem, RailRow};

    let inner = block(t, " ").inner(area);
    frame.render_widget(block(t, " "), area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let w = usize::from(inner.width);

    // Two rows of chrome are reserved at the bottom (Workbench + version) so
    // they cannot be consumed by the destination window.
    const CHROME: u16 = 2;
    let dest_h = inner.height.saturating_sub(CHROME);

    // The flat view order is authoritative; headings are derived from it, so a
    // new view cannot be added without appearing in the rail.
    let mut rows: Vec<RailRow> = Vec::new();
    for group in NavGroup::ALL {
        let members: Vec<View> = View::ALL
            .iter()
            .copied()
            .filter(|v| v.group() == group)
            .collect();
        if members.is_empty() {
            continue;
        }
        rows.push(RailRow::Heading(group.label().to_string()));
        for v in members {
            rows.push(RailRow::Item(RailItem {
                heading: None,
                label: v.label().to_string(),
                index: v.idx(),
            }));
        }
    }

    let active = app.view.idx();
    let win = rail_window(&rows, active, dest_h);

    let mut lines: Vec<Line> = Vec::new();
    // Rectangles are collected while building the lines and registered
    // afterwards, because registering needs an interior-mutable borrow that
    // cannot be held across a loop that also reads app state.
    let mut row_rects: Vec<(Rect, usize)> = Vec::new();
    let mut y = inner.y;
    let bottom = inner.y.saturating_add(dest_h);

    // Markers are appended into whatever space the destinations left, never
    // taking a row from one. `↑3` is also directional: it tells the operator
    // there are destinations *above*, which is the fact that says which way to
    // move. An undirected count would hide that.
    let marker_budget = win.marker_budget(dest_h);
    let mut markers_drawn = 0u16;
    if win.hidden_above > 0 && markers_drawn < marker_budget {
        lines.push(Line::styled(
            format!("\u{2191}{}", win.hidden_above),
            Style::default().fg(t.text.muted),
        ));
        y += 1;
        markers_drawn += 1;
    }
    for row in &win.rows {
        if y >= bottom {
            break;
        }
        match row {
            RailRow::Heading(h) => {
                lines.push(Line::styled(
                    truncate(h, w),
                    Style::default().fg(t.text.muted),
                ));
            }
            RailRow::Item(item) => {
                // Selection is compared against the destination index, the same
                // value the window anchors on. Comparing positions instead would
                // desynchronize as soon as headings were dropped.
                let selected = item.index == active;
                let style = if selected {
                    Style::default()
                        .fg(t.text.inverse)
                        .bg(t.accent.primary)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(t.text.secondary)
                };
                let glyph = if selected { g("\u{203a}", ">") } else { "  " };
                lines.push(Line::styled(
                    format!("{glyph}{}", truncate(&item.label, w)),
                    style,
                ));
                // Every painted destination is a real click target, including
                // the selected row: clicking the row you are already on must not
                // be reported as a click on empty space.
                let lw = inner.width.min(
                    u16::try_from(design::text::display_width(&item.label)).unwrap_or(u16::MAX),
                );
                if lw > 0 && y < bottom {
                    row_rects.push((
                        Rect {
                            x: inner.x,
                            y,
                            width: lw,
                            height: 1,
                        },
                        item.index,
                    ));
                }
            }
        }
        y += 1;
    }
    if win.hidden_below > 0 && markers_drawn < marker_budget {
        lines.push(Line::styled(
            format!("\u{2193}{}", win.hidden_below),
            Style::default().fg(t.text.muted),
        ));
    }

    // Pad before the chrome so the mode toggle and version always sit at the
    // bottom of the rail, where the eye expects a status region.
    while lines.len() < usize::from(dest_h) {
        lines.push(Line::default());
    }
    let in_workbench = app.mode == UiMode::Workbench;
    lines.push(Line::styled(
        truncate(
            if in_workbench {
                "> WORKBENCH"
            } else {
                "  WORKBENCH"
            },
            w,
        ),
        if in_workbench {
            Style::default().fg(t.text.inverse).bg(t.accent.primary)
        } else {
            Style::default().fg(t.accent.primary)
        },
    ));
    lines.push(Line::styled(
        truncate(&format!("v{}", env!("CARGO_PKG_VERSION")), w),
        Style::default().fg(t.text.muted),
    ));

    // Registration is order-sensitive: later registrations win hit tests, so
    // the rail registers before any overlay, which registers its scrim last.
    //
    // While a modal is open the rail's *hit* regions still register — they must,
    // or a click beside the dialog would fall through to nothing — but it
    // registers no *focus* stops. That is what makes the focus trap structural
    // rather than a rule each overlay has to remember: the ring simply contains
    // nothing behind the dialog, so Tab and Shift+Tab have nowhere to escape to
    // and no overlay can implement its own escape hatch by accident.
    let group_len = row_rects.len();
    let modal_open = app.modal_open();
    let mut hits = app.hits.borrow_mut();
    let mut focus = app.focus.borrow_mut();
    for (pos, (rect, index)) in row_rects.into_iter().enumerate() {
        hits.register(rect, HitTarget::NavItem { index });
        if !modal_open {
            focus.push(FocusStop::in_group(FocusTarget::Nav(index), pos, group_len));
        }
    }
    drop(focus);
    drop(hits);

    frame.render_widget(Paragraph::new(lines), inner);
}

/// The masthead: identity, session, posture, and resource meters.
///
/// This is a thin adapter — it decides *what* the operator needs to see and the
/// shared [`design::shell::draw_header`] decides *where* it goes and what gets
/// truncated. That split matters because the truncation policy is the part that
/// rots: a header that quietly drops `RESOURCE BLOCKED` first is a header that
/// hides a blocked admission on a narrow terminal, and that bug had already
/// happened once in a local copy of this function.
fn draw_header(t: &Tokens, frame: &mut Frame, app: &AgentApp, layout: &ShellLayout) {
    use design::widgets::BadgeTone as Tone;

    // ---- title + subtitle (the left group) ----
    // Session identity first, then the extras that are nice-to-have: the shared
    // renderer drops the tail of the subtitle under pressure, so the optional
    // parts must come last.
    let mut subtitle = session_label(app);
    if app.git.is_some() {
        subtitle.push_str(&format!("  {}", git_branch(app)));
    }
    // The answering model is ALWAYS stated — override when switched, else the
    // provider default. An operator who cannot see which model is answering will
    // misattribute behaviour to the wrong one.
    let header_model = app
        .switcher
        .as_ref()
        .and_then(|s| s.selected())
        .filter(|m| !m.is_empty())
        .or_else(|| {
            app.runtime
                .provider
                .info()
                .default_model
                .clone()
                .filter(|m| !m.is_empty())
        });
    if let Some(m) = header_model {
        subtitle.push_str(&format!("  {}", truncate(&m, 24)));
    }
    subtitle.push_str(&format!("  {}", short_path(app)));

    // ---- badges (right group, degrade right-to-left) ----
    //
    // Order is the policy: posture is pushed before the optional detail so the
    // shared renderer's tail-first degradation drops the clock and the context
    // estimate before it ever drops the resource state.
    let mode = app.exec_mode.to_uppercase();
    let posture = app.resource_status_line();
    let posture_tone = match posture.as_str() {
        "RESOURCE BLOCKED" => Tone::Danger,
        "RESOURCE TIGHT" => Tone::Warning,
        "AGENT RUNNING" => Tone::Running,
        _ => Tone::Success,
    };
    // Badges are built as owned text plus a tone, then borrowed once at the
    // end. Collecting `&str` as we go would tie the vector to whichever
    // `format!` temporary produced it, which is exactly the kind of borrow that
    // compiles until the label count changes.
    let mut badge_text: Vec<String> = Vec::new();
    let mut badge_tone: Vec<Tone> = Vec::new();
    let mut push_badge = |text: String, tone: Tone| {
        badge_text.push(text);
        badge_tone.push(tone);
    };

    // Mode semantics: restricted (Sovereign) is amber, open posture is green.
    // Network mirrors it. The header never signals "healthy" for a locked-down
    // state.
    push_badge(
        mode.clone(),
        if app.exec_mode == "sovereign" {
            Tone::Warning
        } else {
            Tone::Success
        },
    );
    push_badge(
        if app.net_allowed { "NET-ON" } else { "NET-OFF" }.to_string(),
        if app.net_allowed {
            Tone::Info
        } else {
            Tone::Warning
        },
    );
    push_badge(posture.clone(), posture_tone);
    push_badge(
        match app.ctx_estimate() {
            Some((est, window)) => {
                let pct = est.saturating_mul(100) / window.max(1);
                format!("ctx ~{pct}%")
            }
            None => "ctx n/a".to_string(),
        },
        Tone::Neutral,
    );
    if let Some(pf) = app.resource_header.as_ref() {
        use openrt_resources::preflight::ResourceStatus;
        push_badge(
            format!("RAM {:.0}% free", 100.0 - pf.memory.used_pct.unwrap_or(0.0)),
            match pf.memory.status {
                ResourceStatus::Blocked => Tone::Danger,
                ResourceStatus::Tight => Tone::Warning,
                _ => Tone::Muted,
            },
        );
        // VRAM: a real number when a probe exists, an honest "n/a" when not.
        // The badge states presence or absence; the exact headroom is a meter.
        push_badge(
            match (pf.vram.status, pf.vram.headroom) {
                (ResourceStatus::Blocked, _) => "VRAM LOW".to_string(),
                (ResourceStatus::Ok | ResourceStatus::Tight, Some(_)) => "VRAM ok".to_string(),
                _ => "VRAM n/a".to_string(),
            },
            match pf.vram.status {
                ResourceStatus::Blocked => Tone::Danger,
                ResourceStatus::Tight => Tone::Warning,
                _ => Tone::Muted,
            },
        );
    }
    push_badge(app.clock.clone(), Tone::Muted);

    // ---- meters (second row when there is room) ----
    // Real numbers, formatted once here. The shared renderer only places them,
    // and collapses to labels rather than clipping mid-number.
    let mut meters: Vec<(&str, String)> = Vec::new();
    if let Some(pf) = app.resource_header.as_ref() {
        // A real fraction, so this is a meter rather than a bare percentage: at
        // a glance an operator can see how close a run is to the ceiling, which
        // is the question the number alone answers slowly.
        let used = pf.memory.used_pct.unwrap_or(0.0) / 100.0;
        meters.push((
            "RAM",
            format!(
                "{} {:.0}%",
                design::widgets::progress_bar(used, 10),
                used * 100.0
            ),
        ));
        if let Some(h) = pf.vram.headroom {
            meters.push(("VRAM", byte_short(h)));
        }
    }

    let badge_refs: Vec<(&str, Tone)> = badge_text
        .iter()
        .map(String::as_str)
        .zip(badge_tone.iter().copied())
        .collect();

    design::shell::draw_header(
        t,
        frame,
        layout,
        "OPENRT agent",
        &subtitle,
        &badge_refs,
        &meters,
    );
}

fn draw_status(t: &Tokens, frame: &mut Frame, app: &AgentApp, area: Rect) {
    let running = if app.running() {
        Span::styled(
            format!(" {} running ", g("●", "*")),
            Style::default().fg(t.status.warning),
        )
    } else {
        Span::styled(
            format!(" {} ", g("○", "o")),
            Style::default().fg(t.text.muted),
        )
    };
    let approval = if app.approvals.is_empty() {
        Span::default()
    } else {
        Span::styled(
            format!("  {} {} approval(s) ", g("⚠", "!"), app.approvals.len()),
            Style::default()
                .fg(t.status.danger)
                .add_modifier(Modifier::BOLD),
        )
    };
    let hint = if app.approvals.is_empty() {
        "   F1 help · Ctrl+K jump · Ctrl+P files · Ctrl+M model · Ctrl+T term · Ctrl+O mode"
    } else {
        "   approval keys: 1 allow once · 2 session · 3 workspace · 4/Esc/Enter deny"
    };
    let line = Line::from(vec![
        running,
        approval,
        Span::styled(
            format!(" {}", sanitize_text(&app.status)),
            Style::default().fg(t.text.muted),
        ),
        Span::styled(hint, Style::default().fg(t.text.muted)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

// ----------------------------------------------------------------------
// chat
// ----------------------------------------------------------------------

fn draw_chat(t: &Tokens, frame: &mut Frame, app: &AgentApp, area: Rect) {
    // When the layout could not grant a side panel, a one-row dashboard strip
    // keeps the chat-primary home informative at 80x24. The decision comes from
    // the breakpoint, not a width literal — the same rule that governed the
    // panel now governs its fallback.
    let compact = !crate::design::layout::Breakpoint::for_area(area).allows_context();
    let mut chunks = vec![Constraint::Min(6), Constraint::Length(4)];
    // Height comes from the card body (title + 6 required fields + 4 grants +
    // the deny note) plus the frame. A fixed 8 clipped CONSEQUENCES and the grant
    // keys — the exact rows an operator must read to decide.
    let approval_needed = approval_card_height(t, app, area.width);
    // Inline only when the WHOLE card fits. On a short terminal a partial card
    // is worse than a modal: the fields would show while the grants were cut
    // off, so the dialog becomes a centered popup instead and every grant stays
    // reachable. The stream keeps at least 4 rows either way.
    let approval_inline =
        !app.approvals.is_empty() && approval_needed <= area.height.saturating_sub(6);
    if approval_inline {
        chunks.insert(0, Constraint::Length(approval_needed));
    }
    // PART 15/20: structured MODEL ERROR card above the stream (fixed
    // height — compact by default, taller while `[v] view` detail is open).
    let model_err_h = if app.model_error.is_some() {
        if app.model_error_detail {
            9
        } else {
            7
        }
    } else {
        0
    };
    if model_err_h > 0 {
        chunks.insert(0, Constraint::Length(model_err_h));
    }
    // Resource refusals render above everything: they are the one state that
    // needs the operator's decision before anything else matters.
    let resource_h = if app.resource_card.is_some() {
        if app.resource_detail {
            14
        } else {
            9
        }
    } else {
        0
    };
    if resource_h > 0 {
        chunks.insert(0, Constraint::Length(resource_h));
    }
    if compact {
        chunks.insert(chunks.len() - 1, Constraint::Length(1));
    }
    let rows = TuiLayout::default()
        .direction(Direction::Vertical)
        .constraints(chunks)
        .split(area);
    let mut row = 0;
    if resource_h > 0 {
        draw_resource_card(t, frame, app, rows[row]);
        row += 1;
    }
    if model_err_h > 0 {
        draw_model_error(t, frame, app, rows[row]);
        row += 1;
    }
    if approval_inline {
        draw_approval(t, frame, app, rows[row]);
        row += 1;
    }
    draw_stream(t, frame, app, rows[row]);
    row += 1;
    if compact {
        draw_dashboard_strip(t, frame, app, rows[row]);
        row += 1;
    }
    draw_input(t, frame, app, rows[row]);

    // A permission that cannot be shown in full inline becomes a modal, so no
    // field and no grant key is ever hidden behind a truncation.
    if !app.approvals.is_empty() && !approval_inline {
        draw_approval_overlay(t, frame, app, area);
    }
}

/// Structured RESOURCE LIMIT card — a host capacity refusal with real numbers,
/// the rule that was not met, and the actions this deployment actually offers.
///
/// Deliberately not styled as an error: nothing here is broken. The tone is a
/// blocked admission, and `WAITING FOR RESOURCES` replaces the headline the
/// moment a bounded retry is in flight so the operator can see the budget.
fn draw_resource_card(t: &Tokens, frame: &mut Frame, app: &AgentApp, area: Rect) {
    let Some(card) = &app.resource_card else {
        return;
    };
    let w = area.width.saturating_sub(4) as usize;

    // Two states, one card. The border vocabulary is a fixed five-role palette,
    // so the waiting/refused distinction is not smuggled in as a sixth border
    // colour: the badge names the state in words and carries the tone, and the
    // panel edge stays within the roles that exist — `destructive` for a
    // refused admission (its documented meaning) and `active` for a bounded
    // wait that is still in flight.
    let (title, tone, badge_tone) = if card.waiting.is_some() {
        (
            "WAITING FOR RESOURCES",
            design::widgets::PanelTone::Active,
            design::widgets::BadgeTone::Warning,
        )
    } else {
        (
            "RESOURCE LIMIT",
            design::widgets::PanelTone::Destructive,
            design::widgets::BadgeTone::Danger,
        )
    };

    let mut lines = vec![Line::from({
        let mut spans = vec![Span::raw(" ")];
        spans.extend(design::widgets::Badge::new(title, badge_tone).spans(t));
        spans
    })];

    if let Some(wait) = &card.waiting {
        // Bounded by construction: both the attempt counter and the wall-clock
        // budget are on screen, so this can never read as an open-ended spinner.
        lines.push(Line::from(Span::styled(
            format!(
                " Waiting for {} to clear — attempt {} of {}, ~{}s to next check",
                card.kind,
                wait.attempt + 1,
                wait.max_attempts + 1,
                wait.next_check_secs
            ),
            Style::default().fg(t.status.warning),
        )));
        lines.push(Line::from(Span::styled(
            format!(
                " Total budget {}ms · Esc cancels immediately",
                wait.total_budget_ms
            ),
            Style::default().fg(t.text.muted),
        )));
        lines.push(Line::from(Span::styled(
            " Free some memory, or pick another action:",
            Style::default().fg(t.text.muted),
        )));
        lines.push(hint_row(
            t,
            &[("Tab", "move"), ("Enter", "run"), ("Esc", "cancel")],
        ));
    } else {
        lines.push(Line::from(Span::styled(
            format!(" {} is limiting this task.", truncate(&card.kind, w)),
            Style::default().fg(t.text.secondary),
        )));
        lines.push(Line::from(vec![
            Span::styled(" Current: ", Style::default().fg(t.text.muted)),
            Span::styled(
                truncate(&card.current_human, w / 3),
                Style::default().fg(t.text.secondary),
            ),
            Span::styled("  Needs: ", Style::default().fg(t.text.muted)),
            Span::styled(
                truncate(&card.limit_human, w / 3),
                Style::default().fg(t.text.secondary),
            ),
            Span::styled("  ", Style::default().fg(t.text.muted)),
            Span::styled(
                truncate(&card.percent_line, w / 4),
                Style::default().fg(t.text.secondary),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            format!(" Reason: {}", truncate(&card.reason, w.saturating_sub(9))),
            Style::default().fg(t.text.secondary),
        )));
        lines.push(Line::from({
            let mut spans = vec![Span::raw(" ")];
            spans.extend(action_bar(t, card));
            spans
        }));
    }

    if app.resource_detail {
        lines.push(Line::from(Span::styled(
            " Details:",
            Style::default()
                .fg(t.text.muted)
                .add_modifier(Modifier::BOLD),
        )));
        for l in card.detail.lines() {
            lines.push(Line::from(Span::styled(
                format!("  {}", truncate(l, w.saturating_sub(2))),
                Style::default().fg(t.text.secondary),
            )));
        }
        lines.push(Line::from({
            let mut spans = vec![Span::raw(" ")];
            spans.extend(action_bar(t, card));
            spans.push(Span::styled(" · ", t.type_scale.metadata));
            spans.extend(design::widgets::key_hints(t, &[("v", "hide")]));
            Line::from(spans)
        }));
    }

    frame.render_widget(
        Paragraph::new(lines).block(design::widgets::panel(t, " resources ", tone)),
        area,
    );
}

/// Compact byte count for a one-cell header pill.
fn byte_short(b: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = MIB * 1024;
    if b >= GIB {
        format!("{:.1}G", b as f64 / GIB as f64)
    } else if b >= MIB {
        format!("{:.0}M", b as f64 / MIB as f64)
    } else {
        format!("{b}B")
    }
}

/// Action bar for the resource card. Only actions the gate offered are shown,
/// so a disabled override never appears as a key that does nothing.
fn action_bar(t: &Tokens, card: &ResourceCard) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, a) in card.actions.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", t.type_scale.metadata));
        }
        // The armed action keeps its `>…<` marker so the preselection is still
        // unmistakable at a glance; the chip around it is the shared one.
        let label = if i == card.selected {
            format!(">{}<", a.label())
        } else {
            a.label().to_string()
        };
        spans.extend(design::widgets::key_hint(t, &a.key().to_string(), &label));
    }
    spans
}

/// PART 15/20: compact MODEL ERROR card — headline, provider/model facts,
/// sanitized reason, and the recovery actions. Never a raw HTTP dump.
fn draw_model_error(t: &Tokens, frame: &mut Frame, app: &AgentApp, area: Rect) {
    let Some(card) = &app.model_error else {
        return;
    };
    let w = area.width.saturating_sub(4) as usize;
    let mut lines = vec![
        Line::from({
            let mut s = vec![Span::raw(" ")];
            s.extend(
                // Uppercase label, matching `State`: a status badge names its
                // state in words, so the card reads correctly in monochrome.
                design::widgets::Badge::new("MODEL ERROR", design::widgets::BadgeTone::Danger)
                    .spans(t),
            );
            s
        }),
        Line::from(Span::styled(
            format!(" {}", truncate(&card.headline, w)),
            Style::default().fg(t.text.secondary),
        )),
        Line::from(vec![
            Span::styled(" Provider: ", Style::default().fg(t.text.muted)),
            Span::styled(
                truncate(&card.provider, w.saturating_sub(12)),
                Style::default().fg(t.text.secondary),
            ),
            Span::styled("  Model: ", Style::default().fg(t.text.muted)),
            Span::styled(
                truncate(&card.model, w.saturating_sub(20)),
                Style::default().fg(t.text.secondary),
            ),
        ]),
    ];
    if app.model_error_detail {
        lines.push(Line::from(Span::styled(
            " Detail:",
            Style::default()
                .fg(t.text.muted)
                .add_modifier(Modifier::BOLD),
        )));
        for l in card.detail.lines() {
            lines.push(Line::from(Span::styled(
                format!("   {}", truncate(l, w.saturating_sub(3))),
                Style::default().fg(t.text.secondary),
            )));
        }
        lines.push(hint_row(
            t,
            &[
                ("r", "retry"),
                ("m", "switch model"),
                ("v", "hide"),
                ("Esc", "dismiss"),
            ],
        ));
    } else {
        lines.push(Line::from(Span::styled(
            format!(" Reason: {}", truncate(&card.reason, w.saturating_sub(9))),
            Style::default().fg(t.text.secondary),
        )));
        lines.push(hint_row(
            t,
            &[
                ("r", "retry"),
                ("m", "switch model"),
                ("v", "view"),
                ("Esc", "dismiss"),
            ],
        ));
    }
    // The card is a destructive surface, so it takes the shared destructive
    // panel: same border, same title treatment, same elevation as every other
    // refusal in the app.
    frame.render_widget(
        Paragraph::new(lines).block(design::widgets::panel(
            t,
            " model error ",
            design::widgets::PanelTone::Destructive,
        )),
        area,
    );
}

/// A card/overlay footer built from the shared key hint chips, indented to the
/// card body and clipped to the width that is actually available.
fn hint_row(t: &Tokens, hints: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    spans.extend(design::widgets::key_hints(t, hints));
    Line::from(spans)
}

/// Compact dashboard for narrow chat homes: real loaded fields only, items
/// dropping right-to-left as width shrinks (kill switch first — safety
/// information never falls off the edge).
fn draw_dashboard_strip(t: &Tokens, frame: &mut Frame, app: &AgentApp, area: Rect) {
    use design::widgets::Badge;
    if area.width == 0 {
        return;
    }
    let mut items: Vec<Badge> = Vec::new();
    if let Some(reason) = app.runtime.kill_switch.engaged() {
        items.push(Badge::new(
            format!("HALT: {}", truncate(&reason, 24)),
            BadgeTone::Danger,
        ));
    }
    items.push(Badge::new(
        truncate(&session_label(app), 18),
        BadgeTone::Accent,
    ));
    if let Some(tag) = app
        .switcher
        .as_ref()
        .and_then(|s| s.selected())
        .or_else(|| {
            app.runtime
                .provider
                .info()
                .default_model
                .clone()
                .filter(|m| !m.is_empty())
        })
    {
        items.push(Badge::new(
            format!("model {}", truncate(&tag, 20)),
            BadgeTone::Muted,
        ));
    }
    if let Some(id) = app.current_task {
        if let Some(task) = app.tasks.iter().find(|x| x.id == id) {
            use openrt_core::task::TaskStatus as Ts;
            let st = task.status;
            let tone = match st {
                Ts::Completed => BadgeTone::Success,
                Ts::Failed | Ts::Interrupted | Ts::Cancelled => BadgeTone::Danger,
                _ if st.is_active() => BadgeTone::Running,
                _ => BadgeTone::Muted,
            };
            let label = format!("{st:?}").to_lowercase();
            items.push(Badge::new(format!("task {label}"), tone));
        }
    }
    if !app.plan.is_empty() {
        let done = app
            .plan
            .iter()
            .filter(|s| s.status == crate::agent::state::PlanStatus::Done)
            .count();
        items.push(Badge::new(
            format!("plan {done}/{}", app.plan.len()),
            if done == app.plan.len() {
                BadgeTone::Success
            } else {
                BadgeTone::Muted
            },
        ));
    }
    items.push(Badge::new(
        format!("findings {}", app.findings.len()),
        if app.findings.is_empty() {
            BadgeTone::Muted
        } else {
            BadgeTone::Danger
        },
    ));
    // Host posture: RAM/VRAM plus the agent's own readiness. Safety-critical
    // items come first so they never fall off the right edge of a narrow strip.
    if let Some(pf) = app.resource_header.as_ref() {
        use openrt_resources::preflight::ResourceStatus;
        let used_pct = pf.memory.used_pct.unwrap_or(0.0);
        let ram_tone = match pf.memory.status {
            ResourceStatus::Blocked => BadgeTone::Danger,
            ResourceStatus::Tight => BadgeTone::Warning,
            ResourceStatus::Unknown | ResourceStatus::Ok => BadgeTone::Muted,
        };
        items.push(Badge::new(
            format!("RAM {:.0}%", 100.0 - used_pct),
            ram_tone,
        ));
        // VRAM is honest about being unmeasured rather than showing a fake 0.
        match (pf.vram.status, pf.vram.used_pct) {
            (ResourceStatus::Ok | ResourceStatus::Tight, Some(_)) => {
                let free = pf.vram.headroom.map(|h| byte_short(h)).unwrap_or_default();
                items.push(Badge::new(format!("VRAM {free}"), BadgeTone::Muted));
            }
            (ResourceStatus::Blocked, _) => items.push(Badge::new("VRAM LOW", BadgeTone::Danger)),
            _ => items.push(Badge::new("VRAM n/a", BadgeTone::Muted)),
        }
    }
    let posture = app.resource_status_line();
    let posture_tone = match posture.as_str() {
        "RESOURCE BLOCKED" => BadgeTone::Danger,
        "RESOURCE TIGHT" => BadgeTone::Warning,
        "AGENT RUNNING" => BadgeTone::Accent,
        _ => BadgeTone::Success,
    };
    items.push(Badge::new(posture, posture_tone));
    // Ordered safety-first and bold: the strip drops from the right so a narrow
    // terminal loses the session label and model, never the halt reason.
    design::widgets::render_badges(frame, area, t, &items, design::widgets::BadgeRow::strip());
}

/// A transcript entry's header row: role badge on the left, timestamp on the
/// right. Every role uses this, so the transcript has one header rhythm instead
/// of a per-kind arrangement.
fn message_header(t: &Tokens, badge: design::widgets::Badge, at: &str) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    spans.extend(badge.spans(t));
    spans.push(Span::styled(
        format!("  {at}"),
        Style::default().fg(t.text.muted),
    ));
    Line::from(spans)
}

fn draw_stream(t: &Tokens, frame: &mut Frame, app: &AgentApp, area: Rect) {
    let block = block(t, " chat ").border_style(Style::default().fg(t.borders.default));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // An empty transcript is a real state, so it renders like every other empty
    // surface in the app rather than as a hand-written hint line: same marker,
    // same label rhythm, and the guidance as the detail row underneath.
    if app.stream.is_empty() {
        let state = crate::design::states::State::empty("no messages yet")
            .with_detail("ask OpenRT anything — Enter sends, Esc cancels a running task");
        state.render(frame, inner, t, &mut app.hits.borrow_mut());
        return;
    }

    // --- Bounded visible window -------------------------------------------
    //
    // The viewport shows the last `height + chat_scroll` rendered rows, so
    // materialising the whole transcript and scrolling the paragraph is O(history)
    // per frame: at 10k messages that is thousands of `Line`s allocated, wrapped
    // and dropped on every repaint, for output that is entirely off screen.
    //
    // Walk *backwards* from the newest message and stop as soon as enough rows
    // have accumulated. Every message contributes at least one row, so the
    // bottom-anchored default (`chat_scroll == 0`) materialises at most
    // `height + 1` messages no matter how long the history is.
    //
    // The window is then scrolled by the same offset the full paragraph would
    // have used. Wrapping is per line, so a line's wrapped row count does not
    // depend on what surrounds it, which is what makes the local count equal to
    // the global one.
    let need = usize::from(inner.height).saturating_add(usize::from(app.chat_scroll));
    let wrap = Wrap { trim: false };
    let mut chunks: Vec<Vec<Line>> = Vec::new();
    let mut rows = 0usize;
    for m in app.stream.iter().rev() {
        let mut chunk: Vec<Line> = Vec::new();
        push_message_lines(t, app, m, &mut chunk);
        let chunk: Vec<Line> = chunk.into_iter().map(ascii_line).collect();
        rows += Paragraph::new(chunk.clone())
            .wrap(wrap)
            .line_count(inner.width);
        chunks.push(chunk);
        if rows > need {
            break;
        }
    }
    // Chunks arrive newest-first, so flatten them back-to-front. Reversing the
    // flattened line list instead would also reverse the rows *within* each
    // message, which reads as garbled text rather than as a scroll bug.
    let window: Vec<Line> = chunks
        .iter()
        .rev()
        .flat_map(|c| c.iter().cloned())
        .collect();

    let paragraph = Paragraph::new(window).wrap(wrap);
    let offset = u16::try_from(rows.saturating_sub(need)).unwrap_or(u16::MAX);
    frame.render_widget(paragraph.scroll((offset, 0)), inner);
}

// Test-only counter: how many transcript entries were materialised this frame.
//
// `draw_stream` walks backwards and stops once the viewport is covered, so this
// is bounded by the viewport — which is the property Phase 8 exists to prove, and
// which no output assertion can demonstrate on its own. Output equality shows a
// window is *correct*; only a count of how much was laid out shows it is
// *bounded*. Compiled out of non-test builds entirely.
#[cfg(test)]
thread_local! {
    static MESSAGES_MATERIALISED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_messages_materialised() {
    MESSAGES_MATERIALISED.with(|c| c.set(0));
}

#[cfg(test)]
pub(crate) fn messages_materialised() -> usize {
    MESSAGES_MATERIALISED.with(std::cell::Cell::get)
}

/// Append the rendered rows for one transcript entry.
///
/// Split out of [`draw_stream`] so the stream can materialise only the messages
/// that can appear in the viewport. This function owns the layout of every
/// `MsgKind`; nothing else may render a message.
fn push_message_lines(t: &Tokens, app: &AgentApp, m: &Msg, lines: &mut Vec<Line>) {
    #[cfg(test)]
    MESSAGES_MATERIALISED.with(|c| c.set(c.get() + 1));
    match &m.kind {
        MsgKind::User(body) => {
            lines.push(message_header(
                t,
                role_badge("you", design::widgets::BadgeTone::Success),
                &m.at,
            ));
            for l in body.lines() {
                lines.push(Line::from(l.to_string()));
            }
            lines.push(Line::from(""));
        }
        MsgKind::Assistant(body) => {
            lines.push(message_header(
                t,
                role_badge("openrt", design::widgets::BadgeTone::Accent),
                &m.at,
            ));
            for l in body.lines() {
                lines.push(Line::from(l.to_string()));
            }
            lines.push(Line::from(""));
        }
        MsgKind::Tool { tool, args, state } => {
            // Marker, then what ran, then how it went: the tool name stays
            // between the status marker and the status label so the row
            // still reads as one clause. Colour and marker both come from
            // the shared tone, so no row can invent its own.
            let badge = tool_state_badge(state);
            let tone = badge.tone;
            let c = tone.color(t);
            lines.push(Line::from(vec![
                Span::styled(format!("  {} ", badge.marker()), Style::default().fg(c)),
                Span::styled(tool.clone(), Style::default().fg(c)),
                Span::styled(
                    format!(" {}", badge.text),
                    Style::default().fg(t.text.muted),
                ),
            ]));
            if !args.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("      {args}"),
                    Style::default().fg(t.text.muted),
                )));
            }
        }
        MsgKind::Plan { id, goal, steps } => {
            lines.push(message_header(
                t,
                role_badge("plan", design::widgets::BadgeTone::Info),
                id,
            ));
            lines.push(Line::from(Span::styled(
                format!("  goal: {goal}"),
                Style::default().fg(t.text.muted),
            )));
            for (label, compile_status) in steps {
                // Live status wins: the stream entry stays bound to the
                // running plan while `/run` executes it.
                let live = app
                    .plan
                    .iter()
                    .find(|s| &s.label == label)
                    .map(|s| s.status.clone())
                    .unwrap_or_else(|| compile_status.clone());
                let tone = plan_status_tone(&live);
                let badge = design::widgets::Badge::new(label.clone(), tone);
                let mut spans = vec![Span::raw("   ")];
                spans.extend(badge.spans(t));
                lines.push(Line::from(spans));
            }
            lines.push(Line::from(""));
        }
        MsgKind::PlanFailed {
            goal,
            code,
            reason,
            fix,
        } => {
            let mut spans = vec![Span::raw(" ")];
            spans.extend(
                design::widgets::Badge::new("plan failed", design::widgets::BadgeTone::Danger)
                    .spans(t),
            );
            spans.push(Span::styled(
                format!("  [{code}]"),
                Style::default().fg(t.text.muted),
            ));
            lines.push(Line::from(spans));
            lines.push(Line::from(Span::styled(
                "  Goal:",
                Style::default()
                    .fg(t.text.muted)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                format!("    {goal}"),
                Style::default().fg(t.text.secondary),
            )));
            lines.push(Line::from(Span::styled(
                "  Reason:",
                Style::default()
                    .fg(t.text.muted)
                    .add_modifier(Modifier::BOLD),
            )));
            for l in reason.lines() {
                lines.push(Line::from(Span::styled(
                    format!("    {l}"),
                    Style::default().fg(t.text.secondary),
                )));
            }
            lines.push(Line::from(Span::styled(
                "  Suggested recovery:",
                Style::default()
                    .fg(t.text.muted)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                format!("    {fix}"),
                Style::default().fg(t.status.warning),
            )));
            lines.push(Line::from(Span::styled(
                "  [r] retry · [e] edit · [Esc] dismiss — the session stays open",
                Style::default().fg(t.text.muted),
            )));
            lines.push(Line::from(""));
        }
        MsgKind::Note(body) => {
            lines.push(Line::from(Span::styled(
                format!("  · {body}"),
                Style::default().fg(t.text.muted),
            )));
        }
        MsgKind::Err(body) => {
            // The same badge language as everything else, so an error in the
            // transcript cannot be mistaken for ordinary body text.
            let mut spans = vec![Span::raw("  ")];
            spans.extend(
                design::widgets::Badge::new("error", design::widgets::BadgeTone::Danger).spans(t),
            );
            spans.push(Span::styled(
                format!(" {body}"),
                Style::default().fg(t.text.secondary),
            ));
            lines.push(Line::from(spans));
        }
    }
}

fn draw_input(t: &Tokens, frame: &mut Frame, app: &AgentApp, area: Rect) {
    // A pending permission owns the keyboard, so the composer is unfocused until
    // it is answered — the border and caret both say so.
    let focused = app.approvals.is_empty();
    if app.model_search_active {
        design::input::draw_search(
            t,
            frame,
            area,
            &app.model_query,
            app.model_query.chars().count(),
            None,
            focused,
        );
        return;
    }
    design::input::draw_field_with(
        t,
        frame,
        area,
        " you ",
        " Enter send · /help commands · Esc clear/cancel ",
        &app.input,
        app.input.chars().count(),
        focused,
        Some("Ask OpenRT anything"),
    );
}

/// True when the requested action looks destructive (red treatment).
fn approval_dangerous(tool: &str, args: &serde_json::Value) -> bool {
    let destructive_tool = tool.starts_with("filesystem.delete")
        || tool == "filesystem.move"
        || tool.starts_with("computer.input")
        || tool.starts_with("browser.download");
    let cmd = args
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let destructive_cmd = ["rm -rf", "mkfs", "dd if=", "shutdown", "halt", ":(){"]
        .iter()
        .any(|p| cmd.contains(p));
    destructive_tool || destructive_cmd
}

/// True when `scope` looks like a path outside the current workspace root.
fn approval_outside_workspace(app: &AgentApp, scope: &str) -> bool {
    if !scope.starts_with('/') && !scope.starts_with("~/") {
        return false;
    }
    let root = app.files_root.display().to_string();
    let expanded = scope.strip_prefix("~/").map(|r| {
        std::env::var("HOME")
            .map(|h| format!("{h}/{r}"))
            .unwrap_or_else(|_| format!("~/{r}"))
    });
    let path = expanded.unwrap_or_else(|| scope.to_string());
    !path.starts_with(&root)
}

/// The permission frame: destructive requests get the destructive border tone,
/// so the treatment follows the risk rather than being hard-coded.
fn permission_block(t: &Tokens, dangerous: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if dangerous {
            t.borders.destructive
        } else {
            t.borders.active
        }))
        .style(Style::default().bg(t.surfaces.overlay))
        .title(Span::styled(
            // Same title as the shared modal dialog. They were "PERMISSION
            // REQUIRED" and "PERMISSION REQUEST" respectively, so the same
            // request announced itself two different ways depending on the view
            // — an operator could not learn the title and rely on it.
            " PERMISSION REQUEST ",
            Style::default()
                .fg(if dangerous {
                    t.status.danger
                } else {
                    t.text.primary
                })
                .add_modifier(Modifier::BOLD),
        ))
}

/// Map a live approval onto the design-system permission request.
///
/// The runtime sends a tool name, args and an optional scope; it does not send
/// a human reason or a risk grade. Those are *derived* here from real signals
/// (the tool, the command text, whether the target escapes the workspace) so
/// the dialog can state them — never invented: a field with no signal becomes an
/// explicit "not stated", which is honest, rather than a plausible-sounding
/// fabrication.
fn permission_request(
    app: &AgentApp,
    p: &crate::agent::sink::PendingApproval,
) -> PermissionRequest {
    use crate::design::modals::Risk;
    let tool = &p.request.tool;
    let args = &p.request.args;
    let scope = p.request.scope.clone().unwrap_or_default();

    let target = args
        .get("command")
        .and_then(|v| v.as_str())
        .map(|c| c.to_string())
        .or_else(|| {
            args.get("path")
                .and_then(|v| v.as_str())
                .map(|p| p.to_string())
        })
        .or_else(|| {
            args.get("url")
                .and_then(|v| v.as_str())
                .map(|u| u.to_string())
        })
        .unwrap_or_else(|| {
            if args.is_null() || args == &serde_json::json!({}) {
                "no arguments".to_string()
            } else {
                truncate(&sanitize_text(&args.to_string()), 120)
            }
        });

    let destructive = approval_dangerous(tool, args);
    let outside = approval_outside_workspace(app, &scope);
    let egress = tool.starts_with("browser")
        || tool.starts_with("network")
        || tool.contains("fetch")
        || tool.starts_with("computer.");
    let risk = match (destructive, outside, egress) {
        (true, _, _) | (_, true, _) => Risk::Critical,
        (_, _, true) => Risk::High,
        (false, false, false) => {
            if tool.starts_with("filesystem.write") || tool.starts_with("filesystem.move") {
                Risk::Medium
            } else {
                Risk::Low
            }
        }
    };

    let reason = if destructive {
        "tool or command is classified destructive by the local policy"
    } else if outside {
        "target resolves outside the current workspace root"
    } else if egress {
        "capability leaves the machine; egress is policy-governed"
    } else {
        "policy marks this capability as ask"
    };

    let consequences = match risk {
        Risk::Critical => format!(
            "{tool} can destroy or exfiltrate data irreversibly. Denying stops it; allowing runs it now."
        ),
        Risk::High => format!(
            "{tool} performs network egress. Denying blocks it; allowing transmits the request."
        ),
        Risk::Medium => format!(
            "{tool} writes inside the workspace. Denying leaves the file untouched; allowing overwrites it."
        ),
        Risk::Low => format!(
            "{tool} reads state. Denying returns control to you; allowing exposes the result to the agent."
        ),
    };

    PermissionRequest::new(tool.replace('.', " "), target, reason, risk)
        .with_scope(if scope.is_empty() {
            "not stated by the caller".to_string()
        } else {
            scope.clone()
        })
        .with_consequences(consequences)
}

/// Shared body of the permission card (chat inline + non-chat overlay).
///
/// Every required field is labeled in text: ACTION, TARGET, REASON, RISK,
/// SCOPE, CONSEQUENCES. A permission request that renders like a yes/no prompt
/// teaches operators to reflexively approve.
fn approval_lines(t: &Tokens, app: &AgentApp) -> Vec<Line<'static>> {
    use crate::design::modals::Risk;
    let Some(p) = app.approvals.first() else {
        return vec![];
    };
    let req = permission_request(app, p);
    let risk = req.risk;
    let destructive =
        risk == Risk::Critical || approval_dangerous(&p.request.tool, &p.request.args);
    let accent = if destructive {
        t.status.danger
    } else {
        t.status.warning
    };
    let muted = Style::default().fg(t.text.muted);

    let field = |label: &'static str, value: &str, style: Style| {
        Line::from(vec![
            Span::styled(format!("{label:<12} "), muted),
            Span::styled(value.to_string(), style),
        ])
    };

    let mut lines = vec![
        Line::from(Span::styled(
            "OpenRT requests permission",
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        )),
        field(
            "ACTION",
            &req.action,
            Style::default()
                .fg(t.text.primary)
                .add_modifier(Modifier::BOLD),
        ),
        field("TARGET", &req.target, Style::default().fg(t.text.primary)),
        field("REASON", &req.reason, Style::default().fg(t.text.secondary)),
        Line::from(vec![
            Span::styled(format!("{:<12} ", "RISK"), muted),
            Span::styled(
                format!(" {}", risk.label()),
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if destructive {
                    "  destructive action — read before allowing"
                } else {
                    ""
                },
                Style::default().fg(t.status.danger),
            ),
        ]),
        field("SCOPE", &req.scope, Style::default().fg(t.text.secondary)),
        field(
            "CONSEQUENCES",
            &req.consequences,
            Style::default().fg(t.text.secondary),
        ),
    ];
    if approval_outside_workspace(app, p.request.scope.as_deref().unwrap_or("")) {
        lines.push(Line::from(Span::styled(
            "target resolves outside the current workspace",
            Style::default().fg(accent),
        )));
    }

    // The grants themselves are NOT listed here. `draw_approval` reserves the
    // last rows for the shared `ButtonGroup`, which renders all four as real,
    // clickable `ButtonSpec`s in `narrowest first` order. This body used to
    // print them a second time as static text, and the duplicate was strictly
    // worse than the buttons it shadowed: wrapping cut the static copy down to
    // two of the four grants, so the text said one thing and the buttons said
    // another while eating rows the `CONSEQUENCES` field needed. One authority
    // for the grants, and the freed rows go to the request the operator has to
    // read.
    //
    // What the buttons cannot express is the part that matters most: every
    // *other* key also denies, and Enter must never grant.
    lines
}

/// The one row below the buttons that says what every *other* key does.
///
/// It is the security-relevant half of the answer set — the buttons show the
/// four grants, this says that `4`/`n`/`d`/`Esc` all deny and that Enter can
/// never grant — so it is reserved with the action block instead of living in
/// the body, where the tail-truncation policy was cutting it first.
fn approval_deny_note(t: &Tokens) -> Line<'static> {
    hint_row(
        t,
        &[
            ("4", "deny"),
            ("n", "deny"),
            ("d", "deny"),
            ("Esc", "deny"),
            ("Enter", "never grants"),
        ],
    )
}

/// Rows the inline card keeps below the request body for the button block, the
/// deny note and the separator between them. All three are reserved so a short
/// terminal shrinks the *description* and never the answer set.
fn approval_footer_rows(dlg: &design::modals::PermissionDialog, body_width: u16) -> u16 {
    // buttons + deny note + one blank separator
    dlg.group.rows_needed(body_width).max(1) + 1 + 1
}

/// Total rows the inline permission card needs at `width`.
///
/// One function owns this arithmetic because the card is *sized* in `draw_chat`
/// (to decide inline vs modal) and *laid out* in `draw_approval`. When those two
/// computed the height separately the budget forgot the button block and the
/// note, so the card was always a few rows short of its own content and the
/// tail — the `CONSEQUENCES` field, the one that explains what the action can
/// destroy — was the part that silently disappeared.
fn approval_card_height(t: &Tokens, app: &AgentApp, width: u16) -> u16 {
    let Some(pending) = app.approvals.first() else {
        return 0;
    };
    let dlg = design::modals::PermissionDialog::new(permission_request(app, pending));
    let body_width = width.saturating_sub(2);
    let body = wrapped_height(&approval_lines(t, app), body_width);
    body + 2 + approval_footer_rows(&dlg, body_width) // + borders
}

/// Rows a set of lines actually occupies once wrapped into `width` columns.
///
/// Chat/overlay bodies are logical lines, but the renderer wraps them. Sizing a
/// container from the logical count under-counts any long field — which is how
/// `CONSEQUENCES` used to be the row that got cut off.
fn wrapped_height(lines: &[Line<'_>], width: u16) -> u16 {
    let width = width.max(1) as usize;
    lines
        .iter()
        .map(|l| {
            let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            let w = design::text::display_width(&text).max(1);
            w.div_ceil(width).max(1) as u16
        })
        .sum()
}

fn draw_approval(t: &Tokens, frame: &mut Frame, app: &AgentApp, area: Rect) {
    let Some(pending) = app.approvals.first() else {
        return;
    };
    let request = permission_request(app, pending);
    let dangerous = approval_dangerous(&pending.request.tool, &pending.request.args);

    let block = permission_block(t, dangerous);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // The action row is reserved FIRST and never truncated, so the grants stay
    // reachable on a short terminal. It uses the same `ButtonGroup` as the modal,
    // which is why clicking "Allow workspace" works here too — an operator who
    // learned the buttons in one view is not surprised in the other.
    let dlg = design::modals::PermissionDialog::new(request);
    // Reserve as many rows as the four grants actually need at this width, and
    // always at least one. A fixed single row silently dropped the trailing
    // buttons on a narrow terminal — and the button that got dropped was Deny,
    // which left three clickable grants and no clickable refusal.
    let action_rows = dlg
        .group
        .rows_needed(inner.width)
        .max(1)
        .min(saturating_sub(inner.height, 1));
    // The note sits under the buttons and shares their reservation, so it is
    // drawn whenever the buttons are.
    let reserved = action_rows.saturating_add(1).min(inner.height);
    let actions = Rect {
        x: inner.x,
        y: inner.y + saturating_sub(inner.height, reserved),
        width: inner.width,
        height: action_rows,
    };
    let note = Rect {
        x: inner.x,
        y: inner.y + saturating_sub(inner.height, reserved - action_rows),
        width: inner.width,
        height: 1,
    };
    let body = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        // the reserved footer + the blank separator above it
        height: saturating_sub(inner.height, reserved + 1),
    };

    let mut lines: Vec<Line> = approval_lines(t, app).into_iter().map(ascii_line).collect();
    // Truncate the TAIL, which is the deny note, never the fields: on a terminal
    // too short for everything, losing "Enter never grants" is recoverable;
    // losing CONSEQUENCES is not.
    if lines.len() > body.height as usize {
        let keep = body.height as usize;
        let mut kept: Vec<Line> = lines.drain(..keep).collect();
        if let Some(last) = kept.last_mut() {
            let marker = g(
                " …more below (the dialog overlay shows the full request)",
                "(see below)",
            );
            last.push_span(Span::styled(marker, accent_style(t, dangerous)));
        }
        lines = kept;
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);

    frame.render_widget(approval_deny_note(t), note);

    let mut hits = app.hits.borrow_mut();
    dlg.group.render_wrapped(frame, actions, t, &mut hits);
}

/// The border tone for a permission frame, derived from risk.
fn accent_style(t: &Tokens, dangerous: bool) -> Style {
    Style::default().fg(if dangerous {
        t.status.danger
    } else {
        t.status.warning
    })
}

/// The approval card as a focused overlay in every non-Chat view (a pending
/// permission must never be hidden behind a different screen).
///
/// This renders through the shared [`render_permission_dialog`] rather than a
/// local copy, which is what makes the four grants *clickable*: the shared
/// primitive draws them as real `ButtonSpec`s and registers their rectangles,
/// plus a scrim so a click cannot reach the view behind the dialog.
///
/// The inline Chat card (see [`draw_approval`]) deliberately stays textual — it
/// shares these body lines so the two can never disagree about what was asked —
/// but it is not a modal, so it has no buttons to hit.
fn draw_approval_overlay(t: &Tokens, frame: &mut Frame, app: &AgentApp, area: Rect) {
    let Some(pending) = app.approvals.first() else {
        return;
    };
    let request = permission_request(app, pending);

    // Pixel sizing derived from the content: the dialog has to fit its own body,
    // so its height comes from the wrapped lines plus the action row.
    let want_w = area.width.saturating_sub(6).min(78).max(30);
    let lines = request.body_lines(t);
    let want_h = wrapped_height(&lines, want_w.saturating_sub(2)) + 5; // fields + actions
    let popup = ShellLayout::build(area, t, false, false).modal_rect(t, want_w, want_h);
    if popup.width == 0 || popup.height == 0 {
        return;
    }

    let dlg = design::modals::PermissionDialog::new(request);
    // Render straight into the frame's own registry, so the buttons the dialog
    // registers are the buttons a click will find.
    let mut hits = app.hits.borrow_mut();
    design::modals::render_permission_dialog_over(frame, area, popup, t, &dlg, &mut hits);
}

// ----------------------------------------------------------------------
// right context column
// ----------------------------------------------------------------------

fn draw_context(t: &Tokens, frame: &mut Frame, app: &AgentApp, area: Rect) {
    let rows = TuiLayout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(22),
            Constraint::Percentage(28),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(area);

    // Project
    let ws = app
        .workbench
        .workspace
        .as_ref()
        .map(|w| w.address_code.clone())
        .unwrap_or_else(|| "(none)".into());
    // The path is truncated from the FRONT so the filename — the part that
    // identifies which project this is — always survives. Clipping it from the
    // right showed `~/Projects/openrt/crates/openrt-` and lost `tui` entirely,
    // which is the only part the reader needed.
    let path = crate::design::collections::truncate_tail(
        &short_path(app),
        rows[0].width.saturating_sub(16),
    );
    let mut project = vec![
        line_kv(t, "workspace", &ws),
        line_kv(t, "path", &path),
        line_kv(t, "branch", &git_branch(app)),
    ];
    if let Some(g) = &app.git {
        project.push(line_kv(t, "modified", &g.status.len().to_string()));
    }
    render_boxed(
        t,
        frame,
        " project ",
        project,
        rows[0],
        design::widgets::PanelTone::Default,
    );

    // Current task
    let task_lines = current_task_lines(t, app);
    render_boxed(
        t,
        frame,
        " current task ",
        task_lines,
        rows[1],
        design::widgets::PanelTone::Active,
    );

    // Git status
    let mut git_lines: Vec<Line> = Vec::new();
    match &app.git {
        Some(g) => {
            git_lines.push(line_kv(t, "branch", &g.branch));
            git_lines.push(line_kv(t, "changes", &g.status.len().to_string()));
            for l in g.log.iter().take(4) {
                git_lines.push(Line::from(Span::styled(
                    format!("  {l}"),
                    Style::default().fg(t.text.muted),
                )));
            }
        }
        None => {
            let msg = match &app.git_error {
                Some(e) if !e.is_empty() => format!("  {e}"),
                _ => "  not loaded — open GIT view".into(),
            };
            git_lines.push(Line::from(Span::styled(
                msg,
                Style::default().fg(t.text.muted),
            )));
        }
    }
    render_boxed(
        t,
        frame,
        " git status ",
        git_lines,
        rows[2],
        design::widgets::PanelTone::Default,
    );

    // System
    let kill = match app.runtime.kill_switch.engaged() {
        Some(r) => format!("HALT: {r}"),
        None => "clear".into(),
    };
    let system = vec![
        line_kv(t, "mode", &app.exec_mode),
        line_kv(t, "provider", app.runtime.provider.id()),
        line_kv(t, "kill switch", &kill),
        line_kv(t, "findings", &app.findings.len().to_string()),
        line_kv(t, "agents", app.runtime.agent.id()),
    ];
    render_boxed(
        t,
        frame,
        " system ",
        system,
        rows[3],
        design::widgets::PanelTone::Default,
    );
}

fn current_task_lines(t: &Tokens, app: &AgentApp) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(id) = app.current_task {
        let found = app.tasks.iter().find(|x| x.id == id);
        match found {
            Some(task) => {
                lines.push(line_kv(t, "task", &format!("{:?}", task.status)));
                lines.push(line_kv(t, "id", &task.id.to_string()));
            }
            None => lines.push(line_kv(t, "task", &id.to_string())),
        }
    } else {
        lines.push(line_kv(t, "task", "idle"));
    }
    if !app.plan.is_empty() {
        let done = app
            .plan
            .iter()
            .filter(|s| s.status == crate::agent::state::PlanStatus::Done)
            .count();
        lines.push(line_kv(
            t,
            "plan",
            &format!("{done}/{} steps", app.plan.len()),
        ));
        for s in app.plan.iter().take(6) {
            let mark = match s.status {
                crate::agent::state::PlanStatus::Done => g("✓", "+"),
                crate::agent::state::PlanStatus::Running => g("▸", ">"),
                crate::agent::state::PlanStatus::Failed => g("✗", "x"),
                crate::agent::state::PlanStatus::Pending => "○",
            };
            lines.push(Line::from(Span::styled(
                format!("  {mark} {}", s.label),
                Style::default().fg(t.text.muted),
            )));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  idle",
            Style::default().fg(t.text.muted),
        )));
    }
    lines
}

/// Honest per-model native tool support label (yes/no/unknown/probe error).
pub(crate) fn native_tools_label(app: &AgentApp) -> String {
    match (&app.native_tools, &app.native_tools_err) {
        (Some(true), _) => "yes".to_string(),
        (Some(false), _) => "no".to_string(),
        (None, Some(e)) => {
            let short: String = e.chars().take(48).collect();
            format!("unavailable ({short})")
        }
        (None, None) => "unknown".to_string(),
    }
}

/// Vision capability row: same honesty contract as `native_tools_label`.
pub(crate) fn vision_label(app: &AgentApp) -> String {
    match (&app.vision, &app.vision_err) {
        (Some(true), _) => "yes".to_string(),
        (Some(false), _) => "no".to_string(),
        (None, Some(e)) => {
            let short: String = e.chars().take(48).collect();
            format!("unavailable ({short})")
        }
        (None, None) => "unknown".to_string(),
    }
}

/// Provider status row from the last probe round-trip — reachability is
/// observed, never assumed.
pub(crate) fn provider_status_label(app: &AgentApp) -> String {
    match &app.provider_status {
        Some(Ok(())) => "reachable".to_string(),
        Some(Err(e)) => {
            let short: String = e.chars().take(48).collect();
            format!("unreachable ({short})")
        }
        None => "unknown".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Model resource rows
//
// Six labels, one job: an operator looking at a model that will not run must be
// able to tell, from this panel alone, what the file is, what the host has, what
// execution is estimated to need, whether that fits, and what to do next. Every
// number carries its provenance, and "we do not know" is rendered as its own
// answer rather than left blank.
// ---------------------------------------------------------------------------

/// The selected artifact's size, and whether the registry holds those bytes.
pub(crate) fn artifact_row(app: &AgentApp) -> String {
    let Some(c) = app.section_card() else {
        return "no artifact selected".into();
    };
    let Some(bytes) = c.artifact_bytes else {
        // A curated entry has no size because the catalog does not have one.
        return "size not declared by the catalog".into();
    };
    let size = openrt_models::artifact::human_bytes(bytes);
    match c.blob_location.as_deref() {
        Some("external") => format!("{size} on disk · 0 stored (registry holds a reference)"),
        Some("stored") => format!("{size} stored in registry"),
        _ => format!("{size} (location not recorded)"),
    }
}

/// Free host memory, or an honest "not probed".
pub(crate) fn mem_free_label(app: &AgentApp) -> String {
    use openrt_resources::preflight::ResourceStatus;
    let Some(pf) = app.resource_header.as_ref() else {
        return "not probed".into();
    };
    match pf.memory.status {
        ResourceStatus::Unknown => format!("not probed ({})", pf.memory.unknown_reason),
        _ => match pf.memory.headroom {
            Some(b) => format!(
                "{} free before the configured floor",
                openrt_models::artifact::human_bytes(b)
            ),
            None => "measured, headroom unbounded".into(),
        },
    }
}

/// What executing the selected artifact is estimated to need.
pub(crate) fn exec_need_label(app: &AgentApp) -> String {
    let Some((id, profile)) = app.selected_profile() else {
        return "UNKNOWN — no artifact selected".into();
    };
    let _ = id;
    let req = profile.execution_requirement();
    let mut s = format!(
        "{} {}",
        req.provenance.as_str(),
        openrt_models::artifact::human_bytes(req.bytes)
    );
    match (&profile.weight_bytes, &profile.kv_cache_bytes) {
        (Some(w), Some(kv)) => s.push_str(&format!(
            " (weights {} + KV {} @ {} ctx)",
            openrt_models::artifact::human_bytes(w.bytes),
            openrt_models::artifact::human_bytes(kv.bytes),
            profile
                .kv_context_length
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".into())
        )),
        (None, _) => s.push_str(" (weights NOT PROBED)"),
        _ => s.push_str(" (KV NOT PROBED)"),
    }
    s
}

/// Whether the selected artifact looks runnable *now*.
///
/// Advisory by construction: the refusal that matters comes from resource
/// admission when a load is actually attempted.
pub(crate) fn exec_status_label(app: &AgentApp) -> String {
    use openrt_models::artifact::ExecutionFit;
    let Some((_, profile)) = app.selected_profile() else {
        return "UNKNOWN — no artifact profiled".into();
    };
    let mem = app
        .resource_header
        .as_ref()
        .and_then(|pf| pf.memory.headroom);
    match openrt_models::artifact::assess_execution_fit(profile, mem) {
        ExecutionFit::Ready { .. } => {
            "READY (advisory) — resource admission still applies at load".into()
        }
        ExecutionFit::ResourceLimited { .. } => {
            "RESOURCE-LIMITED — registered on disk, NOT LOADED".into()
        }
        ExecutionFit::Unknown { reason } => format!("UNKNOWN ({reason}) — not loaded"),
    }
}

/// GPU memory, or the honest absence of a probe.
pub(crate) fn vram_label(app: &AgentApp) -> String {
    use openrt_resources::preflight::ResourceStatus;
    let Some(pf) = app.resource_header.as_ref() else {
        return "n/a — not probed".into();
    };
    match pf.vram.status {
        ResourceStatus::Unknown => {
            if pf.vram.unknown_reason.is_empty() {
                "n/a — no GPU/VRAM probe on this host".into()
            } else {
                format!("n/a — {}", pf.vram.unknown_reason)
            }
        }
        _ => match (pf.vram.current, pf.vram.headroom) {
            (Some(used), Some(free)) => format!(
                "{} used · {} free",
                openrt_models::artifact::human_bytes(used),
                openrt_models::artifact::human_bytes(free)
            ),
            _ => "n/a".into(),
        },
    }
}

/// What the operator can actually do about a resource limit.
pub(crate) fn remediation_label(app: &AgentApp) -> String {
    use openrt_models::artifact::ExecutionFit;
    let Some((_, profile)) = app.selected_profile() else {
        return "none — no artifact profiled".into();
    };
    let mem = app
        .resource_header
        .as_ref()
        .and_then(|pf| pf.memory.headroom);
    match openrt_models::artifact::assess_execution_fit(profile, mem) {
        ExecutionFit::ResourceLimited { .. } => {
            "free memory, or switch to a smaller model — no override is offered from this panel"
                .into()
        }
        ExecutionFit::Ready { .. } => "none needed".into(),
        ExecutionFit::Unknown { .. } => "profile the artifact to get a fit assessment".into(),
    }
}

/// One materialised row of a model-card section: marker, name, provider tag,
/// source and installed size.
fn card_row(t: &Tokens, selected: bool, c: &openrt_registry::RegistryCard) -> Line<'static> {
    let style = if selected {
        Style::default()
            .fg(t.accent.primary)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(t.text.secondary)
    };
    let marker = if selected { g("›", ">") } else { " " };
    let tag =
        crate::agent::setup::suggest_provider_tag(&c.artifact_id).unwrap_or_else(|| "—".into());
    let size = if c.installed_bytes > 0 {
        format!(" · {}", format_size(c.installed_bytes))
    } else {
        String::new()
    };
    Line::from(vec![
        Span::styled(format!(" {marker} "), style),
        Span::styled(format!("{:<32}", truncate(&c.display_name, 31)), style),
        Span::styled(format!("{:<16}", truncate(&tag, 15)), style),
        Span::styled(
            format!("{}{}", c.source.as_str(), size),
            Style::default().fg(t.text.muted),
        ),
    ])
}

/// The visible rows of a model-card section, and the number of rows hidden.
///
/// `Local`, `Installed` and `Recommended` all draw from `app.model_cards`, which
/// is a full catalog scan of every weight file under `models/`. The old path
/// allocated a `Vec<&RegistryCard>` for the section subset, built a `Line` for
/// every card in it, and only then scrolled — so a catalog of a few thousand
/// entries cost a few thousand `format!` allocations per frame for a ~40 row
/// panel.
///
/// Row space is not card space. `Local` interleaves one category header per
/// group, so where a card's row falls depends on every category before it. That
/// mapping is therefore computed twice: once with an allocation-free counting
/// scan to find the total and the selected row, and once to materialise. Only
/// the window's rows are built.
///
/// No card metadata beyond what the panel already displays is touched — nothing
/// here reads weights or probes the filesystem; `c.installed_bytes` and
/// `c.source` are catalog fields, already known from the scan.
fn model_card_rows(
    t: &Tokens,
    app: &AgentApp,
    section: crate::agent::state::ModelSection,
    list_h: u16,
) -> (Vec<Line<'static>>, usize) {
    // Returns the window plus the true row total. The total is only read by
    // tests, so it is recorded through the test-only setter here rather than
    // carried out to the caller, where it would be an unused binding in release
    // builds.
    let is_local = section == crate::agent::state::ModelSection::Local;
    let keep = |i: usize| -> bool {
        let c = &app.model_cards[i];
        match section {
            crate::agent::state::ModelSection::Installed => c.installed_bytes > 0,
            crate::agent::state::ModelSection::Recommended => {
                c.source == openrt_registry::SourceLabel::Curated
            }
            _ => true,
        }
    };

    // Pass 1 — count rows, locate the cursor. No allocation.
    let mut last_cat = "";
    let mut total_rows = 0usize;
    let mut kept = 0usize;
    let mut sel_row = 0usize;
    for i in 0..app.model_cards.len() {
        if !keep(i) {
            continue;
        }
        if is_local {
            let cat = crate::agent::views::model_category(&app.model_cards[i].tags);
            if cat != last_cat {
                last_cat = cat;
                total_rows += 1;
            }
        }
        if kept == app.sel {
            sel_row = total_rows;
        }
        total_rows += 1;
        kept += 1;
    }
    #[cfg(test)]
    set_card_rows_total(total_rows);
    if total_rows == 0 {
        return (Vec::new(), 0);
    }

    let (start, count) = design::collections::window(total_rows, sel_row, list_h);
    #[cfg(test)]
    CARD_ROWS_BUILT.with(|c| c.set(c.get() + count));

    // Pass 2 — materialise only the window.
    let mut out: Vec<Line<'static>> = Vec::with_capacity(count);
    let mut last_cat = "";
    let mut row = 0usize;
    let mut kept = 0usize;
    for i in 0..app.model_cards.len() {
        if !keep(i) {
            continue;
        }
        let c = &app.model_cards[i];
        if is_local {
            let cat = crate::agent::views::model_category(&c.tags);
            if cat != last_cat {
                last_cat = cat;
                if row >= start && row < start + count {
                    out.push(Line::from(Span::styled(
                        format!(" models/{cat} "),
                        Style::default()
                            .fg(t.accent.primary)
                            .add_modifier(Modifier::BOLD),
                    )));
                }
                row += 1;
            }
        }
        if row >= start && row < start + count {
            out.push(card_row(t, kept == app.sel, c));
        }
        row += 1;
        kept += 1;
        if row >= start + count {
            break;
        }
    }
    (out, total_rows)
}

// Test-only counter: model card rows materialised this frame.
#[cfg(test)]
thread_local! {
    static CARD_ROWS_BUILT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_card_rows_built() {
    CARD_ROWS_BUILT.with(|c| c.set(0));
}

#[cfg(test)]
pub(crate) fn card_rows_built() -> usize {
    CARD_ROWS_BUILT.with(std::cell::Cell::get)
}

// Test-only: the *true* row total of the last card section rendered, so a test
// can compare it against how many rows were built.
#[cfg(test)]
thread_local! {
    static CARD_ROWS_TOTAL: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn set_card_rows_total(n: usize) {
    CARD_ROWS_TOTAL.with(|c| c.set(n));
}

#[cfg(test)]
pub(crate) fn card_rows_total() -> usize {
    CARD_ROWS_TOTAL.with(std::cell::Cell::get)
}

fn line_kv(t: &Tokens, k: &str, v: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {k:<12} "), Style::default().fg(t.text.muted)),
        Span::raw(v.to_string()),
    ])
}

/// A section heading in a key/value view.
fn settings_heading(text: &str) -> crate::design::collections::ListItem<'static> {
    crate::design::collections::ListItem {
        label: text.trim().to_string(),
        detail: None,
        tone: Some(crate::design::collections::RowTone::Info),
        id: "",
    }
}

/// A key with its value right-aligned, as the shared list renders it.
fn settings_kv(
    key: &str,
    value: impl Into<String>,
) -> crate::design::collections::ListItem<'static> {
    crate::design::collections::ListItem {
        label: key.to_string(),
        detail: Some(value.into()),
        tone: None,
        id: "",
    }
}

/// Free-form explanatory text under a section.
fn settings_note(text: impl Into<String>) -> crate::design::collections::ListItem<'static> {
    crate::design::collections::ListItem {
        label: text.into(),
        detail: None,
        tone: Some(crate::design::collections::RowTone::Muted),
        id: "",
    }
}

/// A titled box around a block of key/value rows.
///
/// Delegates to the shared [`design::widgets::render_panel`]. This was the last
/// framed surface in Agent Mode still constructing its own `Block` and taking a
/// raw border `Color`, so its background sat on `surfaces.surface` while every
/// other main panel sat on `surfaces.bg` — the context column looked like it was
/// floating when it was meant to be part of the page.
fn render_boxed(
    t: &Tokens,
    frame: &mut Frame,
    title: &str,
    lines: Vec<Line>,
    area: Rect,
    tone: design::widgets::PanelTone,
) {
    design::widgets::render_panel(frame, area, t, title, tone, |f, inner| {
        f.render_widget(
            Paragraph::new(lines.into_iter().map(ascii_line).collect::<Vec<_>>())
                .wrap(Wrap { trim: false }),
            inner,
        );
    });
}

// ----------------------------------------------------------------------
// non-chat views
// ----------------------------------------------------------------------

fn draw_view(t: &Tokens, frame: &mut Frame, app: &AgentApp, area: Rect) {
    // The shared panel primitive, not a hand-rolled `Block`. Every non-chat
    // view therefore gets the same border tone, surface lift, and title
    // treatment from one place.
    let block =
        design::shell::content_block(t, app.view.label(), design::widgets::PanelTone::Default);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    match app.view {
        View::Files => {
            let filtered = app.filter_active && !app.list_filter.is_empty();
            let mut header_txt = format!(
                " {}  (Enter open · Backspace up · Ctrl+D diff",
                app.files_root.display()
            );
            if filtered {
                // Counted with the predicate, not `filtered_files()`: this runs on
                // every repaint and the vector would be one `usize` per file.
                let shown = crate::design::collections::FilteredWindow::new(
                    app.files.len(),
                    &|i| app.file_matches(i),
                    app.sel,
                    0,
                    false,
                )
                .total();
                header_txt.push_str(&format!(
                    " · filter “{}” ({}/{} shown)",
                    app.list_filter,
                    shown,
                    app.files.len()
                ));
            }
            header_txt.push(')');
            let header = Some(Line::from(Span::styled(
                header_txt,
                Style::default().fg(t.text.muted),
            )));
            // An inactive filter is the predicate that matches everything, so
            // "unfiltered" and "filtered" take one code path instead of two.
            let matches: &dyn Fn(usize) -> bool = if filtered {
                &|i| app.file_matches(i)
            } else {
                &|_| true
            };
            render_windowed_filtered_list(
                app,
                frame,
                header,
                app.files.len(),
                matches,
                app.sel,
                |i| {
                    let f = &app.files[i];
                    let (mark, name) = if f.is_dir {
                        (g("▸", ">"), format!("{}/", f.name))
                    } else {
                        ("", f.name.clone())
                    };
                    let size = if f.is_dir {
                        String::new()
                    } else {
                        format_size(f.size)
                    };
                    crate::design::collections::ListItem {
                        label: format!("{mark}{name}"),
                        detail: (!size.is_empty()).then_some(size),
                        tone: None,
                        id: "",
                    }
                },
                inner,
                Some(if filtered {
                    crate::design::states::State::empty("no files match the filter")
                        .with_detail("Esc clears the filter")
                } else {
                    crate::design::states::State::empty("empty directory")
                }),
            );
        }
        View::Search => {
            let header = Some(Line::from(Span::styled(
                format!(
                    " query: “{}”  (type + Enter to search · PgUp/PgDn scroll)",
                    app.search
                ),
                Style::default().fg(t.text.muted),
            )));
            // `search_hits` is the one view collection with no natural bound
            // at all: a query for a common token across a large tree returns
            // whatever ripgrep found, and this loop cloned both the line text and
            // the `path:line` detail for *every* hit, each frame, to paint a
            // couple dozen. The window is measured first and only visible rows
            // are built.
            render_windowed_list(
                app,
                frame,
                header,
                app.search_hits.len(),
                Some(app.sel),
                |i| {
                    let h = &app.search_hits[i];
                    crate::design::collections::ListItem {
                        label: h.text.clone(),
                        detail: Some(format!("{}:{}", h.path, h.line)),
                        tone: None,
                        id: "",
                    }
                },
                inner,
                Some(
                    crate::design::states::State::empty("no hits yet")
                        .with_detail("type a query and press Enter"),
                ),
            );
        }
        View::Tasks => {
            let header = Some(Line::from(Span::styled(
                list_header(
                    app,
                    format!(
                        "{} tasks · Enter preview · ↑/↓ select{}",
                        app.tasks.len(),
                        if app.filter_active {
                            " · type to filter · Esc close"
                        } else {
                            " · Ctrl+F filter"
                        }
                    ),
                ),
                Style::default().fg(t.text.muted),
            )));
            render_windowed_filtered_list(
                app,
                frame,
                header,
                app.tasks.len(),
                &|i| app.task_matches(i),
                app.sel,
                |i| {
                    let task = &app.tasks[i];
                    crate::design::collections::ListItem {
                        label: format!("{:?} {}", task.status, truncate(&task.prompt, 60)),
                        detail: Some(format!("{}", task.id)),
                        // A task's state is the first thing an operator looks for, so
                        // it comes from the shared semantic mapping rather than a
                        // colour re-decided at this call site.
                        tone: Some(task_status_tone(task.status)),
                        id: "",
                    }
                },
                inner,
                Some(if app.tasks.is_empty() {
                    crate::design::states::State::empty("no tasks for this session")
                        .with_detail("send a message or /plan to start one")
                } else {
                    crate::design::states::State::empty("no tasks match the filter")
                }),
            );
        }
        View::Sessions => {
            let header = Some(Line::from(Span::styled(
                list_header(
                    app,
                    format!(
                        "{} sessions · Enter switch · d delete{}",
                        app.sessions.len(),
                        if app.filter_active {
                            " · type to filter · Esc close"
                        } else {
                            " · Ctrl+F filter"
                        }
                    ),
                ),
                Style::default().fg(t.text.muted),
            )));
            render_windowed_filtered_list(
                app,
                frame,
                header,
                app.sessions.len(),
                &|i| app.session_matches(i),
                app.sel,
                |i| {
                    let s = &app.sessions[i];
                    let cur = app.current_session == Some(s.id);
                    let mark = if cur { g("●", "*") } else { g("○", "o") };
                    crate::design::collections::ListItem {
                        label: format!("{mark} {}", s.title),
                        detail: Some(format!("{} msgs", s.content.len())),
                        // The current session is marked by glyph *and* tone, so the
                        // distinction survives Monochrome.
                        tone: cur.then_some(crate::design::collections::RowTone::Info),
                        id: "",
                    }
                },
                inner,
                Some(if app.sessions.is_empty() {
                    crate::design::states::State::empty("no sessions yet")
                        .with_detail("send a message or /new to begin one")
                } else {
                    crate::design::states::State::empty("no sessions match the filter")
                }),
            );
        }
        View::Agents => {
            // Agent rows are derived from task records, so their count is bounded
            // by the task list — which is not bounded. Two problems were fixed
            // here: every agent row was built as a `Line` before scrolling, and
            // de-duplication used `Vec::contains`, making the view quadratic in
            // the number of tasks (10k tasks with distinct agents meant 50M string
            // comparisons per frame). The scan is still O(tasks) — it has to be,
            // the rows *are* derived from them — but it allocates one id per
            // distinct agent instead of one `Line` per agent, and the rendering
            // step materialises only what fits.
            //
            // A `HashSet` would drop the allocation too, but it loses the
            // first-seen ordering the rows rely on, and re-ordering agent rows
            // between frames is worse than the allocation it saves.
            let mut seen: Vec<String> = Vec::new();
            let mut dedup = std::collections::HashSet::new();
            for task in &app.tasks {
                if let Some(a) = &task.agent_id {
                    if dedup.insert(a.to_string()) {
                        seen.push(a.to_string());
                    }
                }
            }
            let header = line_kv(t, "orchestrator", &app.runtime.orchestrator.to_string());
            let agent_row = line_kv(t, "agent", app.runtime.agent.id());
            let footer = Line::from(Span::styled(
                "  (agent rows come from task records · PgUp/PgDn scroll)",
                Style::default().fg(t.text.muted),
            ));
            let total = 2 + seen.len() + 1;
            render_scrolled_windowed(frame, total, app.view_scroll, inner, |i| match i {
                0 => header.clone(),
                1 => agent_row.clone(),
                // The last line is the explanatory footer.
                n if n == total - 1 => footer.clone(),
                n => line_kv(t, "task agent", &seen[n - 2]),
            });
        }
        View::Git => match &app.git {
            Some(g) => {
                // The three sections are indexed rather than concatenated. A
                // repository with a long history produced a `Line` per log entry
                // — each with a `format!` — every frame, to paint ~40 rows. The
                // section boundaries are arithmetic on lengths, so a row can be
                // built only when the window actually reaches it.
                //
                // `status` keeps its existing 20-row cap: that is a deliberate
                // display decision, not a truncation caused by scrolling, and it
                // is unchanged here.
                let n_status = g.status.len().min(20);
                let sec_status_head = 4;
                let sec_log_head = sec_status_head + n_status;
                let sec_stat_head = sec_log_head + 1 + g.log.len();
                let total = sec_stat_head + 1 + g.stat.len();
                let section_head = |label: &'static str| {
                    Line::from(Span::styled(
                        label.to_string(),
                        Style::default()
                            .fg(t.accent.primary)
                            .add_modifier(Modifier::BOLD),
                    ))
                };
                render_scrolled_windowed(frame, total, app.view_scroll, inner, |i| {
                    if i < sec_status_head {
                        // branch / changes / blank / "status" header
                        return [
                            line_kv(t, "branch", &g.branch),
                            line_kv(t, "changes", &g.status.len().to_string()),
                            Line::from(""),
                            section_head(" status"),
                        ][i]
                            .clone();
                    }
                    let i = i - sec_status_head;
                    if i < n_status {
                        return Line::from(Span::styled(
                            format!("  {}", g.status[i]),
                            Style::default().fg(t.text.muted),
                        ));
                    }
                    let i = i - n_status;
                    if i == 0 {
                        return section_head(" recent commits");
                    }
                    let i = i - 1;
                    if i < g.log.len() {
                        return Line::from(Span::styled(
                            format!("  {}", g.log[i]),
                            Style::default().fg(t.text.muted),
                        ));
                    }
                    let i = i - g.log.len();
                    if i == 0 {
                        return section_head(" diff stat");
                    }
                    let i = i - 1;
                    Line::from(Span::styled(
                        format!("  {}", g.stat[i]),
                        Style::default().fg(t.text.muted),
                    ))
                });
            }
            None => {
                let msg = app
                    .git_error
                    .clone()
                    .unwrap_or_else(|| format!("loading{} Ctrl+R to retry", g("…", "...")));
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        format!("  {msg}"),
                        Style::default().fg(t.text.muted),
                    )),
                    inner,
                );
            }
        },
        View::Terminal => {
            // The terminal buffer is the one collection that grows without any
            // bound at all, and the old path was the worst in the codebase: it
            // split and `format!`-styled every line of the *whole* buffer, cloned
            // that `Vec<Line>` in full purely to measure its wrapped height, then
            // wrapped it a second time to draw a screenful. Two complete passes
            // over the entire scrollback, per frame, plus a full duplicate of it
            // in memory.
            //
            // The offset is bottom-anchored (`term_scroll` counts rows scrolled
            // *up* from the newest output), so the window is found by walking
            // backwards until enough rows are collected — the same backwards walk
            // the chat stream uses. Rows are counted as they are produced, using
            // the same `Paragraph::line_count` the old code used for the total, so
            // the scroll position a user sees is unchanged.
            let wrap = Wrap { trim: false };
            let need = usize::from(inner.height).saturating_add(usize::from(app.term_scroll));
            let style_for = |kind: &str| match kind {
                "in" => Style::default().fg(t.accent.primary),
                "err" => Style::default().fg(t.status.danger),
                "note" => Style::default().fg(t.status.warning),
                _ => Style::default().fg(t.text.secondary),
            };

            // Per-source-entry chunks, newest first. Splitting happens one entry
            // at a time *and* stops at the row budget, so neither the buffer nor a
            // single oversized entry is ever fully materialised.
            // `chunks` is collected newest-first, because the walk runs backwards.
            let mut chunks: Vec<Vec<Line>> = Vec::new();
            let mut rows = 0usize;
            // The "still running" footer is the newest thing there is, so it leads
            // the newest-first list and therefore lands *last* on screen once the
            // list is reversed. It is emitted whenever the command is live, and
            // it costs a row the walk budgets for, so the footer survives a full
            // buffer — it used to be dropped exactly when the operator most needed
            // to know the command had not finished.
            if app.term_join.is_some() {
                chunks.push(vec![Line::from(Span::styled(
                    " ⋯ command still running — output appends here when it finishes",
                    Style::default().fg(t.status.warning),
                ))]);
                rows += 1;
            }
            for l in app.term_log.iter().rev() {
                let style = style_for(l.kind);
                let prefix = if l.kind == "in" { " $ " } else { "   " };
                // One entry can itself be enormous — a `cat` of a large file is a
                // single `TermLine` with thousands of `\n`. Splitting all of it
                // before discovering the window is full would make the *entry*
                // the unbounded thing instead of the buffer, so lines are taken
                // only until the row budget is met and the rest is dropped. The
                // budget is what the old whole-buffer `line_count` answered for the
                // rows actually on screen, so the visible output is unchanged.
                let mut chunk: Vec<Line> = Vec::new();
                for (i, text) in l.text.lines().enumerate() {
                    let styled = Line::from(Span::styled(
                        format!("{}{}", if i == 0 { prefix } else { "   " }, text),
                        style,
                    ));
                    // Height of this line on its own. Measuring the whole chunk on
                    // every push would be quadratic in the entry's line count —
                    // exactly the cost this walk exists to avoid — and a wrapped
                    // line's height depends only on the line and the width, since
                    // wrapping never joins across a newline.
                    rows += Paragraph::new(styled.clone())
                        .wrap(wrap)
                        .line_count(inner.width);
                    chunk.push(styled);
                    if rows >= need {
                        break;
                    }
                }
                chunks.push(chunk);
                if rows >= need {
                    break;
                }
            }
            // Newest-first chunks flattened back to front. Reversing the flat list
            // instead would also reverse the lines *within* each entry, which
            // reads as garbled output rather than as a scroll bug.
            let lines: Vec<Line> = chunks
                .iter()
                .rev()
                .flat_map(|c| c.iter().cloned())
                .collect();
            #[cfg(test)]
            TERM_ROWS_BUILT.with(|c| c.set(c.get() + lines.len()));

            if lines.is_empty() {
                let hint = Line::from(Span::styled(
                    "  type a command + Enter — runs through the gated terminal.run tool",
                    Style::default().fg(t.text.muted),
                ));
                frame.render_widget(Paragraph::new(hint), inner);
            } else {
                let offset = rows.saturating_sub(need);
                frame.render_widget(
                    Paragraph::new(lines).wrap(wrap).scroll((offset as u16, 0)),
                    inner,
                );
            }
        }
        View::Tools => {
            let header = Some(Line::from(Span::styled(
                list_header(
                    app,
                    format!(
                        "{} tools · Enter spec{}",
                        app.tools.len(),
                        if app.filter_active {
                            " · type to filter · Esc close"
                        } else {
                            " · Ctrl+F filter"
                        }
                    ),
                ),
                Style::default().fg(t.text.muted),
            )));
            render_windowed_filtered_list(
                app,
                frame,
                header,
                app.tools.len(),
                &|i| app.tool_matches(i),
                app.sel,
                |i| {
                    let s = &app.tools[i];
                    let caps: Vec<String> = s
                        .required_caps
                        .iter()
                        .map(|c| c.as_str().to_string())
                        .collect();
                    crate::design::collections::ListItem {
                        label: s.name.clone(),
                        detail: (!caps.is_empty()).then_some(caps.join(",")),
                        tone: None,
                        id: "",
                    }
                },
                inner,
                Some(if app.tools.is_empty() {
                    crate::design::states::State::empty("no tools registered")
                } else {
                    crate::design::states::State::empty("no tools match the filter")
                }),
            );
        }
        View::Artifacts => {
            let note = vec![
                Line::from(Span::styled(
                    format!(" {}", app.artifacts_note),
                    Style::default().fg(t.text.muted),
                )),
                Line::from(""),
            ];
            // The report is pretty-printed whole, and that one allocation is not
            // avoidable without a streaming serializer. What *was* avoidable was a
            // `format!` per report line, per frame: a large scan report built tens
            // of thousands of styled rows to paint forty of them. Line references
            // are borrowed, so the remaining O(n) cost is a pointer per line and
            // the per-line allocation is now paid only for the window.
            let pretty = serde_json::to_string_pretty(&app.artifacts).unwrap_or_default();
            let body: Vec<&str> = pretty.lines().collect();
            let h = inner
                .height
                .saturating_sub(u16::try_from(note.len()).unwrap_or(u16::MAX));
            let (off, count) =
                design::collections::window_at(body.len(), usize::from(app.view_scroll), h);
            #[cfg(test)]
            ROWS_MATERIALISED.with(|c| c.set(c.get() + count));
            let mut lines = note;
            lines.extend(body[off..off + count].iter().map(|l| {
                Line::from(Span::styled(
                    format!("  {l}"),
                    Style::default().fg(t.text.secondary),
                ))
            }));
            frame.render_widget(Paragraph::new(lines), inner);
        }
        View::Models => {
            let info = app.runtime.provider.info();
            let caps = app.runtime.provider.capabilities();
            // Section tabs through the shared strip, so Models selects its
            // section the same way every other tabbed surface does — and the
            // active tab is marked by the shared selected surface rather than by
            // colour alone.
            let tab_labels: Vec<&str> = crate::agent::state::ModelSection::ALL
                .iter()
                .map(|s| s.title())
                .collect();
            let tab_spans = design::widgets::tab_bar(t, &tab_labels, app.model_section.idx());
            let scope = match app.model_section {
                crate::agent::state::ModelSection::Local => {
                    "local registry catalog (cards + files on this machine)"
                }
                crate::agent::state::ModelSection::Hub if !app.net_allowed => {
                    "live Hugging Face Hub — blocked by current mode"
                }
                crate::agent::state::ModelSection::Hub => "live Hugging Face Hub (metadata only)",
                crate::agent::state::ModelSection::Installed => {
                    // "usable now" was a claim nothing had verified: an entry
                    // here means the bytes are on disk and catalogued, not that
                    // this machine can execute them. The execution rows below
                    // say which of those two is true.
                    "registered on disk (execution readiness shown below)"
                }
                crate::agent::state::ModelSection::Recommended => "curated offline catalog entries",
            };
            // The preamble is split into a mandatory *spine* and a collapsible
            // *detail* block, and the two are measured against a reserved row
            // budget rather than simply concatenated.
            //
            // Before this, the preamble was `lines.len().min(inner.height)` rows of
            // prose, so on an 80x24 terminal it claimed the entire panel: the
            // model rows got a budget of zero and the operator could not reach a
            // single model, let alone import or search one. That is not a
            // cosmetic overflow — the primary content of the view was unreachable
            // at a completely ordinary terminal size.
            //
            // The spine is what an operator cannot work without: which section is
            // active, which provider is in use, the exact model id that goes over
            // the wire, and the keys for this branch. The detail block is
            // diagnostics — scope wording, resource fit, capability flags — which
            // is exactly what should yield when rows are scarce.
            let mut spine: Vec<Line> = vec![
                Line::from(tab_spans),
                line_kv(t, "provider", app.runtime.provider.id()),
                // Exact model ID as it goes over the wire: the operator
                // override if set, else the provider default. Never
                // abbreviated or renamed.
                line_kv(
                    t,
                    "model id",
                    &app.switcher
                        .as_ref()
                        .and_then(|s| s.selected())
                        .filter(|t| !t.is_empty())
                        .or_else(|| info.default_model.clone())
                        .unwrap_or_else(|| "(none)".into()),
                ),
            ];
            let mut detail: Vec<Line> = vec![line_kv(t, "scope", scope)];

            // Resource facts come before the optional capability rows: at a
            // narrow width the pane truncates from the bottom, and "this model
            // is RESOURCE-LIMITED" is state the operator must not lose to a
            // `streaming: no` line. In the order an operator needs them when a
            // model will not run: what the file is, what the host has, what
            // execution is estimated to need, whether that fits, and where to go
            // next. Every number is measured or labelled with its provenance.
            // Skipped entirely for a hub row, which has no artifact.
            if app.section_card().is_some() {
                detail.push(Line::from(Span::raw("")));
                detail.push(Line::from(Span::styled(
                    "resource fit",
                    Style::default()
                        .fg(t.text.muted)
                        .add_modifier(Modifier::BOLD),
                )));
                detail.push(line_kv(t, "artifact", &artifact_row(app)));
                detail.push(line_kv(t, "mem free", &mem_free_label(app)));
                detail.push(line_kv(t, "exec need", &exec_need_label(app)));
                detail.push(line_kv(t, "exec status", &exec_status_label(app)));
                detail.push(line_kv(t, "vram", &vram_label(app)));
                detail.push(line_kv(t, "remediation", &remediation_label(app)));
            }
            detail.push(line_kv(
                t,
                "ctx window",
                &caps
                    .context_window
                    .map(|w| w.to_string())
                    .unwrap_or_else(|| "n/a".into()),
            ));
            detail.push(line_kv(t, "native tools", &native_tools_label(app)));
            detail.push(line_kv(t, "vision", &vision_label(app)));
            detail.push(line_kv(
                t,
                "streaming",
                if caps.supports_streaming { "yes" } else { "no" },
            ));
            detail.push(line_kv(t, "provider status", &provider_status_label(app)));
            spine.push(if app.import_mode {
                let mut spans = vec![Span::styled(
                    format!(" importing as `{}` — path below", app.import_id),
                    Style::default().fg(t.status.warning),
                )];
                spans.extend(design::widgets::key_hints(
                    t,
                    &[("Enter", "import"), ("Esc", "cancel")],
                ));
                Line::from(spans)
            } else if app.model_search_active {
                let mut spans = vec![Span::styled(
                    " search query below",
                    Style::default().fg(t.text.muted),
                )];
                spans.extend(design::widgets::key_hints(
                    t,
                    &[("Enter", "search or apply"), ("Esc", "back")],
                ));
                Line::from(spans)
            } else if app.model_section == crate::agent::state::ModelSection::Hub {
                Line::from({
                    let mut spans = vec![Span::raw(" ")];
                    spans.extend(design::widgets::key_hints(
                        t,
                        &[
                            ("Enter", "hub row"),
                            ("Tab", "sections"),
                            ("p", "facet"),
                            ("f", "filter"),
                            ("[ ]", "page"),
                            ("d", "details"),
                            ("s", "scan"),
                            ("i", "import"),
                        ],
                    ));
                    spans
                })
            } else {
                Line::from({
                    let mut spans = vec![Span::raw(" ")];
                    spans.extend(design::widgets::key_hints(
                        t,
                        &[
                            ("Enter", "applies selected"),
                            ("Tab", "sections"),
                            ("i", "import"),
                            ("Ctrl+F", "search"),
                        ],
                    ));
                    spans
                })
            });
            // The section must always be able to show rows. `MIN_SECTION_ROWS` is
            // enough to see several models and act on one; below that, the
            // diagnostics are the thing that yields, because an operator who
            // cannot see the models cannot do the job the panel exists for.
            const MIN_SECTION_ROWS: u16 = 6;
            let spine_h = u16::try_from(spine.len()).unwrap_or(u16::MAX);
            let detail_h = u16::try_from(detail.len()).unwrap_or(u16::MAX);
            let keep_detail = inner.height.saturating_sub(spine_h) >= MIN_SECTION_ROWS + detail_h;

            let mut lines = Vec::with_capacity(spine.len() + detail.len() + 1);
            lines.extend(spine);
            if keep_detail {
                lines.extend(detail);
                lines.push(Line::from(""));
            } else {
                // Say that the panel is showing a summary, so a collapsed row set
                // is not mistaken for a model that is not there.
                lines.push(Line::from(Span::styled(
                    " summary — rows below · resize for resource fit",
                    Style::default().fg(t.text.muted),
                )));
                lines.push(Line::from(""));
            }
            // Pinned preamble (provider facts + mode hint) for every branch. It
            // is bounded by `inner.height - MIN_SECTION_ROWS` so the section rows
            // always have somewhere to go.
            let pre_max = inner.height.saturating_sub(MIN_SECTION_ROWS);
            let pre_h = (lines.len() as u16).min(pre_max);
            let lines: Vec<Line> = lines.into_iter().take(usize::from(pre_h)).collect();
            frame.render_widget(
                Paragraph::new(lines),
                Rect {
                    height: pre_h,
                    ..inner
                },
            );
            let mut body = Rect {
                y: inner.y + pre_h,
                height: inner.height.saturating_sub(pre_h),
                ..inner
            };
            // Structured import failure card — visible even when the status
            // line has moved on (MODEL IMPORT FAILED / Reason: …).
            if let Some(err) = &app.import_error {
                let flat = err.replace('\n', " ");
                let card = vec![
                    Line::from({
                        let mut spans = vec![Span::raw(" ")];
                        spans.extend(
                            design::widgets::Badge::new("MODEL IMPORT FAILED", BadgeTone::Danger)
                                .spans(t),
                        );
                        spans
                    }),
                    Line::from(Span::styled(
                        format!(
                            " Reason: {}",
                            truncate(&flat, body.width.saturating_sub(10) as usize)
                        ),
                        Style::default().fg(t.text.secondary),
                    )),
                ];
                let card_h = (card.len() as u16 + 2).min(body.height);
                frame.render_widget(
                    Paragraph::new(card).block(design::widgets::panel(
                        t,
                        " import ",
                        design::widgets::PanelTone::Destructive,
                    )),
                    Rect {
                        height: card_h,
                        ..body
                    },
                );
                body.y += card_h;
                body.height = body.height.saturating_sub(card_h);
            }
            if app.import_mode {
                // Import prompt: artifact id + the local path being typed.
                let mut prompt = vec![Line::from(Span::styled(
                    format!(" artifact   {}", app.import_id),
                    Style::default()
                        .fg(t.accent.primary)
                        .add_modifier(Modifier::BOLD),
                ))];
                prompt.push(Line::from(vec![
                    Span::styled(" path       ", Style::default().fg(t.text.muted)),
                    Span::styled(
                        format!("{}▌", app.input),
                        Style::default().fg(t.text.secondary),
                    ),
                ]));
                prompt.push(Line::from(Span::styled(
                    " note       real sha256 + size computed from the bytes read; the",
                    Style::default().fg(t.text.muted),
                )));
                prompt.push(Line::from(Span::styled(
                    "            size guard refuses files over half of MemAvailable",
                    Style::default().fg(t.text.muted),
                )));
                if let Some(h) = app.import_join.as_ref() {
                    if !h.is_finished() {
                        // The shared frame-cycled spinner. This is a bounded
                        // operation with a visible join handle, so the spinner
                        // marks activity while the note states the real bound.
                        prompt.push(Line::from(vec![Span::styled(
                            format!(
                                "            {} import running in the background (UI stays live)",
                                design::widgets::spinner((app.mode_tick % 4) as usize)
                            ),
                            Style::default().fg(t.status.running),
                        )]));
                    }
                }
                frame.render_widget(Paragraph::new(prompt), body);
            } else if app.model_search_active {
                let mut head = vec![Line::from(vec![
                    Span::styled(" query: ", Style::default().fg(t.text.muted)),
                    Span::styled(
                        format!("{}▌", app.model_query),
                        Style::default().fg(t.accent.primary),
                    ),
                ])];
                let hub_typing = app.model_section == crate::agent::state::ModelSection::Hub;
                head.push(Line::from(Span::styled(
                    format!(
                        "{} hits for “{}” · Enter search/apply · i import · Esc close",
                        app.model_results.len(),
                        app.model_results_query
                    ),
                    Style::default().fg(t.text.muted),
                )));
                let header_h = head.len() as u16;
                let list_h = body.height.saturating_sub(header_h);
                // The window is measured before a single result row is built.
                // This loop used to run over *every* hit — five `format!`
                // allocations each — and only then discarded all but the visible
                // rows, so a broad search paid for the whole result set on every
                // frame. `window` is the shared implementation; the local
                // `list_offset` this replaced was a second one that drifted.
                let total = if hub_typing {
                    0
                } else {
                    app.model_results.len()
                };
                let (start, count) = design::collections::window(
                    total,
                    app.sel.min(total.saturating_sub(1)),
                    list_h,
                );
                #[cfg(test)]
                MODEL_ROWS_BUILT.with(|c| c.set(c.get() + count));
                let mut rows: Vec<Line> = Vec::new();
                for i in start..start.saturating_add(count) {
                    let r = &app.model_results[i];
                    let style = if i == app.sel {
                        Style::default()
                            .fg(t.accent.primary)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(t.text.secondary)
                    };
                    let marker = if i == app.sel { g("›", ">") } else { " " };
                    // Real size when the hub answered; probing/absent stays honest.
                    let size = match app.hf_sizes.get(&r.artifact_id) {
                        Some(h) if h.error.is_some() => "size: probe failed".to_string(),
                        Some(h) if h.gated => "gated".to_string(),
                        Some(h) => match h.total_bytes {
                            Some(b) => format!("· {}", format_size(b)),
                            None => "· size n/a".into(),
                        },
                        None => format!("· size {}", g("…", "...")),
                    };
                    let state = if r.installed {
                        "installed"
                    } else {
                        "not installed"
                    };
                    rows.push(Line::from(vec![
                        Span::styled(format!(" {marker} "), style),
                        Span::styled(format!("{:<34}", truncate(&r.display_name, 33)), style),
                        Span::styled(
                            format!("{:<10}", r.source.as_str()),
                            Style::default().fg(t.text.muted),
                        ),
                        Span::styled(format!("{:<14}", state), Style::default().fg(t.text.muted)),
                        Span::styled(size, Style::default().fg(t.text.muted)),
                    ]));
                    if i == app.sel {
                        rows.push(Line::from(Span::styled(
                            format!("    {}", truncate(&r.artifact_id, 70)),
                            Style::default().fg(t.text.muted),
                        )));
                    }
                }
                if rows.is_empty() {
                    rows.push(Line::from(Span::styled(
                        if hub_typing {
                            "  Enter queries the Hugging Face hub (metadata only) · Esc back"
                        } else {
                            "  no hits — try another query (local + curated + HF)"
                        },
                        Style::default().fg(t.text.muted),
                    )));
                }
                frame.render_widget(
                    Paragraph::new(head),
                    Rect {
                        height: header_h.min(body.height),
                        ..body
                    },
                );
                frame.render_widget(
                    Paragraph::new(rows),
                    Rect {
                        y: body.y + header_h,
                        height: list_h,
                        ..body
                    },
                );
            } else {
                // Section-aware list: Local = category-grouped cards,
                // Hugging Face = marketplace rows (+ facts pane on wide
                // terminals), Installed/Recommended = honest card subsets.
                let section = app.model_section;
                let is_hub = section == crate::agent::state::ModelSection::Hub;
                // 3-pane only for the Hub section on wide terminals; every
                // other section keeps the full-width list it always had.
                let split = is_hub && body.width >= 100;
                let list_rect = if split {
                    Rect {
                        width: body.width.saturating_sub(41),
                        ..body
                    }
                } else {
                    body
                };
                let mut section_head: Vec<Line> = Vec::new();
                let mut rows: Vec<(Option<usize>, Line)> = Vec::new();
                // Rows the blocked-mode card occupies above the list.
                let mut card_h: u16 = 0;
                if is_hub {
                    if !app.net_allowed {
                        // Sovereign / network-off status card: the block, the
                        // fact, the way forward, and the offline work that
                        // still works right here.
                        //
                        // Drawn as a real panel in its own rectangle rather than
                        // as five loose lines in the list preamble. As body text
                        // above scrolling rows it read as commentary on those
                        // rows instead of as the reason they are absent, which is
                        // the one thing the operator has to understand first.
                        card_h = 6;
                    }
                    if app.hub_filter_active {
                        section_head.push(Line::from(Span::styled(
                            " filter — ↑/↓ dimension · ←/→ value · c clear · Enter/Esc close",
                            Style::default().fg(t.status.warning),
                        )));
                        section_head.push(Line::from(Span::styled(
                            format!(" → {} row(s) match", app.hub_indices().len()),
                            Style::default().fg(t.text.muted),
                        )));
                    }
                    section_head.push(Line::from(Span::styled(
                        format!(
                            " {} · facet {} · skip {} · page ± with [ ]",
                            app.hub_status,
                            match &app.hub_facet {
                                Some(f) => f.as_str(),
                                None => "all",
                            },
                            app.hub_skip
                        ),
                        Style::default().fg(t.text.muted),
                    )));
                }
                // The section head, the offline card and the rows are three
                // claims on one rectangle, and the rows get whatever is left.
                // That has to be settled *before* the rows are built: building
                // against the full height and letting the shared tail clip the
                // overflow drops the top of the window, and the top of the
                // window is exactly where the cursor sits when the selection
                // is at the start of a long section.
                let head_h = (section_head.len() as u16).min(list_rect.height);
                let after_head = list_rect.height.saturating_sub(head_h);
                let card_claim = card_h.min(after_head.saturating_sub(4));
                let body_h = after_head - card_claim;

                if is_hub {
                    let idxs = app.hub_indices();
                    if app.hub_filter_active {
                        // Filter overlay: dimensions + values (client-side,
                        // only real data from the loaded rows).
                        for (i, dim) in crate::agent::state::HubFilterDim::ALL.iter().enumerate() {
                            let cur = app
                                .hub_filters
                                .iter()
                                .find(|(d, _)| d == dim)
                                .and_then(|(_, v)| v.clone());
                            let vals = app.hub_filter_values(*dim);
                            let style = if i == app.hub_filter_sel {
                                Style::default()
                                    .fg(t.accent.primary)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(t.text.muted)
                            };
                            let marker = if i == app.hub_filter_sel {
                                g("›", ">")
                            } else {
                                " "
                            };
                            let value = if vals.is_empty() {
                                Span::styled(
                                    "no data in loaded rows",
                                    Style::default().fg(t.text.muted),
                                )
                            } else {
                                match &cur {
                                    Some(v) => Span::styled(
                                        v.clone(),
                                        Style::default().fg(t.accent.primary),
                                    ),
                                    None => {
                                        Span::styled("(all)", Style::default().fg(t.text.secondary))
                                    }
                                }
                            };
                            rows.push((
                                None,
                                Line::from(vec![
                                    Span::styled(format!(" {marker} "), style),
                                    Span::styled(format!("{:<14}", dim.title()), style),
                                    value,
                                ]),
                            ));
                        }
                    } else {
                        if idxs.is_empty() {
                            rows.push((
                            None,
                            Line::from(Span::styled(
                                if app.net_allowed {
                                    " no rows — Enter re-queries (Ctrl+F types a query)"
                                } else {
                                    " no rows — i = IMPORT LOCAL MODEL works offline · switch mode to browse the hub"
                                },
                                Style::default().fg(t.text.muted),
                            )),
                        ));
                        }
                        // `idxs` is the filtered/sorted view of the hub; the rows
                        // are built only for the window inside it, so a large
                        // hub page costs a frame rather than a page of `format!`.
                        // `rows` is empty at this point, so the shared tail below
                        // applies no offset of its own.
                        let (h_start, h_count) =
                            design::collections::window(idxs.len(), app.sel, body_h);
                        for pos in h_start..h_start.saturating_add(h_count) {
                            let ri = idxs[pos];
                            let r = &app.hub_rows[ri];
                            let selected = pos == app.sel;
                            let style = if selected {
                                Style::default()
                                    .fg(t.accent.primary)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(t.text.secondary)
                            };
                            let marker = if selected { g("›", ">") } else { " " };
                            let size = match app.hf_sizes.get(&r.id) {
                                Some(h) if h.error.is_some() => "size: fetch failed".to_string(),
                                Some(h) => match h.total_bytes {
                                    Some(b) => format!("· {}", format_size(b)),
                                    None => "· size n/a".into(),
                                },
                                None => format!("· size {}", g("…", "...")),
                            };
                            let gated = match app.hf_sizes.get(&r.id) {
                                Some(h) if h.gated => " · gated",
                                _ => "",
                            };
                            rows.push((
                                Some(pos),
                                Line::from(vec![
                                    Span::styled(format!(" {marker} "), style),
                                    Span::styled(format!("{:<44}", truncate(&r.id, 43)), style),
                                    Span::styled(
                                        format!(
                                            "{:<18}",
                                            truncate(r.pipeline_tag.as_deref().unwrap_or("—"), 17)
                                        ),
                                        Style::default().fg(t.text.muted),
                                    ),
                                    Span::styled(
                                        format!("{size}{gated}"),
                                        Style::default().fg(t.text.muted),
                                    ),
                                ]),
                            ));
                        }
                    } // end !hub_filter_active
                } else {
                    // Local keeps its category sections; Installed and
                    // Recommended list the same card rows, subsetted. All three
                    // come from `model_card_rows`, which builds only the visible
                    // rows and reports the true total.
                    let (card_lines, _) = model_card_rows(t, app, section, body_h);
                    if card_lines.is_empty() {
                        let msg = match section {
                            crate::agent::state::ModelSection::Installed => {
                                " nothing installed on disk — i imports a local file · s scans models/"
                            }
                            crate::agent::state::ModelSection::Local => {
                                " NO MODELS FOUND — drop weight files into models/<category>/ and press s"
                            }
                            _ => " no curated entries in this catalog",
                        };
                        rows.push((
                            None,
                            Line::from(Span::styled(
                                format!("  {msg}"),
                                Style::default().fg(t.text.muted),
                            )),
                        ));
                    } else {
                        // The window is already applied by `model_card_rows`, so
                        // the rows are stored with no cursor identity and the
                        // shared tail below applies no second offset. The
                        // selection marker is drawn inside `card_row`, so it is
                        // keyed off the card's position in its own section rather
                        // than off its screen row — which is what survives
                        // scrolling.
                        rows = card_lines.into_iter().map(|l| (None, l)).collect();
                    }
                }
                if head_h > 0 {
                    frame.render_widget(
                        Paragraph::new(section_head),
                        Rect {
                            height: head_h,
                            ..list_rect
                        },
                    );
                }
                let mut list_y = list_rect.y + head_h;
                let mut list_h = after_head;
                if card_h > 0 {
                    // The card explains the rows beneath it, so it must never
                    // crowd them out: it only claims rows while several rows
                    // still survive underneath. On a short terminal the same
                    // facts are already in the `scope` line above, so dropping
                    // the card loses nothing an operator cannot already read.
                    let card_h = card_claim;
                    // Below the section head, not over it: `list_rect`'s origin
                    // is where the head was painted.
                    let card = Rect {
                        y: list_y,
                        height: card_h,
                        ..list_rect
                    };
                    if card_h >= 4 {
                        let title =
                            format!(" HUB UNAVAILABLE — {} MODE ", app.exec_mode.to_uppercase());
                        design::widgets::render_panel(
                            frame,
                            card,
                            t,
                            &title,
                            design::widgets::PanelTone::Destructive,
                            |f, inner| {
                                let w = inner.width.saturating_sub(2) as usize;
                                let mut lines = vec![
                                    Line::from({
                                        let mut sp = vec![Span::raw(" ")];
                                        sp.extend(
                                            design::widgets::Badge::new(
                                                "EXTERNAL NETWORK: OFF",
                                                BadgeTone::Danger,
                                            )
                                            .spans(t),
                                        );
                                        sp
                                    }),
                                    Line::from(vec![
                                        Span::styled(" ACTION ", t.type_scale.metadata),
                                        Span::styled(
                                            truncate(
                                                "switch to Provisioning Mode to browse the live hub",
                                                w,
                                            ),
                                            Style::default().fg(t.status.warning),
                                        ),
                                    ]),
                                ];
                                // The offline options are the whole point of the
                                // card, so they get a shared key-hint row rather
                                // than a hand-written legend.
                                lines.push(Line::from(Span::raw("")));
                                lines.push(hint_row(
                                    t,
                                    &[("i", "import local model"), ("s", "scan models/")],
                                ));
                                f.render_widget(Paragraph::new(lines), inner);
                            },
                        );
                    }
                    list_y = card.y + card_h;
                    list_h = body_h;
                }
                let pos = rows
                    .iter()
                    .position(|(idx, _)| *idx == Some(app.sel))
                    .unwrap_or(0);
                // `rows` is already windowed to the row budget by whichever
                // branch built it, so this only has to agree with that window; the
                // shared rule keeps the two from drifting.
                let (off, _) = design::collections::window(rows.len(), pos, list_h.max(1));
                let visible: Vec<Line> = rows
                    .into_iter()
                    .skip(off)
                    .take(usize::from(list_h))
                    .map(|(_, l)| l)
                    .collect();
                frame.render_widget(
                    Paragraph::new(visible),
                    Rect {
                        y: list_y,
                        height: list_h,
                        ..list_rect
                    },
                );
                if split {
                    // Right facts pane: real hub/config metadata for the row
                    // under the cursor (empty until a fetch lands — honest).
                    if let Some(detail) = app.hf_detail_lines(39) {
                        let pane = Rect {
                            x: list_rect.x + list_rect.width,
                            y: body.y,
                            width: 41,
                            height: body.height,
                        };
                        // Shared panel chrome: the facts pane was the only box in
                        // this view drawn with a raw `Block`, so it had no border
                        // tone and no surface behind it and sat a shade off from
                        // every other framed region.
                        frame.render_widget(
                            Paragraph::new(
                                detail.into_iter().map(Line::from).collect::<Vec<Line>>(),
                            )
                            .block(design::widgets::panel(
                                t,
                                " detail ",
                                design::widgets::PanelTone::Default,
                            )),
                            pane,
                        );
                    }
                }
            }
        }
        View::Mcp => {
            // Connectivity manager: summary + actions header, table rows
            // (scrollable + filterable like every other list).
            let summary = &app.mcp["summary"];
            let total = app.mcp_rows.len();
            let mcp_err = app.mcp["error"].as_str().map(str::to_string);
            let mut header: Vec<Line> = Vec::new();
            // Honest loading/error states — never fake zero counters.
            if let Some(e) = &mcp_err {
                header.push(Line::from(Span::styled(
                    format!(" mcp registry unavailable: {e}"),
                    Style::default().fg(t.status.danger),
                )));
            } else if app.mcp.is_null() {
                header.push(Line::from(Span::styled(
                    " not loaded — Ctrl+R refresh",
                    Style::default().fg(t.text.muted),
                )));
            } else {
                header.push(Line::from(Span::styled(
                    format!(
                        " {} servers · {} enabled · {} connectable · {} review · {} blocked · {} flagged",
                        total,
                        summary["enabled"].as_i64().unwrap_or(0),
                        summary["connectable"].as_i64().unwrap_or(0),
                        summary["review"].as_i64().unwrap_or(0),
                        summary["blocked"].as_i64().unwrap_or(0),
                        summary["flagged"].as_i64().unwrap_or(0),
                    ),
                    Style::default().fg(t.text.muted),
                )));
            }
            header.push(Line::from(Span::styled(
                format!(
                    " Enter inspect · e enable/disable · t trust · s scan · h health · o operator · Ctrl+F filter · op: {}",
                    app.operator_id
                ),
                Style::default().fg(t.text.muted),
            )));
            // Counted from the predicate; `filtered_mcp()` would allocate a
            // `usize` per server on every repaint just to fill in this number.
            let shown = crate::design::collections::FilteredWindow::new(
                total,
                &|i| app.mcp_matches(i),
                app.sel,
                0,
                false,
            )
            .total();
            if app.filter_active {
                header.push(Line::from(Span::styled(
                    format!(" filter “{}” ({shown} shown)", app.list_filter),
                    Style::default().fg(t.accent.primary),
                )));
            }
            // Row cells are built behind a factory rather than materialised. A
            // registry with many servers previously cost six `String`s per server
            // per frame, for a table that shows at most a screenful — and the
            // `format!("{}", r.tools)` cell in particular was paid on every
            // refresh tick whether or not the row was on screen.
            let no_rows = if mcp_err.is_some() {
                "(registry unavailable — see the error above)"
            } else if app.mcp_rows.is_empty() {
                "no MCP servers registered (declare in config.fabric.mcp or `openrt mcp register`)"
            } else {
                "no servers match the filter"
            };
            let columns = vec![
                crate::design::collections::Column {
                    header: "",
                    width: Some(20),
                    align: crate::design::collections::Align::Left,
                    tail: false,
                },
                crate::design::collections::Column {
                    header: "trust",
                    width: Some(11),
                    align: crate::design::collections::Align::Left,
                    tail: false,
                },
                crate::design::collections::Column {
                    header: "state",
                    width: Some(10),
                    align: crate::design::collections::Align::Left,
                    tail: false,
                },
                crate::design::collections::Column {
                    header: "security",
                    width: Some(13),
                    align: crate::design::collections::Align::Left,
                    tail: true,
                },
                crate::design::collections::Column {
                    header: "tools",
                    width: Some(7),
                    align: crate::design::collections::Align::Right,
                    tail: false,
                },
                crate::design::collections::Column {
                    header: "src",
                    width: None,
                    align: crate::design::collections::Align::Left,
                    tail: true,
                },
            ];
            if total == 0 || shown == 0 {
                let table = crate::design::collections::TableView {
                    title: "",
                    columns,
                    rows: crate::design::collections::TableRows::Cells(vec![vec![
                        no_rows.to_string()
                    ]]),
                    cursor: 0,
                    offset: 0,
                };
                render_table(app, frame, header, &table, inner);
            } else {
                render_table_filtered(
                    app,
                    frame,
                    header,
                    total,
                    &|i| app.mcp_matches(i),
                    app.sel,
                    columns,
                    |i| {
                        let r = &app.mcp_rows[i];
                        // Trust and security are the two things an operator scans
                        // this table for, so they are separate columns rather than
                        // colour baked into a name cell.
                        vec![
                            truncate(&r.name, 19),
                            r.trust.clone(),
                            if r.enabled { "enabled" } else { "disabled" }.to_string(),
                            r.security.clone(),
                            format!("{}", r.tools),
                            r.source.clone(),
                        ]
                    },
                    inner,
                );
            }
        }
        View::Extensions => {
            // Inventory table + install prompt (local path or gated GitHub URL).
            let ext_err = app
                .exts
                .first()
                .and_then(|e| e.get("error"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let mut header: Vec<Line> = Vec::new();
            match &ext_err {
                // Honest error state — never a fake zero count.
                Some(e) => header.push(Line::from(Span::styled(
                    format!(" extension registry unavailable: {e}"),
                    Style::default().fg(t.status.danger),
                ))),
                None => header.push(Line::from(Span::styled(
                    format!(
                        "{} extension version(s) · Enter manifest · v validate · s scan · a activate · d deactivate · x invalidate · i install · o operator · Ctrl+F filter · op: {}",
                        app.ext_rows.len(),
                        app.operator_id
                    ),
                    Style::default().fg(t.text.muted),
                ))),
            }
            if app.filter_active {
                header.push(Line::from(Span::styled(
                    // Counted from the predicate; `filtered_ext()` would allocate
                    // a `usize` per installed extension on every repaint.
                    format!(
                        " filter “{}” ({} shown)",
                        app.list_filter,
                        crate::design::collections::FilteredWindow::new(
                            app.ext_rows.len(),
                            &|i| app.ext_matches(i),
                            app.sel,
                            0,
                            false,
                        )
                        .total()
                    ),
                    Style::default().fg(t.accent.primary),
                )));
            }
            if app.ext_install {
                header.push(Line::from(vec![
                    Span::styled(" install: ", Style::default().fg(t.status.warning)),
                    Span::styled(
                        format!("{}▌", app.input),
                        Style::default().fg(t.text.secondary),
                    ),
                    Span::styled(
                        "  (package JSON path or https URL · Esc cancel)",
                        Style::default().fg(t.text.muted),
                    ),
                ]));
                if let Some(h) = app.op_join.as_ref() {
                    if !h.is_finished() {
                        header.push(Line::from(Span::styled(
                            " ⋯ working in the background (UI stays live)",
                            Style::default().fg(t.status.warning),
                        )));
                    }
                }
            }
            // Installed extensions accumulate — `i` installs one at a time — so the
            // inventory is operator-growable and the cells are built behind a
            // factory rather than all at once.
            let no_rows = if ext_err.is_some() {
                "(inventory unavailable — see the error above)"
            } else if app.ext_rows.is_empty() {
                "no extensions — press i to install a package (path or GitHub URL)"
            } else {
                "no extensions match the filter"
            };
            let columns = vec![
                crate::design::collections::Column {
                    header: "",
                    width: Some(24),
                    align: crate::design::collections::Align::Left,
                    tail: true,
                },
                crate::design::collections::Column {
                    header: "ver",
                    width: Some(8),
                    align: crate::design::collections::Align::Left,
                    tail: false,
                },
                crate::design::collections::Column {
                    header: "name",
                    width: Some(12),
                    align: crate::design::collections::Align::Left,
                    tail: true,
                },
                crate::design::collections::Column {
                    header: "activation",
                    width: Some(14),
                    align: crate::design::collections::Align::Left,
                    tail: true,
                },
                crate::design::collections::Column {
                    header: "security",
                    width: None,
                    align: crate::design::collections::Align::Left,
                    tail: true,
                },
            ];
            let total = app.ext_rows.len();
            let shown = crate::design::collections::FilteredWindow::new(
                total,
                &|i| app.ext_matches(i),
                app.sel,
                0,
                false,
            )
            .total();
            if total == 0 || shown == 0 {
                let table = crate::design::collections::TableView {
                    title: "",
                    columns,
                    rows: crate::design::collections::TableRows::Cells(vec![vec![
                        no_rows.to_string()
                    ]]),
                    cursor: 0,
                    offset: 0,
                };
                render_table(app, frame, header, &table, inner);
            } else {
                render_table_filtered(
                    app,
                    frame,
                    header,
                    total,
                    &|i| app.ext_matches(i),
                    app.sel,
                    columns,
                    |i| {
                        let e = &app.ext_rows[i];
                        // Activation and security are the two gate outcomes an
                        // operator scans for, so they get their own columns.
                        vec![
                            truncate(&e.id, 23),
                            format!("v{}", e.version),
                            truncate(&e.name, 11),
                            e.activation.clone(),
                            e.security.clone(),
                        ]
                    },
                    inner,
                );
            }
        }
        View::Security => {
            // Control center: status + live rules + findings + recovery as
            // one scrollable/filterable list, with the operator key map.
            let mut header = vec![
                Line::from(Span::styled(
                    format!(
                        " Enter policy doc · 1 read-only preset · 2 full-trust preset · n new rule · l audit · Ctrl+F filter · op: {}",
                        app.operator_id
                    ),
                    Style::default().fg(t.text.muted),
                )),
                Line::from(Span::styled(
                    " policy appends are immutable + audited; the live gate keeps its current policy until the next runtime build",
                    Style::default().fg(t.text.muted),
                )),
            ];
            if app.policy_edit {
                header.push(Line::from(vec![
                    Span::styled(" rule: ", Style::default().fg(t.status.warning)),
                    Span::styled(
                        format!("{}▌", app.input),
                        Style::default().fg(t.text.secondary),
                    ),
                    Span::styled(
                        "  <pattern> <allow|deny|ask> [scope] · Enter · Esc",
                        Style::default().fg(t.text.muted),
                    ),
                ]));
            }
            if app.filter_active {
                header.push(Line::from(Span::styled(
                    format!(
                        " filter “{}” ({} shown)",
                        app.list_filter,
                        app.filtered_security().len()
                    ),
                    Style::default().fg(t.accent.primary),
                )));
            }
            // Policy rows are appended by the operator (`Confirm::PolicyAppend`
            // exists for exactly this), so this list is as growable as the policy
            // document and cannot be treated as configuration-sized. Only the
            // visible window is built; the shared `list_window` decides where it
            // starts.
            render_windowed_filtered_list(
                app,
                frame,
                header,
                app.security_row_count(),
                &|i| app.security_matches(i),
                app.sel,
                |i| {
                    let (k, v) = app.security_row(i);
                    let (k, v) = (k.as_str(), v.as_str());
                    // A row with no value is a section heading; a bracketed key is
                    // an error. Both are decided by shape here and expressed with
                    // a shared semantic tone, rather than a colour chosen inline.
                    let is_heading = v.is_empty();
                    let tone = if is_heading {
                        Some(crate::design::collections::RowTone::Info)
                    } else if k.starts_with('[') {
                        Some(crate::design::collections::RowTone::Danger)
                    } else {
                        None
                    };
                    crate::design::collections::ListItem {
                        label: if is_heading {
                            k.to_string()
                        } else {
                            truncate(k, 26)
                        },
                        detail: (!is_heading).then(|| truncate(v, 90)),
                        tone,
                        id: "",
                    }
                },
                inner,
                Some(crate::design::states::State::empty(
                    "no rows match the filter",
                )),
            );
        }
        View::Settings => {
            let rows = settings_rows(app);
            let header = Some(settings_header(t, app));
            render_list(app, frame, header, rows, None, inner, None);
        }
        // Chat owns a multi-pane layout (stream, dashboard, composer, and an
        // inline approval card) that does not fit this list-shaped dispatch, so
        // `draw` routes it elsewhere.
        //
        // This used to be `unreachable!()`. A render-time panic aborts the process,
        // and `run_agent` only restores the terminal *after* the loop returns — so
        // one future caller reaching this arm would leave the operator with a
        // wrecked terminal and no log. Rendering an explicit state is the same
        // information with none of the blast radius.
        View::Chat => {
            let state = design::states::for_collection(
                true,
                Some("chat is rendered by the chat layout, not the list dispatcher"),
                app.stream.len(),
                "message",
            );
            let mut hits = app.hits.borrow_mut();
            state.render(frame, inner, t, &mut hits);
        }
    }
}

/// Settings rows: every line reads the LIVE runtime/config — a field the
/// config does not carry says so instead of guessing.
fn settings_rows(app: &AgentApp) -> Vec<crate::design::collections::ListItem<'static>> {
    let cfg = &app.runtime.config;
    let rt = &cfg.runtime;
    let unknown = |k: &str| format!("{k} UNKNOWN (not configured)");
    let mut rows = vec![
        settings_heading(" system"),
        settings_kv("version", format!("openrt v{}", env!("CARGO_PKG_VERSION"))),
        settings_kv("workspace", app.files_root.display().to_string()),
        settings_kv(
            "workspace dir",
            rt.workspace_dir
                .as_ref()
                .map(|p| p.display().to_string())
                .as_deref()
                .unwrap_or("(detected current directory)"),
        ),
        settings_heading(" agent"),
        settings_kv("mode", format!("agent ({} mode)", app.exec_mode)),
        settings_kv("max turns", rt.max_turns.to_string()),
        settings_kv("max tool calls", rt.max_tool_calls.to_string()),
        settings_kv("model timeout", format!("{}s", rt.model_timeout_secs)),
        settings_kv("default provider", rt.default_provider.clone()),
        settings_kv(
            "default model",
            app.runtime
                .provider
                .info()
                .default_model
                .as_deref()
                .filter(|m| !m.is_empty())
                .unwrap_or(&unknown("default model")),
        ),
        settings_kv(
            "override",
            app.switcher
                .as_ref()
                .and_then(|s| s.selected())
                .unwrap_or_else(|| "(none — provider default in use)".to_string()),
        ),
        settings_heading(" execution"),
        settings_kv(
            "terminal timeout",
            rt.terminal_timeout_secs
                .map(|s| format!("{s}s"))
                .unwrap_or_else(|| "30s (default)".into()),
        ),
        settings_kv(
            "terminal out cap",
            rt.terminal_max_output_bytes
                .map(|b| format!("{b} B"))
                .unwrap_or_else(|| "65536 B (default)".into()),
        ),
        settings_kv(
            "python sandbox",
            if rt.python_sandbox.is_some() {
                "configured"
            } else {
                "default profile"
            },
        ),
        settings_kv(
            "computer backend",
            rt.computer_backend
                .as_deref()
                .unwrap_or("auto (discovered)"),
        ),
        settings_kv(
            "screen backend",
            rt.screen_backend.as_deref().unwrap_or("(provider default)"),
        ),
        settings_heading(" security"),
        settings_kv("operating mode", app.exec_mode.clone()),
        settings_kv(
            "network",
            if app.net_allowed {
                "allowed (policy-governed)"
            } else {
                "blocked by mode"
            },
        ),
        settings_kv("policy rules", cfg.permissions.rules.len().to_string()),
        settings_note("  security policy editing lives in SECURITY (audited appends)"),
        settings_heading(" keys"),
        settings_note("  Ctrl+K palette · Ctrl+P files · Ctrl+M model · Ctrl+T terminal"),
        settings_note("  Ctrl+G git · Ctrl+, settings · Ctrl+O mode · Ctrl+W workbench"),
        settings_note("  ↑/↓ PgUp/PgDn Home/End scroll · Ctrl+F filter · F1 help"),
    ];
    rows.extend(theme_rows(app));
    rows
}

/// Settings header: names the section under the cursor so Tab/Shift+Tab has a
/// visible effect, and states plainly that the rest of the view is read-only.
fn settings_header(t: &Tokens, app: &AgentApp) -> Line<'static> {
    use crate::agent::state::SettingsFocus;
    let (label, hint) = match app.settings_focus {
        SettingsFocus::Info => (
            "INFO",
            "read-only runtime configuration — real values, never invented",
        ),
        SettingsFocus::Theme => (
            "THEME",
            "↑/↓ preview · Enter commit + persist · Esc cancel · Tab section",
        ),
    };
    let focus_style = if app.settings_focus == SettingsFocus::Theme {
        Style::default()
            .fg(t.accent.primary)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default().fg(t.text.secondary)
    };
    Line::from(vec![
        Span::styled(format!(" {} ", label), focus_style),
        Span::styled(format!(" {hint}"), Style::default().fg(t.text.muted)),
    ])
}

/// The theme picker: one row per theme, with the cursor, the live preview and
/// the committed theme all distinguishable without relying on color alone
/// (marker, `LIVE` / `COMMITTED` text, and BOLD).
fn theme_rows(app: &AgentApp) -> Vec<crate::design::collections::ListItem<'static>> {
    use crate::agent::state::SettingsFocus;
    let all = app.theme_rows();
    let active = app.active_theme().kind;
    let committed = app.committed_theme().kind;
    let focused = app.settings_focus == SettingsFocus::Theme;
    let previewing = app.is_previewing_theme();
    let path = app
        .workbench
        .theme_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(no writable config directory)".to_string());

    let mut rows = vec![settings_heading(" appearance")];

    for (i, kind) in all.iter().enumerate() {
        let sel = i == app.theme_sel;
        let is_active = *kind == active;
        let is_committed = *kind == committed;
        let marker = if sel { g("› ", "> ") } else { "  " };
        let state = match (is_active, is_committed) {
            (true, true) => "  LIVE · COMMITTED",
            (true, false) => "  LIVE (preview)",
            (false, true) => "  COMMITTED",
            (false, false) => "",
        };
        let desc = kind.blurb();
        rows.push(crate::design::collections::ListItem {
            label: format!("{marker}{:<14}", kind.label()),
            // Live and committed are both spelled out, so the distinction never
            // depends on the accent colour alone.
            detail: Some(format!("{desc}{state}")),
            tone: is_active.then_some(crate::design::collections::RowTone::Success),
            id: "",
        });
    }

    rows.push(settings_note(if focused {
        "↑/↓ preview · Enter commit · Esc cancel · Tab info"
    } else {
        "Tab to THEME to preview a theme"
    }));
    rows.push(settings_note(format!(
        "preference: {} · {}",
        path,
        if previewing {
            "unsaved preview"
        } else {
            "committed"
        }
    )));
    rows
}

// ----------------------------------------------------------------------
// overlays
// ----------------------------------------------------------------------

/// Operating-mode control (operator-only): shows both modes with the current
/// one marked; Enter proposes the OTHER mode through the confirm dialog (the
/// transition itself runs through the ONE ModeManager — audited, idempotent).
fn draw_mode_overlay(t: &Tokens, frame: &mut Frame, app: &AgentApp, layout: &ShellLayout) {
    let rect = dialog(layout, t, 58, 8);
    frame.render_widget(Clear, rect);
    let block = overlay_block(t, " operating mode ");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let other = if app.exec_mode == "sovereign" {
        "provisioning"
    } else {
        "sovereign"
    };
    let mode_row = |label: &'static str, net: &'static str, active: bool| {
        let (marker, style) = if active {
            (
                g("› ", "> "),
                Style::default()
                    .fg(t.accent.primary)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            ("  ", Style::default().fg(t.text.secondary))
        };
        Line::from(vec![
            Span::styled(marker, style),
            Span::styled(format!("{:<13}", label), style),
            Span::styled(net, Style::default().fg(t.text.muted)),
            if active {
                Span::styled("  (current)", Style::default().fg(t.status.success))
            } else {
                Span::raw("")
            },
        ])
    };
    let lines = vec![
        Line::from(Span::styled(
            " operator-only — the agent/model cannot change modes",
            Style::default().fg(t.text.muted),
        )),
        Line::from(""),
        mode_row(
            "SOVEREIGN",
            "network off — external access blocked",
            app.exec_mode == "sovereign",
        ),
        mode_row(
            "PROVISIONING",
            "network on — policy-governed egress",
            app.exec_mode == "provisioning",
        ),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                " Enter ",
                Style::default()
                    .fg(t.accent.primary)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("switch → {other} (confirm)   "),
                Style::default().fg(t.text.muted),
            ),
            Span::styled("Esc", Style::default().fg(t.status.danger)),
            Span::styled(" close", Style::default().fg(t.text.muted)),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Confirmation box for destructive/operator actions (session delete,
/// policy presets, custom rule appends).
fn draw_confirm(t: &Tokens, frame: &mut Frame, app: &AgentApp, layout: &ShellLayout) {
    let Some(confirm) = &app.confirm else {
        return;
    };
    let (title, body, action): (String, Vec<Line>, String) = match confirm {
        crate::agent::state::Confirm::DeleteSession { id, title } => (
            " confirm · destructive ".into(),
            vec![
                Line::from(Span::styled(
                    " Delete this session?",
                    Style::default()
                        .fg(t.status.warning)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                line_kv(t, "title", title),
                line_kv(t, "id", &id.to_string()),
                Line::from(""),
                Line::from(Span::styled(
                    " removes the persisted transcript from the store.",
                    Style::default().fg(t.text.secondary),
                )),
                Line::from(Span::styled(
                    " task history and audit rows are kept.",
                    Style::default().fg(t.text.muted),
                )),
            ],
            "delete".to_string(),
        ),
        crate::agent::state::Confirm::PolicyPreset { label, full } => {
            let rules = app.preset_rules(*full);
            let mut body = vec![
                Line::from(Span::styled(
                    format!(" Apply policy preset `{label}`?"),
                    Style::default()
                        .fg(t.status.warning)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                line_kv(t, "preset", label),
                line_kv(
                    t,
                    "rules",
                    &format!("{} allow rule(s) appended", rules.len()),
                ),
                line_kv(
                    t,
                    "source",
                    if *full {
                        "every registered tool id + capability"
                    } else {
                        "read-shaped capability/tool names (substring heuristic)"
                    },
                ),
                Line::from(""),
            ];
            for r in rules.iter().take(8) {
                body.push(Line::from(Span::styled(
                    format!("  + {r}"),
                    Style::default().fg(t.text.secondary),
                )));
            }
            if rules.len() > 8 {
                body.push(Line::from(Span::styled(
                    format!("  {} +{} more", g("…", "..."), rules.len() - 8),
                    Style::default().fg(t.text.muted),
                )));
            }
            body.push(Line::from(Span::styled(
                " append-only + audited; the live gate switches at the next build.",
                Style::default().fg(t.text.muted),
            )));
            (
                format!(" confirm · preset · {label} "),
                body,
                "apply".to_string(),
            )
        }
        crate::agent::state::Confirm::Mode { to } => (
            " confirm · operating mode ".into(),
            vec![
                Line::from(Span::styled(
                    format!(" Switch operating mode → {to}?"),
                    Style::default()
                        .fg(t.status.warning)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                line_kv(t, "from", &app.exec_mode),
                line_kv(t, "to", to),
                Line::from(""),
                Line::from(Span::styled(
                    " changes the egress posture of every scope (audited).",
                    Style::default().fg(t.text.secondary),
                )),
                Line::from(Span::styled(
                    " operator action — the agent/model cannot do this.",
                    Style::default().fg(t.text.muted),
                )),
            ],
            "switch".to_string(),
        ),
        crate::agent::state::Confirm::PolicyAppend {
            pattern,
            action,
            scope,
        } => (
            " confirm · policy rule ".into(),
            vec![
                Line::from(Span::styled(
                    " Append this rule?",
                    Style::default()
                        .fg(t.status.warning)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                line_kv(t, "pattern", pattern),
                line_kv(t, "action", action),
                line_kv(t, "scope", scope.as_deref().unwrap_or("*")),
                Line::from(""),
                Line::from(Span::styled(
                    " creates a new immutable policy revision + audit row.",
                    Style::default().fg(t.text.secondary),
                )),
                Line::from(Span::styled(
                    " the live gate keeps its current policy until the next build.",
                    Style::default().fg(t.text.muted),
                )),
            ],
            "append".to_string(),
        ),
    };
    // The confirm dialog sizes to its own message, and uses the shared
    // destructive dialog language (warning border, single affirmative key) so
    // a destructive prompt is recognizable wherever it appears.
    let want_rows = u16::try_from(body.len() + 2).unwrap_or(u16::MAX);
    let rect = dialog(layout, t, 70, want_rows);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t.borders.destructive))
        .style(Style::default().bg(t.surfaces.overlay))
        .title(Line::styled(
            title.clone(),
            Style::default()
                .fg(t.status.warning)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let mut lines = body;
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(
            " y ",
            Style::default()
                .fg(t.accent.primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{action}   ·   any other key cancels", action = action),
            Style::default().fg(t.text.muted),
        ),
    ]));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn overlay_block(t: &Tokens, title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t.accent.primary))
        .style(Style::default().bg(t.surfaces.surface))
        .title(title.to_string())
}

fn draw_help(t: &Tokens, frame: &mut Frame, app: &AgentApp, layout: &ShellLayout) {
    let lines = vec![
        row(t, "Enter", "send message / run view action"),
        row(t, "Esc", "clear input · cancel task · close overlay"),
        row(t, "Ctrl+W", "toggle Agent ↔ Workbench"),
        row(t, "Ctrl+K", "command palette (jump to view, fuzzy)"),
        row(t, "Ctrl+P", "Files view · Ctrl+F filter toggle"),
        row(t, "Ctrl+M", "model overlay (per-request switch)"),
        row(t, "Ctrl+T", "Terminal view"),
        row(t, "Ctrl+G", "Git view"),
        row(t, "Ctrl+,", "Settings view (read-only config)"),
        row(t, "Ctrl+O", "operating mode (operator-only switch)"),
        row(t, "Ctrl+←/→", "previous / next view"),
        row(t, "Ctrl+R", "refresh current view from source"),
        row(t, "Ctrl+D", "git diff of selected file (Files)"),
        row(t, "Ctrl+E", "expand latest tool card — full args (Chat)"),
        row(
            t,
            "Ctrl+Q",
            "quit anyway (denies/cancels first, drops unsent text)",
        ),
        // Ctrl+C is a layered interrupt, not a hard quit: it denies a pending
        // permission, cancels a confirmation, closes an overlay, cancels a
        // running task, and quits only once nothing is left to stop. Saying
        // "quit" here was a lie a live terminal found — with the help card
        // open, Ctrl+C dismissed the card and kept running.
        row(t, "Ctrl+C", "interrupt: deny / cancel / close, then quit"),
        row(t, "F1 / ?", "this help"),
        row(t, "↑/↓", "scroll chat · move list selection"),
        row(
            t,
            "PgUp/PgDn/Home/End",
            "page or jump selection · scroll chat/terminal",
        ),
        row(
            t,
            "Ctrl+F",
            "filter current list (Files/Tools/Tasks/Sessions/MCP/Exts/Security)",
        ),
        row(
            t,
            "1 / 2 / 3 / 4",
            "approval: allow once · session · workspace · deny (Esc/Enter = deny)",
        ),
        row(t, "y / n", "answer an approval (while pending)"),
        row(
            t,
            "d",
            "delete selected session — confirms first (Sessions)",
        ),
        Line::from(""),
        Line::from(Span::styled(
            " slash commands",
            Style::default()
                .fg(t.accent.primary)
                .add_modifier(Modifier::BOLD),
        )),
        row(t, "/help", "this help"),
        row(t, "/clear", "clear the stream (view only)"),
        row(t, "/new", "new session"),
        row(t, "/sessions", "session switcher"),
        row(t, "/plan <goal>", "compile a real workflow plan"),
        row(t, "/run", "execute the compiled plan"),
        row(t, "/cancel", "cancel the running task"),
        row(t, "/model", "model overlay (per-request switch)"),
        row(t, "/setup", "setup wizard (host/provider/models)"),
        row(t, "/status", "session/task/mode status"),
        row(t, "/debug model-response", "last response body (redacted)"),
        row(t, "/refresh", "reload current view"),
        row(t, "/workbench", "switch to Workbench mode"),
        row(t, "/quit", "exit"),
        Line::from(Span::styled(
            " ↑/↓ · PgUp/PgDn scroll · Esc closes",
            Style::default().fg(t.text.muted),
        )),
    ];
    // Content-sized box (capped by the terminal) + scroll: the key table
    // outgrows a 80x24 screen, so it pages instead of clipping.
    let w = 64u16;
    // Content-sized and layout-clamped. The key table outgrows 80x24, so the
    // dialog takes what fits and the remainder is reached with PgUp/PgDn via
    // `help_scroll` rather than being clipped away.
    let rect = dialog(layout, t, w, u16::try_from(lines.len()).unwrap_or(u16::MAX));
    frame.render_widget(Clear, rect);
    let block = overlay_block(t, " help · AGENT TUI ");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    frame.render_widget(Paragraph::new(lines).scroll((app.help_scroll, 0)), inner);
}

fn row(t: &Tokens, key: &str, desc: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {key:<12}"), Style::default().fg(t.status.warning)),
        Span::styled(desc.to_string(), Style::default().fg(t.text.secondary)),
    ])
}

fn draw_palette(t: &Tokens, frame: &mut Frame, app: &AgentApp, layout: &ShellLayout) {
    let matches = app.palette_matches();
    let items = app.palette_items();
    let mut lines = vec![
        Line::from(Span::styled(
            format!(" {}_", app.palette),
            Style::default().fg(t.text.muted),
        )),
        Line::from(""),
    ];
    for (i, m) in matches.iter().enumerate() {
        let label = items.get(*m).cloned().unwrap_or_default();
        let style = if i == app.palette_sel {
            Style::default()
                .fg(t.accent.primary)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(t.text.secondary)
        };
        lines.push(Line::from(Span::styled(format!("  {label}"), style)));
    }
    if matches.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no match",
            Style::default().fg(t.text.muted),
        )));
    }
    // Content-sized: the dialog is exactly as tall as the query plus its
    // matches, so a one-result palette is a small dialog rather than a large
    // mostly-empty box.
    let rect = dialog(
        layout,
        t,
        52,
        u16::try_from(lines.len()).unwrap_or(u16::MAX),
    );
    frame.render_widget(Clear, rect);
    let block = overlay_block(t, " jump to ");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    frame.render_widget(Paragraph::new(lines), inner);

    // Register the scrim first, then the rows on top of it: `HitTargets` is
    // topmost-wins, so a row rectangle overrides the scrim it sits inside and a
    // click beside the dialog still falls to the scrim.
    let mut hits = app.hits.borrow_mut();
    hits.register(rect, crate::design::HitTarget::ModalScrim);
    // Row 0 and 1 are the query echo and its blank spacer; matches start at 2.
    for (i, _) in matches.iter().enumerate() {
        let y = inner
            .y
            .saturating_add(2)
            .saturating_add(u16::try_from(i).unwrap_or(u16::MAX));
        if y >= inner.y.saturating_add(inner.height) {
            // The dialog is shorter than its own content: do not register rows
            // that were never painted, or a click below the frame would act on
            // an entry the operator cannot see.
            break;
        }
        let row = Rect {
            x: inner.x,
            y,
            width: inner.width,
            height: 1,
        };
        hits.register(row, crate::design::HitTarget::PaletteItem { index: i });
    }
}

fn draw_model(t: &Tokens, frame: &mut Frame, app: &AgentApp, layout: &ShellLayout) {
    let info = app.runtime.provider.info();
    let selected = app
        .switcher
        .as_ref()
        .and_then(|s| s.selected())
        .unwrap_or_else(|| "(provider default)".into());
    let lines = vec![
        line_kv(t, "provider", app.runtime.provider.id()),
        line_kv(t, "kind", &format!("{:?}", info.kind)),
        line_kv(t, "display", &info.display_name),
        line_kv(
            t,
            "default",
            info.default_model.as_deref().unwrap_or("(none)"),
        ),
        line_kv(t, "selected", &selected),
        Line::from(""),
        Line::from(format!(" > {}_", app.model_input)),
        Line::from(""),
        Line::from(Span::styled(
            " provider tag (e.g. llama3.1:8b) · Enter applies · Esc",
            Style::default().fg(t.text.muted),
        )),
    ];
    let rect = dialog(
        layout,
        t,
        68,
        u16::try_from(lines.len()).unwrap_or(u16::MAX),
    );
    frame.render_widget(Clear, rect);
    let block = overlay_block(t, " model · per-request switch ");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_sessions(t: &Tokens, frame: &mut Frame, app: &AgentApp, layout: &ShellLayout) {
    // Dialog width is decided first so rows can be fitted to it; a fixed
    // `{:<44}` title column overflowed a narrow terminal and pushed the message
    // count off the right edge.
    let rect = dialog(layout, t, 66, 0);
    let body_w = saturating_sub(rect.width, 2);
    // 1 marker + 1 space, then the title, then the count.
    let count_w = 8u16;
    let title_w = saturating_sub(saturating_sub(body_w, 2), count_w);
    // The row count is known from the collection itself, so the dialog can be
    // sized before a single row is built. This loop used to build every session
    // row first and size the dialog from `rows.len()`, which meant a long session
    // history cost one `format!` per session per frame for a dialog that cannot
    // show them all anyway.
    let total = usize::from(!app.sessions.is_empty())
        + app
            .sessions
            .len()
            .saturating_sub(usize::from(!app.sessions.is_empty()));
    let total = total.max(1);
    let want_rows = u16::try_from(total).unwrap_or(u16::MAX).saturating_add(3);
    let rect = dialog(layout, t, 66, want_rows);
    frame.render_widget(Clear, rect);
    let block = overlay_block(t, " sessions ");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    // Pinned footer; the list above it follows the selection.
    let footer_h = 2u16;
    let list_h = inner.height.saturating_sub(footer_h);
    let (off, count) =
        design::collections::list_window(total, Some(app.sel.min(total - 1)), list_h, false);
    #[cfg(test)]
    ROWS_MATERIALISED.with(|c| c.set(c.get() + count));
    let mut view: Vec<Line> = Vec::with_capacity(count);
    if app.sessions.is_empty() {
        view.push(Line::from(Span::styled(
            " no sessions yet",
            Style::default().fg(t.text.muted),
        )));
    } else {
        for (i, s) in app.sessions.iter().enumerate().skip(off).take(count) {
            let style = if i == app.sel {
                Style::default()
                    .fg(t.accent.primary)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t.text.secondary)
            };
            let cur = app.current_session == Some(s.id);
            view.push(Line::from(vec![
                Span::styled(
                    format!(" {} ", if cur { g("●", "*") } else { g("○", "o") }),
                    style,
                ),
                Span::styled(pad_to(&sanitize_text(&s.title), title_w), style),
                Span::styled(
                    format!("{} msgs", s.content.len()),
                    Style::default().fg(t.text.muted),
                ),
            ]));
        }
    }
    let view: Vec<Line> = view.into_iter().map(ascii_line).collect();
    frame.render_widget(
        Paragraph::new(view),
        Rect {
            height: list_h,
            ..inner
        },
    );
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                " ↑/↓ select · Enter switch · d delete · Esc close",
                Style::default().fg(t.text.muted),
            )),
        ]),
        Rect {
            y: inner.y + list_h,
            height: footer_h,
            ..inner
        },
    );
}

fn draw_preview(t: &Tokens, frame: &mut Frame, app: &AgentApp, layout: &ShellLayout) {
    let rect = dialog(layout, t, 84, 18);
    frame.render_widget(Clear, rect);
    let title = app
        .preview
        .as_ref()
        .map(|p| format!(" {} ", sanitize_text(&p.title)))
        .unwrap_or_else(|| " preview ".into());
    let block = overlay_block(t, &title);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    // The Preview overlay opens on audit chains, diffs and file contents, all of
    // which grow without bound — so it cannot hand the whole set to `Paragraph`,
    // which needs every line to decide what to scroll to. The window comes from the
    // shared `render_scrolled_windowed` clamp.
    let total = app.preview.as_ref().map(|p| p.lines.len()).unwrap_or(0);
    let empty: Vec<String> = Vec::new();
    let all = app.preview.as_ref().map(|p| &p.lines).unwrap_or(&empty);
    render_scrolled_windowed(frame, total, app.preview_scroll, inner, |i| {
        Line::from(Span::styled(
            all[i].clone(),
            Style::default().fg(t.text.secondary),
        ))
    });
}

// ----------------------------------------------------------------------
// small helpers
// ----------------------------------------------------------------------

/// A resting main-surface panel.
///
/// Delegates to [`design::widgets::panel`] so there is exactly one definition of
/// border colour, title styling and surface elevation. Titles are TEXT: a thin
/// decorative border colour must never double as low-contrast body text.
fn block(t: &Tokens, title: &str) -> Block<'static> {
    design::widgets::panel(t, title, design::widgets::PanelTone::Default)
}

pub(crate) fn format_size(n: u64) -> String {
    if n < 1024 {
        format!("{n}B")
    } else if n < 1024 * 1024 {
        format!("{:.1}K", n as f64 / 1024.0)
    } else {
        format!("{:.1}M", n as f64 / (1024.0 * 1024.0))
    }
}

fn truncate(s: &str, n: usize) -> String {
    design::text::truncate_width(
        s,
        u16::try_from(n).unwrap_or(u16::MAX),
        design::text::Truncation::Head,
    )
}

/// Pinned header lines (counts/keys/filter state) + a selection-following
/// list beneath them. One physical row per entry — never wraps.
/// Accepts `Option<Line>` (single header) or `Vec<Line>` (multi-line header).
/// Semantic badge for a tool call's state.
///
/// One mapping for the whole app. Previously the same `ToolState` match was
/// written out separately in the chat stream and in the tool-card header, so the
/// two could disagree about what "denied" looks like.
fn tool_state_badge(state: &ToolState) -> design::widgets::Badge {
    use design::widgets::{Badge, BadgeTone};
    match state {
        ToolState::Running { since } => Badge::new(
            format!("running {}s", since.elapsed().as_secs()),
            BadgeTone::Running,
        ),
        ToolState::Done {
            ok: true, took_ms, ..
        } => Badge::new(format!("ok · {took_ms}ms"), BadgeTone::Success),
        ToolState::Done {
            ok: false,
            detail,
            took_ms,
        } => Badge::new(format!("{detail} · {took_ms}ms"), BadgeTone::Danger),
        ToolState::Denied { reason, .. } => {
            Badge::new(format!("denied · {reason}"), BadgeTone::Blocked)
        }
    }
}

/// Semantic tone for a plan step, shared by the plan card and the plan overlay
/// so a step cannot read as running in one place and pending in another.
fn plan_status_tone(status: &crate::agent::state::PlanStatus) -> design::widgets::BadgeTone {
    use crate::agent::state::PlanStatus as P;
    use design::widgets::BadgeTone;
    match status {
        P::Done => BadgeTone::Success,
        P::Running => BadgeTone::Running,
        P::Failed => BadgeTone::Danger,
        P::Pending => BadgeTone::Pending,
    }
}

/// The identity badge for a transcript entry.
///
/// Role is carried by a word and a tone, never by position or colour alone.
fn role_badge(role: &str, tone: design::widgets::BadgeTone) -> design::widgets::Badge {
    design::widgets::Badge::new(role, tone)
}

/// Semantic tone for a task state.
///
/// One mapping for the whole app, so `Failed` cannot read as `Running` in one
/// view and as `Cancelled` in another.
fn task_status_tone(status: openrt_core::task::TaskStatus) -> crate::design::collections::RowTone {
    use crate::design::collections::RowTone;
    use openrt_core::task::TaskStatus as S;
    match status {
        S::Created | S::Queued => RowTone::Muted,
        S::Running | S::Waiting => RowTone::Info,
        S::Completed => RowTone::Success,
        S::Failed | S::Interrupted => RowTone::Danger,
        S::Cancelled => RowTone::Warning,
        S::Paused => RowTone::Muted,
    }
}

/// Draw a selectable list inside a panel the caller has already chrome'd.
///
/// Views declare *what* each row is — a label, an optional right-aligned detail,
/// an optional semantic tone — and this delegates to
/// [`crate::design::collections::draw_list_rows`], so selection styling,
/// windowing, displacement hints, ASCII folding, truncation and hit registration
/// exist in exactly one place. A view that hand-styles its own `Span`s cannot be
/// kept consistent with the rest of the app, and was the reason 43 raw
/// `Color::Gray` sites existed.
fn render_list<'a, H>(
    app: &AgentApp,
    frame: &mut Frame,
    header: H,
    items: Vec<crate::design::collections::ListItem<'a>>,
    sel_pos: Option<usize>,
    inner: Rect,
    empty: Option<crate::design::states::State>,
) where
    H: IntoIterator<Item = Line<'static>>,
{
    let header: Vec<Line> = header.into_iter().map(ascii_line).collect();
    let header_h = (header.len() as u16).min(inner.height);
    if header_h > 0 {
        frame.render_widget(
            Paragraph::new(header),
            Rect {
                height: header_h,
                ..inner
            },
        );
    }
    let list_h = inner.height.saturating_sub(header_h);
    if list_h == 0 {
        return;
    }
    let list_rect = Rect {
        y: inner.y + header_h,
        height: list_h,
        ..inner
    };
    // An empty collection is a *state*, not a zero-height list. Rendering it
    // through the shared state component is what makes "nothing here", "failed",
    // and "still loading" look and read the same in every view, instead of each
    // view inventing its own grey sentence.
    if items.is_empty() {
        if let Some(state) = empty {
            state.render(frame, list_rect, &app.tokens(), &mut app.hits.borrow_mut());
        }
        return;
    }
    let view = crate::design::collections::ListView {
        title: "",
        items,
        cursor: sel_pos,
        show_displacement: true,
        total: None,
        offset: 0,
    };
    crate::design::collections::draw_list_rows(
        &app.tokens(),
        frame,
        list_rect,
        &view,
        &mut app.hits.borrow_mut(),
    );
}

// Test-only counter: model search-result rows materialised this frame.
//
// The model browser is the surface a broad search makes worst: one row per hit,
// five `format!` allocations each. This makes "bounded" measurable.
#[cfg(test)]
thread_local! {
    static MODEL_ROWS_BUILT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_model_rows_built() {
    MODEL_ROWS_BUILT.with(|c| c.set(0));
}

#[cfg(test)]
pub(crate) fn model_rows_built() -> usize {
    MODEL_ROWS_BUILT.with(std::cell::Cell::get)
}

// Test-only counter: list rows materialised this frame.
//
// The companion to [`MESSAGES_MATERIALISED`]. Together they let a test assert
// that per-frame work tracks the viewport rather than the collection, which no
// output assertion can establish.
#[cfg(test)]
thread_local! {
    static ROWS_MATERIALISED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_rows_materialised() {
    ROWS_MATERIALISED.with(|c| c.set(0));
}

#[cfg(test)]
pub(crate) fn rows_materialised() -> usize {
    ROWS_MATERIALISED.with(std::cell::Cell::get)
}

/// Like [`render_list`], but materialises only the rows that fit on screen.
///
/// [`render_list`] needs every row up front because the caller owns the filter
/// and the per-row mapping. That is the Phase 8 defect: a session list of
/// 10,000 entries built 10,000 `ListItem`s — each with its own label and detail
/// `String` — for a viewport showing about twenty, on *every frame*. The
/// allocation churn was proportional to the user's archive, not to their screen.
///
/// This entry point owns the window instead. `row_for` is called at most once per
/// visible row, and the collection is only ever *indexed*, never copied. The
/// window comes from [`crate::design::collections::list_window`], the same
/// function the painter uses, so the two cannot disagree about where the window
/// starts or how many rows are hidden.
///
/// `row_for` is called with indices in `[start, start + count)`, so callers that
/// filter must map that index space back onto their own collection.
fn render_windowed_list<'a, H>(
    app: &AgentApp,
    frame: &mut Frame,
    header: H,
    total: usize,
    cursor: Option<usize>,
    mut row_for: impl FnMut(usize) -> crate::design::collections::ListItem<'a>,
    inner: Rect,
    empty: Option<crate::design::states::State>,
) where
    H: IntoIterator<Item = Line<'static>>,
{
    let header: Vec<Line> = header.into_iter().map(ascii_line).collect();
    let header_h = (header.len() as u16).min(inner.height);
    if header_h > 0 {
        frame.render_widget(
            Paragraph::new(header),
            Rect {
                height: header_h,
                ..inner
            },
        );
    }
    let list_rect = Rect {
        y: inner.y.saturating_add(header_h),
        height: inner.height.saturating_sub(header_h),
        ..inner
    };

    let (start, count) =
        crate::design::collections::list_window(total, cursor, list_rect.height, true);
    if count == 0 {
        if let Some(state) = empty {
            state.render(frame, list_rect, &app.tokens(), &mut app.hits.borrow_mut());
        }
        return;
    }
    #[cfg(test)]
    ROWS_MATERIALISED.with(|c| c.set(c.get() + count));
    // The only per-row allocation in the whole path, and it is bounded by the
    // viewport rather than by the collection.
    let mut items: Vec<crate::design::collections::ListItem<'a>> = Vec::with_capacity(count);
    for i in start..start.saturating_add(count) {
        items.push(row_for(i));
    }

    let view = crate::design::collections::ListView {
        title: "",
        items,
        cursor,
        show_displacement: true,
        total: Some(total),
        offset: start,
    };
    crate::design::collections::draw_list_rows(
        &app.tokens(),
        frame,
        list_rect,
        &view,
        &mut app.hits.borrow_mut(),
    );
}

/// [`render_windowed_list`] for a collection behind a filter.
///
/// The filtered variant used to call `app.filtered_*()`, which returns a
/// `Vec<usize>` of every matching index — so a 10,000-row session list rebuilt a
/// 10,000-element vector on every repaint just to know which ~40 rows to draw.
/// The window is now taken over the *filtered sequence* directly, by
/// [`crate::design::collections::FilteredWindow`], and the painter is the same one.
///
/// `matches` is asked about a source index; `row_for` is called with source
/// indices too, so the two agree by construction rather than by the caller
/// remembering to map between coordinate spaces.
fn render_windowed_filtered_list<'a, H>(
    app: &AgentApp,
    frame: &mut Frame,
    header: H,
    total: usize,
    matches: &dyn Fn(usize) -> bool,
    cursor: usize,
    mut row_for: impl FnMut(usize) -> crate::design::collections::ListItem<'a>,
    inner: Rect,
    empty: Option<crate::design::states::State>,
) where
    H: IntoIterator<Item = Line<'static>>,
{
    let header: Vec<Line> = header.into_iter().map(ascii_line).collect();
    let header_h = (header.len() as u16).min(inner.height);
    if header_h > 0 {
        frame.render_widget(
            Paragraph::new(header),
            Rect {
                height: header_h,
                ..inner
            },
        );
    }
    let list_rect = Rect {
        y: inner.y.saturating_add(header_h),
        height: inner.height.saturating_sub(header_h),
        ..inner
    };
    let win = crate::design::collections::FilteredWindow::new(
        total,
        matches,
        cursor,
        list_rect.height,
        // The list painter always reserves rows for the displacement markers.
        true,
    );
    if win.total() == 0 {
        if let Some(state) = empty {
            state.render(frame, list_rect, &app.tokens(), &mut app.hits.borrow_mut());
        }
        return;
    }
    #[cfg(test)]
    ROWS_MATERIALISED.with(|c| c.set(c.get() + win.count()));
    let mut items: Vec<crate::design::collections::ListItem<'a>> = Vec::with_capacity(win.count());
    for i in win.indices() {
        items.push(row_for(i));
    }
    // `offset`, `total` and `cursor` are all in filtered coordinates, which is
    // exactly what the painter and the hit registration expect — so the row that
    // gets a hit target is the row that was painted, and the `↑N`/`↓N` counts are
    // derived from the number of *survivors*, not of rows.
    let view = crate::design::collections::ListView {
        title: "",
        items,
        cursor: Some(win.cursor()),
        show_displacement: true,
        total: Some(win.total()),
        offset: win.start(),
    };
    crate::design::collections::draw_list_rows(
        &app.tokens(),
        frame,
        list_rect,
        &view,
        &mut app.hits.borrow_mut(),
    );
}

/// Draw a table inside a panel the caller has already chrome'd.
///
/// The counterpart to [`render_list`]: views declare columns and cell text, and
/// column widths, padding, truncation and hit registration are decided once, in
/// [`crate::design::collections::draw_table_rows`].
fn render_table<'a, H>(
    app: &AgentApp,
    frame: &mut Frame,
    header: H,
    table: &crate::design::collections::TableView<'a>,
    inner: Rect,
) where
    H: IntoIterator<Item = Line<'static>>,
{
    render_table_chrome(app, frame, header, inner, |app, frame, body| {
        crate::design::collections::draw_table_rows(
            &app.tokens(),
            frame,
            body,
            table,
            &mut app.hits.borrow_mut(),
        );
    });
}

/// [`render_table`] for a collection behind a filter.
///
/// Same arrangement as [`render_windowed_filtered_list`]: the window is taken over
/// the filtered sequence so the repaint never allocates a `usize` per matching row.
/// The surviving indices are collected once, bounded by the viewport, and the cells
/// are built from them lazily — the collection is still never fully formatted.
fn render_table_filtered<H>(
    app: &AgentApp,
    frame: &mut Frame,
    header: H,
    total: usize,
    matches: &dyn Fn(usize) -> bool,
    cursor: usize,
    columns: Vec<crate::design::collections::Column>,
    cell_for: impl Fn(usize) -> Vec<String>,
    inner: Rect,
) where
    H: IntoIterator<Item = Line<'static>>,
{
    render_table_chrome(app, frame, header, inner, |app, frame, body| {
        let win = crate::design::collections::FilteredWindow::new(
            total,
            matches,
            cursor,
            body.height,
            false,
        );
        let idx: Vec<usize> = win.indices().collect();
        let table = crate::design::collections::TableView {
            title: "",
            columns,
            rows: crate::design::collections::TableRows::lazy(idx.len(), |i| cell_for(idx[i])),
            // `rows` holds only the window, but `cursor` and every `ListRow` hit
            // index are positions in the whole filtered table, so `offset` carries
            // where the window began and the cursor stays absolute.
            cursor: win.cursor(),
            offset: win.start(),
        };
        crate::design::collections::draw_table_rows(
            &app.tokens(),
            frame,
            body,
            &table,
            &mut app.hits.borrow_mut(),
        );
    });
}

/// Header chrome + body rect, shared by every table so the two table entry points
/// cannot drift on where the body starts.
fn render_table_chrome<H>(
    app: &AgentApp,
    frame: &mut Frame,
    header: H,
    inner: Rect,
    body: impl FnOnce(&AgentApp, &mut Frame, Rect),
) where
    H: IntoIterator<Item = Line<'static>>,
{
    let _ = app;
    let header: Vec<Line> = header.into_iter().map(ascii_line).collect();
    let header_h = (header.len() as u16).min(inner.height);
    if header_h > 0 {
        frame.render_widget(
            Paragraph::new(header),
            Rect {
                height: header_h,
                ..inner
            },
        );
    }
    let body_h = inner.height.saturating_sub(header_h);
    if body_h == 0 {
        return;
    }
    let body_rect = Rect {
        y: inner.y.saturating_add(header_h),
        height: body_h,
        ..inner
    };
    body(app, frame, body_rect);
}

/// A scrollable block for a source too large to materialise.
///
/// An eager scroll helper has to build every line up front, because it works out
/// the scroll clamp from `rows.len()`. A caller that knows the row count can skip
/// the off-screen rows entirely: this takes the total and a line factory, and
/// materialises at most `inner.height` lines — each with its own `format!`
/// allocations — no matter how large the source is.
///
/// The clamp is [`design::collections::window_at`], shared with every other
/// offset-scrolled surface. A local copy of that arithmetic is exactly what
/// Phase 8 forbids.
fn render_scrolled_windowed(
    frame: &mut Frame,
    total: usize,
    scroll: u16,
    inner: Rect,
    mut line_at: impl FnMut(usize) -> Line<'static>,
) {
    let (off, count) = design::collections::window_at(total, usize::from(scroll), inner.height);
    if count == 0 {
        return;
    }
    #[cfg(test)]
    SCROLLED_ROWS_BUILT.with(|c| c.set(c.get() + count));
    let view: Vec<Line> = (off..off + count).map(|i| ascii_line(line_at(i))).collect();
    frame.render_widget(Paragraph::new(view), inner);
}

// Test-only counter: terminal rows materialised this frame.
#[cfg(test)]
thread_local! {
    static TERM_ROWS_BUILT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_term_rows_built() {
    TERM_ROWS_BUILT.with(|c| c.set(0));
}

#[cfg(test)]
pub(crate) fn term_rows_built() -> usize {
    TERM_ROWS_BUILT.with(std::cell::Cell::get)
}

// Test-only counter: lines materialised by [`render_scrolled_windowed`].
#[cfg(test)]
thread_local! {
    static SCROLLED_ROWS_BUILT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_scrolled_rows_built() {
    SCROLLED_ROWS_BUILT.with(|c| c.set(0));
}

#[cfg(test)]
pub(crate) fn scrolled_rows_built() -> usize {
    SCROLLED_ROWS_BUILT.with(std::cell::Cell::get)
}

/// One-line list header carrying counts + filter state.
fn list_header(app: &AgentApp, keys: impl Into<String>) -> String {
    let keys = keys.into();
    if app.filter_active {
        // Counting survivors used to mean materialising a `Vec<&Row>` for the whole
        // collection on every repaint, which is the same defect the row renderer
        // just had: a 10,000-row task list paid 10,000 pointer pushes to print a
        // header. `FilteredWindow` at height 0 walks the predicate and allocates
        // nothing, because its only job here is the total.
        let shown = match app.view {
            View::Tasks => app.count_matching(app.tasks.len(), &|i| app.task_matches(i)),
            View::Sessions => app.count_matching(app.sessions.len(), &|i| app.session_matches(i)),
            _ => app.count_matching(app.tools.len(), &|i| app.tool_matches(i)),
        };
        format!(" filter “{}” ({shown} shown) · {keys}", app.list_filter)
    } else {
        format!(" {keys}")
    }
}

/// First-run setup wizard overlay: SYSTEM → PROVIDER → MODELS → REVIEW.
fn draw_setup(t: &Tokens, frame: &mut Frame, app: &AgentApp, layout: &ShellLayout) {
    let Some(st) = &app.setup else {
        return;
    };
    let rect = dialog(layout, t, 100, 22);
    frame.render_widget(Clear, rect);
    let block = overlay_block(t, " setup · first run ");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if inner.width == 0 || inner.height < 4 {
        return;
    }

    // Step tabs
    let mut tabs: Vec<Span> = Vec::new();
    for (i, s) in crate::agent::setup::SetupStep::ALL.iter().enumerate() {
        let active = *s == st.step;
        let style = if active {
            Style::default()
                .fg(t.accent.primary)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(t.text.muted)
        };
        tabs.push(Span::styled(format!(" {} ", s.title()), style));
        if i + 1 < crate::agent::setup::SetupStep::ALL.len() {
            tabs.push(Span::styled(g("›", ">"), Style::default().fg(t.text.muted)));
        }
    }
    let mut lines = vec![Line::from(tabs), Line::from("")];

    match st.step {
        crate::agent::setup::SetupStep::System => {
            lines.push(Line::from(Span::styled(
                " detected from /proc + statvfs (real, this host)",
                Style::default().fg(t.status.success),
            )));
            for (k, v) in &st.specs {
                lines.push(line_kv(t, k, v));
            }
        }
        crate::agent::setup::SetupStep::Provider => {
            lines.push(Line::from(Span::styled(
                " live provider report (probe = real transport call)",
                Style::default().fg(t.status.success),
            )));
            for (k, v) in &st.provider {
                lines.push(line_kv(t, k, v));
            }
        }
        crate::agent::setup::SetupStep::Models => {
            let hf_line = if st.hf_typing {
                Line::from(vec![
                    Span::styled(" HF query: ", Style::default().fg(t.text.muted)),
                    Span::styled(
                        format!("{}_", st.hf_query),
                        Style::default().fg(t.accent.primary),
                    ),
                    Span::styled(
                        "  Enter search · Esc back",
                        Style::default().fg(t.text.muted),
                    ),
                ])
            } else {
                Line::from(Span::styled(
                    " ↑/↓ select · Enter applies · f HF search (mode-gated)",
                    Style::default().fg(t.text.muted),
                ))
            };
            lines.push(hf_line);
            lines.push(Line::from(Span::styled(
                format!(" {}", st.note),
                Style::default().fg(t.text.muted),
            )));
            lines.push(Line::from(""));
            let body_h = (inner.height as usize).saturating_sub(9).max(1);
            let (off, count) = design::collections::window(
                st.models.len(),
                st.sel,
                u16::try_from(body_h).unwrap_or(u16::MAX),
            );
            #[cfg(test)]
            ROWS_MATERIALISED.with(|c| c.set(c.get() + count));
            for (i, m) in st.models.iter().enumerate().skip(off).take(count) {
                let sel = i == st.sel;
                let base = if sel {
                    Style::default()
                        .fg(t.accent.primary)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(t.text.secondary)
                };
                let marker = if sel { g("›", ">") } else { " " };
                let tag = m.tag.clone().unwrap_or_else(|| "—".into());
                let fit = match m.fit {
                    Some(true) => Span::styled(" fits ", Style::default().fg(t.status.success)),
                    Some(false) => Span::styled(" over ", Style::default().fg(t.status.danger)),
                    None => Span::styled(" size? ", Style::default().fg(t.status.warning)),
                };
                let w = inner.width as usize;
                let fixed = marker.len() + 1 + 30 + 1 + 18 + 1 + 12 + 1 + 7;
                let detail_w = w.saturating_sub(fixed).max(8);
                lines.push(Line::from(vec![
                    Span::styled(format!(" {marker} "), base),
                    Span::styled(format!("{:<30}", truncate(&m.display, 29)), base),
                    Span::styled(format!("{:<18}", truncate(&tag, 17)), base),
                    Span::styled(
                        format!("{:<12}", m.origin),
                        Style::default().fg(t.text.muted),
                    ),
                    fit,
                    Span::styled(
                        truncate(&m.detail, detail_w),
                        Style::default().fg(t.text.muted),
                    ),
                ]));
            }
            if st.models.is_empty() {
                lines.push(Line::from(Span::styled(
                    " no recommendations — registry empty",
                    Style::default().fg(t.text.muted),
                )));
            }
        }
        crate::agent::setup::SetupStep::Review => {
            let picked = st.models.get(st.sel);
            lines.push(Line::from(Span::styled(
                " review — Enter finishes (persists setup completion only; /setup reopens)",
                Style::default().fg(t.status.success),
            )));
            match picked {
                Some(m) => {
                    lines.push(line_kv(t, "model", &m.display));
                    lines.push(line_kv(
                        t,
                        "provider tag",
                        m.tag.as_deref().unwrap_or("(none)"),
                    ));
                    lines.push(line_kv(t, "origin", m.origin));
                    lines.push(line_kv(t, "detail", &m.detail));
                }
                None => lines.push(line_kv(t, "model", "(none selected)")),
            }
            lines.push(line_kv(
                t,
                "native tools",
                &crate::agent::render::native_tools_label(app),
            ));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!(" {}", st.note),
                Style::default().fg(t.text.muted),
            )));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " ↑/↓ select · ←/→ step · Enter next/apply/finish · Esc close",
        Style::default().fg(t.text.muted),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

// ---- test accessors -------------------------------------------------------
//
// The four helpers above were reconstructed after an accidental deletion and
// then corrected against the pre-migration build. Exposing them to the test
// module lets their exact output be asserted, rather than inferred from what
// happens to survive header truncation at 80 columns — which is how three wrong
// reconstructions shipped green in the first place.

#[cfg(test)]
pub(crate) fn session_label_for_test(app: &AgentApp) -> String {
    session_label(app)
}

#[cfg(test)]
pub(crate) fn git_branch_for_test(app: &AgentApp) -> String {
    git_branch(app)
}

#[cfg(test)]
pub(crate) fn short_path_for_test(app: &AgentApp) -> String {
    short_path(app)
}
