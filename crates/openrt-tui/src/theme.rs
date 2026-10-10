//! The OpenRT theme system: six built-in themes, a runtime switchable
//! registry, and persisted selection.
//!
//! # Architecture
//!
//! ```text
//! ThemeKind (6 built-ins, stable names, persisted by name)
//!   -> Palette (semantic color slots, one value per role)
//!      -> Tokens (crate::design::tokens — the fixed vocabulary)
//!         -> components -> views
//! ```
//!
//! All six palettes are **original OpenRT themes**. They are not ports of any
//! other product's palette: the dark set shares a construction — near-black
//! root, a short surface ladder, one primary accent, semantic status hues
//! that never double as decoration — but every theme has its own hue identity,
//! surface ladder and accent pair.
//!
//! Two rules keep the set coherent:
//!
//! * **Semantic statuses mean the same thing in every theme.** `success`,
//!   `danger`, `blocked`, `pending` are one ladder of meaning, not a per-theme
//!   mood. `High Contrast` deliberately re-specifies them because contrast is
//!   its entire purpose.
//! * **`Monochrome` proves the token set.** A theme that renders every role
//!   from a single color family is the strongest available evidence that no
//!   view smuggled in a palette dependency — and it is the right fallback for
//!   a terminal that cannot show color at all.
//!
//! Palettes are written as **named struct literals**, not positional
//! arguments: with 36 roles a positional list is a silent mis-ordering bug
//! waiting to happen, and a named field cannot be swapped by accident.

use std::str::FromStr;

use ratatui::style::Color;

const fn rgb(c: (u8, u8, u8)) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

/// A theme's complete color assignment. Every slot is mandatory: a palette
/// that cannot fill a role honestly is not a valid theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    // -- surfaces ------------------------------------------------------
    /// Application root. Painted explicitly by both renderers.
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
    // -- borders -------------------------------------------------------
    /// Decorative separation: panel outlines, dividers.
    pub border_subtle: Color,
    /// The default panel edge.
    pub border: Color,
    /// Hovered or selected-but-not-focused.
    pub border_active: Color,
    /// The focused element. The strongest border in the UI.
    pub border_focus: Color,
    /// Destructive: confirmations, denied actions, refused admissions.
    pub border_destructive: Color,
    // -- text ----------------------------------------------------------
    /// Body prose, the agent's own voice.
    pub text_primary: Color,
    /// Secondary copy: explanations, hints, inactive labels.
    pub text_secondary: Color,
    /// Metadata: timestamps, counts, paths, provenance.
    pub text_muted: Color,
    /// Anything the operator cannot currently act on.
    pub text_disabled: Color,
    /// Text drawn on `accent`/`selected` fills.
    pub text_inverse: Color,
    /// Selectable references: paths, model ids, session ids.
    pub text_link: Color,
    /// Literal source: command lines, JSON, identifiers.
    pub text_code: Color,
    // -- semantic status -----------------------------------------------
    pub success: Color,
    pub warning: Color,
    pub danger: Color,
    pub info: Color,
    pub running: Color,
    /// Host will not let us proceed. Distinct from `danger`: a blocked state
    /// is not a failure.
    pub blocked: Color,
    pub pending: Color,
    // -- interaction ---------------------------------------------------
    pub inter_focus: Color,
    pub inter_hover: Color,
    pub inter_selected: Color,
    pub inter_pressed: Color,
    pub inter_disabled: Color,
    // -- accent --------------------------------------------------------
    pub accent_primary: Color,
    /// Cross-highlight: a second signal that must never be mistaken for the
    /// first (plan vs. result, requested vs. granted).
    pub accent_secondary: Color,
    /// Large quiet fills that must not shout.
    pub accent_muted: Color,
}

/// Built-in theme identity. The `name()` string is the persisted form and is
/// append-only compatibility surface; the enum order is presentation order in
/// the Settings list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ThemeKind {
    /// Default. Deep neutral-black, one violet accent.
    #[default]
    PitchBlack,
    /// Near-black with a faintly warm stone ladder and a cyan accent.
    Obsidian,
    /// Neutral cold graphite: no hue anywhere in the surfaces.
    Graphite,
    /// Blue-shifted dark, quieter chrome, azure accent.
    Midnight,
    /// Maximum separation for impaired or bright terminals. Emphasis is
    /// filled rather than implied by weight — the accessibility theme.
    HighContrast,
    /// One color family. Proves no view depends on hue; serves terminals
    /// with no color support.
    Monochrome,
}

impl ThemeKind {
    pub const ALL: [ThemeKind; 6] = [
        ThemeKind::PitchBlack,
        ThemeKind::Obsidian,
        ThemeKind::Graphite,
        ThemeKind::Midnight,
        ThemeKind::HighContrast,
        ThemeKind::Monochrome,
    ];

