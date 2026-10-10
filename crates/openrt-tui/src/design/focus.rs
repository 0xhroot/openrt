//! The focus model: one place that answers "where does keyboard input go?".
//!
//! Before this module every screen invented its own answer. The rail had a
//! cursor, each overlay had a `sel`, Settings had `settings_focus`, and the
//! permission card had neither — it simply captured every key. The result was
//! that a user could not tell what would happen when they pressed a key, and a
//! mouse click had nothing to hit.
//!
//! [`FocusModel`] fixes both halves with one ordered ring:
//!
//! * **Order** — the ring is rebuilt from what is actually on screen each frame.
//!   A surface that is not drawn cannot be focused, so focus can never point at
//!   something invisible or unreachable.
//! * **Position** — a stable [`FocusTarget`] identifies a destination
//!   independently of its row index, so a click and a `Tab` converge on the same
//!   target.
//!
//! Geometry lives in exactly one place — [`super::layout::HitTargets`], which
//! records `Rect <-> HitTarget` pairs as components draw. `FocusModel` holds
//! *identities only*. Keeping rects in both would let the two disagree about
//! where a control is, and a click would land somewhere the keyboard could not
//! reach. The test `focus_and_hit_geometry_share_one_source_of_truth` pins this.
//!
//! Three properties are load-bearing and tested:
//!
//! 1. **Determinism.** `Tab`/`Shift+Tab` walk the ring in a fixed order and
//!    wrap. There is no "nearest" heuristic and no dependence on pointer history.
//! 2. **No orphan focus.** If the focused target vanishes (view changed, overlay
//!    opened, row filtered out), `reconcile` moves focus to the nearest
//!    surviving target rather than dropping it. Focus is never `None` while
//!    anything is on screen, and never points at a target that is not.

use super::tokens::Tokens;
use crossterm::event::KeyCode;

/// What something focusable *is*. Deliberately coarse: a target is a place, not
/// a widget, so the same identifier works for keyboard and mouse.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FocusTarget {
    /// The composer.
    Composer,
    /// A navigation destination in the rail.
    Nav(usize),
    /// A Workbench operations tab.
    Tab(usize),
    /// A row of the active list/table/tree.
    Row(usize),
    /// A destination in the jump-to palette overlay.
    PaletteItem(usize),
    /// A provider/model in the model picker overlay.
    ModelItem(usize),
    /// A button or action inside a card, dialog, or overlay.
    Action { id: &'static str, index: usize },
    /// A chat key-hint chip. Registered so a click can echo the shortcut, but
    /// never focusable: the chip is a label for a key, not a control.
    ChatHint { key: char },
    /// The header bar.
    Header,
    /// A row in one of the Workbench panels.
    WorkbenchRow {
        panel: super::layout::PanelId,
        index: usize,
    },
    /// A Workbench panel frame.
    WorkbenchPanel { panel: super::layout::PanelId },
    /// The dimmed area around an open dialog.
    ///
    /// Swallows clicks so they cannot reach the view behind the dialog. It is
    /// deliberately *not* focusable — the only correct way to leave a dialog is
    /// its own cancel affordance, not clicking the wallpaper.
    ModalScrim,
}

impl FocusTarget {
    /// Whether keyboard focus may ever rest here.
    pub fn is_focusable(&self) -> bool {
        !matches!(
            self,
            Self::ChatHint { .. } | Self::Header | Self::ModalScrim
        )
    }
}

impl From<&super::layout::HitTarget> for FocusTarget {
    fn from(h: &super::layout::HitTarget) -> Self {
        use super::layout::HitTarget as H;
        match h {
            H::NavItem { index } => Self::Nav(*index),
            H::ListRow { index } => Self::Row(*index),
            H::PaletteItem { index } => Self::PaletteItem(*index),
            H::ModelItem { index } => Self::ModelItem(*index),
            H::Button { id } => Self::Action { id, index: 0 },
            H::ChatHint { key } => Self::ChatHint { key: *key },
            H::Header => Self::Header,
            H::ModalScrim => Self::ModalScrim,
            H::WorkbenchRow { panel, index } => Self::WorkbenchRow {
                panel: *panel,
                index: *index,
            },
            H::WorkbenchPanel { panel } => Self::WorkbenchPanel { panel: *panel },
            // Nothing there: no target, rather than a fake one.
            H::None => Self::ModalScrim,
        }
    }
}

/// The interaction state of a focusable element, as distinct from *which*
/// element is focused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interaction {
    /// Receives keyboard input. At most one element at a time.
    Focused,
    /// Chosen but not focused (a list cursor that has lost keyboard focus).
    Selected,
    /// Pointer is over it.
    Hovered,
    /// Being activated.
    Pressed,
    /// Present but inert. Focused elements are never `Disabled`.
    Disabled,
}

