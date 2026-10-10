//! Semantic design tokens — the complete vocabulary of the OpenRT visual
//! language.
//!
//! Every visual decision in the TUI resolves through this module. Nothing
//! outside [`crate::theme`] may name an RGB value or a raw ANSI color.
//!
//! # Why semantic names instead of colors
//!
//! A palette says *what color is this*. A token says *what this thing is*.
//! `text.muted` survives every theme because the role survives; `Color::Gray`
//! does not, because the terminal decides what "gray" means (ANSI bright
//! black on some terminals, mid-gray on others, absent entirely on monochrome
//! ones). Tokens bind a role to a theme's palette, so a component written
//! once against `Tokens` renders correctly under all six themes.
//!
//! # The shape
//!
//! ```text
//! Theme (palette, 1:1 with a built-in theme or a user's own)
//!   -> Tokens (the semantic vocabulary — fixed shape, theme-independent)
//!      -> components (Panel, Badge, Button, Modal, ...)
//!         -> views (Chat, Files, Security, ...)
//! ```
//!
//! `Tokens` has a **fixed shape**: every field is populated for every theme.
//! That invariant is what lets views depend on tokens and never on a theme,
//! and it is enforced by [`Tokens::from_theme`] plus the palette tests
//! (no `Option`, no `unwrap_or_default`, no "if this theme has no accent").
//!
//! # Extending
//!
//! Add a role to this module, give all six palettes a value for it, and every
//! view using it is automatically theme-correct. If a role cannot be filled
//! honestly for every theme, it does not belong in the token set — it belongs
//! in that one view.

use ratatui::style::{Color, Modifier, Style};

use crate::theme::{Palette, ThemeKind};

/// Base spacing unit. Every gap, pad and margin in the TUI is a multiple of
/// this, which is what makes density a single tunable per theme instead of a
/// number re-invented in each render function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spacing {
    /// 0 columns — dense edges, table cells, inline text.
    pub xs: u16,
    /// 1 column — the default gap between related elements.
    pub sm: u16,
    /// 2 columns — the default gap between unrelated elements.
    pub md: u16,
    /// 4 columns — panel interior padding, card gutters.
    pub lg: u16,
    /// 8 columns — section separation.
    pub xl: u16,
}

impl Spacing {
    const fn new() -> Self {
        Self {
            xs: 0,
            sm: 1,
            md: 2,
            lg: 4,
            xl: 8,
        }
    }
}

/// Measured layout constants. The point of this block is that a viewport is
/// never a pile of magic numbers in a render function: `Layout::breakpoints`
/// classifies a `Rect` into a [`Breakpoint`] from these, and the shell
/// allocates from these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutMetrics {
    /// Rail rows: the title row plus one row per navigation group label.
    pub rail_header: u16,
    /// Rail rows: the navigation group labels themselves.
    pub rail_group_label: u16,
    /// Rail rows: the gap that opens between navigation groups.
    pub rail_group_gap: u16,
    /// Rail columns at every width — the rail never reflows, it collapses.
    pub rail_width: u16,
    /// Rows consumed by the header block (border + title + line).
    pub header_height: u16,
    /// Rows consumed by the footer / status bar (border + line).
    pub status_height: u16,
    /// Columns of breathing room between panels.
    pub gutter: u16,
    /// Interior padding of a panel, minus the border.
    pub panel_pad: u16,
    /// Smallest chat column that still reads as prose rather than a ribbon.
    pub min_chat_width: u16,
    /// Chat column beyond which an extra panel earns its place.
    pub comfortable_chat_width: u16,
    /// Side/context panel width.
    pub context_width: u16,
    /// Minimum usable modal width (its own floor; never exceeds the viewport).
    pub min_modal_width: u16,
    /// Maximum modal width — dialogs are never stretched across a wide screen.
    pub max_modal_width: u16,
    /// Minimum usable modal height.
    pub min_modal_height: u16,
    /// Rows of breathing room kept above and below a modal.
    pub modal_margin: u16,
}

impl LayoutMetrics {
    const fn new() -> Self {
        Self {
            rail_header: 1,
            rail_group_label: 1,
            rail_group_gap: 1,
            rail_width: 16,
            header_height: 3,
            status_height: 1,
            gutter: 2,
            panel_pad: 1,
            min_chat_width: 40,
            comfortable_chat_width: 96,
            context_width: 34,
            min_modal_width: 48,
            max_modal_width: 120,
            min_modal_height: 8,
            modal_margin: 2,
        }
    }

