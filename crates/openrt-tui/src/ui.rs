//! Ratatui rendering for [`crate::App`] (view only; no state mutation).

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Block;
use ratatui::Frame;

use crate::app::{App, Focus, OpsTab, PanelLoad};
use crate::design::collections::{self, ListItem, RowTone};
use crate::design::layout::panel_grid;
use crate::design::shell::AppShell;
use crate::design::states::{self, StateKind};
use crate::design::tokens::Tokens;
use crate::design::widgets::{self, PanelTone};
use crate::design::{HitTarget, PanelId};

/// Draw the whole Workbench.
///
/// The header, the status bar and the vertical geometry all come from the shared
/// [`AppShell`], which is the same authority Agent Mode uses. Workbench used to
/// compute its own `Length(1) / Min(3) / Length(1)` split and paint its own
/// header and footer, which meant two shells that could disagree: a terminal too
/// short for three bands behaved one way in Agent Mode and another here, and any
/// change to the shared breakpoints had to be made twice to stay consistent.
///
/// The shell also decides whether there is a rail, and Workbench passes no rail
/// rows — its four panels *are* the navigation surface, and charging it 16
/// columns for a gutter it never draws is width stolen from the panels at exactly
/// the sizes where they are tightest.
///
/// What stays Workbench-specific is the body: four dashboard panels in a
/// responsive grid, with panel-scoped focus and `WorkbenchRow` hit targets. That
/// is a genuinely different interaction model from Agent's single view plus rail,
/// so it keeps its own renderer — but it draws into `layout.body`, the rectangle
/// the shared shell computed, so the two modes can no longer disagree about
/// where the body starts.
pub fn draw(frame: &mut Frame, app: &App) {
    let tokens = app.tokens();
    let t = &tokens;
    // Explicit root surface — same identity as Agent Mode (shared theme).
    frame.render_widget(
        Block::default().style(Style::default().bg(t.surfaces.bg)),
        frame.area(),
    );

    // Posture as a badge rather than a hand-coloured span: `restricted` and
    // `open` are meanings, and both modes now read them from the same tone. The
    // header line already names the mode in words, so the badge is the redundant
    // half of the pair and is kept only for the tone.
    let (posture, tone) = app.mode_badge();
    let badges = [(posture, tone)];
    // The header is one row shared with the title and the posture badge, so the
    // subtitle has to know how much room is actually left.
    let header_line = app.header_line_for(frame.area().width >= 100);
    let meters = app.header_meters();

    let shell = AppShell {
        title: "OpenRT Workbench",
        subtitle: &header_line,
        badges: &badges,
        meters: &meters,
        hints: &status_hints(app),
        status: &app.status,
        status_tone: app.status_tone(),
        // Workbench has no rail: its four panels *are* the navigation surface,
        // and adding a rail would take width the panels need at 80 columns.
        rail: &[],
        active: 0,
        context: false,
    };
    let layout = shell.draw(t, frame, frame.area());
    draw_body(t, frame, layout.body, app);
}

/// The status bar's left segment: the steer buffer while steering, otherwise the
/// key map for whatever the focused panel can actually do.
///
/// Keys are stated per panel because a flat list cannot stay honest. `p`/`r`/`s`
/// are room actions and only exist while the Rooms sub-view is focused; `r` alone
/// means "run prompt" everywhere else, so one hint line cannot mention both
/// without lying about half of them. The previous line was ~110 characters, so it
/// was ellipsized away at every acceptance width and `q:quit` — the one binding
/// with no other route — never rendered.
///
/// The shared status bar ellipsizes what does not fit, so the shortest bindings
/// come first: those are the ones an operator loses.
fn status_hints(app: &App) -> String {
    if let Some(buf) = &app.steer_input {
        return format!("steer> {buf} · Enter submit · Esc cancel");
    }
    let panel = match app.focus {
        Focus::Workspaces => "↑↓ select · Enter open",
        Focus::Sessions => "↑↓ select · n new · r run",
        // Tasks are a read-only view of the store; saying so is better than
        // listing keys that do nothing here.
        Focus::Tasks => "↑↓ select · read-only",
        // `p`/`r`/`s` are room actions and exist only in this sub-view. Elsewhere
        // `r` means "run prompt", so one flat hint line cannot mention both
        // without lying about half of them.
        Focus::Ops if app.ops_tab == OpsTab::Rooms => "p pause · r resume · s steer · 1-8 views",
        Focus::Ops => "1-8 views · f/c/a/v time-machine",
    };
    // Kept short on purpose: the status bar ellipsizes the tail, and at 80
    // columns the panel half was being cut while the panel it described stayed
    // on screen.
    format!("q quit · Tab focus · {panel}")
}