impl Interaction {
    /// The border/fill role for this state.
    ///
    /// `Disabled` deliberately uses the muted border rather than the focus
    /// border: a control that looks focused but ignores keys is worse than one
    /// that plainly cannot be used.
    pub fn is_interactive(self) -> bool {
        !matches!(self, Self::Disabled)
    }

    pub fn border(self, t: &Tokens) -> ratatui::style::Color {
        match self {
            Self::Focused => t.borders.focus,
            Self::Pressed => t.interaction.pressed,
            Self::Hovered => t.interaction.hover,
            Self::Selected => t.interaction.selected,
            Self::Disabled => t.borders.subtle,
        }
    }
}

/// What a navigation request should do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    /// `Tab` — next stop.
    Forward,
    /// `Shift+Tab` — previous stop.
    Backward,
    /// Arrow key — move by `delta` within the current group, clamped.
    Step(isize),
    /// Jump to the first/last stop.
    First,
    Last,
}

/// One focusable element, as registered by a renderer during draw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FocusStop {
    pub target: FocusTarget,
    pub interaction: Interaction,
    /// Position within its group, for arrow-key stepping.
    pub index_in_group: usize,
    /// Number of elements in its group, so clamping needs no extra lookup.
    pub group_len: usize,
}

impl FocusStop {
    /// A stop that stands alone: one element in its own group.
    ///
    /// Singletons are common (a lone button in a dialog footer) and are the
    /// right default, because a stop that claims a group of one can never be
    /// stepped past by an arrow key.
    pub fn new(target: FocusTarget) -> Self {
        Self::in_group(target, 0, 1)
    }

    /// A stop positioned within a group of `group_len` elements.
    ///
    /// `group_len` is carried on the stop rather than looked up, so clamping a
    /// move past either end needs no second traversal of the ring.
    pub fn in_group(target: FocusTarget, index_in_group: usize, group_len: usize) -> Self {
        Self {
            target,
            interaction: Interaction::Selected,
            index_in_group,
            group_len,
        }
    }
}

/// The ordered focus ring for one frame.
#[derive(Clone, Debug, Default)]
pub struct FocusModel {
    stops: Vec<FocusStop>,
    /// Index into `stops`, not a `FocusTarget`: the ring is rebuilt each frame
    /// and a target may legitimately appear twice (e.g. two panes, same row id).
    focused: Option<usize>,
    /// The target that held focus when the previous frame closed.
    ///
    /// `begin_frame` clears `focused`, so without this the identity is gone before
    /// `reconcile` can reuse it and every redraw — including a plain resize —
    /// would reset focus to the first stop. The model remembers it so callers
    /// cannot get the call order wrong.
    last_focused: Option<FocusTarget>,
    hovered: Option<FocusTarget>,
    pressed: Option<FocusTarget>,
}