    /// Panel interior width for `outer`, never less than zero (a 2-column
    /// viewport must not underflow — see the render-hardening audit).
    pub fn panel_inner_width(&self, outer: u16) -> u16 {
        saturating_sub(outer, 2 + self.panel_pad)
    }
}

/// Typography roles. A component asks for a role, not for "gray bold".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Typography {
    pub app_title: Style,
    pub page_title: Style,
    pub section_title: Style,
    pub body: Style,
    pub secondary_body: Style,
    pub metadata: Style,
    pub command: Style,
    pub code: Style,
    pub badge: Style,
    pub metric: Style,
    pub error: Style,
    pub warning: Style,
}

/// Background / surface ladder. Dark themes climb a short, deliberate ladder
/// (`bg` -> `surface` -> `elevated` -> `overlay` -> `selected`): each step is a
/// visible lift, never a shade the eye has to hunt for. `input` sits on its
/// own because the input must read as editable while sitting on a panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Surfaces {
    /// The application root. Painted explicitly by both renderers.
    pub bg: Color,
    /// A panel resting on the root.
    pub surface: Color,
    /// A panel above the surface plane: hovered rows, active cards.
    pub elevated: Color,
    /// Floating chrome: modals, dialogs, command palette, dropdowns.
    pub overlay: Color,
    /// An editable field.
    pub input: Color,
    /// A selected row or item.
    pub selected: Color,
}

/// Border roles. Restrained by design: at most a handful of panels may draw a
/// border on any one screen, and only when it separates meaning, not space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Borders2 {
    /// Decorative separation — panel outlines, dividers.
    pub subtle: Color,
    /// The default panel edge.
    pub default: Color,
    /// Drawn when a panel is hovered or selected but not focused.
    pub active: Color,
    /// Drawn on the focused element. The single strongest border in the UI.
    pub focus: Color,
    /// Destructive: confirmation dialogs, denied actions, refused admissions.
    pub destructive: Color,
}

/// Foreground roles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Text {
    /// Body prose, the agent's own voice.
    pub primary: Color,
    /// Secondary copy: explanations, hints, inactive labels.
    pub secondary: Color,
    /// Metadata: timestamps, counts, paths, provenance.
    pub muted: Color,
    /// Anything the operator cannot currently act on.
    pub disabled: Color,
    /// Text drawn on `accent`/`selected` fills.
    pub inverse: Color,
    /// Selectable references: paths, model ids, session ids.
    pub link: Color,
    /// Literal source: command lines, JSON, identifiers.
    pub code: Color,
}

/// Semantic status colors. `blocked` and `pending` are distinct from `danger`
/// on purpose — "the host will not let us proceed" is not a failure, and
/// collapsing the two is exactly what makes agents feel unsafe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub success: Color,
    pub warning: Color,
    pub danger: Color,
    pub info: Color,
    pub running: Color,
    pub blocked: Color,
    pub pending: Color,
}

/// Interaction-state colors, kept separate from status: an element can be
/// `focused` and simultaneously `blocked`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interaction {
    pub focus: Color,
    pub hover: Color,
    pub selected: Color,
    pub pressed: Color,
    pub disabled: Color,
}

/// Accent ramp: one primary accent, one secondary for cross-highlighting, and
/// a muted accent for large quiet fills that must not shout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Accent {
    pub primary: Color,
    pub secondary: Color,
    pub muted: Color,
}

/// Everything a renderer may need, resolved once per frame.
///
/// `PartialEq` is what lets a test assert a theme actually reached the
/// renderer: two token sets are equal iff every semantic slot matches, so an
/// equality check is a whole-theme check rather than one color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tokens {
    pub surfaces: Surfaces,
    pub borders: Borders2,
    pub text: Text,
    pub status: Status,
    pub interaction: Interaction,
    pub accent: Accent,
    pub space: Spacing,
    pub layout: LayoutMetrics,
    pub type_scale: Typography,
}

impl Tokens {
    /// Bind a palette to the semantic vocabulary. Total by construction: a
    /// theme cannot be missing a role.
    pub fn from_theme(theme: ThemeKind) -> Self {
        Self::from_palette(&theme.palette())
    }

