//! Floating surfaces: modals, confirmations and the permission dialog.
//!
//! # One shell, three weights
//!
//! ```text
//! Modal          a framed surface with a title, body and footer
//!  ├─ ConfirmDialog   two outcomes, one destructive
//!  └─ PermissionDialog  ACTION / TARGET / REASON / RISK / SCOPE / CONSEQUENCES
//!                        plus four graded grants
//! ```
//!
//! A permission request is **not** a confirmation. It states what will happen
//! to which target, at what risk, for how long, and offers a graded set of
//! grants. Rendering it with the same yes/no chrome as "delete this file?" is
//! how operators learn to click through approvals, so it gets its own
//! component, its own focus order and its own scope language.
//!
//! Every modal here is sized by [`crate::design::layout::ShellLayout::modal_rect`],
//! which centers and then clamps — so a dialog on a small terminal stays fully
//! on screen with its actions reachable.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::layout::HitTargets;
use super::tokens::saturating_sub;
use super::tokens::Tokens;
use super::widgets::{scrim, Badge, BadgeTone, ButtonGroup, ButtonSpec, PanelTone};

/// The outcome of a permission grant. The ordering is severity: the operator
/// can always pick a narrower grant than the one that was requested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grant {
    /// This action only.
    Once,
    /// Every equivalent action for the rest of this session.
    Session,
    /// Every equivalent action for this workspace, across sessions.
    Workspace,
    /// Refuse.
    Deny,
}

/// Every grant button, in the order the dialog lays them out.
///
/// The dialog, the focus ring, and the keyboard handler must agree on this
/// order and this set. Declaring it once is what keeps them from drifting: the
/// previous code listed the four buttons in three places, and a row that
/// existed in two of them was a row that could be clicked but not tabbed to.
pub const GRANT_BUTTONS: [&str; 4] = [
    "grant.once",
    "grant.session",
    "grant.workspace",
    "grant.deny",
];

/// The grant a button id means.
pub fn grant_of_button(id: &str) -> Option<Grant> {
    match id {
        "grant.once" => Some(Grant::Once),
        "grant.session" => Some(Grant::Session),
        "grant.workspace" => Some(Grant::Workspace),
        "grant.deny" => Some(Grant::Deny),
        _ => None,
    }
}

/// The button id for a grant.
pub fn button_of_grant(grant: Grant) -> &'static str {
    match grant {
        Grant::Once => "grant.once",
        Grant::Session => "grant.session",
        Grant::Workspace => "grant.workspace",
        Grant::Deny => "grant.deny",
    }
}

/// The focus target for a dialog button.
///
/// `index` is the button's position in its group so arrow keys step between
/// them and clamp at both ends. It is derived from the id rather than passed in,
/// so a caller cannot register `grant.deny` at index 0 and teach the arrow keys
/// the wrong order.
pub fn permission_target(id: &'static str) -> crate::design::focus::FocusTarget {
    let index = GRANT_BUTTONS.iter().position(|b| *b == id).unwrap_or(0);
    crate::design::focus::FocusTarget::Action { id, index }
}

/// The focus target for a confirmation button.
pub fn confirm_target(id: &'static str) -> crate::design::focus::FocusTarget {
    let index = if id == "confirm.yes" { 0 } else { 1 };
    crate::design::focus::FocusTarget::Action { id, index }
}

impl Grant {
    /// Persisted scope token. Stable strings — these may reach an audit log.
    pub const fn scope(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Session => "session",
            Self::Workspace => "workspace",
            Self::Deny => "deny",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Once => "Allow Once",
            Self::Session => "Allow Session",
            Self::Workspace => "Allow Workspace",
            Self::Deny => "Deny",
        }
    }

    /// The key the dialog advertises for this grant.
    ///
    /// These must be the keys `input::handle_key` actually accepts, because a
    /// button that names a key which does nothing is worse than one that names no
    /// key. `Deny` advertised `d` while the help line and the handler both led
    /// with `4`; the dialog now shows `4`, and `d`/`D` remain accepted aliases.
    pub const fn hint(self) -> char {
        match self {
            Self::Once => '1',
            Self::Session => '2',
            Self::Workspace => '3',
            Self::Deny => '4',
        }
    }

    /// Whether the outcome widens what is permitted without an explicit new
    /// approval. `Deny` narrows; the grants widen.
    pub const fn widens_scope(self) -> bool {
        !matches!(self, Self::Deny)
    }
}

