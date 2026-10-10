//! Agent Mode key handling: approvals first, overlays second, then the
//! always-live input line and rail navigation. Workbench keys delegate to the
//! existing Workbench handler (one terminal, two surfaces).

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};

use crate::agent::state::{AgentApp, Overlay, ResourceAction, UiMode, View};
use crate::app::AppError;
use crate::design::{self, FocusTarget, Move};

/// Translate one pointer event into the action it stands for.
/// A click is translated into the key it stands for and handed to
/// [`handle_key`].
///
/// This is the whole mouse strategy, and it is deliberate: there is exactly one
/// implementation of every action. A click cannot reach a code path the keyboard
/// cannot, which matters most for permissions — if clicking "Allow workspace"
/// had its own branch, the `Enter`-never-grants rule and the fail-closed default
/// would exist twice and one of them would rot.
///
/// Two consequences are accepted on purpose:
/// * A row click *selects*; activating it still needs `Enter`. Clicking a row
///   must never be able to run a command, so selection and activation stay
///   separate acts.
/// * A click on a scrim is swallowed and does nothing at all.
pub async fn handle_click(app: &mut AgentApp, kind: MouseEventKind) -> Result<(), AppError> {
    // A mode gate comes first, before any hit test.
    //
    // The Agent's hit registry is only rewritten when the Agent draws. If the
    // user switches to Workbench, the registry still holds the Agent's last
    // frame — including the rectangles of a permission card that is no longer on
    // screen. Dispatching against it would let a click answer an approval the
    // operator cannot see, which is precisely the failure a permission prompt
    // exists to prevent. Workbench input is handled by its own path.
    if app.mode == UiMode::Workbench {
        // Clear any stale press so a release after a mode switch cannot fire it.
        app.cancel_press();
        return Ok(());
    }

    let target = match kind {
        MouseEventKind::Down(MouseButton::Left) => {
            // A press only *arms* the target: it records where the button went
            // down and moves focus there. It must not activate, or a single
            // click fires twice and — for a permission grant — the second
            // dispatch runs against an app that has already consumed the
            // approval.
            match app.press_at(app.pointer.0, app.pointer.1) {
                Some(t) => t,
                // Nothing under the pointer: swallow it. A click on empty space
                // must not fall through to whatever key the view would bind to
                // that position, and must not move focus.
                None => return Ok(()),
            };
            return Ok(());
        }
        MouseEventKind::Up(MouseButton::Left) => match app.release_at(app.pointer.0, app.pointer.1)
        {
            Some(t) => t,
            None => return Ok(()),
        },
        MouseEventKind::Drag(MouseButton::Left) => {
            // Dragging off an armed target disarms it. Pressing a button then
            // dragging away must not fire it.
            let was = app.pressed_target();
            app.hover_at(app.pointer.0, app.pointer.1);
            return match was {
                Some(t) if Some(&t) == app.hit_test(app.pointer.0, app.pointer.1).as_ref() => {
                    let _ = t;
                    Ok(())
                }
                Some(_) => {
                    app.cancel_press();
                    Ok(())
                }
                None => Ok(()),
            };
        }
        // Motion never activates anything, but it does have to end a press that
        // has left its target. Without mouse capture, terminals report an
        // unarmed drag as a plain `Moved`, so treating motion as hover-only
        // means a press on one control followed by moving off it and releasing
        // still fires the original control.
        MouseEventKind::Moved => {
            app.hover_at(app.pointer.0, app.pointer.1);
            if let Some(armed) = app.pressed_target() {
                if app.hit_test(app.pointer.0, app.pointer.1).as_ref() != Some(&armed) {
                    app.cancel_press();
                }
            }
            return Ok(());
        }
        _ => return Ok(()),
    };

    match target {
        FocusTarget::Nav(index) => {
            if let Some(v) = View::ALL.get(index) {
                app.set_view(*v);
            }
        }
        FocusTarget::Tab(index) => app.select_workbench_tab(index),
        FocusTarget::Row(index) => app.select_row(index),
        // A palette row is addressed by its *position in the match list*, which
        // is what was painted, so the click cannot name a destination the
        // current query does not contain. The bounds check also covers a
        // release that lands after the query changed underneath the press.
        FocusTarget::PaletteItem(index) => {
            if let Some(item) = app.palette_matches().get(index).copied() {
                jump_from_palette(app, item).await;
            }
        }
        // The model overlay is a form (a provider tag plus Enter to apply), not
        // a selectable list, so there is nothing here for a click to activate.
        FocusTarget::ModelItem(_) => {}
        FocusTarget::Action { id, .. } => {
            if let Some(key) = key_for_action(id) {
                return handle_key(app, KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE))
                    .await;
            }
        }
        FocusTarget::Composer => app.focus_composer(),
        // A click on the dimmed area around a dialog must not reach the view
        // behind it. That is the entire reason a scrim exists.
        FocusTarget::ModalScrim | FocusTarget::Header | FocusTarget::ChatHint { .. } => {}
        // Workbench regions can only exist in the Workbench registry. If one is
        // somehow resolved here, the mode gate above has already been bypassed —
        // so refuse it explicitly rather than falling through to a match arm
        // that would have to guess an Agent meaning for it.
        FocusTarget::WorkbenchRow { .. } | FocusTarget::WorkbenchPanel { .. } => {}
    }
    Ok(())
}

/// Insert a pasted payload into the composer.
///
/// Control bytes are dropped rather than inserted. A paste is arbitrary bytes
/// from outside the program, and the composer is a single line: inserting a
/// pasted newline would submit the message, and inserting a pasted escape could
/// move the cursor or repaint the screen. Only printable text is taken, so a
/// paste can fill the composer but can never *submit* one.
/// The largest paste accepted into any single input line.
///
/// A paste is unbounded at the terminal, so an accidental `cat bigfile | xsel`
/// (or a terminal that re-sends the scrollback) puts megabytes into a single
/// `String` and then re-renders it every frame. The bound is what keeps one
/// paste from becoming a rendering stall.
///
/// It is a limit on *stored* text, not on what the user may type: the composer
/// is not truncated, an over-long paste is cut and says so, because silently
/// eating the tail of a paste is how someone loses a paragraph.
pub const MAX_PASTE_CHARS: usize = 64 * 1024;