    /// Stable persisted name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::PitchBlack => "pitch-black",
            Self::Obsidian => "obsidian",
            Self::Graphite => "graphite",
            Self::Midnight => "midnight",
            Self::HighContrast => "high-contrast",
            Self::Monochrome => "monochrome",
        }
    }

    /// Short label for the Settings list.
    pub const fn label(self) -> &'static str {
        match self {
            Self::PitchBlack => "Pitch Black",
            Self::Obsidian => "Obsidian",
            Self::Graphite => "Graphite",
            Self::Midnight => "Midnight",
            Self::HighContrast => "High Contrast",
            Self::Monochrome => "Monochrome",
        }
    }

    /// One line of intent, shown under the theme name.
    pub const fn blurb(self) -> &'static str {
        match self {
            Self::PitchBlack => "Default. Neutral black, one violet accent.",
            Self::Obsidian => "Warm stone ladder, cool cyan accent.",
            Self::Graphite => "Hue-free surfaces; the accent carries all emphasis.",
            Self::Midnight => "Deep blue-black, quietest chrome, azure accent.",
            Self::HighContrast => "Maximum separation, filled emphasis. Accessibility.",
            Self::Monochrome => "No hue at all. State carried by labels, not color.",
        }
    }

    /// Whether the palette is built from a single color family. Components
    /// may lean on filled emphasis here and must rely on text markers
    /// elsewhere.
    pub const fn is_monochrome(self) -> bool {
        matches!(self, Self::Monochrome)
    }

    /// Whether the terminal cannot be assumed to render color faithfully, so
    /// decorative fills must be suppressed and state carried in text.
    pub const fn prefers_strong_marks(self) -> bool {
        matches!(self, Self::HighContrast | Self::Monochrome)
    }

    pub const fn palette(self) -> Palette {
        match self {
            Self::PitchBlack => Palette {
                bg: rgb((0, 0, 0)),
                surface: rgb((14, 14, 18)),
                elevated: rgb((22, 22, 28)),
                overlay: rgb((26, 26, 33)),
                input: rgb((16, 16, 21)),
                selected: rgb((46, 38, 96)),
                border_subtle: rgb((34, 34, 44)),
                border: rgb((52, 52, 64)),
                border_active: rgb((100, 100, 124)),
                border_focus: rgb((137, 101, 255)),
                border_destructive: rgb((235, 80, 100)),
                text_primary: rgb((233, 234, 240)),
                text_secondary: rgb((176, 178, 190)),
                text_muted: rgb((132, 132, 144)),
                text_disabled: rgb((94, 94, 108)),
                text_inverse: rgb((10, 10, 14)),
                text_link: rgb((150, 170, 255)),
                text_code: rgb((198, 210, 240)),
                success: rgb((72, 200, 120)),
                warning: rgb((235, 190, 70)),
                danger: rgb((235, 80, 100)),
                info: rgb((70, 190, 220)),
                running: rgb((124, 160, 255)),
                blocked: rgb((244, 122, 96)),
                pending: rgb((150, 150, 168)),
                inter_focus: rgb((46, 38, 96)),
                inter_hover: rgb((30, 30, 40)),
                inter_selected: rgb((46, 38, 96)),
                inter_pressed: rgb((86, 62, 170)),
                inter_disabled: rgb((94, 94, 108)),
                accent_primary: rgb((124, 92, 255)),
                accent_secondary: rgb((108, 148, 255)),
                accent_muted: rgb((52, 40, 110)),
            },
            Self::Obsidian => Palette {
                bg: rgb((9, 9, 11)),
                surface: rgb((17, 17, 21)),
                elevated: rgb((26, 26, 31)),
                overlay: rgb((31, 31, 37)),
                input: rgb((19, 19, 24)),
                selected: rgb((26, 48, 58)),
                border_subtle: rgb((36, 36, 43)),
                border: rgb((56, 56, 65)),
                border_active: rgb((103, 103, 115)),
                border_focus: rgb((86, 178, 196)),
                border_destructive: rgb((235, 80, 100)),
                text_primary: rgb((236, 233, 228)),
                text_secondary: rgb((180, 177, 172)),
                text_muted: rgb((136, 134, 130)),
                text_disabled: rgb((98, 97, 94)),
                text_inverse: rgb((9, 9, 11)),
                text_link: rgb((130, 196, 210)),
                text_code: rgb((214, 214, 204)),
                success: rgb((72, 200, 120)),
                warning: rgb((235, 190, 70)),
                danger: rgb((235, 80, 100)),
                info: rgb((70, 190, 220)),
                running: rgb((124, 160, 255)),
                blocked: rgb((244, 122, 96)),
                pending: rgb((150, 150, 168)),
                inter_focus: rgb((26, 48, 58)),
                inter_hover: rgb((34, 34, 42)),
                inter_selected: rgb((26, 48, 58)),
                inter_pressed: rgb((58, 122, 138)),
                inter_disabled: rgb((98, 97, 94)),
                accent_primary: rgb((86, 178, 196)),
                accent_secondary: rgb((232, 168, 96)),
                accent_muted: rgb((24, 44, 52)),
            },
            Self::Graphite => Palette {
                bg: rgb((10, 10, 10)),
                surface: rgb((18, 18, 18)),
                elevated: rgb((27, 27, 27)),
                overlay: rgb((32, 32, 32)),
                input: rgb((20, 20, 20)),
                selected: rgb((40, 40, 40)),
                border_subtle: rgb((37, 37, 37)),
                border: rgb((58, 58, 58)),
                border_active: rgb((105, 105, 105)),
                border_focus: rgb((222, 222, 222)),
                border_destructive: rgb((235, 80, 100)),
                text_primary: rgb((240, 240, 240)),
                text_secondary: rgb((183, 183, 183)),
                text_muted: rgb((138, 138, 138)),
                text_disabled: rgb((100, 100, 100)),
                text_inverse: rgb((10, 10, 10)),
                text_link: rgb((214, 214, 214)),
                text_code: rgb((222, 222, 222)),
                success: rgb((72, 200, 120)),
                warning: rgb((235, 190, 70)),
                danger: rgb((235, 80, 100)),
                info: rgb((70, 190, 220)),
                running: rgb((124, 160, 255)),
                blocked: rgb((244, 122, 96)),
                pending: rgb((150, 150, 168)),
                inter_focus: rgb((40, 40, 40)),
                inter_hover: rgb((31, 31, 31)),
                inter_selected: rgb((40, 40, 40)),
                inter_pressed: rgb((96, 96, 96)),
                inter_disabled: rgb((100, 100, 100)),
                accent_primary: rgb((222, 222, 222)),
                accent_secondary: rgb((198, 198, 198)),
                accent_muted: rgb((30, 30, 30)),
            },
            Self::Midnight => Palette {
                bg: rgb((6, 8, 14)),
                surface: rgb((12, 16, 26)),
                elevated: rgb((19, 24, 37)),
                overlay: rgb((24, 30, 45)),
                input: rgb((15, 19, 30)),
                selected: rgb((22, 38, 68)),
                border_subtle: rgb((26, 34, 51)),
                border: rgb((40, 51, 75)),
                border_active: rgb((82, 102, 147)),
                border_focus: rgb((96, 158, 255)),
                border_destructive: rgb((235, 80, 100)),
                text_primary: rgb((226, 233, 245)),
                text_secondary: rgb((166, 178, 200)),
                text_muted: rgb((120, 133, 158)),
                text_disabled: rgb((86, 98, 120)),
                text_inverse: rgb((6, 8, 14)),
                text_link: rgb((128, 178, 255)),
                text_code: rgb((186, 205, 238)),
                success: rgb((72, 200, 120)),
                warning: rgb((235, 190, 70)),
                danger: rgb((235, 80, 100)),
                info: rgb((70, 190, 220)),
                running: rgb((124, 160, 255)),
                blocked: rgb((244, 122, 96)),
                pending: rgb((150, 150, 168)),
                inter_focus: rgb((22, 38, 68)),
                inter_hover: rgb((28, 36, 54)),
                inter_selected: rgb((22, 38, 68)),
                inter_pressed: rgb((60, 106, 180)),
                inter_disabled: rgb((86, 98, 120)),
                accent_primary: rgb((96, 158, 255)),
                accent_secondary: rgb((168, 140, 255)),
                accent_muted: rgb((20, 34, 58)),
            },
            Self::HighContrast => Palette {
                bg: rgb((0, 0, 0)),
                surface: rgb((0, 0, 0)),
                elevated: rgb((24, 24, 24)),
                overlay: rgb((32, 32, 32)),
                input: rgb((0, 0, 0)),
                selected: rgb((255, 255, 0)),
                border_subtle: rgb((120, 120, 120)),
                // Recessed but bright: the resting edge steps down from active
                // so the focus/active ladder still exists at full contrast.
                border: rgb((200, 200, 200)),
                border_active: rgb((255, 255, 255)),
                border_focus: rgb((255, 255, 0)),
                border_destructive: rgb((255, 0, 0)),
                text_primary: rgb((255, 255, 255)),
                text_secondary: rgb((224, 224, 224)),
                text_muted: rgb((170, 170, 170)),
                text_disabled: rgb((128, 128, 128)),
                text_inverse: rgb((0, 0, 0)),
                text_link: rgb((0, 200, 255)),
                text_code: rgb((255, 255, 0)),
                success: rgb((0, 255, 0)),
                warning: rgb((255, 200, 0)),
                danger: rgb((255, 0, 0)),
                info: rgb((0, 200, 255)),
                running: rgb((255, 140, 0)),
                // Distinct from `danger` on purpose: "the host will not let us
                // proceed" is not a failure, and collapsing the two is exactly
                // what makes an agent feel unsafe. Magenta-red is equally loud
                // in this palette but unmistakably not the same signal.
                blocked: rgb((255, 0, 140)),
                pending: rgb((255, 255, 255)),
                inter_focus: rgb((255, 255, 0)),
                inter_hover: rgb((24, 24, 24)),
                inter_selected: rgb((255, 255, 0)),
                inter_pressed: rgb((0, 200, 255)),
                inter_disabled: rgb((160, 160, 160)),
                accent_primary: rgb((255, 255, 0)),
                accent_secondary: rgb((0, 200, 255)),
                accent_muted: rgb((70, 70, 0)),
            },
            // Surfaces ladder by lightness only; every semantic role resolves
            // inside the same family. Nothing here depends on hue.
            Self::Monochrome => Palette {
                bg: rgb((0, 0, 0)),
                surface: rgb((16, 16, 16)),
                elevated: rgb((28, 28, 28)),
                overlay: rgb((36, 36, 36)),
                input: rgb((20, 20, 20)),
                selected: rgb((58, 58, 58)),
                border_subtle: rgb((32, 32, 32)),
                border: rgb((56, 56, 56)),
                border_active: rgb((105, 105, 105)),
                border_focus: rgb((238, 238, 238)),
                border_destructive: rgb((238, 238, 238)),
                text_primary: rgb((240, 240, 240)),
                text_secondary: rgb((188, 188, 188)),
                text_muted: rgb((146, 146, 146)),
                text_disabled: rgb((104, 104, 104)),
                text_inverse: rgb((8, 8, 8)),
                text_link: rgb((238, 238, 238)),
                text_code: rgb((212, 212, 212)),
                // Distinct values, no hue: each status tone must differ from
                // every other by brightness alone. `blocked` sits mid-ramp
                // because a host refusal is a hold, not a crash and not a
                // success — it should not read as the loudest thing on screen.
                success: rgb((240, 240, 240)),
                warning: rgb((222, 222, 222)),
                danger: rgb((238, 238, 238)),
                info: rgb((206, 206, 206)),
                running: rgb((232, 232, 232)),
                blocked: rgb((190, 190, 190)),
                pending: rgb((150, 150, 150)),
                inter_focus: rgb((58, 58, 58)),
                inter_hover: rgb((44, 44, 44)),
                inter_selected: rgb((58, 58, 58)),
                inter_pressed: rgb((70, 70, 70)),
                inter_disabled: rgb((104, 104, 104)),
                accent_primary: rgb((238, 238, 238)),
                accent_secondary: rgb((190, 190, 190)),
                accent_muted: rgb((44, 44, 44)),
            },
        }
    }
}