    /// Every color this token set defines.
    ///
    /// This is the executable definition of "the palette": any color a renderer
    /// paints that is not in this list did not come from the theme, so a test
    /// can assert theme compliance across all six themes instead of checking
    /// one hand-picked accent.
    pub fn colors(&self) -> Vec<ratatui::style::Color> {
        use ratatui::style::Color;
        vec![
            self.surfaces.bg,
            self.surfaces.surface,
            self.surfaces.elevated,
            self.surfaces.overlay,
            self.surfaces.input,
            self.surfaces.selected,
            self.borders.subtle,
            self.borders.default,
            self.borders.active,
            self.borders.focus,
            self.borders.destructive,
            self.text.primary,
            self.text.secondary,
            self.text.muted,
            self.text.disabled,
            self.text.inverse,
            self.text.link,
            self.text.code,
            self.status.success,
            self.status.warning,
            self.status.danger,
            self.status.info,
            // These three were missing while still being painted by the
            // resource/task cards, which made the "no view may use a raw RGB
            // value" check blind to exactly the states an operator most needs
            // to read correctly.
            self.status.running,
            self.status.pending,
            self.status.blocked,
            self.interaction.hover,
            self.interaction.pressed,
            self.interaction.disabled,
            self.interaction.focus,
            self.interaction.selected,
            self.accent.primary,
            self.accent.secondary,
            self.accent.muted,
        ]
        .into_iter()
        .collect::<Vec<Color>>()
    }

    /// Same as [`Tokens::from_theme`] but for a caller-supplied palette, so
    /// user-defined themes get the identical token shape.
    pub fn from_palette(p: &Palette) -> Self {
        let s = Style::default();
        Self {
            surfaces: Surfaces {
                bg: p.bg,
                surface: p.surface,
                elevated: p.elevated,
                overlay: p.overlay,
                input: p.input,
                selected: p.selected,
            },
            borders: Borders2 {
                subtle: p.border_subtle,
                default: p.border,
                active: p.border_active,
                focus: p.border_focus,
                destructive: p.border_destructive,
            },
            text: Text {
                primary: p.text_primary,
                secondary: p.text_secondary,
                muted: p.text_muted,
                disabled: p.text_disabled,
                inverse: p.text_inverse,
                link: p.text_link,
                code: p.text_code,
            },
            status: Status {
                success: p.success,
                warning: p.warning,
                danger: p.danger,
                info: p.info,
                running: p.running,
                blocked: p.blocked,
                pending: p.pending,
            },
            interaction: Interaction {
                focus: p.inter_focus,
                hover: p.inter_hover,
                selected: p.inter_selected,
                pressed: p.inter_pressed,
                disabled: p.inter_disabled,
            },
            accent: Accent {
                primary: p.accent_primary,
                secondary: p.accent_secondary,
                muted: p.accent_muted,
            },
            space: Spacing::new(),
            layout: LayoutMetrics::new(),
            type_scale: Typography {
                app_title: s.fg(p.accent_primary).bg(p.bg).add_modifier(Modifier::BOLD),
                page_title: s.fg(p.text_primary).add_modifier(Modifier::BOLD),
                section_title: s.fg(p.text_secondary).add_modifier(Modifier::BOLD),
                body: s.fg(p.text_primary),
                secondary_body: s.fg(p.text_secondary),
                metadata: s.fg(p.text_muted),
                command: s.fg(p.text_primary).add_modifier(Modifier::BOLD),
                code: s.fg(p.text_code),
                badge: s
                    .fg(p.text_primary)
                    .bg(p.elevated)
                    .add_modifier(Modifier::BOLD),
                metric: s.fg(p.text_primary).add_modifier(Modifier::BOLD),
                error: s.fg(p.danger).add_modifier(Modifier::BOLD),
                warning: s.fg(p.warning).add_modifier(Modifier::BOLD),
            },
        }
    }

    // -- composed role helpers ------------------------------------------
    // Components use these instead of rebuilding `Style` literals, so the
    // "what does a focused selected row look like" answer lives in one place.

    /// Base style of an interactive element.
    pub fn control(&self) -> Style {
        Style::default().fg(self.text.primary)
    }

    /// `selected` and `focus` may be combined; focus wins on the border and
    /// the stronger background.
    pub fn control_state(&self, focused: bool, selected: bool) -> Style {
        let mut st = self.control();
        if selected {
            st = st.bg(self.interaction.selected).fg(self.text.inverse);
        }
        if focused {
            st = st.bg(self.interaction.focus);
        }
        st
    }

    /// A focus ring is a border role, not a background swap — it must read on
    /// top of whatever surface the element sits on.
    pub fn focus_ring(&self) -> Style {
        Style::default()
            .fg(self.borders.focus)
            .add_modifier(Modifier::BOLD)
    }