pub fn handle_paste(app: &mut AgentApp, text: &str) {
    // Control bytes are dropped before anything else looks at the text.
    //
    // This is the boundary that keeps a paste from becoming synthetic keypresses:
    // CR, LF, tab, and escape are exactly the bytes that a terminal would
    // otherwise turn into Enter/Tab/Esc, so filtering them means a pasted
    // multi-line block lands in the composer as one inert payload that can never
    // submit or activate anything. Newlines become a single space so the pasted
    // block stays readable instead of collapsing into one word.
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cleaned = if cleaned.trim().is_empty() {
        return;
    } else if cleaned.chars().all(|c| c.is_whitespace()) {
        // Whitespace-only: nothing to insert.
        return;
    } else {
        cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
    };

    if cleaned.is_empty() {
        return;
    }

    // A paste goes to the field the operator can currently see.
    //
    // While any modal is open the background composer is not a target: a paste
    // arriving during a dialog went straight into the composer behind it, which
    // then had text the operator never saw land in it, submitted later by a
    // reflexive Enter. The permission prompt is the security-critical case and is
    // named explicitly; the general modal check covers the rest.
    if app.modal_open() {
        return;
    }

    let line: &mut String = if app.view == View::Models {
        // The Models view owns the input line; a paste there would be clobbered
        // by the stashed draft, so it goes to the draft slot instead.
        &mut app.draft
    } else {
        &mut app.input
    };

    // Bound the result. `line` is left holding the older content rather than
    // being cleared: a paste that arrives while the composer is full should not
    // silently destroy what is already there.
    let room = MAX_PASTE_CHARS.saturating_sub(line.chars().count());
    if room == 0 {
        app.status = format!(
            "paste ignored — the input line is already at its {MAX_PASTE_CHARS} character limit"
        );
        return;
    }
    let total = line.chars().count();
    let accepted: String = cleaned.chars().take(room).collect();
    let dropped = cleaned.chars().count().saturating_sub(room);
    line.push_str(&accepted);
    if dropped > 0 {
        app.status = format!(
            "pasted {total}+{accepted_count} characters; {dropped} dropped at the {MAX_PASTE_CHARS} character limit",
            total = total,
            accepted_count = accepted.chars().count(),
        );
    }
}

/// Jump to a palette destination and enter the view.
///
/// Shared by `Enter` and by a click on a palette row so the two cannot drift:
/// jumping alone leaves the palette open on top of the destination it just
/// navigated to.
async fn jump_from_palette(app: &mut AgentApp, item: usize) {
    app.palette_jump(item).await;
    on_view_enter(app).await;
}

/// The key each dialog action stands for.
///
/// Ids are the stable strings the shared `ButtonGroup` registers, so the mapping
/// lives beside the buttons rather than being duplicated per call site.
fn key_for_action(id: &str) -> Option<char> {
    Some(match id {
        // Permission grants mirror the documented keys exactly.
        "grant.once" => '1',
        "grant.session" => '2',
        "grant.workspace" => '3',
        "grant.deny" => '4',
        "confirm.yes" => 'y',
        "confirm.no" => 'n',
        _ => return None,
    })
}

/// Route `Tab` / `Shift+Tab` through the focus ring.
///
/// Returns `true` when the ring consumed the key, so the caller knows not to
/// fall through to a view-specific meaning for the same key.
pub fn handle_focus_key(app: &mut AgentApp, code: KeyCode, shift: bool) -> bool {
    let mv = if code == KeyCode::Tab {
        if shift {
            design::Move::Backward
        } else {
            design::Move::Forward
        }
    } else if code == KeyCode::BackTab {
        design::Move::Backward
    } else {
        return false;
    };
    app.move_focus(mv)
}