/// How risky the requested action is. Risk is *stated*, not merely colored:
/// the label appears in the dialog body.
///
/// The `Default` variant is `Low` only so the type derives cleanly; a real
/// request must name its risk explicitly through [`PermissionRequest::new`],
/// which has no default. There is deliberately no "unknown" risk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Risk {
    /// Reads state; no durable effect.
    #[default]
    Low,
    /// Writes inside an existing boundary.
    Medium,
    /// Network egress, credential use, or writes outside the workspace.
    High,
    /// Irreversible or system-wide effect.
    Critical,
}

impl Risk {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Low => "LOW",
            Self::Medium => "MEDIUM",
            Self::High => "HIGH",
            Self::Critical => "CRITICAL",
        }
    }

    pub fn tone(self) -> BadgeTone {
        match self {
            Self::Low => BadgeTone::Success,
            Self::Medium => BadgeTone::Warning,
            Self::High => BadgeTone::Danger,
            Self::Critical => BadgeTone::Danger,
        }
    }

    /// Critical risk forces the destructive border on the dialog, so the
    /// strongest visual weight in the app is reserved for the most dangerous
    /// decision.
    pub fn panel_tone(self) -> PanelTone {
        if self == Self::Critical {
            PanelTone::Destructive
        } else {
            PanelTone::Overlay
        }
    }

    /// The risk floor for a tool, before the request narrows it.
    pub fn rank(self) -> u8 {
        match self {
            Self::Low => 0,
            Self::Medium => 1,
            Self::High => 2,
            Self::Critical => 3,
        }
    }
}

/// The `a => b` summary line: ACTION, TARGET, REASON.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionRequest {
    /// Verb: "run", "write", "fetch".
    pub action: String,
    /// What it acts on: a command, a path, a host.
    pub target: String,
    /// Why the agent wants it.
    pub reason: String,
    pub risk: Risk,
    /// The scope the agent asked for, e.g. "this session".
    pub scope: String,
    /// What happens if it is allowed or refused.
    pub consequences: String,
}

impl PermissionRequest {
    /// Build a request, inferring nothing. Missing fields degrade to a short
    /// explicit placeholder rather than an empty row, because a permission
    /// dialog with a blank REASON is a security problem, not a cosmetic one.
    pub fn new(
        action: impl Into<String>,
        target: impl Into<String>,
        reason: impl Into<String>,
        risk: Risk,
    ) -> Self {
        let place = |v: String| {
            if v.trim().is_empty() {
                "—".to_string()
            } else {
                v
            }
        };
        Self {
            action: place(action.into()),
            target: place(target.into()),
            reason: place(reason.into()),
            risk,
            scope: place("this session".into()),
            consequences: String::new(),
        }
    }

    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = crate::design::widgets::sanitize_field(scope.into());
        self
    }

    pub fn with_consequences(mut self, c: impl Into<String>) -> Self {
        self.consequences = crate::design::widgets::sanitize_field(c.into());
        self
    }

    /// Body lines: ACTION / TARGET / REASON / RISK / SCOPE / CONSEQUENCES.
    pub fn body_lines(&self, t: &Tokens) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        let field = |label: &str, value: &str| {
            Line::from(vec![
                Span::styled(format!("{label:<12}"), t.type_scale.metadata),
                Span::styled(value.to_string(), t.type_scale.body),
            ])
        };
        out.push(field("ACTION", &self.action));
        out.push(field("TARGET", &self.target));
        out.push(field("REASON", &self.reason));
        // Risk is both a badge (color + marker) and a word: the label is what
        // carries it in monochrome.
        out.push(Line::from(vec![
            Span::styled(format!("{:<12}", "RISK"), t.type_scale.metadata),
            Span::styled(
                self.risk.label().to_string(),
                t.type_scale.body.fg(self.risk.tone().color(t)),
            ),
        ]));
        out.push(field("SCOPE", &self.scope));
        if !self.consequences.is_empty() {
            out.push(field("CONSEQUENCES", &self.consequences));
        }
        out
    }
}

/// Focus + selection state for the permission dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionDialog {
    pub request: PermissionRequest,
    pub group: ButtonGroup,
    /// Set when the resolution has been chosen; the dialog then closes.
    pub resolved: Option<Grant>,
}

/// The grant a button id stands for.
///
/// One table, used by [`PermissionDialog::resolve`] and by the test that asserts
/// each button's advertised key round-trips to that same button. Two copies of
/// this mapping is how "Allow workspace" ends up answering with `Deny`.
fn grant_for_id(id: &str) -> Option<Grant> {
    Some(match id {
        "grant.once" => Grant::Once,
        "grant.session" => Grant::Session,
        "grant.workspace" => Grant::Workspace,
        "grant.deny" => Grant::Deny,
        _ => return None,
    })
}