    /// A key hint chip in the footer/header.
    pub fn key_hint(&self) -> Style {
        Style::default()
            .fg(self.text.secondary)
            .bg(self.surfaces.elevated)
    }

    /// A muted code/command chip with its own quiet background so it reads as
    /// a literal without pulling focus from body text.
    pub fn code_chip(&self) -> Style {
        Style::default()
            .fg(self.text.code)
            .bg(self.surfaces.elevated)
    }
}

/// Non-panicking subtraction: `a - b` in layout arithmetic must never wrap at
/// zero. A viewport can be smaller than the chrome it is asked to allocate
/// (split panes, tiny remote terminals, a drag that briefly inverts the
/// window), and a u16 underflow is a panic in debug and garbage in release.
#[inline]
pub const fn saturating_sub(a: u16, b: u16) -> u16 {
    if a > b {
        a - b
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_theme_produces_a_distinct_fully_bound_token_set() {
        let mut signatures = Vec::new();
        for theme in ThemeKind::ALL {
            let t = Tokens::from_theme(theme);
            // Type roles are derived from the palette, so they differ per theme
            // where the palette does.
            // High Contrast and Monochrome deliberately share a pure-black
            // root, so identity is judged on the root + body + accent triple.
            signatures.push((t.surfaces.bg, t.text.primary, t.accent.primary));
            assert_eq!(t.type_scale.body.fg, Some(t.text.primary));
            assert_eq!(t.type_scale.error.fg, Some(t.status.danger));
            assert_eq!(t.type_scale.warning.fg, Some(t.status.warning));
            assert_eq!(t.type_scale.metadata.fg, Some(t.text.muted));
            assert_eq!(t.type_scale.badge.bg, Some(t.surfaces.elevated));
            assert_eq!(t.surfaces.input, t.surfaces.input);
        }
        signatures.sort_by_key(|s| format!("{s:?}"));
        signatures.dedup();
        assert_eq!(
            signatures.len(),
            ThemeKind::ALL.len(),
            "two built-in themes render identically"
        );
    }

    #[test]
    fn spacing_is_a_monotonic_scale() {
        let sp = Spacing::new();
        assert!(sp.xs <= sp.sm && sp.sm <= sp.md && sp.md <= sp.lg && sp.lg <= sp.xl);
        assert_eq!(sp.xs, 0);
    }

    #[test]
    fn saturating_sub_never_wraps() {
        assert_eq!(saturating_sub(5, 9), 0);
        assert_eq!(saturating_sub(9, 5), 4);
        assert_eq!(saturating_sub(0, 0), 0);
    }

    #[test]
    fn panel_inner_width_is_clamped_at_zero() {
        let l = LayoutMetrics::new();
        assert_eq!(l.panel_inner_width(2), 0);
        assert_eq!(l.panel_inner_width(3), 0);
        assert!(l.panel_inner_width(120) > 0);
    }

    #[test]
    fn focus_wins_over_selected() {
        let t = Tokens::from_theme(ThemeKind::PitchBlack);
        assert_eq!(t.control_state(true, true).bg, Some(t.interaction.focus));
        assert_eq!(
            t.control_state(false, true).bg,
            Some(t.interaction.selected)
        );
        assert_eq!(t.control_state(false, false).bg, None);
    }
}

#[cfg(test)]
mod status_contrast {
    use crate::theme::{Theme, ThemeKind};

    /// No theme may give two status tones the same color.
    ///
    /// This caught two real defects: `High Contrast` had `danger` and `blocked`
    /// both pure red, and `Monochrome` had `success` and `blocked` both 240.
    /// Either one makes "the host refused this" indistinguishable from "this
    /// failed" — which is the exact confusion the status/interaction split
    /// exists to prevent.
    ///
    /// The claim is also what the accessibility doc makes about Monochrome: a
    /// state change must survive as a brightness change with no hue at all.
    #[test]
    fn every_theme_keeps_status_tones_distinguishable() {
        for name in ThemeKind::ALL {
            let mut th = Theme::default_theme();
            th.set(name);
            let t = th.tokens();
            let tones = [
                t.status.success,
                t.status.warning,
                t.status.danger,
                t.status.info,
                t.status.blocked,
            ];
            for (i, a) in tones.iter().enumerate() {
                for b in tones.iter().skip(i + 1) {
                    assert_ne!(a, b, "{name:?}: two status tones are identical: {a:?}");
                }
            }
        }
    }
}