/// The runtime theme handle. One enum wide, cheap to clone, held by both
/// renderers and the Settings view, switched without a restart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Theme {
    pub kind: ThemeKind,
}

impl Theme {
    pub const fn new(kind: ThemeKind) -> Self {
        Self { kind }
    }

    pub fn default_theme() -> Self {
        Self {
            kind: ThemeKind::default(),
        }
    }

    pub const fn palette(&self) -> Palette {
        self.kind.palette()
    }

    /// Resolve the semantic vocabulary for this theme.
    pub fn tokens(&self) -> crate::design::tokens::Tokens {
        crate::design::tokens::Tokens::from_theme(self.kind)
    }

    /// Switch in place. The next frame renders the new palette.
    pub fn set(&mut self, kind: ThemeKind) {
        self.kind = kind;
    }
}

impl FromStr for ThemeKind {
    type Err = ();

    /// Unknown values fall back to the default rather than failing: a
    /// corrupt or hand-edited preference must never stop the TUI starting.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ThemeKind::ALL
            .into_iter()
            .find(|t| t.name() == s.trim())
            .ok_or(())
    }
}

// ----------------------------------------------------------------------
// Persistence
// ----------------------------------------------------------------------

/// Environment override (CI and ephemeral runs). Purely visual.
const THEME_ENV: &str = "OPENRT_TUI_THEME";

/// Where the selected theme is stored.
///
/// This is **visual preference only** and is deliberately not routed through
/// `openrt-config`, which owns runtime semantics (resource limits, override
/// policy, provider selection). UI state must not be able to change behavior,
/// and a UI preference must never make a config load fail.
///
/// Resolution order: `OPENRT_TUI_THEME` →
/// `$XDG_CONFIG_HOME/openrt/tui.toml` → `$HOME/.config/openrt/tui.toml`.
///
/// Resolved **once** at startup by [`App::new`](crate::app::App) and stored on
/// the app, so a save never re-reads the environment mid-session.
pub fn theme_config_path() -> Option<std::path::PathBuf> {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        if !dir.trim().is_empty() {
            return Some(
                std::path::PathBuf::from(dir)
                    .join("openrt")
                    .join("tui.toml"),
            );
        }
    }
    let home = std::env::var("HOME")
        .ok()
        .filter(|h| !h.trim().is_empty())?;
    Some(
        std::path::PathBuf::from(home)
            .join(".config")
            .join("openrt")
            .join("tui.toml"),
    )
}

/// Load the persisted theme, falling back to the default.
///
/// Reads the operator's real preference file. Production entry point only —
/// tests must call [`load_theme_from`], because a test that resolves the real
/// path is a test whose result depends on the developer's own configuration.
pub fn load_theme() -> Theme {
    if let Ok(v) = std::env::var(THEME_ENV) {
        if let Ok(kind) = ThemeKind::from_str(&v) {
            return Theme::new(kind);
        }
    }
    load_theme_from(theme_config_path().as_deref())
}

/// Load the persisted theme from an explicit `path`.
///
/// Never fails the caller: a missing, unreadable or malformed file simply
/// yields [`Theme::default_theme`]. Taking the path as an argument is what
/// makes the behaviour testable without touching the developer's real
/// `~/.config`, mirroring [`save_theme_to`].
pub fn load_theme_from(path: Option<&std::path::Path>) -> Theme {
    let Some(path) = path else {
        return Theme::default_theme();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Theme::default_theme();
    };
    parse_theme_file(&text).unwrap_or_else(Theme::default_theme)
}

/// Tolerant parse of the tiny preferences file.
///
/// Deliberately lenient: this file is a visual preference, and hand-editing it
/// must not be a startup failure. Values are unquoted, trimmed and cut at the
/// first inline comment.
pub fn parse_theme_file(text: &str) -> Option<Theme> {
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "theme" {
            continue;
        }
        let raw = value.trim().trim_matches(|c| c == '"' || c == '\'');
        return ThemeKind::from_str(raw).ok().map(Theme::new);
    }
    None
}

/// Persist the selection.
///
/// `Ok(())` when the theme was written, or when there is no config path at all
/// (a container without `HOME` simply does not persist — it still runs).
/// Errors are returned so Settings can show an honest `ErrorState` instead of
/// pretending the save worked.
pub fn save_theme(theme: Theme) -> Result<(), String> {
    save_theme_to(theme_config_path().as_deref(), theme)
}

/// Persist `theme` to an explicit `path`.
///
/// Taking the path as an argument (rather than looking it up again) is what
/// keeps a commit writing to the place the session actually resolved at
/// startup, and lets a test redirect the write without touching the
/// developer's real preference file.
pub fn save_theme_to(path: Option<&std::path::Path>, theme: Theme) -> Result<(), String> {
    let Some(path) = path else {
        return Ok(());
    };
    let path = path.to_path_buf();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let body = format!(
        "# OpenRT TUI visual preference.\n\
         # Runtime semantics (resource limits, override policy, providers) live in\n\
         # the openrt runtime config. This file is presentation only.\n\
         theme = \"{}\"\n",
        theme.kind.name()
    );
    std::fs::write(&path, body).map_err(|e| format!("write {}: {e}", path.display()))
}

// ----------------------------------------------------------------------
// Compatibility surface
// ----------------------------------------------------------------------

/// The pre-design-system palette constants, as aliases over
/// [`ThemeKind::PitchBlack`].
///
/// They existed so the migration could be incremental. **No view uses them any
/// more** — every renderer resolves colors from
/// [`crate::design::tokens::Tokens`], which is why a theme switch now actually
/// repaints the whole shell. They are frozen here only as a source-compatibility
/// surface for external callers, and they are *permanently* Pitch Black: using
/// one in a view silently pins that surface to the default theme, which is the
/// exact bug this phase removed.
///
/// New code must use `Tokens`.
pub use compat::*;