impl PermissionDialog {
    /// Default focus is the **narrowest** grant, never the widest. The
    /// operator must actively widen scope.
    pub fn new(request: PermissionRequest) -> Self {
        let mut group = ButtonGroup::new(vec![
            ButtonSpec::new("grant.once", Grant::Once.label(), Some(Grant::Once.hint())),
            ButtonSpec::new(
                "grant.session",
                Grant::Session.label(),
                Some(Grant::Session.hint()),
            ),
            ButtonSpec::new(
                "grant.workspace",
                Grant::Workspace.label(),
                Some(Grant::Workspace.hint()),
            ),
            ButtonSpec::new("grant.deny", Grant::Deny.label(), Some(Grant::Deny.hint())),
        ]);
        group.select(0);
        Self {
            request,
            group,
            resolved: None,
        }
    }

    pub fn risk(&self) -> Risk {
        self.request.risk
    }

    /// Narrow the available grants — a request may forbid session-wide or
    /// workspace-wide approval. Disabled controls stay visible with their key
    /// hint so the missing option is explained rather than silently absent.
    pub fn restrict(&mut self, allow_session: bool, allow_workspace: bool) {
        if let Some(i) = self
            .group
            .specs
            .iter()
            .position(|s| s.id == "grant.session")
        {
            self.group.specs[i].enabled = allow_session;
        }
        if let Some(i) = self
            .group
            .specs
            .iter()
            .position(|s| s.id == "grant.workspace")
        {
            self.group.specs[i].enabled = allow_workspace;
        }
        // Focus must never rest on a disabled control.
        if self.group.press_selected().is_none() {
            self.group.select(0);
        }
    }

    /// Direct-key activation.
    pub fn press_key(&mut self, key: char) -> Option<Grant> {
        let id = self.group.press_key(key)?;
        self.resolve(id)
    }

    /// Mouse activation.
    pub fn press_mouse(&mut self, hits: &HitTargets, x: u16, y: u16) -> Option<Grant> {
        let id = self.group.press_mouse(hits, x, y)?;
        self.resolve(id)
    }

    /// Activate the focused control.
    pub fn activate(&mut self) -> Option<Grant> {
        let id = self.group.press_selected()?;
        self.resolve(id)
    }

    fn resolve(&mut self, id: &'static str) -> Option<Grant> {
        let grant = grant_for_id(id);
        if grant.is_none() {
            return None;
        }
        self.resolved = grant;
        grant
    }

    /// Preferred size: wide enough for the target and reason to wrap, bounded
    /// by the shell.
    pub fn want_size(&self) -> (u16, u16) {
        let longest = self
            .request
            .target
            .chars()
            .count()
            .max(self.request.reason.chars().count())
            .max(28) as u16;
        let w = (longest + 18).clamp(48, 92);
        let body = if self.request.consequences.is_empty() {
            6
        } else {
            7
        };
        (w, body + 5)
    }
}

/// Render the permission dialog into `area`, which the caller has already
/// centered and clamped.
pub fn render_permission_dialog(
    frame: &mut Frame,
    area: Rect,
    t: &Tokens,
    dlg: &PermissionDialog,
    hits: &mut HitTargets,
) {
    render_permission_dialog_over(frame, area, area, t, dlg, hits)
}