fn draw_body(t: &Tokens, frame: &mut Frame, area: Rect, app: &App) {
    // Panel geometry comes from the shared grid, which reflows by width and
    // balances its last row. This used to be four hardcoded `Percentage(25)`
    // columns, which at 80x24 gave every panel 20 columns — too narrow for a
    // bordered title and a value, and unfixable because the constraint never
    // asked for fewer columns.
    let panels = panel_grid(area, 4, 1);

    // Rebuild the registry from the exact rects the panels are drawn into. The
    // renderer never invents a coordinate that input handling has to guess at,
    // and clearing first is what stops a region from a previously-wider viewport
    // surviving into this frame.
    app.hits.borrow_mut().clear();

    draw_workspaces(t, frame, panels[0], app);
    draw_sessions(t, frame, panels[1], app);
    draw_tasks(t, frame, panels[2], app);
    draw_ops(t, frame, panels[3], app);
}

/// Panel chrome for a Workbench dashboard panel.
///
/// Delegates to the shared [`crate::design::widgets::panel`] so the border tone,
/// title treatment and surface elevation have exactly one definition. This file
/// used to carry a third copy of panel construction — alongside the shared one
/// and Agent's own — and the copies had already drifted: this one set no
/// background at all, so a Workbench panel sat on whatever the root painted
/// while the same panel in Agent Mode sat on `surfaces.surface`.
///
/// The focused panel also names itself in its title. A border tone alone is not
/// enough: in the High Contrast and Monochrome themes the focus and default
/// tones are close, and a keyboard operator moving with `Tab` needs to know
/// where they are without relying on a colour difference at all.
fn panel_block(t: &Tokens, title: &str, focused: bool) -> Block<'static> {
    widgets::panel(
        t,
        title,
        if focused {
            PanelTone::Focus
        } else {
            PanelTone::Default
        },
    )
}

/// A panel title carrying its own state, so a panel never looks alive when it is
/// not.
///
/// `label` alone is ambiguous: "Sessions" with zero rows is either "no sessions
/// yet" or "the read failed". The state word (`0`, `LOADING`, `UNAVAILABLE`) is
/// what tells them apart, and putting it in the title means it survives even when
/// the panel body has no room for an explanation.
fn panel_title(label: &str, load: &PanelLoad, count: usize, focused: bool) -> String {
    // The focused panel names itself in its title, not only by border colour.
    //
    // `PanelTone::Focus` is a *colour* difference, and two of the six themes —
    // High Contrast and Monochrome — draw a focused border close enough to the
    // resting one that a keyboard operator tracking `Tab` cannot see where they
    // are. The caret is text, so it survives every theme and every terminal that
    // cannot show the colour at all.
    let mark = if focused { "▸ " } else { "" };
    // The same words the shared state vocabulary uses, so the title and the body
    // cannot disagree: `FAILED` on the title, `FAILED` in the panel. The tokens
    // also have to survive a 21-column panel, which is why these are the short
    // labels and not prose.
    let state = match crate::design::states::for_collection(
        load.loaded,
        load.failed.as_deref(),
        count,
        label,
    )
    .kind
    {
        StateKind::Error => StateKind::Error.label(),
        StateKind::Loading => StateKind::Loading.label(),
        // Loaded and non-empty: the count is the useful number, and it is the
        // real number of selectable rows.
        _ => return format!(" {mark}{label} · {count} "),
    };
    format!(" {mark}{label} · {state} ")
}