mod compat {
    use super::ThemeKind;
    use ratatui::style::Color;

    macro_rules! compat {
        ($name:ident, $field:ident, $doc:literal) => {
            #[doc = $doc]
            #[doc = ""]
            #[doc = "Deprecated: permanently Pitch Black. Use `Tokens`."]
            #[deprecated(
                since = "0.1.0",
                note = "permanently Pitch Black; use crate::design::tokens::Tokens"
            )]
            #[allow(non_upper_case_globals)]
            pub const $name: Color = {
                let p = ThemeKind::PitchBlack.palette();
                p.$field
            };
        };
    }

    compat!(BG, bg, "App background (alias of `Tokens::surfaces.bg`).");
    compat!(
        SURFACE,
        surface,
        "Panel surface (alias of `Tokens::surfaces.surface`)."
    );
    compat!(
        ACCENT,
        accent_primary,
        "Primary accent: focus, selection, primary actions."
    );
    compat!(
        DIM,
        text_muted,
        "Faint/secondary text (was the one and only dim)."
    );
    compat!(OK, success, "Success / healthy / allowed.");
    compat!(BAD, danger, "Failure / danger / denied.");
    compat!(WARN, warning, "Caution / restricted posture.");
    compat!(INFO, info, "Informational.");
    compat!(
        BORDER,
        border,
        "Thin border color — borders only, never text."
    );
    compat!(BORDER_ON, border_focus, "Focused border.");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every palette slot, so the completeness test cannot silently skip a
    /// field added later.
    fn slots(p: &Palette) -> Vec<(&'static str, Color)> {
        vec![
            ("bg", p.bg),
            ("surface", p.surface),
            ("elevated", p.elevated),
            ("overlay", p.overlay),
            ("input", p.input),
            ("selected", p.selected),
            ("border_subtle", p.border_subtle),
            ("border", p.border),
            ("border_active", p.border_active),
            ("border_focus", p.border_focus),
            ("border_destructive", p.border_destructive),
            ("text_primary", p.text_primary),
            ("text_secondary", p.text_secondary),
            ("text_muted", p.text_muted),
            ("text_disabled", p.text_disabled),
            ("text_inverse", p.text_inverse),
            ("text_link", p.text_link),
            ("text_code", p.text_code),
            ("success", p.success),
            ("warning", p.warning),
            ("danger", p.danger),
            ("info", p.info),
            ("running", p.running),
            ("blocked", p.blocked),
            ("pending", p.pending),
            ("inter_focus", p.inter_focus),
            ("inter_hover", p.inter_hover),
            ("inter_selected", p.inter_selected),
            ("inter_pressed", p.inter_pressed),
            ("inter_disabled", p.inter_disabled),
            ("accent_primary", p.accent_primary),
            ("accent_secondary", p.accent_secondary),
            ("accent_muted", p.accent_muted),
        ]
    }

    /// The text/background pairs the app actually paints, with the floor each
    /// one has to clear in a normal theme and in `HighContrast`.
    ///
    /// Two floors exist because WCAG 2.x deliberately exempts *disabled*
    /// controls from the contrast minimum: dimming is how a control says "not
    /// actionable", and holding disabled text to the body-text figure would
    /// mean disabled and enabled look identical. 3:1 is the figure WCAG uses
    /// for UI components and graphical objects, which is what a disabled label
    /// is. `HighContrast` raises even that, because its entire promise is that
    /// nothing on screen is hard to read.
    ///
    /// Note `selected`: the list renderer paints a selected row as
    /// `text_inverse` on `accent.primary`. It does **not** use `text_primary`
    /// on `surfaces.selected`, which in `HighContrast` measures 1.07:1 — so the
    /// inversion convention is load-bearing, not decorative.
    const MEASURED: [(
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        f64,
        f64,
    ); 11] = [
        ("primary/bg", "text_primary", "bg", "body prose", 4.5, 7.0),
        (
            "primary/surface",
            "text_primary",
            "surface",
            "body in a panel",
            4.5,
            7.0,
        ),
        (
            "primary/elevated",
            "text_primary",
            "elevated",
            "body on a card",
            4.5,
            7.0,
        ),
        ("secondary/bg", "text_secondary", "bg", "hints", 4.5, 7.0),
        ("muted/bg", "text_muted", "bg", "metadata", 4.5, 7.0),
        (
            "disabled/bg",
            "text_disabled",
            "bg",
            "disabled label",
            3.0,
            4.5,
        ),
        (
            "primary/overlay",
            "text_primary",
            "overlay",
            "dialog body",
            4.5,
            7.0,
        ),
        (
            "primary/input",
            "text_primary",
            "input",
            "composer",
            4.5,
            7.0,
        ),
        (
            "inverse/accent",
            "text_inverse",
            "accent_primary",
            "selected row",
            4.5,
            7.0,
        ),
        (
            "primary/selected",
            "text_primary",
            "selected",
            "primary on selected (unused)",
            0.0,
            0.0,
        ),
        (
            "mono_floor",
            "text_primary",
            "bg",
            "Monochrome body",
            4.5,
            0.0,
        ),
    ];

    fn slot(p: &Palette, name: &str) -> ratatui::style::Color {
        match name {
            "bg" => p.bg,
            "surface" => p.surface,
            "elevated" => p.elevated,
            "overlay" => p.overlay,
            "input" => p.input,
            "selected" => p.selected,
            "text_primary" => p.text_primary,
            "text_secondary" => p.text_secondary,
            "text_muted" => p.text_muted,
            "text_disabled" => p.text_disabled,
            "text_inverse" => p.text_inverse,
            "accent_primary" => p.accent_primary,
            other => panic!("unknown slot {other}"),
        }
    }

    /// Measured, reproducible contrast for all six themes.
    ///
    /// The thresholds are the WCAG 2.x 4.5:1 body-text figure and the 7:1
    /// large-text/UI figure that `HighContrast` promises. This is a measurement
    /// of the palette, **not** a claim of WCAG conformance — see
    /// [`crate::design::contrast`] for what a terminal ratio can and cannot
    /// tell you.
    #[test]
    fn measured_contrast_across_all_six_themes() {
        let mut failures: Vec<String> = Vec::new();
        for kind in ThemeKind::ALL {
            let p = kind.palette();
            let hc = kind == ThemeKind::HighContrast;
            for (label, fg, bg, what, normal, high) in MEASURED {
                // Rows with a zero floor are recorded but not asserted: they
                // document a pair the renderer deliberately avoids.
                if label.starts_with("primary/selected") {
                    continue;
                }
                if label.starts_with("mono_") && kind != ThemeKind::Monochrome {
                    continue;
                }
                if normal == 0.0 && high == 0.0 {
                    continue;
                }
                let want = if hc { high } else { normal };
                let got = crate::design::contrast::ratio(slot(&p, fg), slot(&p, bg)).unwrap_or(0.0);
                println!("{label:20} {got:9.4}  {what:36} {kind:?} (>= {want:.2})");
                if got + 1e-9 < want {
                    failures.push(format!("{kind:?} {label} ({what}): {got:.4} < {want:.2}"));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} measured pair(s) below their floor:\n{}",
            failures.len(),
            failures.join("\n"),
        );
    }

    /// Documents the trap rather than hiding it: `text_primary` on `selected`
    /// must not be readable, because that is precisely why every renderer has
    /// to invert on a selected row. If this ever becomes readable, someone
    /// changed the palette without revisiting the inversion.
    #[test]
    fn the_selected_row_is_unreadable_without_inversion() {
        let p = ThemeKind::HighContrast.palette();
        let plain = crate::design::contrast::ratio(p.text_primary, p.selected).unwrap();
        let inverted = crate::design::contrast::ratio(p.text_inverse, p.selected).unwrap();
        println!("HighContrast selected row: primary {plain:.4}:1, inverse {inverted:.4}:1",);
        assert!(
            plain < 4.5,
            "primary-on-selected is now readable ({plain:.4}); the inversion convention is dead code",
        );
        assert!(
            inverted >= 4.5,
            "inverted selection is unreadable: {inverted:.4}"
        );
    }

    #[test]
    fn there_are_exactly_six_built_in_themes_with_unique_names() {
        assert_eq!(ThemeKind::ALL.len(), 6);
        let mut names: Vec<&str> = ThemeKind::ALL.iter().map(|t| t.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 6, "theme names must be unique");
    }

    #[test]
    fn every_theme_defines_all_33_slots_explicitly() {
        for kind in ThemeKind::ALL {
            assert_eq!(slots(&kind.palette()).len(), 33, "slot list drifted");
            for (name, c) in slots(&kind.palette()) {
                let ok = matches!(c, Color::Rgb(..));
                assert!(
                    ok,
                    "{} slot `{name}` is not a palette value: {c:?}",
                    kind.name()
                );
            }
        }
    }

    #[test]
    fn focus_is_always_visible_against_the_background() {
        for kind in ThemeKind::ALL {
            let p = kind.palette();
            assert_ne!(
                p.border_focus,
                p.bg,
                "{} focus border equals the background",
                kind.name()
            );
            assert_ne!(
                p.inter_focus,
                p.surface,
                "{} focus fill equals the surface",
                kind.name()
            );
        }
    }

    #[test]
    fn no_status_color_is_invisible_on_its_own_background() {
        for kind in ThemeKind::ALL {
            let p = kind.palette();
            for (label, c) in [
                ("danger", p.danger),
                ("blocked", p.blocked),
                ("success", p.success),
                ("warning", p.warning),
                ("running", p.running),
                ("pending", p.pending),
            ] {
                assert_ne!(
                    c,
                    p.bg,
                    "{} {label} is invisible on the background",
                    kind.name()
                );
                assert_ne!(
                    c,
                    p.surface,
                    "{} {label} is invisible on a panel",
                    kind.name()
                );
            }
        }
    }

    #[test]
    fn body_text_is_never_the_panel_color() {
        for kind in ThemeKind::ALL {
            let p = kind.palette();
            for (label, c) in [
                ("primary", p.text_primary),
                ("secondary", p.text_secondary),
                ("muted", p.text_muted),
                ("link", p.text_link),
                ("code", p.text_code),
            ] {
                assert_ne!(
                    c,
                    p.surface,
                    "{} {label} text is invisible on a panel",
                    kind.name()
                );
            }
        }
    }

    #[test]
    fn high_contrast_maximizes_separation_but_keeps_the_state_ladder() {
        let p = ThemeKind::HighContrast.palette();
        assert_eq!(p.text_primary, Color::Rgb(255, 255, 255));
        assert_eq!(p.border_active, Color::Rgb(255, 255, 255));
        assert_eq!(p.border_focus, Color::Rgb(255, 255, 0));
        // The resting < active < focus ladder must survive, or focus becomes
        // indistinguishable from merely-active.
        assert!(lum(p.border_subtle) < lum(p.border));
        assert!(lum(p.border) < lum(p.border_active));
        assert!(ThemeKind::HighContrast.prefers_strong_marks());
    }

    /// Perceptual-ish luminance, only ever compared against other grays in
    /// the same test.
    fn lum(c: Color) -> u32 {
        let Color::Rgb(r, g, b) = c else {
            panic!("expected a gray, got {c:?}");
        };
        (0.2126 * f64::from(r) + 0.7152 * f64::from(g) + 0.0722 * f64::from(b)) as u32
    }

    #[test]
    fn monochrome_is_entirely_neutral() {
        let p = ThemeKind::Monochrome.palette();
        for (name, c) in slots(&p) {
            let Color::Rgb(r, g, b) = c else {
                panic!("monochrome slot `{name}` must be literal RGB: {c:?}");
            };
            assert_eq!(
                (r, g, b),
                (r, r, r),
                "monochrome slot `{name}` carries hue: {c:?}"
            );
        }
        assert!(ThemeKind::Monochrome.is_monochrome());
        assert!(ThemeKind::Monochrome.prefers_strong_marks());
    }

    #[test]
    fn dark_themes_share_one_status_ladder() {
        // Semantic meaning is shared across the dark set: only High Contrast
        // (contrast is its purpose) and Monochrome (no hue) re-specify it.
        let ref_p = ThemeKind::PitchBlack.palette();
        for kind in [
            ThemeKind::Obsidian,
            ThemeKind::Graphite,
            ThemeKind::Midnight,
        ] {
            let p = kind.palette();
            assert_eq!(p.success, ref_p.success, "{} success drifted", kind.name());
            assert_eq!(p.danger, ref_p.danger, "{} danger drifted", kind.name());
            assert_eq!(p.warning, ref_p.warning, "{} warning drifted", kind.name());
            assert_eq!(p.blocked, ref_p.blocked, "{} blocked drifted", kind.name());
            assert_eq!(p.running, ref_p.running, "{} running drifted", kind.name());
        }
    }

    #[test]
    fn dark_themes_have_their_own_accent_identity() {
        let a = ThemeKind::PitchBlack.palette().accent_primary;
        let b = ThemeKind::Obsidian.palette().accent_primary;
        let c = ThemeKind::Midnight.palette().accent_primary;
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        assert_ne!(
            ThemeKind::Midnight.palette().accent_primary,
            ThemeKind::PitchBlack.palette().accent_secondary,
            "primary and secondary accents must be distinguishable"
        );
    }

    #[test]
    fn theme_names_round_trip() {
        for kind in ThemeKind::ALL {
            assert_eq!(ThemeKind::from_str(kind.name()), Ok(kind));
        }
    }

    #[test]
    fn unknown_names_and_garbage_parse_to_nothing() {
        assert_eq!(ThemeKind::from_str("nope"), Err(()));
        assert_eq!(ThemeKind::from_str(""), Err(()));
        assert_eq!(
            ThemeKind::from_str("Pitch Black"),
            Err(()),
            "names are kebab-case"
        );
    }

    #[test]
    fn the_file_parser_tolerates_normal_user_messiness() {
        let cases = [
            "theme = \"midnight\"\n",
            "theme = midnight\n",
            "theme='midnight'\n",
            "  theme   =   midnight   \n",
            "theme = midnight # the blue one\n",
            "# theme = obsidian\ntheme = midnight\n",
            "[ui]\ntheme = \"midnight\"\n",
            "\n\n\n",
        ];
        for case in cases {
            assert_eq!(
                parse_theme_file(case).map(|t| t.kind),
                if case.trim().is_empty() || case.starts_with('#') && case.lines().count() == 1 {
                    None
                } else {
                    Some(ThemeKind::Midnight)
                },
                "failed to parse: {case:?}"
            );
        }
    }

    #[test]
    fn a_malformed_file_falls_back_instead_of_failing() {
        assert_eq!(parse_theme_file("theme = \"\x00garbage\""), None);
        assert_eq!(parse_theme_file("theme"), None);
        assert_eq!(parse_theme_file("theme = 42"), None);
    }

    #[test]
    #[allow(deprecated)] // this test is the point of the aliases
    fn compat_aliases_match_the_pitch_black_palette() {
        let p = ThemeKind::PitchBlack.palette();
        assert_eq!(BG, p.bg);
        assert_eq!(SURFACE, p.surface);
        assert_eq!(ACCENT, p.accent_primary);
        assert_eq!(DIM, p.text_muted);
        assert_eq!(OK, p.success);
        assert_eq!(BAD, p.danger);
        assert_eq!(WARN, p.warning);
        assert_eq!(INFO, p.info);
        assert_eq!(BORDER, p.border);
        assert_eq!(BORDER_ON, p.border_focus);
    }
}