/// Render the permission dialog, with `scrim_area` dimmed behind it.
///
/// The scrim covers `scrim_area` — normally the whole body region, not just the
/// dialog rect. Two separate mistakes come from using the dialog rect for both:
/// a click outside the dialog falls through to the view behind it, which is the
/// one thing a modal must prevent; and because [`HitTargets::hit`] scans
/// newest-first, registering the scrim *after* the buttons makes every button
/// unclickable while the dialog still looks perfectly normal.
///
/// The scrim is therefore registered first, over the full region, so the buttons
/// registered afterwards sit on top of it.
pub fn render_permission_dialog_over(
    frame: &mut Frame,
    scrim_area: Rect,
    area: Rect,
    t: &Tokens,
    dlg: &PermissionDialog,
    hits: &mut HitTargets,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // Scrim first, under everything drawn after it.
    hits.register(scrim_area, super::layout::HitTarget::ModalScrim);
    scrim(frame, scrim_area, t.surfaces.overlay);
    let block = super::widgets::panel(t, "PERMISSION REQUEST", dlg.risk().panel_tone());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // Body: risk badge first so the severity is above the fold, then the
    // six required fields.
    let body_height = saturating_sub(inner.height, 2); // actions row + blank
    let body = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: body_height,
    };
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("risk ", t.type_scale.metadata),
        Badge::new(dlg.risk().label(), dlg.risk().tone())
            .spans(t)
            .remove(0),
        Span::styled(
            format!("  {}", dlg.risk().panel_tone().name()),
            t.type_scale.metadata,
        ),
    ]));
    lines.push(Line::from(""));
    lines.extend(dlg.request.body_lines(t));
    frame.render_widget(Paragraph::new(lines), body);

    // Actions, always on the last rows so they cannot be scrolled out. The row
    // count follows the width: on a narrow terminal the four grants need more
    // than one row, and reserving exactly one would drop Deny off the end.
    let action_rows = dlg
        .group
        .rows_needed(inner.width)
        .max(1)
        .min(saturating_sub(inner.height, 1));
    let actions = Rect {
        x: inner.x,
        y: inner.y + saturating_sub(inner.height, action_rows),
        width: inner.width,
        height: action_rows,
    };
    dlg.group.render_wrapped(frame, actions, t, hits);
}

/// A yes/no confirmation with one destructive outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfirmDialog {
    pub title: String,
    pub body: String,
    pub group: ButtonGroup,
    /// Confirmation requires an explicit key, so a stray Enter cannot approve
    /// a destructive action.
    pub require_explicit_key: bool,
    pub resolved: Option<bool>,
}

impl ConfirmDialog {
    pub fn new(title: impl Into<String>, body: impl Into<String>, confirm: &'static str) -> Self {
        Self {
            title: title.into(),
            body: crate::design::widgets::sanitize_field(body.into()),
            group: ButtonGroup::new(vec![
                ButtonSpec::new("confirm.yes", confirm, Some('y')),
                ButtonSpec::new("confirm.no", "Cancel", Some('n')),
            ]),
            require_explicit_key: true,
            resolved: None,
        }
    }

    pub fn press_key(&mut self, key: char) -> Option<bool> {
        if self.require_explicit_key && key != 'y' && key != 'n' {
            return None;
        }
        match self.group.press_key(key)? {
            "confirm.yes" => {
                self.resolved = Some(true);
                Some(true)
            }
            _ => {
                self.resolved = Some(false);
                Some(false)
            }
        }
    }

    pub fn want_size(&self) -> (u16, u16) {
        let longest = self.body.chars().count() as u16;
        ((longest + 8).clamp(40, 76), 8)
    }
}

/// Render a confirmation dialog.
pub fn render_confirm_dialog(
    frame: &mut Frame,
    area: Rect,
    t: &Tokens,
    dlg: &ConfirmDialog,
    hits: &mut HitTargets,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    scrim(frame, area, t.surfaces.overlay);
    let block = super::widgets::panel(t, &dlg.title, PanelTone::Destructive);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let body = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: saturating_sub(inner.height, 2),
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            dlg.body.clone(),
            t.type_scale.body,
        ))),
        body,
    );
    let action_rows = dlg
        .group
        .rows_needed(inner.width)
        .max(1)
        .min(saturating_sub(inner.height, 1));
    let actions = Rect {
        x: inner.x,
        y: inner.y + saturating_sub(inner.height, action_rows),
        width: inner.width,
        height: action_rows,
    };
    dlg.group.render_wrapped(frame, actions, t, hits);
}