/// Draw one panel: chrome, then either a shared state or a shared list.
///
/// `rows` and `count` are passed separately because the count is what decides
/// which state to show, and it is the count of *real* rows — never the number of
/// lines drawn. Passing the drawn-line count would let a placeholder line
/// register itself as a selectable row, which is how an empty panel ends up
/// with a phantom target under the pointer.
#[allow(clippy::too_many_arguments)]
/// One Workbench panel.
///
/// `count` and `row` replace a `Vec<ListItem>`: every panel here is a list over a
/// collection the operator grows without limit (sessions, tasks, workspaces, and
/// the Ops sub-views), so building all the rows to paint about twenty of them cost
/// one `format!` per row per frame. `count` is arithmetic on the collection, and
/// `row` is called only for the rows the shared window decides to paint.
fn draw_panel(
    t: &Tokens,
    frame: &mut Frame,
    area: Rect,
    app: &App,
    panel: PanelId,
    label: &str,
    load: &PanelLoad,
    count: usize,
    cursor: Option<usize>,
    noun: Option<&str>,
    mut row: impl FnMut(usize) -> ListItem<'static>,
) {
    let focused = app.focus == Focus::from(panel);
    let block = panel_block(t, &panel_title(label, load, count, focused), focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // The panel frame focuses the panel, so a click on its border or title moves
    // focus without selecting a row. Registered first, because
    // `HitTargets::hit` is newest-first and the row rects must win.
    app.hits
        .borrow_mut()
        .register(area, HitTarget::WorkbenchPanel { panel });

    let mut hits = app.hits.borrow_mut();
    // Shared state rendering covers loading, empty *and* failed with one
    // vocabulary and one set of words, and it declines to draw anything when the
    // panel has rows. Returning `false` is what keeps the list below.
    let noun = noun
        .map(str::to_string)
        .unwrap_or_else(|| label.to_lowercase());
    if states::render_collection(
        frame,
        inner,
        t,
        &mut hits,
        load.loaded,
        load.failed.as_deref(),
        count,
        &noun,
    ) {
        return;
    }

    // The shared list renderer picks the window from `count`/`cursor` and then
    // calls `row` for the rows inside it, so nothing outside the viewport is
    // built. Collecting every row first and windowing afterwards is what made this
    // O(n) in the collection.
    collections::draw_list_rows_lazy_targeted(
        t,
        frame,
        inner,
        count,
        cursor,
        0,
        true,
        &mut hits,
        |hits, rect, index| {
            hits.register(rect, HitTarget::WorkbenchRow { panel, index });
        },
        &mut row,
    );
}

fn status_tone(status: &str) -> RowTone {
    let s = status.trim().to_ascii_uppercase();
    match s.as_str() {
        "GREEN" | "SUPPORTED" | "OK" | "AVAILABLE" | "ACTIVE" | "CLEAN" | "COMPLETED"
        | "SUCCEEDED" | "DONE" => RowTone::Success,
        "PARTIAL" | "TIGHT" | "PENDING" | "RUNNING" | "IN_PROGRESS" | "DEGRADED" | "WARNING"
        | "MEDIUM" => RowTone::Warning,
        "RED" | "BLOCKED" | "FAILED" | "ERROR" | "UNAVAILABLE" | "INTERRUPTED" | "INCOMPLETE"
        | "FLAGGED" | "CRITICAL" | "HIGH" | "DENIED" => RowTone::Danger,
        "WAITING" | "IDLE" | "PAUSED" | "STALE" | "IMPORTED" | "UNKNOWN" => RowTone::Muted,
        _ => RowTone::Info,
    }
}

fn draw_workspaces(t: &Tokens, frame: &mut Frame, area: Rect, app: &App) {
    draw_panel(
        t,
        frame,
        area,
        app,
        PanelId::Workspaces,
        "Workspaces",
        &app.load_workspaces,
        app.workspaces.len(),
        app.sel_workspace,
        None,
        |i| {
            let w = &app.workspaces[i];
            ListItem {
                // The active workspace keeps its `*` marker: it is the answer to
                // "which one am I in", and a row tone alone would not say so in
                // Monochrome.
                label: format!(
                    "{} {} {}",
                    if w.active { "*" } else { " " },
                    w.address_code,
                    w.title
                ),
                detail: None,
                tone: w.active.then_some(RowTone::Success),
                id: "",
            }
        },
    );
}

fn draw_sessions(t: &Tokens, frame: &mut Frame, area: Rect, app: &App) {
    let current = app.current_session;
    draw_panel(
        t,
        frame,
        area,
        app,
        PanelId::Sessions,
        "Sessions",
        &app.load_sessions,
        app.sessions.len(),
        app.sel_session,
        None,
        |i| {
            let s = &app.sessions[i];
            ListItem {
                label: format!(
                    "{} {}  [{}]",
                    if current == Some(s.id) { ">" } else { " " },
                    s.title,
                    s.status
                ),
                // The message count is the number an operator actually compares, so
                // it goes in the shared right-aligned detail slot instead of being
                // concatenated into the label and truncated away.
                detail: Some(format!("{} msg", s.messages)),
                tone: (current == Some(s.id)).then_some(RowTone::Success),
                id: "",
            }
        },
    );
}

fn draw_tasks(t: &Tokens, frame: &mut Frame, area: Rect, app: &App) {
    draw_panel(
        t,
        frame,
        area,
        app,
        PanelId::Tasks,
        "Tasks",
        &app.load_tasks,
        app.tasks.len(),
        app.sel_task,
        None,
        |i| {
            let task = &app.tasks[i];
            ListItem {
                // The shared row renderer keeps a row's *tail* when it has to
                // truncate, which is right for a file path and wrong for a prompt:
                // "…ello there, this is a real prompt" hides the words that
                // identify the task. Cutting here, at the front, leaves the shared
                // rule nothing to do in the common case.
                label: head_excerpt(&task.prompt, 26),
                // Status moves to the right-aligned detail column: the shared row
                // renderer drops the *head* of a label that is too long, so a status
                // prefix in the label was the first thing lost in a narrow panel —
                // leaving a row that looked like it had no state at all.
                detail: Some(task.status.to_lowercase()),
                tone: Some(status_tone(&task.status)),
                id: "",
            }
        },
    );
}

/// The Ops panel — one of four real sub-views (keys 1–8).
///
/// Each sub-view is reduced to `(label, detail, tone)` and rendered by the same
/// shared list as the other three panels. The sub-views used to build
/// `ListItem`s by hand and colour rows with inline matches, which meant the Ops
/// panel had its own idea of what a row looks like and its own idea of what a
/// status colour is.
/// The first `n` characters of a string, marked when anything was dropped.
///
/// Keeps the head, unlike the shared [`collections::truncate_tail`], which keeps
/// the tail because a row usually ends in a file path.
fn head_excerpt(s: &str, n: usize) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i == n {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

fn draw_ops(t: &Tokens, frame: &mut Frame, area: Rect, app: &App) {
    // Every Ops sub-view is a list over an operator-growable collection — rooms,
    // extensions, shield findings, recovery records — so the row is a factory and
    // the count is arithmetic. Building a `Vec` of tuples first meant formatting
    // every row in the sub-view to paint about twenty of them.
    let count = match app.ops_tab {
        OpsTab::Timeline => app.ops_timeline.len(),
        OpsTab::Skills => app.ops_skills.len(),
        OpsTab::Shield => app.ops_findings.len(),
        OpsTab::Recovery => app.ops_recovery.len(),
        OpsTab::Rooms => app.ops_rooms.len(),
        OpsTab::Extensions => app.ops_extensions.len(),
        OpsTab::Computer => app.ops_computer.len(),
        OpsTab::Intel => app.ops_intel.len(),
    };
    let mut row = |i: usize| -> ListItem<'static> {
        let (label, detail, tone) = match app.ops_tab {
            OpsTab::Timeline => app
                .ops_timeline
                .get(i)
                .map(|r| {
                    (
                        format!("{:>4}  {}  {}", r.seq, r.status, r.event_kind),
                        Some(r.note.clone()),
                        Some(status_tone(&r.status)),
                    )
                })
                .unwrap_or_else(|| (String::new(), None, Some(RowTone::Muted))),
            OpsTab::Skills => app
                .ops_skills
                .get(i)
                .map(|s| {
                    (
                        s.id.clone(),
                        Some(s.state.clone()),
                        Some(status_tone(&s.state)),
                    )
                })
                .unwrap_or_else(|| (String::new(), None, Some(RowTone::Muted))),
            OpsTab::Shield => app
                .ops_findings
                .get(i)
                .map(|f| {
                    (
                        format!("{}  {}", f.severity, f.category),
                        Some(f.reason.clone()),
                        Some(status_tone(&f.severity)),
                    )
                })
                .unwrap_or_else(|| (String::new(), None, Some(RowTone::Muted))),
            OpsTab::Recovery => app
                .ops_recovery
                .get(i)
                .map(|r| {
                    (
                        format!(
                            "{} {} int={} inc={} stale={}",
                            if r.clean { "ok" } else { "!!" },
                            r.record_id,
                            r.interrupted,
                            r.incomplete,
                            r.stale_sandboxes
                        ),
                        None,
                        Some(if r.clean {
                            RowTone::Success
                        } else {
                            RowTone::Danger
                        }),
                    )
                })
                .unwrap_or_else(|| (String::new(), None, Some(RowTone::Muted))),
            OpsTab::Rooms => app
                .ops_rooms
                .get(i)
                .map(|r| {
                    (
                        // The full room id is an 18-character hash; in a 38-column
                        // panel it pushed the title and the status off the edge.
                        format!("{}  {}  [{}]", head_excerpt(&r.id, 8), r.title, r.status),
                        Some(format!(
                            "wf={} a={} m={} s={}",
                            r.active_workflow.as_deref().unwrap_or("-"),
                            r.agents,
                            r.materials,
                            r.steer
                        )),
                        Some(if r.active_workflow.is_some() {
                            RowTone::Success
                        } else {
                            status_tone(&r.status)
                        }),
                    )
                })
                .unwrap_or_else(|| (String::new(), None, Some(RowTone::Muted))),
            OpsTab::Extensions => app
                .ops_extensions
                .get(i)
                .map(|e| {
                    (
                        format!("{}@{}", e.id, e.version),
                        Some(format!("{} · {}", e.activation, e.security)),
                        Some(if e.activation == "active" {
                            RowTone::Success
                        } else {
                            status_tone(&e.security)
                        }),
                    )
                })
                .unwrap_or_else(|| (String::new(), None, Some(RowTone::Muted))),
            OpsTab::Computer => app
                .ops_computer
                .get(i)
                .map(|r| {
                    (
                        r.capability.clone(),
                        Some(r.status.clone()),
                        Some(status_tone(&r.status)),
                    )
                })
                .unwrap_or_else(|| (String::new(), None, Some(RowTone::Muted))),
            OpsTab::Intel => app
                .ops_intel
                .get(i)
                .map(|r| {
                    (
                        r.label.clone(),
                        Some(r.value.clone()),
                        Some(status_tone(&r.value)),
                    )
                })
                .unwrap_or_else(|| (String::new(), None, Some(RowTone::Muted))),
        };
        ListItem {
            label,
            detail,
            tone,
            id: "",
        }
    };
    let label = app.ops_tab.title();
    // The state sentence needs a noun, not a title: `title()` is "Ops · Rooms",
    // which produced the nonsense "no ops · rooms yet". Only the collection part
    // names a collection.
    let noun = label
        .split('·')
        .nth(1)
        .unwrap_or(label)
        .trim()
        .to_lowercase();
    // Only the Rooms sub-view has a row selection; the rest are read-only, so
    // they pass `None` rather than borrowing an index that is not theirs.
    let cursor = match app.ops_tab {
        OpsTab::Rooms => app.sel_room,
        _ => None,
    };
    draw_panel(
        t,
        frame,
        area,
        app,
        PanelId::Ops,
        label,
        &app.load_ops,
        count,
        cursor,
        Some(&noun),
        &mut row,
    );
}