// ===========================================================================
// P3A — token completeness, and P3B — semantic separation.
//
// The executable spec of "complete": every semantic role the design language
// names must resolve to a real value for every theme, so no view ever has to
// reach for a raw RGB value or invent a fallback.
// ===========================================================================

#[cfg(test)]
mod token_completeness_tests {
    use super::*;
    use crate::design::tokens::Tokens;
    use ratatui::style::Color;

    /// The required semantic vocabulary, spelled out. Rust has no reflection,
    /// so this list *is* the contract: adding a token to `Tokens` without
    /// adding it here fails the "every token is covered" test below, and
    /// removing one fails the per-theme completeness tests.
    const REQUIRED_SURFACES: [&str; 6] = [
        "background",
        "elevated",
        "panel",
        "overlay",
        "input",
        "selection",
    ];
    const REQUIRED_BORDERS: [&str; 5] = ["subtle", "normal", "active", "focus", "danger"];
    const REQUIRED_TEXT: [&str; 7] = [
        "primary",
        "secondary",
        "muted",
        "disabled",
        "inverse",
        "link",
        "code",
    ];
    const REQUIRED_STATUS: [&str; 7] = [
        "success", "warning", "danger", "info", "running", "pending", "blocked",
    ];
    const REQUIRED_INTERACTION: [&str; 5] = ["focus", "hover", "selected", "pressed", "disabled"];
    const REQUIRED_ACCENT: [&str; 3] = ["primary", "secondary", "muted"];
    const REQUIRED_SPACING: [&str; 5] = ["xs", "sm", "md", "lg", "xl"];
    const REQUIRED_TYPE_ROLES: [&str; 12] = [
        "app_title",
        "page_title",
        "section",
        "body",
        "secondary",
        "metadata",
        "code",
        "badge",
        "metric",
        "warning",
        "error",
        "command",
    ];