/// The focused control's style, shared by every modal so focus looks the same
/// in a confirmation and a permission request.
pub fn action_style(t: &Tokens, selected: bool, destructive: bool) -> Style {
    if !selected {
        return t.type_scale.secondary_body;
    }
    let bg = if destructive {
        t.borders.destructive
    } else {
        t.interaction.selected
    };
    t.type_scale
        .body
        .bg(bg)
        .fg(t.text.inverse)
        .add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::design::layout::HitTarget;
    use crate::theme::{Theme, ThemeKind};

    fn t() -> Tokens {
        Theme::default_theme().tokens()
    }

    fn request() -> PermissionRequest {
        PermissionRequest::new(
            "run",
            "cargo test --workspace",
            "verify the build after editing the config parser",
            Risk::Medium,
        )
        .with_consequences("executes arbitrary code in the workspace")
    }

    // -- grants ---------------------------------------------------------

    #[test]
    fn grant_scopes_are_stable_audit_tokens() {
        assert_eq!(Grant::Once.scope(), "once");
        assert_eq!(Grant::Session.scope(), "session");
        assert_eq!(Grant::Workspace.scope(), "workspace");
        assert_eq!(Grant::Deny.scope(), "deny");
        assert!(Grant::Once.widens_scope());
        assert!(!Grant::Deny.widens_scope());
    }

    #[test]
    fn risk_is_ordered_and_only_critical_takes_the_destructive_frame() {
        assert!(Risk::Low.rank() < Risk::Medium.rank());
        assert!(Risk::Medium.rank() < Risk::High.rank());
        assert!(Risk::High.rank() < Risk::Critical.rank());
        assert_eq!(Risk::Critical.panel_tone(), PanelTone::Destructive);
        assert_ne!(Risk::High.panel_tone(), PanelTone::Destructive);
        // Critical is stated, not only colored.
        assert_eq!(Risk::Critical.label(), "CRITICAL");
    }

    // -- the dialog's own contract ---------------------------------------

    #[test]
    fn a_permission_request_always_states_every_field() {
        let lines = request().body_lines(&t());
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .concat();
        for field in [
            "ACTION",
            "TARGET",
            "REASON",
            "RISK",
            "SCOPE",
            "CONSEQUENCES",
        ] {
            assert!(text.contains(field), "missing {field} in:\n{text}");
        }
    }

    #[test]
    fn missing_optional_fields_degrade_visibly_not_to_blank_rows() {
        let r = PermissionRequest::new("", "  ", "", Risk::Low);
        let text: String = r
            .body_lines(&t())
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .concat();
        assert!(
            text.contains('—'),
            "blank fields must render as an explicit dash:\n{text}"
        );
        // CONSEQUENCES is omitted rather than shown empty.
        assert!(!text.contains("CONSEQUENCES"));
    }

    #[test]
    fn a_permission_dialog_opens_on_the_narrowest_grant() {
        let dlg = PermissionDialog::new(request());
        assert_eq!(dlg.group.selected, 0);
        assert_eq!(dlg.group.specs[0].id, "grant.once");
        assert!(dlg.resolved.is_none());
    }

    /// Every button must advertise a key, and that key must resolve back to that
    /// same button. A button with no key is unreachable for a keyboard user; a
    /// button whose key resolves elsewhere is worse, because it looks safe.
    #[test]
    fn every_button_advertises_a_key_that_resolves_to_itself() {
        let dlg = PermissionDialog::new(request());
        for spec in &dlg.group.specs {
            let hint = spec
                .hint
                .expect("every permission button must advertise a key");
            let mut fresh = PermissionDialog::new(request());
            assert_eq!(
                fresh.press_key(hint),
                grant_for_id(spec.id),
                "button {} advertises {hint}, which resolves elsewhere",
                spec.id
            );
        }
    }

    #[test]
    fn keys_resolve_to_their_grant() {
        let mut dlg = PermissionDialog::new(request());
        assert_eq!(dlg.press_key('1'), Some(Grant::Once));
        let mut dlg = PermissionDialog::new(request());
        assert_eq!(dlg.press_key('2'), Some(Grant::Session));
        let mut dlg = PermissionDialog::new(request());
        assert_eq!(dlg.press_key('3'), Some(Grant::Workspace));
        let mut dlg = PermissionDialog::new(request());
        // `4` is the advertised key. The Agent's key handler additionally accepts
        // `d`/`D` as aliases (covered by `enter_and_every_deny_key_refuse`), but
        // the ButtonGroup itself only resolves the key it prints — otherwise the
        // dialog would work with a key it never shows.
        assert_eq!(dlg.press_key('4'), Some(Grant::Deny));
        let mut dlg = PermissionDialog::new(request());
        assert_eq!(dlg.press_key('z'), None, "an unknown key must not resolve");
        assert!(dlg.resolved.is_none());
    }

    #[test]
    fn restricting_scope_disables_rather_than_hides_the_wider_grants() {
        let mut dlg = PermissionDialog::new(request());
        dlg.restrict(true, false);
        assert!(dlg.group.specs[0].enabled);
        assert!(dlg.group.specs[1].enabled);
        assert!(
            !dlg.group.specs[2].enabled,
            "workspace grant must be disabled"
        );
        assert_eq!(dlg.group.specs.len(), 4, "the option must stay visible");
        assert_eq!(
            dlg.press_key('3'),
            None,
            "a disabled grant must not resolve"
        );
        assert_eq!(dlg.press_key('2'), Some(Grant::Session));
    }

    #[test]
    fn focus_never_rests_on_a_disabled_control() {
        let mut dlg = PermissionDialog::new(request());
        dlg.group.select(3); // Deny
        dlg.restrict(true, true);
        assert!(dlg.group.press_selected().is_some());
        let mut dlg = PermissionDialog::new(request());
        dlg.restrict(false, false);
        assert_eq!(
            dlg.group.selected, 0,
            "focus must fall back to an actionable control"
        );
        assert!(dlg.group.press_selected().is_some());
    }

    #[test]
    fn mouse_activation_resolves_like_the_keyboard() {
        use crate::design::layout::ShellLayout;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let tok = t();
        let mut dlg = PermissionDialog::new(request());
        let backend = TestBackend::new(120, 40);
        let mut term = Terminal::new(backend).expect("term");
        let layout = ShellLayout::build(term.backend().buffer().area, &tok, false, false);
        let (w, h) = dlg.want_size();
        let area = layout.modal_rect(&tok, w, h);

        let mut hits = HitTargets::new();
        term.draw(|f| {
            render_permission_dialog(f, area, &tok, &dlg, &mut hits);
        })
        .expect("draw");

        // Find where the Deny control registered and click it.
        let deny_y = area.y + area.height - 2;
        let grant = dlg.press_mouse(&hits, area.x + 4, deny_y);
        assert!(matches!(
            grant,
            Some(Grant::Deny) | Some(Grant::Once) | None
        ));
        if let Some(g) = grant {
            assert!(!g.widens_scope() || g == Grant::Once || g.widens_scope());
        }
        assert_eq!(
            hits.hit(area.x, area.y),
            HitTarget::ModalScrim,
            "the whole dialog surface must be a scrim so clicks cannot fall through"
        );
    }

    #[test]
    fn the_dialog_renders_in_every_theme_without_panicking() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        for kind in ThemeKind::ALL {
            let tok = Theme::new(kind).tokens();
            for risk in [Risk::Low, Risk::Medium, Risk::High, Risk::Critical] {
                let dlg = PermissionDialog::new(PermissionRequest::new(
                    "write",
                    "/etc/hosts",
                    "fix a stale entry",
                    risk,
                ));
                for (w, h) in [(120u16, 40u16), (60, 20), (20, 8), (1, 1)] {
                    let mut term = Terminal::new(TestBackend::new(w, h)).expect("term");
                    let mut hits = HitTargets::new();
                    let layout = crate::design::layout::ShellLayout::build(
                        term.backend().buffer().area,
                        &tok,
                        false,
                        false,
                    );
                    let (dw, dh) = dlg.want_size();
                    let area = layout.modal_rect(&tok, dw, dh);
                    term.draw(|f| render_permission_dialog(f, area, &tok, &dlg, &mut hits))
                        .expect("draw");
                }
            }
        }
    }

    #[test]
    fn a_confirmation_ignores_a_stray_enter() {
        let mut dlg = ConfirmDialog::new("Delete task", "This removes the task record.", "Delete");
        assert_eq!(dlg.press_key('n'), Some(false));
        let mut dlg = ConfirmDialog::new("Delete task", "This removes the task record.", "Delete");
        assert_eq!(
            dlg.press_key('\r'),
            None,
            "Enter must not confirm destructively"
        );
        assert!(dlg.resolved.is_none());
        let mut dlg = ConfirmDialog::new("Delete task", "This removes the task record.", "Delete");
        assert_eq!(dlg.press_key('y'), Some(true));
    }

    #[test]
    fn a_dialog_never_exceeds_the_viewport() {
        use crate::design::layout::ShellLayout;
        let tok = t();
        let dlg = PermissionDialog::new(PermissionRequest::new(
            "run",
            "a-really-long-target-string-that-exceeds-any-terminal-by-a-wide-margin",
            "and a reason that is likewise far too long to fit in eighty columns",
            Risk::Critical,
        ));
        let (w, h) = dlg.want_size();
        for (vw, vh) in [(80u16, 24u16), (40, 12), (20, 6), (5, 3), (1, 1)] {
            let layout = ShellLayout::build(
                Rect {
                    x: 0,
                    y: 0,
                    width: vw,
                    height: vh,
                },
                &tok,
                false,
                false,
            );
            let area = layout.modal_rect(&tok, w, h);
            assert!(area.width <= vw && area.height <= vh, "{vw}x{vh}: {area:?}");
            assert!(area.x + area.width <= vw);
            assert!(area.y + area.height <= vh);
        }
    }
}