/// Translate one key press into state changes.
pub async fn handle_key(app: &mut AgentApp, key: KeyEvent) -> Result<(), AppError> {
    if key.kind == KeyEventKind::Release {
        return Ok(());
    }

    // Ctrl+C is resolved by the layered interrupt policy, not by a hard quit.
    //
    // It used to quit unconditionally, which meant an interrupt during a
    // permission prompt exited with the gate still blocked and the waiting task
    // abandoned, and an interrupt mid-composition destroyed unsent text. Now it
    // stops whatever is stoppable in the current context and quits only when
    // nothing is. See `AgentApp::handle_interrupt` for the order and the
    // reasoning; the short version is that cancellation and quitting are
    // different operations and only one of them is what a bare Ctrl+C should be.
    if is_ctrl(key, 'c') {
        app.should_quit = app.handle_interrupt().await;
        return Ok(());
    }

    // Ctrl+Q is the advertised escape hatch: the same teardown as Ctrl+C, minus
    // the composer guard. It has to run *before* the modal layers, or the key
    // that the composer guard tells the operator to press would be swallowed by
    // whichever overlay happens to be open.
    if is_ctrl(key, 'q') {
        app.should_quit = app.handle_quit_now().await;
        return Ok(());
    }

    // Pending approval owns the keyboard until answered (fail-closed: only an
    // explicit allow reaches the gate). Tiers: 1/y = allow once,
    // 2 = allow for session, 3 = allow for workspace, 4/n/Esc/Enter = deny —
    // Enter never grants (a reflexive Enter must not open a permission).
    if !app.approvals.is_empty() {
        use openrt_permissions::PermissionDecision as Pd;
        let decision = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Char('1') => Some(Pd::Allow),
            KeyCode::Char('2') => Some(Pd::AllowSession),
            KeyCode::Char('3') => Some(Pd::AllowWorkspace),
            // `d` is the deny key the permission card advertises, so it must
            // actually work; `n`/`4`/Esc/Enter are the long-standing aliases.
            KeyCode::Char('n')
            | KeyCode::Char('N')
            | KeyCode::Char('d')
            | KeyCode::Char('D')
            | KeyCode::Char('4')
            | KeyCode::Esc
            | KeyCode::Enter => Some(Pd::Deny),
            _ => None,
        };
        if let Some(d) = decision {
            let p = app.approvals.remove(0);
            let tool = p.request.tool.clone();
            let label = match d {
                Pd::Allow => "approved".to_string(),
                Pd::AllowSession => "approved for session".to_string(),
                Pd::AllowWorkspace => "approved for workspace".to_string(),
                Pd::Deny => "denied".to_string(),
            };
            p.answer(d);
            app.status = format!("{label} {tool}");
        } else {
            app.status = "1 allow once · 2 session · 3 workspace · 4/esc deny".into();
        }
        return Ok(());
    }

    // Destructive-action confirmation owns the keyboard until answered.
    //
    // Only `y` proceeds and only `n`/`Esc` cancels. Every other key is
    // an arbitrary key is *swallowed* rather than treated as a cancel. When any
    // key cancelled, a key-repeat storm dismissed the dialog with the first
    // character and the following keys went straight through to the view behind
    // it — a held-down `r` cancelled a mode-switch confirmation and then started
    // a run. The explicit dismiss keys below still cancel, so the dialog is
    // never a trap; it just cannot be dismissed by accident.
    if app.confirm.is_some() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.confirm_accept().await?;
            }
            // Enter cancels rather than proceeds: it is the conventional
            // dismiss key and the dialog's default must be the safe answer.
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Enter => {
                app.confirm_cancel();
            }
            _ => app.status = "confirm: y to proceed · n or esc to cancel".into(),
        }
        return Ok(());
    }

    // Workbench surface: delegate to the existing key handler; Ctrl+W (and
    // the palette) come back to Agent Mode.
    if app.mode == UiMode::Workbench {
        if is_ctrl(key, 'w') {
            // Leaving Workbench drops its registry: the next Agent frame owns
            // the screen, and a surviving Workbench region would resolve
            // clicks against geometry that no longer describes it.
            app.enter_agent();
            return Ok(());
        }
        if is_ctrl(key, 'k') {
            // Drop the Workbench registry *before* opening the palette. The
            // palette draws Agent coordinates over a screen that is about to
            // belong to the Agent, so a surviving Workbench row would swallow
            // the first click on a palette entry.
            app.enter_agent();
            app.overlay = Some(Overlay::Palette);
            app.palette.clear();
            app.palette_sel = 0;
            return Ok(());
        }
        let result = crate::run::workbench_key(&mut app.workbench, key).await;
        if app.workbench.should_quit {
            app.should_quit = true;
        }
        return result;
    }

    // ---- overlays ----
    if let Some(overlay) = app.overlay {
        return overlay_key(app, key, overlay).await;
    }

    // ---- RESOURCE LIMIT recovery: the session stays alive ----
    // A host capacity refusal is not a model failure and not a broken tool.
    // Every action here is reversible and none of them destroy the prompt.
    if app.resource_card.is_some()
        && app.view == View::Chat
        && app.input.is_empty()
        && !app.filter_active
        && !app.import_mode
        && !app.model_search_active
    {
        let waiting = app
            .resource_card
            .as_ref()
            .and_then(|c| c.waiting.as_ref())
            .is_some();
        if waiting {
            // While a bounded retry is in flight the only safe keys are the ones
            // that change or stop the wait; typing must not be swallowed.
            match key.code {
                KeyCode::Esc => {
                    app.clear_resource_card();
                    app.status = "resource wait cancelled — the session stays open".into();
                    return Ok(());
                }
                KeyCode::Tab | KeyCode::Down => {
                    if let Some(c) = app.resource_card.as_mut() {
                        if !c.actions.is_empty() {
                            c.selected = (c.selected + 1) % c.actions.len();
                        }
                    }
                    return Ok(());
                }
                KeyCode::Up => {
                    if let Some(c) = app.resource_card.as_mut() {
                        if !c.actions.is_empty() {
                            c.selected = (c.selected + c.actions.len() - 1) % c.actions.len();
                        }
                    }
                    return Ok(());
                }
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Tab | KeyCode::Down => {
                    if let Some(c) = app.resource_card.as_mut() {
                        if !c.actions.is_empty() {
                            c.selected = (c.selected + 1) % c.actions.len();
                        }
                    }
                    return Ok(());
                }
                KeyCode::Up => {
                    if let Some(c) = app.resource_card.as_mut() {
                        if !c.actions.is_empty() {
                            c.selected = (c.selected + c.actions.len() - 1) % c.actions.len();
                        }
                    }
                    return Ok(());
                }
                KeyCode::Enter => {
                    let action = app
                        .resource_card
                        .as_ref()
                        .and_then(|c| c.actions.get(c.selected).copied());
                    if let Some(a) = action {
                        return run_resource_action(app, a).await;
                    }
                    return Ok(());
                }
                KeyCode::Char('v') | KeyCode::Char('V') => {
                    app.resource_detail = !app.resource_detail;
                    return Ok(());
                }
                KeyCode::Char(c) => {
                    // Direct shortcuts for the offered actions, so keyboard and
                    // the rendered action bar agree.
                    let action = app
                        .resource_card
                        .as_ref()
                        .and_then(|card| card.actions.iter().find(|a| a.key() == c).copied());
                    if let Some(a) = action {
                        return run_resource_action(app, a).await;
                    }
                }
                KeyCode::Esc => {
                    app.clear_resource_card();
                    app.status = "resource limit dismissed — the session stays open".into();
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    // ---- MODEL ERROR recovery: the session stays alive (PART 15/20) ----
    // `r` re-sends the last prompt · `m` opens the model switcher ·
    // `v` toggles the full sanitized detail · `Esc` dismisses the card.
    if app.model_error.is_some()
        && app.view == View::Chat
        && app.input.is_empty()
        && !app.filter_active
        && !app.import_mode
        && !app.model_search_active
    {
        match key.code {
            KeyCode::Char('r') | KeyCode::Char('R') => {
                let prompt = app.last_prompt.clone();
                match prompt {
                    Some(p) if !p.is_empty() => {
                        app.model_error = None;
                        app.model_error_detail = false;
                        app.input = p;
                        app.send().await?;
                        app.status = "retrying the failed prompt".into();
                    }
                    _ => {
                        app.status = "nothing to retry — type a message and press Enter".into();
                    }
                }
                return Ok(());
            }
            KeyCode::Char('m') | KeyCode::Char('M') => {
                app.overlay = Some(Overlay::Model);
                app.status = "switch model — pick a tag, Enter applies".into();
                return Ok(());
            }
            KeyCode::Char('v') | KeyCode::Char('V') => {
                app.model_error_detail = !app.model_error_detail;
                return Ok(());
            }
            KeyCode::Esc => {
                app.model_error = None;
                app.model_error_detail = false;
                app.status = "model error dismissed — the session stays open".into();
                return Ok(());
            }
            _ => {}
        }
    }

    // ---- /plan failure recovery: the session stays alive ----
    // `r` retries the same goal · `e`/Enter restores it for editing ·
    // `Esc` dismisses (the PLAN FAILED block keeps the goal in history).
    if app.plan_failure.is_some()
        && app.view == View::Chat
        && app.input.is_empty()
        && !app.filter_active
        && !app.import_mode
        && !app.model_search_active
    {
        match key.code {
            KeyCode::Char('r') | KeyCode::Char('R') => {
                let goal = app.plan_failure.clone().unwrap_or_default();
                app.compile_plan(&goal).await?;
                return Ok(());
            }
            KeyCode::Char('e') | KeyCode::Char('E') | KeyCode::Enter => {
                let goal = app.plan_failure.take().unwrap_or_default();
                app.input = format!("/plan {goal}");
                app.status = "editing the failed plan — Enter re-runs /plan · Esc clears".into();
                return Ok(());
            }
            KeyCode::Esc => {
                app.plan_failure = None;
                app.status =
                    "plan failure dismissed — the goal stays in the PLAN FAILED block".into();
                return Ok(());
            }
            _ => {}
        }
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('f'), KeyModifiers::CONTROL)
            if app.view == View::Models && !app.hub_filter_active =>
        {
            app.toggle_model_search();
            Ok(())
        }
        (KeyCode::Char('f'), KeyModifiers::CONTROL)
            if matches!(
                app.view,
                View::Files
                    | View::Tools
                    | View::Tasks
                    | View::Sessions
                    | View::Mcp
                    | View::Extensions
                    | View::Security
            ) =>
        {
            app.toggle_list_filter();
            Ok(())
        }
        (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
            // Same on the way in: the Agent registry and any armed press must
            // not survive into Workbench, where a coordinate means a panel row.
            app.enter_workbench().await;
            Ok(())
        }
        (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
            app.overlay = Some(Overlay::Palette);
            app.palette.clear();
            app.palette_sel = 0;
            Ok(())
        }
        (KeyCode::Char('m'), KeyModifiers::CONTROL) => {
            app.refresh_models().await;
            app.overlay = Some(Overlay::Model);
            Ok(())
        }
        (KeyCode::Char('t'), KeyModifiers::CONTROL) => {
            app.overlay = None;
            app.set_view(View::Terminal);
            on_view_enter(app).await;
            Ok(())
        }
        (KeyCode::Char('g'), KeyModifiers::CONTROL) => {
            app.overlay = None;
            app.set_view(View::Git);
            on_view_enter(app).await;
            Ok(())
        }
        (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
            app.overlay = None;
            app.set_view(View::Files);
            app.toggle_list_filter();
            app.status = "file picker — type to filter · Enter opens · Esc closes filter".into();
            on_view_enter(app).await;
            Ok(())
        }
        (KeyCode::Char('o'), KeyModifiers::CONTROL) => {
            app.overlay = Some(Overlay::Mode);
            app.status = "operating mode — Enter switches (confirm) · Esc closes".into();
            Ok(())
        }
        (KeyCode::Char(','), KeyModifiers::CONTROL) => {
            app.overlay = None;
            app.set_view(View::Settings);
            on_view_enter(app).await;
            Ok(())
        }
        (KeyCode::Left, KeyModifiers::CONTROL) => {
            let v = app.view.prev();
            app.set_view(v);
            on_view_enter(app).await;
            Ok(())
        }
        (KeyCode::Right, KeyModifiers::CONTROL) => {
            let v = app.view.next();
            app.set_view(v);
            on_view_enter(app).await;
            Ok(())
        }
        (KeyCode::Char('e'), KeyModifiers::CONTROL) if app.view == View::Chat => {
            app.expand_last_tool();
            Ok(())
        }
        (KeyCode::Char('r'), KeyModifiers::CONTROL) => {
            if let Err(e) = app.refresh_view().await {
                app.status = format!("refresh failed: {e}");
            }
            Ok(())
        }
        (KeyCode::Char('d'), KeyModifiers::CONTROL) if app.view == View::Files => {
            app.preview_diff().await;
            Ok(())
        }
        // ---- Hub `f` filter overlay (before every generic nav/esc/enter) ----
        (KeyCode::Esc, _) if app.hub_filter_active => {
            app.hub_filter_close();
            Ok(())
        }
        (KeyCode::Enter, _) if app.hub_filter_active => {
            app.hub_filter_close();
            Ok(())
        }
        (KeyCode::Up, _) if app.hub_filter_active => {
            app.hub_filter_move(-1);
            Ok(())
        }
        (KeyCode::Down, _) if app.hub_filter_active => {
            app.hub_filter_move(1);
            Ok(())
        }
        (KeyCode::Left, _) if app.hub_filter_active => {
            app.hub_filter_cycle(-1);
            Ok(())
        }
        (KeyCode::Right, _) if app.hub_filter_active => {
            app.hub_filter_cycle(1);
            Ok(())
        }
        (KeyCode::Char('c'), _) if app.hub_filter_active => {
            app.hub_filter_clear();
            Ok(())
        }
        (KeyCode::Esc, _) if app.op_edit => {
            app.op_edit = false;
            app.input.clear();
            app.status = "operator id unchanged".into();
            Ok(())
        }
        (KeyCode::Esc, _) if app.policy_edit => {
            app.policy_edit = false;
            app.input.clear();
            app.status = "rule edit cancelled".into();
            Ok(())
        }
        (KeyCode::Esc, _) if app.ext_install => {
            app.ext_install = false;
            app.input.clear();
            app.status = "install cancelled".into();
            Ok(())
        }
        // Settings: Esc abandons a live theme preview and restores the
        // committed theme before any other Esc meaning applies.
        (KeyCode::Esc, _)
            if app.view == View::Settings
                && app.settings_focus == crate::agent::state::SettingsFocus::Theme
                && app.is_previewing_theme() =>
        {
            app.cancel_theme_preview();
            app.status = format!(
                "preview cancelled — theme is {}",
                app.committed_theme().kind.label()
            );
            Ok(())
        }
        (KeyCode::Esc, _) if app.import_mode => {
            app.import_mode = false;
            app.import_id.clear();
            app.input.clear();
            app.status = "import cancelled".into();
            Ok(())
        }
        (KeyCode::Esc, _) if app.filter_active => {
            app.filter_active = false;
            app.list_filter.clear();
            app.sel = 0;
            app.status = "filter closed".into();
            Ok(())
        }
        (KeyCode::Esc, _) if app.model_search_active => {
            app.model_search_active = false;
            app.status = "search closed".into();
            Ok(())
        }
        (KeyCode::Esc, _) => {
            if !app.input.is_empty() {
                app.input.clear();
                app.status = "input cleared".into();
            } else if app.running() {
                app.cancel().await;
            } else {
                app.status = "nothing to cancel".into();
            }
            Ok(())
        }
        (KeyCode::Enter, _) if app.op_edit => {
            let op = app.input.trim().to_string();
            if op.is_empty() {
                app.status = "operator id cannot be empty (Esc to cancel)".into();
            } else {
                app.input.clear();
                app.operator_id = op.clone();
                app.op_edit = false;
                app.status = format!("operator id = {op}");
            }
            Ok(())
        }
        (KeyCode::Enter, _) if app.policy_edit => {
            let raw = app.input.trim().to_string();
            let parts: Vec<&str> = raw.split_whitespace().collect();
            let ok = (2..=3).contains(&parts.len()) && matches!(parts[1], "allow" | "deny" | "ask");
            if !ok {
                app.status =
                    "usage: <pattern> <allow|deny|ask> [scope] — e.g. filesystem.read allow".into();
                return Ok(());
            }
            app.input.clear();
            app.policy_edit = false;
            app.confirm = Some(crate::agent::state::Confirm::PolicyAppend {
                pattern: parts[0].to_string(),
                action: parts[1].to_string(),
                scope: parts.get(2).map(|s| s.to_string()),
            });
            app.status = format!("append rule `{}` → {}? y = append", parts[0], parts[1]);
            Ok(())
        }
        (KeyCode::Enter, _) if app.ext_install => {
            let source = app.input.trim().to_string();
            app.input.clear();
            app.ext_spawn(crate::agent::state::ExtOp::Install { source });
            Ok(())
        }
        // Settings: Enter commits + persists the previewed theme.
        (KeyCode::Enter, _)
            if app.view == View::Settings
                && app.settings_focus == crate::agent::state::SettingsFocus::Theme
                && app.is_previewing_theme() =>
        {
            match app.commit_theme_preview() {
                Ok(kind) => {
                    app.status = format!("theme committed — {}", kind.label());
                }
                Err(e) => {
                    // The switch is already live in this session; only the
                    // preference file failed. Say exactly that.
                    app.status = format!("theme applied for this session, NOT saved: {e}");
                }
            }
            Ok(())
        }
        (KeyCode::Enter, _) if app.import_mode => {
            app.import_run();
            Ok(())
        }
        (KeyCode::Enter, _) => match app.view {
            View::Mcp => {
                app.mcp_spawn(crate::agent::state::McpOp::Inspect);
                Ok(())
            }
            View::Extensions => {
                app.ext_spawn(crate::agent::state::ExtOp::Inspect);
                Ok(())
            }
            View::Security => {
                app.security_doc();
                Ok(())
            }
            View::Terminal => {
                app.run_terminal().await;
                Ok(())
            }
            View::Search => app.run_search().await,
            View::Files => {
                app.files_enter().await;
                Ok(())
            }
            View::Models => {
                if app.model_search_active {
                    if app.model_section == crate::agent::state::ModelSection::Hub {
                        // Hub search box: marketplace rows (gate inside task).
                        app.run_hub_search().await;
                    } else if app.model_results.is_empty()
                        || app.model_results_query != app.model_query
                    {
                        app.run_model_search().await;
                    } else {
                        app.apply_selected_result().await;
                    }
                } else {
                    app.apply_selected_model().await;
                }
                Ok(())
            }
            View::Sessions => {
                if let Some(&i) = app.filtered_sessions().get(app.sel) {
                    app.select_session(i).await?;
                }
                Ok(())
            }
            View::Tasks => {
                let idx = app.filtered_tasks().get(app.sel).copied();
                if let Some(i) = idx.and_then(|i| app.tasks.get(i)) {
                    let t = i;
                    let mut lines = vec![
                        format!("id       {}", t.id),
                        format!("status   {:?}", t.status),
                        format!("session  {}", t.session_id),
                        format!("prompt   {}", t.prompt),
                        format!("turns    {}", t.turns.len()),
                    ];
                    if let Some(r) = &t.result {
                        lines.push(String::new());
                        lines.push("result:".into());
                        lines.extend(r.lines().map(str::to_string));
                    }
                    if let Some(e) = &t.error {
                        lines.push(format!("error    {e}"));
                    }
                    app.preview = Some(crate::agent::state::PreviewData {
                        title: format!("task {}", t.id),
                        lines,
                    });
                    app.preview_scroll = 0;
                    app.overlay = Some(Overlay::Preview);
                }
                Ok(())
            }
            View::Tools => {
                let idx = app.filtered_tools().get(app.sel).copied();
                if let Some(spec) = idx.and_then(|i| app.tools.get(i)) {
                    let pretty =
                        serde_json::to_string_pretty(spec).unwrap_or_else(|_| spec.name.clone());
                    app.preview = Some(crate::agent::state::PreviewData {
                        title: spec.name.clone(),
                        lines: pretty.lines().map(str::to_string).collect(),
                    });
                    app.preview_scroll = 0;
                    app.overlay = Some(Overlay::Preview);
                }
                Ok(())
            }
            _ => app.send().await,
        },
        // Filter editing MUST win over Files' Backspace = go up, otherwise a
        // filter-active picker ascends directories while typing.
        (KeyCode::Backspace, _) if app.filter_active => {
            app.list_filter.pop();
            app.filter_sel_reset();
            Ok(())
        }
        (KeyCode::Backspace, _) if app.input.is_empty() && app.view == View::Files => {
            app.files_up().await;
            Ok(())
        }
        (KeyCode::Backspace, _) if app.model_search_active => {
            app.model_query.pop();
            Ok(())
        }
        (KeyCode::Backspace, _) => {
            app.input.pop();
            Ok(())
        }
        (KeyCode::Delete, _) if app.filter_active => {
            app.list_filter.clear();
            app.filter_sel_reset();
            Ok(())
        }
        (KeyCode::Delete, _) if app.model_search_active => {
            app.model_query.clear();
            Ok(())
        }
        (KeyCode::Delete, _) => {
            app.input.clear();
            Ok(())
        }
        // ---- Settings THEME: arrows / jk preview live, ends do not wrap ----
        (KeyCode::Up, _)
            if app.view == View::Settings
                && app.settings_focus == crate::agent::state::SettingsFocus::Theme =>
        {
            theme_preview_step(app, -1);
            Ok(())
        }
        (KeyCode::Down, _)
            if app.view == View::Settings
                && app.settings_focus == crate::agent::state::SettingsFocus::Theme =>
        {
            theme_preview_step(app, 1);
            Ok(())
        }
        (KeyCode::Char('k'), _)
            if app.view == View::Settings
                && app.settings_focus == crate::agent::state::SettingsFocus::Theme
                && app.input.is_empty() =>
        {
            theme_preview_step(app, -1);
            Ok(())
        }
        (KeyCode::Char('j'), _)
            if app.view == View::Settings
                && app.settings_focus == crate::agent::state::SettingsFocus::Theme
                && app.input.is_empty() =>
        {
            theme_preview_step(app, 1);
            Ok(())
        }
        (KeyCode::Up, _) if app.view == View::Terminal && app.input.is_empty() => {
            app.term_scroll = app.term_scroll.saturating_add(1);
            Ok(())
        }
        (KeyCode::Down, _) if app.view == View::Terminal && app.input.is_empty() => {
            app.term_scroll = app.term_scroll.saturating_sub(1);
            Ok(())
        }
        (KeyCode::Up, _) if app.view != View::Chat => {
            let len = list_len(app);
            if len == 0 {
                app.view_scroll = app.view_scroll.saturating_add(1);
            } else {
                move_sel(app, -1, len);
            }
            app.models_nav_hook().await;
            Ok(())
        }
        (KeyCode::Down, _) if app.view != View::Chat => {
            let len = list_len(app);
            if len == 0 {
                app.view_scroll = app.view_scroll.saturating_sub(1);
            } else {
                move_sel(app, 1, len);
            }
            app.models_nav_hook().await;
            Ok(())
        }
        (KeyCode::PageUp, _) if app.view == View::Chat => {
            app.chat_scroll = app.chat_scroll.saturating_add(10);
            Ok(())
        }
        (KeyCode::PageDown, _) if app.view == View::Chat => {
            app.chat_scroll = app.chat_scroll.saturating_sub(10);
            Ok(())
        }
        (KeyCode::PageUp, _) if app.view == View::Terminal && app.input.is_empty() => {
            app.term_scroll = app.term_scroll.saturating_add(10);
            Ok(())
        }
        (KeyCode::PageDown, _) if app.view == View::Terminal && app.input.is_empty() => {
            app.term_scroll = app.term_scroll.saturating_sub(10);
            Ok(())
        }
        (KeyCode::PageUp, _) if app.view != View::Chat => {
            let len = list_len(app);
            if len == 0 {
                app.view_scroll = app.view_scroll.saturating_add(10);
            } else {
                move_sel(app, -10, len);
            }
            app.models_nav_hook().await;
            Ok(())
        }
        (KeyCode::PageDown, _) if app.view != View::Chat => {
            let len = list_len(app);
            if len == 0 {
                app.view_scroll = app.view_scroll.saturating_sub(10);
            } else {
                move_sel(app, 10, len);
            }
            app.models_nav_hook().await;
            Ok(())
        }
        (KeyCode::Home, _) if app.view == View::Terminal && app.input.is_empty() => {
            app.term_scroll = u16::MAX;
            Ok(())
        }
        (KeyCode::End, _) if app.view == View::Terminal && app.input.is_empty() => {
            app.term_scroll = 0;
            Ok(())
        }
        (KeyCode::Home, _) if app.view != View::Chat => {
            let len = list_len(app);
            if len > 0 {
                move_sel(app, -10_000, len);
            } else {
                app.sel = 0;
            }
            app.view_scroll = 0;
            Ok(())
        }
        (KeyCode::End, _) if app.view != View::Chat => {
            let len = list_len(app);
            if len == 0 {
                app.view_scroll = u16::MAX;
            } else {
                move_sel(app, 10_000, len);
            }
            Ok(())
        }
        (KeyCode::Up, _) if app.view == View::Chat => {
            app.chat_scroll = app.chat_scroll.saturating_add(3);
            Ok(())
        }
        (KeyCode::Down, _) if app.view == View::Chat => {
            app.chat_scroll = app.chat_scroll.saturating_sub(3);
            Ok(())
        }
        (KeyCode::F(1), _) | (KeyCode::Char('?'), _)
            if app.input.is_empty()
                && !app.model_search_active
                && !app.filter_active
                && !app.import_mode =>
        {
            app.overlay = Some(Overlay::Help);
            Ok(())
        }
        // ---- Extension lifecycle (single-char operator actions) ----
        (KeyCode::Char('v'), _)
            if app.view == View::Extensions
                && app.input.is_empty()
                && !app.filter_active
                && !app.op_edit
                && !app.ext_install =>
        {
            app.ext_spawn(crate::agent::state::ExtOp::Validate);
            Ok(())
        }
        (KeyCode::Char('s'), _)
            if app.view == View::Extensions
                && app.input.is_empty()
                && !app.filter_active
                && !app.op_edit
                && !app.ext_install =>
        {
            app.ext_spawn(crate::agent::state::ExtOp::Scan);
            Ok(())
        }
        (KeyCode::Char('a'), _)
            if app.view == View::Extensions
                && app.input.is_empty()
                && !app.filter_active
                && !app.op_edit
                && !app.ext_install =>
        {
            app.ext_spawn(crate::agent::state::ExtOp::Activate);
            Ok(())
        }
        (KeyCode::Char('d'), _)
            if app.view == View::Extensions
                && app.input.is_empty()
                && !app.filter_active
                && !app.op_edit
                && !app.ext_install =>
        {
            app.ext_spawn(crate::agent::state::ExtOp::Deactivate);
            Ok(())
        }
        (KeyCode::Char('x'), _)
            if app.view == View::Extensions
                && app.input.is_empty()
                && !app.filter_active
                && !app.op_edit
                && !app.ext_install =>
        {
            app.ext_spawn(crate::agent::state::ExtOp::Invalidate);
            Ok(())
        }
        (KeyCode::Char('i'), _)
            if app.view == View::Extensions
                && app.input.is_empty()
                && !app.filter_active
                && !app.op_edit
                && !app.ext_install =>
        {
            app.ext_install = true;
            app.input.clear();
            app.status =
                "install extension — type a package JSON path or https URL (GitHub raw), then Enter"
                    .into();
            Ok(())
        }
        // ---- Security control center ----
        (KeyCode::Char('1'), _)
            if app.view == View::Security
                && app.input.is_empty()
                && !app.filter_active
                && !app.policy_edit
                && !app.op_edit =>
        {
            let n = app.preset_rules(false).len();
            app.confirm = Some(crate::agent::state::Confirm::PolicyPreset {
                label: "read-only".into(),
                full: false,
            });
            app.status =
                format!("apply preset `read-only` ({n} allow rules from registered capabilities)? y = apply");
            Ok(())
        }
        (KeyCode::Char('2'), _)
            if app.view == View::Security
                && app.input.is_empty()
                && !app.filter_active
                && !app.policy_edit
                && !app.op_edit =>
        {
            let n = app.preset_rules(true).len();
            app.confirm = Some(crate::agent::state::Confirm::PolicyPreset {
                label: "full trust".into(),
                full: true,
            });
            app.status = format!(
                "apply preset `full trust` ({n} allow rules — every registered tool + capability)? y = apply"
            );
            Ok(())
        }
        (KeyCode::Char('n'), _)
            if app.view == View::Security
                && app.input.is_empty()
                && !app.filter_active
                && !app.policy_edit
                && !app.op_edit =>
        {
            app.policy_edit = true;
            app.input.clear();
            app.status =
                "new rule — type: <pattern> <allow|deny|ask> [scope] · Enter reviews · Esc cancels"
                    .into();
            Ok(())
        }
        (KeyCode::Char('l'), _)
            if app.view == View::Security
                && app.input.is_empty()
                && !app.filter_active
                && !app.policy_edit
                && !app.op_edit =>
        {
            app.security_audit();
            Ok(())
        }
        // ---- MCP connectivity manager (single-char operator actions) ----
        (KeyCode::Char('e'), _)
            if app.view == View::Mcp
                && app.input.is_empty()
                && !app.filter_active
                && !app.op_edit =>
        {
            let op = app
                .mcp_selected()
                .map(|r| {
                    if r.enabled {
                        crate::agent::state::McpOp::Disable
                    } else {
                        crate::agent::state::McpOp::Enable
                    }
                })
                .unwrap_or(crate::agent::state::McpOp::Enable);
            app.mcp_spawn(op);
            Ok(())
        }
        (KeyCode::Char('t'), _)
            if app.view == View::Mcp
                && app.input.is_empty()
                && !app.filter_active
                && !app.op_edit =>
        {
            if let Some(row) = app.mcp_selected() {
                let next = match row.trust.as_str() {
                    "untrusted" => "review",
                    "review" => "trusted",
                    "trusted" => "blocked",
                    _ => "untrusted",
                }
                .to_string();
                app.mcp_spawn(crate::agent::state::McpOp::Trust(next));
            } else {
                app.status = "no server selected — ↑/↓ first".into();
            }
            Ok(())
        }
        (KeyCode::Char('s'), _)
            if app.view == View::Mcp
                && app.input.is_empty()
                && !app.filter_active
                && !app.op_edit =>
        {
            app.mcp_spawn(crate::agent::state::McpOp::Scan);
            Ok(())
        }
        (KeyCode::Char('h'), _)
            if app.view == View::Mcp
                && app.input.is_empty()
                && !app.filter_active
                && !app.op_edit =>
        {
            app.mcp_spawn(crate::agent::state::McpOp::Health);
            Ok(())
        }
        (KeyCode::Char('o'), _)
            if matches!(app.view, View::Mcp | View::Extensions)
                && app.input.is_empty()
                && !app.filter_active
                && !app.op_edit
                && !app.ext_install =>
        {
            app.op_edit = true;
            app.input = app.operator_id.clone();
            app.status =
                "operator id — type, Enter saves, Esc cancels (recorded on every action)".into();
            Ok(())
        }
        (KeyCode::Char('i'), _)
            if app.view == View::Models
                && app.input.is_empty()
                && !app.model_search_active
                && !app.import_mode
                && !app.hub_filter_active =>
        {
            app.import_begin();
            Ok(())
        }
        // ---- Settings: Tab / BackTab move between INFO and THEME ----
        (KeyCode::Tab, _)
            if app.view == View::Settings
                && app.input.is_empty()
                && !app.filter_active
                && !app.op_edit
                && !app.policy_edit
                && !app.import_mode =>
        {
            app.cycle_settings_focus(true);
            Ok(())
        }
        (KeyCode::BackTab, _)
            if app.view == View::Settings
                && app.input.is_empty()
                && !app.filter_active
                && !app.op_edit
                && !app.policy_edit
                && !app.import_mode =>
        {
            app.cycle_settings_focus(false);
            Ok(())
        }
        // ---- Models sections: Tab / BackTab cycle the four panes ----
        (KeyCode::Tab, _)
            if app.view == View::Models
                && app.input.is_empty()
                && !app.model_search_active
                && !app.import_mode
                && !app.hub_filter_active
                && !app.filter_active =>
        {
            app.cycle_section(1).await;
            Ok(())
        }
        (KeyCode::BackTab, _)
            if app.view == View::Models
                && app.input.is_empty()
                && !app.model_search_active
                && !app.import_mode
                && !app.hub_filter_active
                && !app.filter_active =>
        {
            app.cycle_section(-1).await;
            Ok(())
        }
        // ---- Hub section: `p` pipeline facet, `d` detail overlay ----
        (KeyCode::Char('p'), _)
            if app.view == View::Models
                && app.model_section == crate::agent::state::ModelSection::Hub
                && app.input.is_empty()
                && !app.model_search_active
                && !app.import_mode
                && !app.hub_filter_active =>
        {
            app.cycle_facet();
            Ok(())
        }
        (KeyCode::Char('d'), _)
            if app.view == View::Models
                && app.model_section == crate::agent::state::ModelSection::Hub
                && app.input.is_empty()
                && !app.model_search_active
                && !app.import_mode
                && !app.hub_filter_active =>
        {
            app.open_hub_detail();
            Ok(())
        }
        // ---- Hub section: `f` filter overlay, `[` / `]` page the listing ----
        (KeyCode::Char('f'), _)
            if app.view == View::Models
                && app.model_section == crate::agent::state::ModelSection::Hub
                && app.input.is_empty()
                && !app.model_search_active
                && !app.import_mode
                && !app.hub_filter_active =>
        {
            app.hub_filter_open();
            Ok(())
        }
        (KeyCode::Char(']'), _)
            if app.view == View::Models
                && app.model_section == crate::agent::state::ModelSection::Hub
                && app.input.is_empty()
                && !app.model_search_active
                && !app.import_mode
                && !app.hub_filter_active =>
        {
            app.hub_page(1).await;
            Ok(())
        }
        (KeyCode::Char('['), _)
            if app.view == View::Models
                && app.model_section == crate::agent::state::ModelSection::Hub
                && app.input.is_empty()
                && !app.model_search_active
                && !app.import_mode
                && !app.hub_filter_active =>
        {
            app.hub_page(-1).await;
            Ok(())
        }
        // ---- Models: `s` drop-in scan of models/<category>/ ----
        (KeyCode::Char('s'), _)
            if app.view == View::Models
                && app.input.is_empty()
                && !app.model_search_active
                && !app.import_mode
                && !app.hub_filter_active =>
        {
            app.dropin_scan();
            Ok(())
        }
        (KeyCode::Char('d'), _)
            if app.view == View::Sessions && app.input.is_empty() && !app.filter_active =>
        {
            app.sessions_delete_request();
            Ok(())
        }
        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
            if app.model_search_active {
                app.model_query.push(c);
            } else if app.filter_active {
                app.list_filter.push(c);
                app.filter_sel_reset();
            } else if !app.hub_filter_active {
                app.input.push(c);
            }
            Ok(())
        }
        // ---- focus ring fallback ----
        //
        // Placed last on purpose. `Tab` already means "next thing" inside a
        // resource card, a Settings section, and a Models pane, and those
        // within-surface meanings are more specific than anything a global ring
        // could offer. Only when no surface claimed the key does focus move
        // between surfaces — so this adds navigation without redefining any
        // existing binding.
        (KeyCode::Tab, m) => {
            handle_focus_key(app, KeyCode::Tab, m.contains(KeyModifiers::SHIFT));
            Ok(())
        }
        (KeyCode::BackTab, _) => {
            handle_focus_key(app, KeyCode::BackTab, true);
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Called after a rail jump: load the real data for the destination view.
async fn on_view_enter(app: &mut AgentApp) {
    if let Err(e) = app.refresh_view().await {
        app.status = format!("refresh failed: {e}");
    }
}

async fn overlay_key(app: &mut AgentApp, key: KeyEvent, overlay: Overlay) -> Result<(), AppError> {
    // One resolver, consulted before any overlay-specific branch.
    //
    // Tab/Shift+Tab and the arrow keys mean the same thing in every modal, so
    // they are answered from the scope rather than in nine separate `match`
    // arms. That is the whole point of the scope: a key's meaning is a property
    // of the modal, and the background's meaning cannot leak in because the
    // background is unreachable once this function is entered.
    let scope = app.modal_scope();
    if let Some(scope) = scope.as_ref() {
        match key.code {
            KeyCode::Tab if scope.keys.tab => {
                app.move_focus(Move::Forward);
                return Ok(());
            }
            KeyCode::BackTab if scope.keys.tab => {
                app.move_focus(Move::Backward);
                return Ok(());
            }
            // Arrows are left to the overlay when it declares them: several
            // overlays use arrows for a text cursor rather than a list, and a
            // ring step would fight that.
            _ => {}
        }
    }

    match overlay {
        Overlay::Help => match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::F(1) | KeyCode::Char('q') => {
                app.overlay = None;
                app.help_scroll = 0;
                Ok(())
            }
            KeyCode::Up => {
                app.help_scroll = app.help_scroll.saturating_add(1);
                Ok(())
            }
            KeyCode::Down => {
                app.help_scroll = app.help_scroll.saturating_sub(1);
                Ok(())
            }
            KeyCode::PageUp => {
                app.help_scroll = app.help_scroll.saturating_add(10);
                Ok(())
            }
            KeyCode::PageDown => {
                app.help_scroll = app.help_scroll.saturating_sub(10);
                Ok(())
            }
            _ => Ok(()),
        },
        Overlay::Preview => match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                app.overlay = None;
                app.preview = None;
                app.preview_scroll = 0;
                Ok(())
            }
            KeyCode::Up => {
                app.preview_scroll = app.preview_scroll.saturating_add(1);
                Ok(())
            }
            KeyCode::Down => {
                app.preview_scroll = app.preview_scroll.saturating_sub(1);
                Ok(())
            }
            KeyCode::PageUp => {
                app.preview_scroll = app.preview_scroll.saturating_add(10);
                Ok(())
            }
            KeyCode::PageDown => {
                app.preview_scroll = app.preview_scroll.saturating_sub(10);
                Ok(())
            }
            _ => Ok(()),
        },
        Overlay::Palette => {
            let matches = app.palette_matches();
            match key.code {
                KeyCode::Esc => {
                    app.overlay = None;
                    Ok(())
                }
                KeyCode::Backspace => {
                    app.palette.pop();
                    app.palette_sel = 0;
                    Ok(())
                }
                KeyCode::Up => {
                    app.palette_sel = app.palette_sel.saturating_sub(1);
                    Ok(())
                }
                KeyCode::Down => {
                    app.palette_sel = app.palette_sel.saturating_add(1);
                    if app.palette_sel >= matches.len().max(1) {
                        app.palette_sel = matches.len().saturating_sub(1);
                    }
                    Ok(())
                }
                KeyCode::Enter => {
                    // No match: the query selects nothing, so leave the palette
                    // rather than jumping to whatever happens to be at index 0.
                    match matches.get(app.palette_sel).copied() {
                        Some(item) => {
                            jump_from_palette(app, item).await;
                        }
                        None => app.overlay = None,
                    }
                    Ok(())
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.palette.push(c);
                    app.palette_sel = 0;
                    Ok(())
                }
                _ => Ok(()),
            }
        }
        Overlay::Mode => match key.code {
            KeyCode::Esc => {
                app.overlay = None;
                Ok(())
            }
            KeyCode::Enter => {
                app.overlay = None;
                let to = if app.exec_mode == "sovereign" {
                    "provisioning"
                } else {
                    "sovereign"
                };
                app.confirm = Some(crate::agent::state::Confirm::Mode { to: to.into() });
                app.status = format!("switch mode → {to}? y = switch · any other key cancels");
                Ok(())
            }
            _ => Ok(()),
        },
        Overlay::Model => match key.code {
            KeyCode::Esc => {
                app.overlay = None;
                Ok(())
            }
            KeyCode::Enter => {
                app.apply_model().await;
                Ok(())
            }
            KeyCode::Backspace => {
                app.model_input.pop();
                Ok(())
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.model_input.push(c);
                Ok(())
            }
            _ => Ok(()),
        },
        Overlay::Setup => {
            let typing = app.setup.as_ref().is_some_and(|s| s.hf_typing);
            if typing {
                match key.code {
                    KeyCode::Esc => {
                        if let Some(s) = app.setup.as_mut() {
                            s.hf_typing = false;
                        }
                        Ok(())
                    }
                    KeyCode::Enter => {
                        app.setup_hf_search().await;
                        Ok(())
                    }
                    KeyCode::Backspace => {
                        if let Some(s) = app.setup.as_mut() {
                            s.hf_query.pop();
                        }
                        Ok(())
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if let Some(s) = app.setup.as_mut() {
                            s.hf_query.push(c);
                        }
                        Ok(())
                    }
                    _ => Ok(()),
                }
            } else {
                match key.code {
                    KeyCode::Esc => {
                        app.overlay = None;
                        app.status = "setup closed — /setup reopens it".into();
                        Ok(())
                    }
                    KeyCode::Enter => {
                        app.setup_enter().await;
                        Ok(())
                    }
                    KeyCode::Right => {
                        app.setup_enter().await;
                        Ok(())
                    }
                    KeyCode::Left => {
                        app.setup_back();
                        Ok(())
                    }
                    KeyCode::Up => {
                        app.setup_move(-1);
                        Ok(())
                    }
                    KeyCode::Down => {
                        app.setup_move(1);
                        Ok(())
                    }
                    KeyCode::Char('f')
                        if app
                            .setup
                            .as_ref()
                            .is_some_and(|s| s.step == crate::agent::setup::SetupStep::Models) =>
                    {
                        if let Some(s) = app.setup.as_mut() {
                            s.hf_typing = true;
                        }
                        Ok(())
                    }
                    _ => Ok(()),
                }
            }
        }
        Overlay::Sessions => match key.code {
            KeyCode::Esc => {
                app.overlay = None;
                Ok(())
            }
            KeyCode::Up => {
                app.sel = app.sel.saturating_sub(1);
                Ok(())
            }
            KeyCode::Down => {
                app.sel = app.sel.saturating_add(1);
                if app.sel >= app.sessions.len().max(1) {
                    app.sel = app.sessions.len().saturating_sub(1);
                }
                Ok(())
            }
            KeyCode::Enter => {
                app.select_session(app.sel).await?;
                Ok(())
            }
            KeyCode::Char('d') => {
                app.sessions_delete_request();
                Ok(())
            }
            _ => Ok(()),
        },
    }
}

fn is_ctrl(key: KeyEvent, c: char) -> bool {
    key.code == KeyCode::Char(c) && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// Length of the current view's selectable list (for arrow navigation).
fn list_len(app: &AgentApp) -> usize {
    match app.view {
        View::Files => {
            if app.filter_active && !app.list_filter.is_empty() {
                app.filtered_files().len()
            } else {
                app.files.len()
            }
        }
        View::Search => app.search_hits.len(),
        View::Tasks => app.filtered_tasks().len(),
        View::Sessions => app.filtered_sessions().len(),
        View::Tools => app.filtered_tools().len(),
        View::Mcp => app.filtered_mcp().len(),
        View::Extensions => app.filtered_ext().len(),
        View::Security => app.filtered_security().len(),
        View::Models => {
            if app.model_search_active {
                app.model_results.len()
            } else {
                app.models_section_len()
            }
        }
        _ => 0,
    }
}

/// View-local list navigation shared by the view renders (j/k + arrows).
pub fn move_sel(app: &mut AgentApp, delta: i32, len: usize) {
    // Files with an active filter: `sel` is the ORIGINAL row index (Enter
    // descends by it), so move through the filtered index list and land on a
    // visible row — never on an entry the filter hid.
    if app.view == View::Files && app.filter_active && !app.list_filter.is_empty() {
        let vis = app.filtered_files();
        if vis.is_empty() {
            app.sel = 0;
            return;
        }
        let cur = vis.iter().position(|&i| i == app.sel).unwrap_or(0) as i32;
        let next = (cur + delta).clamp(0, vis.len() as i32 - 1);
        app.sel = vis[next as usize];
        return;
    }
    if len == 0 {
        app.sel = 0;
        return;
    }
    let cur = app.sel as i32;
    let next = (cur + delta).clamp(0, len as i32 - 1);
    app.sel = next as usize;
}

/// Move the Settings theme cursor and preview the result immediately.
///
/// The list does not wrap: pressing Up on the first theme leaves it alone and
/// says so, instead of silently jumping to the last one.
fn theme_preview_step(app: &mut AgentApp, delta: isize) {
    if app.preview_theme(delta) {
        app.status = format!(
            "previewing {} — Enter to commit",
            app.active_theme().kind.label()
        );
    } else {
        app.status = "end of theme list — nothing further".into();
    }
}

/// Run the operator's chosen resource action.
///
/// Every branch keeps the session and the refused prompt intact: a resource
/// refusal must never cost the operator their work. The override branch is the
/// only one that bypasses a limit, and it is refused outright unless the
/// deployment allows it — the card never offers the key otherwise.
pub(crate) async fn run_resource_action(
    app: &mut AgentApp,
    action: ResourceAction,
) -> Result<(), AppError> {
    match action {
        ResourceAction::Retry => {
            let prompt = app.resource_prompt.clone();
            match prompt.filter(|p| !p.is_empty()) {
                Some(p) => {
                    app.clear_resource_card();
                    app.resource_prompt = None;
                    app.input = p;
                    app.send().await?;
                    app.status = "retrying — the same prompt, once".into();
                }
                None => {
                    app.status =
                        "no refused prompt to retry — type a message and press Enter".into();
                }
            }
        }
        ResourceAction::Wait => {
            app.start_resource_wait();
        }
        ResourceAction::SwitchModel => {
            app.overlay = Some(Overlay::Model);
            app.status = "switch model — pick a smaller tag to fit the host".into();
        }
        ResourceAction::Details => {
            app.resource_detail = !app.resource_detail;
        }
        ResourceAction::Override => {
            // The runtime is the authority, not this branch. It checks operator
            // policy, writes the audit row fail-closed, and only then arms the
            // one-shot slot. The UI's job is to ask and report the answer.
            //
            // This deliberately does NOT call `resource_gate.grant_override()`
            // directly. It used to, which made the TUI the thing that decided a
            // declared limit could be bypassed: one config flag was the whole
            // check, the operator identity was the literal string "tui", and no
            // audit row was written at all.
            match app
                .runtime
                .grant_resource_override(
                    "operator chose one-shot override on the RESOURCE LIMIT card",
                    "tui-operator",
                )
                .await
            {
                Ok(_) => {
                    let prompt = app.resource_prompt.clone();
                    app.clear_resource_card();
                    app.status = "one-shot override granted and audited — re-run the task".into();
                    if let Some(p) = prompt.filter(|p| !p.is_empty()) {
                        app.input = p;
                    }
                }
                Err(e) => {
                    // Fail closed: nothing is armed, and the reason is shown
                    // rather than swallowed.
                    app.status = format!("override refused: {e}");
                }
            }
        }
        ResourceAction::Cancel => {
            app.clear_resource_card();
            app.status = "resource limit dismissed — the session stays open".into();
        }
    }
    Ok(())
}