    /// The concrete fields backing each required name. Kept adjacent to the
    /// names above so a rename cannot silently drop a role.
    fn surfaces(t: &Tokens) -> [(&'static str, Color); 6] {
        [
            ("background", t.surfaces.bg),
            ("elevated", t.surfaces.elevated),
            ("panel", t.surfaces.surface),
            ("overlay", t.surfaces.overlay),
            ("input", t.surfaces.input),
            ("selection", t.surfaces.selected),
        ]
    }

    fn borders(t: &Tokens) -> [(&'static str, Color); 5] {
        [
            ("subtle", t.borders.subtle),
            ("normal", t.borders.default),
            ("active", t.borders.active),
            ("focus", t.borders.focus),
            ("danger", t.borders.destructive),
        ]
    }

    fn text(t: &Tokens) -> [(&'static str, Color); 7] {
        [
            ("primary", t.text.primary),
            ("secondary", t.text.secondary),
            ("muted", t.text.muted),
            ("disabled", t.text.disabled),
            ("inverse", t.text.inverse),
            ("link", t.text.link),
            ("code", t.text.code),
        ]
    }

    fn status(t: &Tokens) -> [(&'static str, Color); 7] {
        [
            ("success", t.status.success),
            ("warning", t.status.warning),
            ("danger", t.status.danger),
            ("info", t.status.info),
            ("running", t.status.running),
            ("pending", t.status.pending),
            ("blocked", t.status.blocked),
        ]
    }

    fn interaction(t: &Tokens) -> [(&'static str, Color); 5] {
        [
            ("focus", t.interaction.focus),
            ("hover", t.interaction.hover),
            ("selected", t.interaction.selected),
            ("pressed", t.interaction.pressed),
            ("disabled", t.interaction.disabled),
        ]
    }

    fn accent(t: &Tokens) -> [(&'static str, Color); 3] {
        [
            ("primary", t.accent.primary),
            ("secondary", t.accent.secondary),
            ("muted", t.accent.muted),
        ]
    }

    fn spacing(t: &Tokens) -> [(&'static str, u16); 5] {
        [
            ("xs", t.space.xs),
            ("sm", t.space.sm),
            ("md", t.space.md),
            ("lg", t.space.lg),
            ("xl", t.space.xl),
        ]
    }

    fn type_roles(t: &Tokens) -> [(&'static str, ratatui::style::Style); 12] {
        [
            ("app_title", t.type_scale.app_title),
            ("page_title", t.type_scale.page_title),
            ("section", t.type_scale.section_title),
            ("body", t.type_scale.body),
            ("secondary", t.type_scale.secondary_body),
            ("metadata", t.type_scale.metadata),
            ("code", t.type_scale.code),
            ("badge", t.type_scale.badge),
            ("metric", t.type_scale.metric),
            ("warning", t.type_scale.warning),
            ("error", t.type_scale.error),
            ("command", t.type_scale.command),
        ]
    }

    /// A color that renders as "unset" would silently inherit the terminal
    /// default, which is how a theme ends up half-applied.
    fn is_real(c: Color) -> bool {
        !matches!(c, Color::Reset)
    }

    // -----------------------------------------------------------------
    // P3A — every theme, every token
    // -----------------------------------------------------------------

    #[test]
    fn every_theme_provides_every_surface_border_and_text_token() {
        for theme in ThemeKind::ALL {
            let t = Tokens::from_theme(theme);
            for (name, c) in surfaces(&t).into_iter().chain(borders(&t)).chain(text(&t)) {
                assert!(is_real(c), "{theme:?} has no {name} token");
            }
            assert_eq!(REQUIRED_SURFACES.len(), 6);
            assert_eq!(REQUIRED_BORDERS.len(), 5);
            assert_eq!(REQUIRED_TEXT.len(), 7);
        }
    }

    #[test]
    fn every_theme_provides_every_status_interaction_and_accent_token() {
        for theme in ThemeKind::ALL {
            let t = Tokens::from_theme(theme);
            for (name, c) in status(&t)
                .into_iter()
                .chain(interaction(&t))
                .chain(accent(&t))
            {
                assert!(is_real(c), "{theme:?} has no {name} token");
            }
        }
    }

    #[test]
    fn every_theme_provides_the_full_spacing_scale() {
        for theme in ThemeKind::ALL {
            let t = Tokens::from_theme(theme);
            let s = spacing(&t);
            // `xs` is deliberately 0 — it is the "no padding" token. Every
            // other step must be usable, or a component asking for `sm` gets
            // an invisible margin.
            for (name, v) in s.iter().skip(1) {
                assert!(*v > 0, "{theme:?} spacing {name} must be usable");
            }
            // The scale has to actually scale, or "md" is a lie.
            assert!(
                s[0].1 <= s[1].1 && s[1].1 <= s[2].1 && s[2].1 <= s[3].1 && s[3].1 <= s[4].1,
                "{theme:?} spacing is not monotonic: {s:?}"
            );
            assert!(
                s[0].1 == 0,
                "{theme:?}: xs must stay the zero-padding token"
            );
        }
    }

    #[test]
    fn every_theme_resolves_every_typographic_role_to_a_real_style() {
        for theme in ThemeKind::ALL {
            let t = Tokens::from_theme(theme);
            for (name, style) in type_roles(&t) {
                let fg = style.fg;
                assert!(
                    fg.is_some(),
                    "{theme:?} type role {name} has no foreground — it would inherit the terminal default"
                );
                assert!(
                    is_real(fg.unwrap()),
                    "{theme:?} type role {name} uses Color::Reset"
                );
            }
        }
    }

    #[test]
    fn the_required_vocabulary_is_fully_covered_by_the_accessors() {
        // Guards the guard: if a role is renamed in the struct but not here,
        // or here but not in the struct, the suite above is quietly testing a
        // smaller contract than the design language claims.
        let t = Tokens::from_theme(ThemeKind::PitchBlack);
        assert_eq!(surfaces(&t).len(), REQUIRED_SURFACES.len());
        assert_eq!(borders(&t).len(), REQUIRED_BORDERS.len());
        assert_eq!(text(&t).len(), REQUIRED_TEXT.len());
        assert_eq!(status(&t).len(), REQUIRED_STATUS.len());
        assert_eq!(interaction(&t).len(), REQUIRED_INTERACTION.len());
        assert_eq!(accent(&t).len(), REQUIRED_ACCENT.len());
        assert_eq!(spacing(&t).len(), REQUIRED_SPACING.len());
        assert_eq!(type_roles(&t).len(), REQUIRED_TYPE_ROLES.len());
    }

    #[test]
    fn tokens_cover_exactly_the_documented_palette() {
        // `Tokens::colors` is the executable definition of the palette. Every
        // theme must yield the same count, or one theme has extra roles no
        // other theme defines.
        // Pinned literally rather than derived from `colors()`: a test that
        // computes its own expectation from the same function under test proves
        // only that the function is self-consistent.
        const EXPECTED: usize = 33;
        let expected = EXPECTED;
        assert_eq!(expected, 33, "the palette is 33 colors");
        for theme in ThemeKind::ALL {
            assert_eq!(
                Tokens::from_theme(theme).colors().len(),
                expected,
                "{theme:?} defines a different number of colors"
            );
        }
    }

    // -----------------------------------------------------------------
    // P3B — semantic separation
    // -----------------------------------------------------------------

    /// Perceptual distance, so "different" means visibly different rather than
    /// merely a different triple.
    fn distance(a: Color, b: Color) -> f64 {
        let (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) = (a, b) else {
            return f64::MAX;
        };
        let dr = f64::from(r1) - f64::from(r2);
        let dg = f64::from(g1) - f64::from(g2);
        let db = f64::from(b1) - f64::from(b2);
        (dr * dr + dg * dg + db * db).sqrt()
    }

    #[test]
    fn success_is_never_the_same_color_as_blocked() {
        for theme in ThemeKind::ALL {
            let t = Tokens::from_theme(theme);
            assert_ne!(
                t.status.success, t.status.blocked,
                "{theme:?}: SUCCESS and BLOCKED are the same color"
            );
        }
    }

    #[test]
    fn blocked_is_never_the_same_color_as_failed() {
        for theme in ThemeKind::ALL {
            let t = Tokens::from_theme(theme);
            assert_ne!(
                t.status.blocked, t.status.danger,
                "{theme:?}: BLOCKED and FAILED are the same color"
            );
        }
    }

    #[test]
    fn warning_is_never_the_same_color_as_failed() {
        for theme in ThemeKind::ALL {
            let t = Tokens::from_theme(theme);
            assert_ne!(
                t.status.warning, t.status.danger,
                "{theme:?}: WARNING and FAILED are the same color"
            );
        }
    }

    #[test]
    fn running_is_never_the_same_color_as_pending() {
        // This is the collision class that bit before: both read as "busy", so
        // a stream in flight and a queued job looked identical.
        for theme in ThemeKind::ALL {
            let t = Tokens::from_theme(theme);
            assert_ne!(
                t.status.running, t.status.pending,
                "{theme:?}: RUNNING and PENDING are the same color"
            );
        }
    }

    #[test]
    fn the_whole_status_ladder_is_internally_distinct_in_every_theme() {
        for theme in ThemeKind::ALL {
            let t = Tokens::from_theme(theme);
            let named = status(&t);
            for i in 0..named.len() {
                for j in i + 1..named.len() {
                    assert_ne!(
                        named[i].1, named[j].1,
                        "{theme:?}: status {} and {} collide",
                        named[i].0, named[j].0
                    );
                }
            }
        }
    }

    #[test]
    fn high_contrast_maximises_separation_across_the_whole_ladder() {
        // High Contrast exists for impaired and bright terminals, so it must
        // be the most separated theme, not merely "different".
        let hc = Tokens::from_theme(ThemeKind::HighContrast);
        let other = ThemeKind::ALL
            .into_iter()
            .filter(|t| *t != ThemeKind::HighContrast)
            .map(|t| Tokens::from_theme(t))
            .collect::<Vec<_>>();

        let min_pair = |t: &Tokens| {
            let s = status(t);
            let mut min = f64::MAX;
            for i in 0..s.len() {
                for j in i + 1..s.len() {
                    min = min.min(distance(s[i].1, s[j].1));
                }
            }
            min
        };

        let hc_min = min_pair(&hc);
        for t in &other {
            let min = min_pair(t);
            assert!(
                hc_min >= min,
                "High Contrast's tightest status pair ({hc_min:.1}) is not at least as separated as {:?}'s ({min:.1})",
                t
            );
        }
    }

    #[test]
    fn monochrome_stays_semantically_understandable_without_hue() {
        // Monochrome proves no view depends on hue. It must stay readable, but
        // it is *allowed* to collide on color because it separates by weight and
        // marker instead — so this asserts legibility, not distinctness.
        let m = Tokens::from_theme(ThemeKind::Monochrome);
        let s = status(&m);
        for (name, c) in s {
            let Color::Rgb(r, g, b) = c else {
                panic!("Monochrome {name} is not a concrete color");
            };
            assert_eq!((r, g, b).0, r, "Monochrome {name} is not neutral in red");
            assert_eq!(r, g, "Monochrome {name} is not neutral in green");
            assert_eq!(g, b, "Monochrome {name} is not neutral in blue");
        }
        // Body text must still contrast with the background.
        assert!(
            distance(m.text.primary, m.surfaces.bg) > 100.0,
            "Monochrome body text does not separate from the background"
        );
    }

    #[test]
    fn the_focus_affordance_is_visible_on_every_surface() {
        // Focus is the one signal whose absence makes the app unusable. It is
        // painted as a selected surface plus inverse text, so that is what has
        // to be checked: the highlight must differ from every surface it can
        // land on, and its label must differ from the highlight behind it.
        for theme in ThemeKind::ALL {
            let t = Tokens::from_theme(theme);
            let selected = t.interaction.selected;
            // `surfaces.selection` *is* the highlight — they are the same token
            // by design. Every other surface is a background the highlight has
            // to be distinguishable from.
            assert_eq!(
                selected, t.surfaces.selected,
                "{theme:?}: the selection highlight and the selection surface diverged"
            );
            for (sname, sc) in surfaces(&t) {
                if sname == "selection" {
                    continue;
                }
                assert_ne!(
                    selected, sc,
                    "{theme:?}: the focus highlight is invisible on the {sname} surface"
                );
            }
            assert_ne!(
                t.text.inverse, selected,
                "{theme:?}: focused text matches the highlight behind it"
            );
            assert_ne!(
                selected, t.borders.default,
                "{theme:?}: the focus highlight equals a normal border"
            );
        }
    }
}

#[cfg(test)]
mod persistence_tests {
    //! P3D — theme persistence.
    //!
    //! Every test here goes through `load_theme_from` / `save_theme_to` with an
    //! explicit temporary path. None of them resolve `theme_config_path()`, so
    //! none of them can read or overwrite the developer's real
    //! `~/.config/openrt/tui.toml` — which matters because that file exists and
    //! currently selects `monochrome`.

    use super::*;
    use std::path::{Path, PathBuf};

    /// A unique temporary directory per test, removed on drop so a failing
    /// assertion cannot leave a stale config behind to confuse the next run.
    struct TempConfig(PathBuf);

    impl TempConfig {
        fn new(tag: &str) -> Self {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!(
                "openrt-theme-persist-{tag}-{}-{seq}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp config dir");
            Self(dir.join("tui.toml"))
        }
        fn path(&self) -> &Path {
            &self.0
        }
        fn write(&self, body: &str) {
            if let Some(p) = self.0.parent() {
                std::fs::create_dir_all(p).expect("create parent");
            }
            std::fs::write(&self.0, body).expect("write temp config");
        }
    }

    impl Drop for TempConfig {
        fn drop(&mut self) {
            if let Some(p) = self.0.parent() {
                let _ = std::fs::remove_dir_all(p);
            }
        }
    }

    // -----------------------------------------------------------------
    // Round trip
    // -----------------------------------------------------------------

    #[test]
    fn every_theme_round_trips_through_a_real_file() {
        for kind in ThemeKind::ALL {
            let cfg = TempConfig::new("roundtrip");
            save_theme_to(Some(cfg.path()), Theme::new(kind)).expect("save must succeed");
            let loaded = load_theme_from(Some(cfg.path()));
            assert_eq!(
                loaded.kind,
                kind,
                "{} did not survive a save/load round trip",
                kind.name()
            );
        }
    }

    #[test]
    fn a_saved_file_is_readable_and_commented() {
        let cfg = TempConfig::new("readable");
        save_theme_to(Some(cfg.path()), Theme::new(ThemeKind::Midnight)).unwrap();
        let text = std::fs::read_to_string(cfg.path()).unwrap();
        assert!(text.contains("theme = \"midnight\""), "got:\n{text}");
        assert!(text.contains('#'), "the file must explain itself:\n{text}");
    }

    // -----------------------------------------------------------------
    // The default
    // -----------------------------------------------------------------

    #[test]
    fn a_missing_config_yields_the_default_theme() {
        let cfg = TempConfig::new("missing");
        assert!(!cfg.path().exists(), "precondition: nothing written yet");
        assert_eq!(
            load_theme_from(Some(cfg.path())).kind,
            Theme::default_theme().kind
        );
    }

    #[test]
    fn no_config_path_at_all_yields_the_default_theme() {
        // A container without HOME must still run, with the default theme.
        assert_eq!(load_theme_from(None).kind, Theme::default_theme().kind);
    }

    #[test]
    fn a_save_with_no_path_is_a_no_op_not_an_error() {
        assert!(
            save_theme_to(None, Theme::new(ThemeKind::Graphite)).is_ok(),
            "a machine with no config path must still run"
        );
    }

    // -----------------------------------------------------------------
    // Unknown and malformed values — the tolerant-parse contract
    // -----------------------------------------------------------------

    #[test]
    fn an_unknown_theme_name_falls_back_to_the_default() {
        let cfg = TempConfig::new("unknown");
        cfg.write("theme = \"ultraviolet\"\n");
        assert_eq!(
            load_theme_from(Some(cfg.path())).kind,
            Theme::default_theme().kind,
            "an unknown theme name must not fail startup"
        );
    }

    #[test]
    fn a_malformed_config_falls_back_to_the_default() {
        for (tag, body) in [
            ("no-equals", "this is not a config file at all\n"),
            ("empty", ""),
            ("only-comments", "# just a comment\n\n"),
            ("wrong-key", "colour = \"midnight\"\n"),
            ("empty-value", "theme = \"\"\n"),
            ("binary-ish", "\u{0}\u{1}\u{2}theme\n"),
        ] {
            let cfg = TempConfig::new(tag);
            cfg.write(body);
            assert_eq!(
                load_theme_from(Some(cfg.path())).kind,
                Theme::default_theme().kind,
                "{tag} must fall back rather than fail"
            );
        }
    }

    #[test]
    fn user_messiness_is_tolerated() {
        // Real config files carry quotes, comments, spaces, and stray keys.
        let cfg = TempConfig::new("messy");
        cfg.write(
            "# my prefs\n\
             unrelated = 3\n\
             \n\
             theme   =    'midnight'   # trailing comment\n\
             another = \"x\"\n",
        );
        assert_eq!(load_theme_from(Some(cfg.path())).kind, ThemeKind::Midnight);
    }

    #[test]
    fn the_first_theme_key_wins() {
        // Deterministic, and it must not depend on how many keys follow.
        let cfg = TempConfig::new("firstkey");
        cfg.write("theme = \"graphite\"\ntheme = \"obsidian\"\n");
        assert_eq!(load_theme_from(Some(cfg.path())).kind, ThemeKind::Graphite);
    }

    #[test]
    fn every_theme_name_round_trips_through_the_parser() {
        for kind in ThemeKind::ALL {
            let cfg = TempConfig::new("name");
            cfg.write(&format!("theme = \"{}\"\n", kind.name()));
            assert_eq!(
                load_theme_from(Some(cfg.path())).kind,
                kind,
                "name {} did not round trip",
                kind.name()
            );
        }
    }

    // -----------------------------------------------------------------
    // The developer's own machine must be irrelevant
    // -----------------------------------------------------------------

    #[test]
    fn a_developers_real_config_cannot_influence_these_results() {
        // If the developer's `~/.config/openrt/tui.toml` says `monochrome`, a
        // test that accidentally resolved the real path would read that. Prove
        // the injected path is authoritative instead: write a different theme
        // to a temp file and assert the temp file wins regardless of what is on
        // this machine.
        let cfg = TempConfig::new("machine-independent");
        save_theme_to(Some(cfg.path()), Theme::new(ThemeKind::HighContrast)).unwrap();
        assert_eq!(
            load_theme_from(Some(cfg.path())).kind,
            ThemeKind::HighContrast
        );

        // And with no file at all, the default is used — not the developer's
        // configured theme.
        let empty = TempConfig::new("machine-independent-empty");
        let default_kind = load_theme_from(Some(empty.path())).kind;
        assert_eq!(default_kind, Theme::default_theme().kind);
    }

    #[test]
    fn persistence_is_independent_of_machine_specific_preferences() {
        // The file is presentation only: it must round trip a theme and carry
        // nothing that would make one machine's file meaningful on another.
        let cfg = TempConfig::new("portable");
        save_theme_to(Some(cfg.path()), Theme::new(ThemeKind::Obsidian)).unwrap();
        let text = std::fs::read_to_string(cfg.path()).unwrap();
        for leak in [
            "/home",
            "/root",
            "C:\\",
            std::env::var("HOME").unwrap_or_default().as_str(),
        ] {
            if leak.is_empty() {
                continue;
            }
            assert!(
                !text.contains(&leak),
                "the persisted file leaked a machine-specific path {leak:?}:\n{text}"
            );
        }
    }

    #[test]
    fn a_write_to_an_unwritable_location_reports_an_error() {
        // Settings must be able to show an honest ErrorState rather than
        // pretending the save worked.
        let bad = Path::new("/proc/openrt-theme-cannot-write/tui.toml");
        let err = save_theme_to(Some(bad), Theme::new(ThemeKind::PitchBlack));
        assert!(
            err.is_err(),
            "writing to an impossible path must surface an error"
        );
    }
}