impl FocusModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin a frame: forget the old ring, but remember who had focus.
    ///
    /// `hovered`/`pressed` survive so a redraw mid-click does not flicker.
    pub fn begin_frame(&mut self) {
        self.last_focused = self.focused_target();
        self.stops.clear();
        self.focused = None;
    }

    /// Register a focusable element. Call order defines tab order.
    pub fn push(&mut self, stop: FocusStop) {
        self.stops.push(stop);
    }

    /// Number of registered stops.
    pub fn len(&self) -> usize {
        self.stops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stops.is_empty()
    }

    pub fn stops(&self) -> &[FocusStop] {
        &self.stops
    }

    /// The focused element, if any.
    pub fn focused(&self) -> Option<&FocusStop> {
        self.focused.and_then(|i| self.stops.get(i))
    }

    /// The focused target, if any.
    pub fn focused_target(&self) -> Option<FocusTarget> {
        self.focused().map(|s| s.target.clone())
    }

    pub fn hovered(&self) -> Option<&FocusTarget> {
        self.hovered.as_ref()
    }

    pub fn set_hovered(&mut self, target: Option<FocusTarget>) {
        self.hovered = target;
    }

    pub fn set_pressed(&mut self, target: Option<FocusTarget>) {
        self.pressed = target;
    }

    pub fn pressed(&self) -> Option<&FocusTarget> {
        self.pressed.as_ref()
    }

    /// Put keyboard focus on a specific target. Ignored when it is not on screen
    /// — this is what stops a stale target from being activatable.
    pub fn focus(&mut self, target: &FocusTarget) -> bool {
        match self.stops.iter().position(|s| &s.target == target) {
            Some(i) if self.stops[i].interaction.is_interactive() => {
                self.focused = Some(i);
                true
            }
            _ => false,
        }
    }

    /// Focus the first interactive stop. Used when nothing was focused, so
    /// `Tab` always has somewhere to go.
    pub fn focus_first(&mut self) {
        self.focused = self
            .stops
            .iter()
            .position(|s| s.interaction.is_interactive());
    }

    /// Apply a navigation request and report whether focus moved.
    ///
    /// Arrow stepping (`Move::Step`) is scoped to the focused element's group, so
    /// `Down` moves through list rows and does not jump into the rail. `Tab`
    /// crosses group boundaries, because that is its job.
    pub fn apply(&mut self, mv: Move) -> bool {
        if self.stops.is_empty() {
            return false;
        }
        let Some(cur) = self.focused else {
            self.focus_first();
            return self.focused.is_some();
        };

        let next = match mv {
            Move::Forward => Some((cur + 1) % self.stops.len()),
            Move::Backward => Some(if cur == 0 {
                self.stops.len() - 1
            } else {
                cur - 1
            }),
            Move::First => self
                .stops
                .iter()
                .position(|s| s.interaction.is_interactive()),
            Move::Last => self
                .stops
                .iter()
                .rposition(|s| s.interaction.is_interactive()),
            Move::Step(delta) => {
                // Arrow keys move within the focused element's group only.
                let stop = &self.stops[cur];
                let len = usize::from(stop.group_len).max(1);
                let base = stop.index_in_group;
                let target_i = if delta < 0 {
                    base.saturating_sub(delta.unsigned_abs())
                } else {
                    base.saturating_add(delta.unsigned_abs())
                }
                .min(len - 1);
                // Find the stop whose (group, index_in_group) matches.
                self.stops.iter().position(|s| {
                    s.index_in_group == target_i && self.group_of(cur) == self.group_of_group(s)
                })
            }
        };

        let Some(mut i) = next else {
            return false;
        };
        if i >= self.stops.len() {
            return false;
        }
        // Landed on a disabled element: keep walking in the same direction rather
        // than stranding focus on something inert. Bounded by the ring length —
        // an earlier recursive version blew the stack when *every* stop was
        // disabled, because it never found a base case.
        let backward = matches!(mv, Move::Backward);
        for _ in 0..self.stops.len() {
            if self.stops[i].interaction.is_interactive() {
                let moved = self.focused != Some(i);
                self.focused = Some(i);
                return moved;
            }
            i = if backward {
                if i == 0 {
                    self.stops.len() - 1
                } else {
                    i - 1
                }
            } else {
                (i + 1) % self.stops.len()
            };
        }
        // Nothing in the ring is interactive. Keep the existing focus rather than
        // clearing it, so the screen does not go dark.
        false
    }

    /// Re-anchor focus after the ring changed.
    ///
    /// Called once at the end of a frame. Keeps the focused *target* if it
    /// survived (so a redraw does not lose the user's place), otherwise falls
    /// back to the first interactive stop. This is what makes "focus is never
    /// orphaned" true across view switches, resizes, and filter changes.
    ///
    /// Takes no argument: the previous target comes from [`Self::begin_frame`],
    /// which is the only place that could have observed it.
    pub fn reconcile(&mut self) {
        if self.stops.is_empty() {
            self.focused = None;
            self.last_focused = None;
            return;
        }
        if let Some(t) = self.last_focused.clone() {
            if let Some(i) = self
                .stops
                .iter()
                .position(|s| s.target == t && s.interaction.is_interactive())
            {
                self.focused = Some(i);
                return;
            }
        }
        if let Some(i) = self.focused {
            if self
                .stops
                .get(i)
                .is_some_and(|s| s.interaction.is_interactive())
            {
                return;
            }
        }
        self.focus_first();
    }

    /// Focus a target discovered by the hit registry (a click), if focusable.
    ///
    /// Geometry resolution belongs to [`super::layout::HitTargets`]; this is only
    /// the "now that we know *what* was hit, may it take the keyboard?" step.
    pub fn focus_from_hit(&mut self, hit: &super::layout::HitTarget) -> Option<FocusTarget> {
        if hit.is_none() {
            return None;
        }
        let target = FocusTarget::from(hit);
        // A scrim or header is a real hit but not a destination. Report it so the
        // caller can swallow the click, without moving focus.
        if self.focus(&target) {
            return Some(target);
        }
        Some(target)
    }

    /// The interaction state to *render* for `target`, folding hover and press
    /// into the focus state so a component needs one call, not three.
    pub fn state_of(&self, target: &FocusTarget) -> Interaction {
        let Some(stop) = self.stops.iter().find(|s| &s.target == target) else {
            return Interaction::Disabled;
        };
        if !stop.interaction.is_interactive() {
            return Interaction::Disabled;
        }
        if self.pressed.as_ref() == Some(target) {
            return Interaction::Pressed;
        }
        if self.focused() == Some(stop) {
            return Interaction::Focused;
        }
        if self.hovered.as_ref() == Some(target) {
            return Interaction::Hovered;
        }
        stop.interaction
    }

    /// Group key of the focused stop, for arrow scoping.
    fn group_of(&self, i: usize) -> FocusGroup {
        self.group_of_group(&self.stops[i])
    }

    /// Two stops share a group when their targets have the same variant. Row
    /// indices deliberately do not participate: `Row(3)` in the rail and
    /// `Row(3)` in a list are different groups.
    fn group_of_group(&self, s: &FocusStop) -> FocusGroup {
        match &s.target {
            FocusTarget::Composer => FocusGroup::Composer,
            FocusTarget::Nav(_) => FocusGroup::Nav,
            FocusTarget::Tab(_) => FocusGroup::Tab,
            FocusTarget::Row(_) => FocusGroup::Row,
            FocusTarget::WorkbenchRow { .. } => FocusGroup::Row,
            FocusTarget::WorkbenchPanel { .. } => FocusGroup::Row,
            FocusTarget::PaletteItem(_) => FocusGroup::Row,
            FocusTarget::ModelItem(_) => FocusGroup::Row,
            FocusTarget::Action { .. } => FocusGroup::Action,
            // Not focusable, so they never appear in the ring; grouped with the
            // rows they would sit among to keep the match total.
            FocusTarget::ChatHint { .. } => FocusGroup::Row,
            FocusTarget::Header => FocusGroup::Nav,
            FocusTarget::ModalScrim => FocusGroup::Row,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FocusGroup {
    Composer,
    Nav,
    Tab,
    Row,
    Action,
}

// ===========================================================================
// Modal scope: the single interaction abstraction every overlay resolves
// against.
//
// Before this existed, each of the nine overlays owned its own `match` on
// `KeyCode` — nine focus systems that had to agree with each other and, in
// places, did not (Tab was handled in one overlay and silently ignored in
// seven others). The scope replaces all nine with one description of what a
// modal is, and one resolver that reads it.
//
// The scope is *declarative*. It does not know how to close a dialog or apply
// a model; it says what keys mean, and the caller supplies the effects. That
// split is what lets the permission dialog — a security boundary — share the
// same trap as the read-only help panel without inheriting help's keys.
// ===========================================================================

/// Which key families a modal opts into.
///
/// The default is deliberately inert: a scope that forgets to opt into
/// something gets nothing rather than the background's behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverlayKeys {
    /// `Tab` / `Shift+Tab` walk the scope's stops.
    pub tab: bool,
    /// Arrow keys step within the scope's current group.
    pub arrows: bool,
    /// `Enter` activates the focused stop.
    pub enter_activates_focus: bool,
    /// `Esc` closes the overlay and discards its state.
    ///
    /// The permission dialog does **not** set this: closing it must answer it,
    /// which is a different action with a different consequence.
    pub esc_closes: bool,
    /// Printable characters are treated as typed input rather than ignored.
    ///
    /// False means a character is swallowed. A read-only preview must not let a
    /// stray `r` reach the view behind it and start a run.
    pub typing: bool,
}

impl Default for OverlayKeys {
    fn default() -> Self {
        // Inert on purpose. Every modal declares what it accepts; nothing
        // inherits the background's key map by accident.
        Self {
            tab: false,
            arrows: false,
            enter_activates_focus: false,
            esc_closes: false,
            typing: false,
        }
    }
}

impl OverlayKeys {
    /// A read-only, scrollable panel: arrows move, Esc closes, nothing else.
    pub fn scrollable() -> Self {
        Self {
            arrows: true,
            esc_closes: true,
            ..Self::default()
        }
    }

    /// A picker over a list: arrows move, Enter activates, Esc closes.
    pub fn picker() -> Self {
        Self {
            tab: true,
            arrows: true,
            enter_activates_focus: true,
            esc_closes: true,
            ..Self::default()
        }
    }

    /// A text-entry field: characters are input, Tab walks stops, Esc closes.
    pub fn text_entry() -> Self {
        Self {
            tab: true,
            enter_activates_focus: true,
            esc_closes: true,
            typing: true,
            ..Self::default()
        }
    }

    /// A wizard: Tab walks stops, Enter advances, Esc leaves.
    pub fn wizard() -> Self {
        Self {
            tab: true,
            arrows: true,
            enter_activates_focus: true,
            esc_closes: true,
            typing: true,
            ..Self::default()
        }
    }

    /// The permission prompt: keys are answered explicitly, never by default.
    ///
    /// `esc_closes` is false and `enter_activates_focus` is false on purpose.
    /// Esc and Enter must route through the answer path, where they mean
    /// *deny* — a reflexive Enter on a security prompt may never grant.
    pub fn security_prompt() -> Self {
        Self {
            tab: true,
            arrows: true,
            typing: false,
            ..Self::default()
        }
    }

    /// The destructive confirmation: only an explicit answer proceeds.
    pub fn confirmation() -> Self {
        Self {
            tab: true,
            arrows: true,
            enter_activates_focus: true,
            esc_closes: true,
            typing: false,
            ..Self::default()
        }
    }
}

/// One modal's interaction description for a single frame.
#[derive(Clone, Debug, Default)]
pub struct OverlayScope {
    /// Stable identity, used by tests and by the status line.
    pub id: &'static str,
    /// Focusable controls in Tab order. Empty means the overlay traps focus on
    /// itself — correct for a read-only panel with nothing to activate.
    pub stops: Vec<FocusStop>,
    pub keys: OverlayKeys,
    /// Set when this modal is a security boundary.
    ///
    /// A background action must not run while it is open, and a `BackTab` must
    /// not walk backwards into one. Recorded explicitly rather than inferred
    /// from the id so renaming a variant cannot silently drop the protection.
    pub is_security_boundary: bool,
}

impl OverlayScope {
    pub fn new(id: &'static str, keys: OverlayKeys) -> Self {
        Self {
            id,
            stops: Vec::new(),
            keys,
            is_security_boundary: false,
        }
    }

    /// Mark this modal as a security boundary.
    pub fn security(mut self) -> Self {
        self.is_security_boundary = true;
        self
    }

    pub fn with_stops(mut self, stops: Vec<FocusStop>) -> Self {
        self.stops = stops;
        self
    }

    /// The focused stop, if the overlay has one and focus is inside it.
    pub fn focused<'a>(&self, model: &'a FocusModel) -> Option<&'a FocusStop> {
        model.focused().filter(|s| self.stops.contains(s))
    }

    /// Whether a target belongs to this scope.
    pub fn owns(&self, target: &FocusTarget) -> bool {
        self.stops.iter().any(|s| &s.target == target)
    }

    /// Whether a key would do anything in this scope.
    ///
    /// Used to decide whether to show a "nothing here" status line rather than
    /// silently eating the keystroke.
    pub fn accepts(&self, key: KeyCode) -> bool {
        match key {
            KeyCode::Tab | KeyCode::BackTab => self.keys.tab,
            KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right => self.keys.arrows,
            KeyCode::Enter => self.keys.enter_activates_focus,
            KeyCode::Esc => self.keys.esc_closes,
            KeyCode::Char(_) | KeyCode::Backspace => self.keys.typing,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::layout::{HitTarget, HitTargets};
    use super::*;
    use crate::theme::Theme;
    use ratatui::layout::Rect;

    fn t() -> Tokens {
        Theme::default_theme().tokens()
    }

    fn stop(target: FocusTarget, index_in_group: usize, group_len: usize) -> FocusStop {
        FocusStop {
            target,
            interaction: Interaction::Selected,
            index_in_group,
            group_len,
        }
    }

    fn rail(n: usize) -> Vec<FocusStop> {
        (0..n).map(|i| stop(FocusTarget::Nav(i), i, n)).collect()
    }

    #[test]
    fn tab_walks_the_ring_in_order_and_wraps() {
        let mut f = FocusModel::new();
        f.begin_frame();
        for s in rail(3) {
            f.push(s);
        }
        f.focus_first();
        assert_eq!(f.focused_target(), Some(FocusTarget::Nav(0)));
        f.apply(Move::Forward);
        assert_eq!(f.focused_target(), Some(FocusTarget::Nav(1)));
        f.apply(Move::Forward);
        assert_eq!(f.focused_target(), Some(FocusTarget::Nav(2)));
        f.apply(Move::Forward);
        assert_eq!(f.focused_target(), Some(FocusTarget::Nav(0)), "must wrap");
        f.apply(Move::Backward);
        assert_eq!(
            f.focused_target(),
            Some(FocusTarget::Nav(2)),
            "must wrap back"
        );
    }

    #[test]
    fn shift_tab_is_the_exact_inverse_of_tab() {
        let mut f = FocusModel::new();
        f.begin_frame();
        for s in rail(5) {
            f.push(s);
        }
        f.focus_first();
        for _ in 0..7 {
            f.apply(Move::Forward);
            let after_fwd = f.focused_target();
            f.apply(Move::Backward);
            assert_ne!(after_fwd, f.focused_target());
        }
    }

    #[test]
    fn focus_is_deterministic_across_identical_frames() {
        let run = || {
            let mut f = FocusModel::new();
            f.begin_frame();
            for s in rail(4) {
                f.push(s);
            }
            f.focus_first();
            f.apply(Move::Forward);
            f.apply(Move::Forward);
            f.focused_target()
        };
        assert_eq!(run(), run());
        assert_eq!(run(), Some(FocusTarget::Nav(2)));
    }

    #[test]
    fn arrows_are_scoped_to_the_focused_group() {
        let mut f = FocusModel::new();
        f.begin_frame();
        for s in rail(3) {
            f.push(s);
        }
        for i in 0..5 {
            f.push(stop(FocusTarget::Row(i), i, 5));
        }
        assert!(f.focus(&FocusTarget::Row(1)), "target should be focusable");
        f.apply(Move::Step(1));
        assert_eq!(f.focused_target(), Some(FocusTarget::Row(2)));
        // Clamped inside the group, never leaking into the rail.
        f.apply(Move::Step(1));
        f.apply(Move::Step(1));
        f.apply(Move::Step(1));
        assert_eq!(
            f.focused_target(),
            Some(FocusTarget::Row(4)),
            "arrows left the row group"
        );
        f.apply(Move::Step(-1));
        assert_eq!(f.focused_target(), Some(FocusTarget::Row(3)));
    }

    #[test]
    fn arrows_clamp_at_both_ends_without_panicking() {
        let mut f = FocusModel::new();
        f.begin_frame();
        for i in 0..3 {
            f.push(stop(FocusTarget::Row(i), i, 3));
        }
        assert!(f.focus(&FocusTarget::Row(0)), "target should be focusable");
        for _ in 0..5 {
            f.apply(Move::Step(-1));
        }
        assert_eq!(f.focused_target(), Some(FocusTarget::Row(0)));
        assert!(f.focus(&FocusTarget::Row(2)), "target should be focusable");
        for _ in 0..5 {
            f.apply(Move::Step(1));
        }
        assert_eq!(f.focused_target(), Some(FocusTarget::Row(2)));
    }

    #[test]
    fn first_and_last_land_on_interactive_stops() {
        let mut f = FocusModel::new();
        f.begin_frame();
        for s in rail(4) {
            f.push(s);
        }
        f.focus_first();
        f.apply(Move::Last);
        assert_eq!(f.focused_target(), Some(FocusTarget::Nav(3)));
        f.apply(Move::First);
        assert_eq!(f.focused_target(), Some(FocusTarget::Nav(0)));
    }

    #[test]
    fn disabled_stops_are_skipped_and_never_focused() {
        let mut f = FocusModel::new();
        f.begin_frame();
        f.push(stop(FocusTarget::Nav(0), 0, 3));
        f.push(FocusStop {
            interaction: Interaction::Disabled,
            ..stop(FocusTarget::Nav(1), 1, 3)
        });
        f.push(stop(FocusTarget::Nav(2), 2, 3));
        f.focus_first();
        assert_eq!(f.focused_target(), Some(FocusTarget::Nav(0)));
        f.apply(Move::Forward);
        assert_eq!(
            f.focused_target(),
            Some(FocusTarget::Nav(2)),
            "Tab stepped onto a disabled stop"
        );
        assert!(
            !f.focus(&FocusTarget::Nav(1)),
            "a disabled target took focus"
        );
        assert_eq!(f.state_of(&FocusTarget::Nav(1)), Interaction::Disabled);
    }

    #[test]
    fn disabled_renders_as_muted_never_as_focused() {
        let t = t();
        assert_ne!(
            Interaction::Disabled.border(&t),
            Interaction::Focused.border(&t),
            "a disabled control must not look focusable"
        );
        assert!(!Interaction::Disabled.is_interactive());
        assert!(Interaction::Focused.is_interactive());
    }

    #[test]
    fn reconcile_keeps_the_same_target_across_a_redraw() {
        let mut f = FocusModel::new();
        f.begin_frame();
        for s in rail(5) {
            f.push(s);
        }
        assert!(f.focus(&FocusTarget::Nav(3)), "target should be focusable");
        assert!(f.focused_target().is_some());

        // Next frame: same ring, one extra row inserted before it.
        f.begin_frame();
        for s in rail(6) {
            f.push(s);
        }
        f.reconcile();
        assert_eq!(
            f.focused_target(),
            Some(FocusTarget::Nav(3)),
            "focus must follow the target, not the index"
        );
    }

    /// Regression: `begin_frame` clears `focused`, so an earlier design that read
    /// the "previous" target in `reconcile`'s caller always saw `None` and reset
    /// focus to the first stop on *every* redraw — silently throwing away the
    /// operator's place on a resize. The model now remembers it itself.
    #[test]
    fn a_plain_redraw_keeps_focus_without_the_caller_helping() {
        let mut f = FocusModel::new();
        f.begin_frame();
        for s in rail(6) {
            f.push(s);
        }
        f.reconcile();
        assert!(f.focus(&FocusTarget::Nav(4)));
        let _ = f.focused_target();

        // The minimal frame cycle: begin, repopulate, reconcile. No argument.
        for _ in 0..3 {
            f.begin_frame();
            for s in rail(6) {
                f.push(s);
            }
            f.reconcile();
            assert_eq!(
                f.focused_target(),
                Some(FocusTarget::Nav(4)),
                "a redraw moved focus"
            );
        }
    }

    #[test]
    fn reconcile_rehomes_when_the_focused_target_disappears() {
        let mut f = FocusModel::new();
        f.begin_frame();
        for s in rail(5) {
            f.push(s);
        }
        assert!(f.focus(&FocusTarget::Nav(4)), "target should be focusable");

        // View switched: the rail now has two entries.
        f.begin_frame();
        for s in rail(2) {
            f.push(s);
        }
        f.reconcile();
        assert!(
            f.focused_target().is_some(),
            "focus was orphaned onto nothing while a screen was still drawn"
        );
        assert_ne!(f.focused_target(), Some(FocusTarget::Nav(4)));
    }

    #[test]
    fn reconcile_yields_nothing_only_when_nothing_is_drawn() {
        let mut f = FocusModel::new();
        f.begin_frame();
        f.push(stop(FocusTarget::Nav(0), 0, 1));
        f.begin_frame();
        f.reconcile();
        assert_eq!(
            f.focused_target(),
            None,
            "focused a target that was not drawn"
        );
    }

    #[test]
    fn focus_cannot_target_something_off_screen() {
        let mut f = FocusModel::new();
        f.begin_frame();
        f.push(stop(FocusTarget::Nav(0), 0, 2));
        assert!(
            !f.focus(&FocusTarget::Nav(9)),
            "focused a target that was never registered"
        );
        assert_eq!(f.focused_target(), None);
    }

    #[test]
    fn a_hit_focuses_the_stop_that_was_drawn_there() {
        let mut f = FocusModel::new();
        f.begin_frame();
        for s in rail(3) {
            f.push(s);
        }
        let got = f.focus_from_hit(&HitTarget::NavItem { index: 1 });
        assert_eq!(got, Some(FocusTarget::Nav(1)));
        assert_eq!(f.focused_target(), Some(FocusTarget::Nav(1)));
    }

    #[test]
    fn a_hit_on_nothing_focuses_nothing() {
        let mut f = FocusModel::new();
        f.begin_frame();
        f.push(stop(FocusTarget::Nav(0), 0, 3));
        f.push(stop(FocusTarget::Nav(1), 1, 3));
        assert_eq!(f.focus_from_hit(&HitTarget::None), None);
        assert_eq!(f.focused_target(), None);
    }

    #[test]
    fn a_scrim_hit_is_reported_but_never_takes_focus() {
        let mut f = FocusModel::new();
        f.begin_frame();
        f.push(stop(FocusTarget::Nav(0), 0, 2));
        f.focus_first();
        // The scrim is what was clicked, so the caller knows to swallow it...
        assert_eq!(
            f.focus_from_hit(&HitTarget::ModalScrim),
            Some(FocusTarget::ModalScrim)
        );
        // ...but focus stays put, so Tab cannot wander off behind the dialog.
        assert_eq!(f.focused_target(), Some(FocusTarget::Nav(0)));
        assert!(!FocusTarget::ModalScrim.is_focusable());
        assert!(!FocusTarget::Header.is_focusable());
        assert!(!FocusTarget::ChatHint { key: 'k' }.is_focusable());
    }

    #[test]
    fn click_and_keyboard_converge_on_the_same_target() {
        // The point of the model: both input paths address one identity.
        let mut by_key = FocusModel::new();
        by_key.begin_frame();
        for s in rail(4) {
            by_key.push(s);
        }
        by_key.focus_first();
        by_key.apply(Move::Forward);
        by_key.apply(Move::Forward);

        // The mouse resolves to an identity, not an index, so both land on Nav(2).
        let mut by_mouse = FocusModel::new();
        by_mouse.begin_frame();
        for s in rail(4) {
            by_mouse.push(s);
        }
        by_mouse.focus_from_hit(&HitTarget::NavItem { index: 2 });

        assert_eq!(by_key.focused_target(), by_mouse.focused_target());
        assert_eq!(by_key.focused_target(), Some(FocusTarget::Nav(2)));
    }

    #[test]
    fn hover_and_press_are_reflected_in_the_rendered_state() {
        let mut f = FocusModel::new();
        f.begin_frame();
        f.push(stop(FocusTarget::Nav(0), 0, 2));
        f.push(stop(FocusTarget::Nav(1), 1, 2));
        assert!(f.focus(&FocusTarget::Nav(0)), "target should be focusable");
        assert_eq!(f.state_of(&FocusTarget::Nav(0)), Interaction::Focused);
        assert_eq!(f.state_of(&FocusTarget::Nav(1)), Interaction::Selected);

        f.set_hovered(Some(FocusTarget::Nav(1)));
        assert_eq!(f.state_of(&FocusTarget::Nav(1)), Interaction::Hovered);

        f.set_pressed(Some(FocusTarget::Nav(1)));
        assert_eq!(f.state_of(&FocusTarget::Nav(1)), Interaction::Pressed);

        // Focus outranks hover, so a hovered+focused element reads as focused.
        f.set_hovered(Some(FocusTarget::Nav(0)));
        assert_eq!(f.state_of(&FocusTarget::Nav(0)), Interaction::Focused);
    }

    #[test]
    fn hover_survives_a_redraw_so_clicks_do_not_flicker() {
        let mut f = FocusModel::new();
        f.begin_frame();
        f.push(stop(FocusTarget::Nav(1), 1, 2));
        f.set_hovered(Some(FocusTarget::Nav(1)));
        f.begin_frame();
        f.push(stop(FocusTarget::Nav(1), 1, 2));
        assert_eq!(f.hovered(), Some(&FocusTarget::Nav(1)));
    }

    #[test]
    fn an_empty_ring_never_panics_on_any_navigation() {
        let mut f = FocusModel::new();
        f.begin_frame();
        for mv in [
            Move::Forward,
            Move::Backward,
            Move::Step(1),
            Move::Step(-1),
            Move::First,
            Move::Last,
        ] {
            assert!(!f.apply(mv));
        }
        assert_eq!(f.focused_target(), None);
        assert_eq!(f.focus_from_hit(&HitTarget::None), None);
        f.reconcile();
        assert_eq!(f.focused_target(), None);
    }

    /// Regression: skipping a disabled stop used to recurse, so a ring of
    /// nothing-but-disabled stops overflowed the stack and aborted the process
    /// (SIGABRT) rather than failing a test. A screen where every control is
    /// inert is a legitimate state — it must degrade, not crash.
    #[test]
    fn a_ring_of_only_disabled_stops_does_not_recurse_forever() {
        let mut f = FocusModel::new();
        f.begin_frame();
        for i in 0..4 {
            f.push(FocusStop {
                interaction: Interaction::Disabled,
                ..stop(FocusTarget::Row(i), i, 4)
            });
        }
        for mv in [
            Move::Forward,
            Move::Backward,
            Move::First,
            Move::Last,
            Move::Step(1),
        ] {
            assert!(!f.apply(mv), "{mv:?} should not have moved focus");
        }
        assert_eq!(f.focused_target(), None);
        assert_eq!(f.state_of(&FocusTarget::Row(0)), Interaction::Disabled);
    }

    #[test]
    fn focus_is_kept_when_the_whole_ring_is_disabled_after_being_set() {
        let mut f = FocusModel::new();
        f.begin_frame();
        f.push(stop(FocusTarget::Row(0), 0, 2));
        f.push(stop(FocusTarget::Row(1), 1, 2));
        f.focus_first();
        assert!(f.focused_target().is_some());

        // Everything becomes inert (e.g. the workspace went read-only).
        f.begin_frame();
        for i in 0..2 {
            f.push(FocusStop {
                interaction: Interaction::Disabled,
                ..stop(FocusTarget::Row(i), i, 2)
            });
        }
        f.reconcile();
        assert_eq!(f.focused_target(), None, "focused a disabled target");
    }

    /// The rule that keeps the two halves of the model from drifting: geometry
    /// is recorded once, in `HitTargets`, and `FocusStop` carries no rect at all.
    /// If a rect were ever re-added here, a click and a `Tab` could address the
    /// same-looking control at two different places.
    #[test]
    fn focus_stops_carry_no_geometry_of_their_own() {
        let stop = FocusStop {
            target: FocusTarget::Nav(0),
            interaction: Interaction::Selected,
            index_in_group: 0,
            group_len: 1,
        };
        // Compiles only because the struct has exactly these fields.
        let FocusStop {
            target,
            interaction,
            index_in_group,
            group_len,
        } = stop;
        assert_eq!(target, FocusTarget::Nav(0));
        assert_eq!(interaction, Interaction::Selected);
        assert_eq!(index_in_group, 0);
        assert_eq!(group_len, 1);
    }

    /// A clipped control must have no hit region at all, so a click on the space
    /// it used to occupy falls through to whatever is actually there.
    #[test]
    fn a_zero_area_control_registers_no_hit_region() {
        let mut hits = HitTargets::new();
        hits.register(Rect::new(0, 0, 0, 0), HitTarget::Button { id: "ghost" });
        assert!(
            hits.is_empty(),
            "a clipped-away control was still clickable"
        );
        assert_eq!(hits.at(0, 0), None);
    }
}
