//! Responsive layout: measured allocation instead of hard-coded assumptions.
//!
//! # The contract
//!
//! Every screen answers the same four questions through [`ShellLayout`]:
//!
//! 1. Which [`Breakpoint`] is this viewport?
//! 2. Which chrome is visible (rail, header, footer, context panel)?
//! 3. How many rows does each region get?
//! 4. Where is everything, for mouse hit-testing?
//!
//! Nothing else in the TUI may ask "is the width >= 110". That number lives in
//! [`Breakpoint::for_area`] and nowhere else, so a breakpoint change is a
//! one-line edit that cannot leave half the views behind.
//!
//! # Breakpoints
//!
//! | class     | reference    | rail | side panel | chrome                        |
//! |-----------|--------------|------|------------|-------------------------------|
//! | Compact   | 80x24        | hide | no         | minimal; rail folds to a key  |
//! | Normal    | 100x30       | show | no         | rail + chat                   |
//! | Wide      | 120x40       | show | yes        | rail + chat + context         |
//! | XWide     | 160x50       | show | yes        | chat + context + richer meta  |
//! | UltraWide | 180x50+      | show | yes        | chat capped at comfortable    |
//!
//! Widths are classified first, because a narrow window that is 60 rows tall
//! is still a narrow window: the rail must collapse regardless of height.
//!
//! # Guarantees
//!
//! * No region is ever zero-width when it is visible: [`ShellLayout::build`]
//!   clamps every allocation against the space actually available.
//! * The chat column never falls below `min_chat_width` while the context
//!   panel is visible — below that, the panel is dropped instead of crushing
//!   the conversation.
//! * Chat never stretches past `comfortable_chat_width` on a very wide
//!   terminal; the surplus goes to context, not to unreadable line lengths.
//! * Nothing is placed behind the footer: body height is the remainder after
//!   header and footer, never `frame.height()`.

use ratatui::layout::Rect;

use super::tokens::{saturating_sub, Tokens};

/// Breakpoints are keyed on width: a narrow window is narrow at any height.
const COMPACT_MAX: u16 = 89;
const NORMAL_MAX: u16 = 119;
const WIDE_MAX: u16 = 149;
const XWIDE_MAX: u16 = 179;

/// A named, tested size class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Breakpoint {
    /// ~80x24 floor: rail folds away, chrome reduces to what matters.
    Compact,
    /// ~100x30: rail + chat.
    Normal,
    /// ~120x40: rail + chat + context panel.
    Wide,
    /// ~160x50: richer context, more metadata.
    XWide,
    /// 180x50+: maximum context; chat held at comfortable width.
    UltraWide,
}

impl Breakpoint {
    /// Classify by width. Height never widens the layout — a 200-column,
    /// 10-row terminal is `Compact`, because vertical space is the scarce
    /// resource there.
    pub fn for_area(area: Rect) -> Self {
        Self::for_width(area.width)
    }

    pub const fn for_width(width: u16) -> Self {
        if width <= COMPACT_MAX {
            Self::Compact
        } else if width <= NORMAL_MAX {
            Self::Normal
        } else if width <= WIDE_MAX {
            Self::Wide
        } else if width <= XWIDE_MAX {
            Self::XWide
        } else {
            Self::UltraWide
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Normal => "normal",
            Self::Wide => "wide",
            Self::XWide => "xwide",
            Self::UltraWide => "ultrawide",
        }
    }

    /// Whether the navigation rail is a visible region (not a key-driven
    /// overlay).
    /// Whether the navigation rail may be shown at all.
    ///
    /// This is deliberately **not** a breakpoint question. Compact terminals
    /// still need navigation to be *discoverable* — a rail that simply vanishes
    /// below 90 columns leaves the user with no way to know the other 14
    /// destinations exist. Compact mode collapses the rail's *contents*
    /// (headings first, then inactive rows) instead of removing it, and the
    /// rail keeps its fixed width so the transcript never reflows.
    ///
    /// The rail is dropped only when there is genuinely no room for it plus a
    /// readable chat column; see [`ShellLayout::build`].
    pub const fn shows_rail(self) -> bool {
        true
    }

    /// Whether a contextual side panel may appear next to the chat.
    pub const fn allows_context(self) -> bool {
        !matches!(self, Self::Compact | Self::Normal)
    }

    /// How much metadata may be shown inline before it becomes noise.
    pub const fn metadata_budget(self) -> MetadataBudget {
        match self {
            Self::Compact => MetadataBudget::Minimal,
            Self::Normal => MetadataBudget::Basic,
            Self::Wide => MetadataBudget::Rich,
            Self::XWide | Self::UltraWide => MetadataBudget::Full,
        }
    }

    /// Whether the header may show optional pills (git branch, provider,
    /// session id) in addition to the always-on state.
    pub const fn allows_header_pills(self) -> bool {
        !matches!(self, Self::Compact)
    }

    pub const fn as_str(self) -> &'static str {
        self.label()
    }
}

/// How much secondary detail a view may afford at this size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetadataBudget {
    /// Identity and runtime state only.
    Minimal,
    /// Identity, state, and one context field.
    Basic,
    /// Identity, state, session, workspace, resources.
    Rich,
    /// Everything, including per-item metadata columns.
    Full,
}

/// Which regions are visible in the current layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShellChrome {
    pub header: bool,
    pub rail: bool,
    pub status: bool,
    pub context: bool,
}

/// How a row of equally-weighted panels is arranged at a given width.
///
/// Four fixed `Percentage(25)` columns are the obvious way to lay out a
/// dashboard and the wrong one: at 80 columns every panel is 20 wide, which
/// cannot hold a title plus a value, and no amount of shrinking helps because
/// the constraint never asks for fewer columns. Panels are therefore *reflowed*
/// — the count drops as the width does — and only then divided evenly.
/// The narrowest a panel may get before the grid must drop a column.
///
/// Derived from the widest title Workbench shows plus its border, so the floor
/// is a number someone can check rather than a taste judgement.
pub const PANEL_MIN_WIDTH: u16 = 22;

/// Arrange `count` panels in `area`, reflowing by width.
///
/// Returns one rect per panel in reading order (left to right, top to bottom),
/// always exactly `count` long, so a caller can index it positionally.
pub fn panel_grid(area: Rect, count: usize, gap: u16) -> Vec<Rect> {
    if count == 0 {
        return Vec::new();
    }
    // `gap` is spacing *between* panels; n panels need n-1 gaps.
    let gap_total = gap.saturating_mul(u16::try_from(count - 1).unwrap_or(u16::MAX));
    let usable = saturating_sub(area.width, gap_total);

    // Columns = how many panels of at least PANEL_MIN_WIDTH fit, never fewer
    // than one (a single column of whatever width remains beats nothing).
    let fit = usize::from((usable / PANEL_MIN_WIDTH).max(1))
        .min(count)
        .max(1);
    // Balance the last row. Purely width-driven column counts orphan panels:
    // four panels in three columns renders 3 + 1, so one panel sits under a
    // wide empty band while the row above is full. Preferring a column count
    // that divides the panel count keeps every row the same depth, and it never
    // *adds* a column — the balanced choice is always at or below the width
    // limit, so this can only spend space the panels were not using anyway.
    let columns = (1..=fit).rev().find(|c| count % c == 0).unwrap_or(fit);

    let rows = count.div_ceil(columns);
    let gap_v = gap.min(
        saturating_sub(area.height, u16::try_from(rows).unwrap_or(u16::MAX) as u16)
            / u16::try_from(rows.max(1)).unwrap_or(1),
    );
    let cell_w = usable / u16::try_from(columns).unwrap_or(u16::MAX);
    let row_h = saturating_sub(
        area.height,
        gap_v.saturating_mul(u16::try_from(rows.saturating_sub(1)).unwrap_or(u16::MAX)),
    ) / u16::try_from(rows.max(1)).unwrap_or(1);

    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let col = i % columns;
        let row = i / columns;
        // The last column is measured from its own origin to the right edge,
        // not by adding a remainder. Computing a remainder instead leaves a
        // dead strip whenever the division does not come out even, which is
        // most widths.
        let w = if col + 1 == columns {
            saturating_sub(
                area.x.saturating_add(area.width),
                area.x.saturating_add(
                    u16::try_from(col)
                        .unwrap_or(u16::MAX)
                        .saturating_mul(cell_w.saturating_add(gap)),
                ),
            )
        } else {
            cell_w
        };
        out.push(Rect {
            x: area.x.saturating_add(
                u16::try_from(col)
                    .unwrap_or(u16::MAX)
                    .saturating_mul(cell_w.saturating_add(gap)),
            ),
            y: area.y.saturating_add(
                u16::try_from(row)
                    .unwrap_or(u16::MAX)
                    .saturating_mul(row_h.saturating_add(gap_v)),
            ),
            width: w,
            height: row_h,
        });
    }
    out
}

/// The measured layout for one frame. Every field is a real `Rect`, so a
/// renderer cannot allocate something the shell did not reserve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShellLayout {
    pub area: Rect,
    pub breakpoint: Breakpoint,
    pub chrome: ShellChrome,
    pub header: Rect,
    pub status: Rect,
    pub rail: Rect,
    /// Main content: chat, or the active list view.
    pub body: Rect,
    /// Contextual side panel beside the chat. Zero-size when absent.
    pub context: Rect,
}

impl ShellLayout {
    /// Measure. `context` requests the side panel; the layout grants it only
    /// if the width budget allows, so a caller can never accidentally produce
    /// a zero-width panel.
    ///
    /// `has_rail` says whether the caller *has* rail content to draw. The rail
    /// is not merely a breakpoint question: a surface whose panels are its own
    /// navigation (Workbench) passes `false` and must not be charged 16 columns
    /// for an empty gutter. The layout owns the rail's *width* when there is a
    /// rail; it does not own the decision to have one.
    pub fn build(area: Rect, tokens: &Tokens, context: bool, has_rail: bool) -> Self {
        let m = &tokens.layout;
        let bp = Breakpoint::for_area(area);

        // Vertical first: the body is what remains, so content can never be
        // hidden behind the footer.
        //
        // Precedence when space is scarce: body > header > status. The status
        // bar needs 4 rows before it is worth keeping, and the header may never
        // consume more than half of what is left — an 80x4 window must show
        // chat, not a three-row masthead.
        let status_h = if area.height >= 4 { m.status_height } else { 0 };
        let after_status = saturating_sub(area.height, status_h);
        let header_h = m
            .header_height
            .min(after_status)
            .min(saturating_sub(after_status, 1))
            .max(if after_status > 0 { 1 } else { 0 });
        let header = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: header_h,
        };
        let after_header = saturating_sub(after_status, header_h);
        let status = Rect {
            x: area.x,
            y: area.y + saturating_sub(area.height, status_h),
            width: area.width,
            height: status_h,
        };
        // `after_header` already excludes the status bar (it was removed from
        // `after_status`), so the body is simply what is left.
        let body_h = after_header;
        let body_full = Rect {
            x: area.x,
            y: area.y + header_h,
            width: area.width,
            height: body_h,
        };

        // Horizontal: rail, then body/context.
        let mut chrome = ShellChrome {
            header: header_h > 0,
            rail: false,
            status: status_h > 0,
            context: false,
        };
        // The rail is kept whenever a readable chat column still fits beside
        // it. Dropping navigation to gain 16 columns is the wrong trade: the
        // transcript is scrollable, the destination list is not discoverable.
        let rail_fits = has_rail
            && bp.shows_rail()
            && area.width >= m.rail_width.saturating_add(m.min_chat_width);
        let rail_w = if rail_fits {
            m.rail_width.min(area.width)
        } else {
            0
        };
        chrome.rail = rail_w > 0;
        let rail = Rect {
            x: area.x,
            y: body_full.y,
            width: rail_w,
            height: body_full.height,
        };
        let after_rail_w = saturating_sub(body_full.width, rail_w);

        // The context panel only appears when the chat still fits at its
        // minimum. Crushing prose into a ribbon to keep a secondary panel is
        // the wrong trade in a chat-first tool.
        let (body_w, context_w) = if context && bp.allows_context() && after_rail_w > 0 {
            let gutter = m.gutter.min(after_rail_w);
            let available = saturating_sub(after_rail_w, gutter);
            let ctx = m.context_width.min(available);
            let chat = saturating_sub(available, ctx);
            if chat >= m.min_chat_width {
                (chat, ctx)
            } else {
                (available, 0)
            }
        } else {
            (after_rail_w, 0)
        };
        chrome.context = context_w > 0;

        let body = Rect {
            x: body_full.x + rail_w,
            y: body_full.y,
            width: body_w,
            height: body_full.height,
        };
        // An absent panel is anchored at the body's right edge, not *past* it.
        // The gutter only exists between two columns, so when no context column
        // was granted, `body.x + body_w + gutter` points outside the window: at
        // 80x24 the compact layout produced `x: 82` in an 80-wide frame. The
        // rect is zero-area and never painted, but `context` is public, and any
        // caller measuring `x + width` on it would read past the edge.
        let context = Rect {
            x: body.x.saturating_add(body_w) + if context_w > 0 { m.gutter } else { 0 },
            y: body_full.y,
            width: context_w,
            height: body_full.height,
        };

        Self {
            area,
            breakpoint: bp,
            chrome,
            header,
            status,
            rail,
            body,
            context,
        }
    }

    /// Chat width actually granted, honoring the comfortable cap: on a very
    /// wide terminal the surplus belongs to context, never to line length.
    pub fn chat_width(&self, tokens: &Tokens) -> u16 {
        if !self.chrome.context {
            return self.body.width;
        }
        tokens
            .layout
            .comfortable_chat_width
            .max(tokens.layout.min_chat_width)
            .min(self.body.width)
    }

    /// Whether a modal of `width`x`height` should be shown at all: a dialog
    /// larger than the viewport is unreachable, so callers must degrade to a
    /// full-bleed surface rather than draw something unusable.
    pub fn modal_fits(&self, tokens: &Tokens, width: u16, height: u16) -> bool {
        let fits = |a: Rect, w: u16, h: u16| a.width >= w && a.height >= h;
        let m = &tokens.layout;
        let normal = |w: u16| w.min(m.max_modal_width).max(0);
        fits(self.area, normal(width), normal(height))
    }

    /// Centered rect for a floating surface, clamped to the viewport.
    ///
    /// The clamp order matters: shrink to the available space *first*, then
    /// center. Centering first and clamping after can push a dialog's title
    /// and buttons off-screen — the "impossible-to-reach dialog" failure.
    pub fn modal_rect(&self, tokens: &Tokens, want_w: u16, want_h: u16) -> Rect {
        let m = &tokens.layout;
        let w = want_w.min(m.max_modal_width).min(self.area.width);
        let h = want_h
            .min(m.min_modal_height.max(want_h))
            .min(self.area.height);
        let w = w.max(0);
        let h = h.max(0);
        let x = self.area.x + saturating_sub(self.area.width, w) / 2;
        let y = self.area.y + saturating_sub(self.area.height, h) / 2;
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }
}

// ----------------------------------------------------------------------
// Mouse hit-testing
// ----------------------------------------------------------------------

/// What a click landed on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HitTarget {
    /// Nothing interactive at that point — clicks must not fire a default.
    None,
    /// A navigation entry in the rail.
    NavItem { index: usize },
    /// A chat key hint chip.
    ChatHint { key: char },
    /// A row in the active list view.
    ListRow { index: usize },
    /// A destination row in the jump-to palette overlay.
    ///
    /// Distinct from `ListRow` on purpose: the index means different things in
    /// the two places (a collection row versus a palette match), and collapsing
    /// them would make the meaning of a click depend on which overlay happened
    /// to be open.
    PaletteItem { index: usize },
    /// A provider/model row in the model picker overlay.
    ModelItem { index: usize },
    /// A button in a card or dialog.
    Button { id: &'static str },
    /// The header (reserved: clicking the header clears focus rings).
    Header,
    /// A click inside a modal that hit no control (swallow, never fall
    /// through to the view behind it).
    ModalScrim,
    /// A row in one of the Workbench panels.
    ///
    /// Carries a [`PanelId`] rather than a bare index so a Workbench coordinate
    /// can never be read as an Agent list row. The two modes own different
    /// surfaces and different meanings for "row 3"; collapsing them into
    /// `ListRow` is what would let a Workbench click resolve to an Agent action.
    WorkbenchRow { panel: PanelId, index: usize },
    /// A Workbench panel frame — focuses the panel without selecting a row.
    WorkbenchPanel { panel: PanelId },
}

/// Which Workbench panel a region belongs to.
///
/// Lives in the design layer so [`HitTarget`] can name it without depending on
/// the app state module that also has a `Focus` enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PanelId {
    Workspaces,
    Sessions,
    Tasks,
    Ops,
}

impl PanelId {
    /// Every panel, in the order the grid lays them out.
    pub const ALL: [PanelId; 4] = [
        PanelId::Workspaces,
        PanelId::Sessions,
        PanelId::Tasks,
        PanelId::Ops,
    ];

    /// The next panel in the focus ring.
    pub const fn next(self) -> Self {
        match self {
            Self::Workspaces => Self::Sessions,
            Self::Sessions => Self::Tasks,
            Self::Tasks => Self::Ops,
            Self::Ops => Self::Workspaces,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Workspaces => "Workspaces",
            Self::Sessions => "Sessions",
            Self::Tasks => "Tasks",
            Self::Ops => "Ops",
        }
    }
}

impl HitTarget {
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

/// Every registered hit region for the current frame.
///
/// Registered during render, consumed by the next input event. A region is
/// only honoured when it lies inside the owning element's `Rect`, so a stale
/// region from a smaller viewport can never trigger an invisible control.
#[derive(Clone, Debug, Default)]
pub struct HitTargets {
    regions: Vec<(Rect, HitTarget)>,
}

impl HitTargets {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a region. A zero-area rect is ignored: a control that was
    /// clipped away has no hit target, which is the correct behaviour.
    pub fn register(&mut self, area: Rect, target: HitTarget) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.regions.push((area, target));
    }

    /// Register a single-row region, clamped to the bounds of `owner` so a
    /// partially visible list row can never be clicked in empty space.
    pub fn register_row(&mut self, owner: Rect, row: usize, target: HitTarget) {
        if row >= owner.height as usize {
            return;
        }
        let y = owner.y + row as u16;
        self.register(
            Rect {
                x: owner.x,
                y,
                width: owner.width,
                height: 1,
            },
            target,
        );
    }

    /// Topmost (last registered) region containing the point. Later
    /// registrations are painted later and therefore sit on top.
    /// Every registered target, newest first.
    ///
    /// Exposed so a frame can be *audited* rather than probed: "did this panel
    /// register a region" is a question about the whole frame, and answering it
    /// by clicking one point per panel misses a panel whose region is somewhere
    /// the test did not think to click.
    pub fn all(&self) -> Vec<HitTarget> {
        self.regions.iter().rev().map(|(_, t)| t.clone()).collect()
    }

    pub fn hit(&self, x: u16, y: u16) -> HitTarget {
        for (area, target) in self.regions.iter().rev() {
            if x >= area.x
                && x < area.x.saturating_add(area.width)
                && y >= area.y
                && y < area.y.saturating_add(area.height)
            {
                return target.clone();
            }
        }
        HitTarget::None
    }

    /// Like [`Self::hit`], but `None` for "nothing interactive here" instead of
    /// [`HitTarget::None`]. Callers routing input want the absence, not a
    /// sentinel they must remember to check.
    pub fn at(&self, x: u16, y: u16) -> Option<HitTarget> {
        match self.hit(x, y) {
            HitTarget::None => None,
            t => Some(t),
        }
    }

    /// Drop every region. Called at the start of a frame so the registry
    /// describes only what is on screen now — a stale region from a wider
    /// viewport must never activate a control that is no longer drawn.
    pub fn clear(&mut self) {
        self.regions.clear();
    }

    /// Every registered `(rect, target)` pair, in paint order.
    ///
    /// Exposed so a test — or a tooltip anchor — can find a control by identity
    /// and then use its real coordinates, instead of hard-coding a position that
    /// silently rots the first time the layout changes.
    pub fn iter_regions(&self) -> impl Iterator<Item = (&Rect, &HitTarget)> {
        self.regions.iter().map(|(a, t)| (a, t))
    }

    /// The rect of a registered control. Lets a test — or a tooltip anchor —
    /// address a specific control instead of guessing coordinates.
    pub fn region_of(&self, id: &str) -> Option<Rect> {
        self.regions
            .iter()
            .find(|(_, target)| matches!(target, HitTarget::Button { id: i } if *i == id))
            .map(|(area, _)| *area)
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    pub fn len(&self) -> usize {
        self.regions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    fn tokens() -> Tokens {
        Theme::default_theme().tokens()
    }

    fn r(width: u16, height: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    #[test]
    fn breakpoints_classify_by_width_alone() {
        assert_eq!(Breakpoint::for_area(r(80, 24)), Breakpoint::Compact);
        assert_eq!(Breakpoint::for_area(r(89, 60)), Breakpoint::Compact);
        assert_eq!(Breakpoint::for_area(r(90, 24)), Breakpoint::Normal);
        assert_eq!(Breakpoint::for_area(r(100, 30)), Breakpoint::Normal);
        assert_eq!(Breakpoint::for_area(r(120, 40)), Breakpoint::Wide);
        assert_eq!(Breakpoint::for_area(r(149, 40)), Breakpoint::Wide);
        assert_eq!(Breakpoint::for_area(r(150, 40)), Breakpoint::XWide);
        assert_eq!(Breakpoint::for_area(r(160, 50)), Breakpoint::XWide);
        assert_eq!(Breakpoint::for_area(r(179, 50)), Breakpoint::XWide);
        assert_eq!(Breakpoint::for_area(r(180, 50)), Breakpoint::UltraWide);
        assert_eq!(Breakpoint::for_area(r(400, 100)), Breakpoint::UltraWide);
    }

    #[test]
    fn every_reference_size_is_classified_and_laid_out() {
        let t = tokens();
        for (w, h) in [(80, 24), (100, 30), (120, 40), (160, 50), (180, 50)] {
            let l = ShellLayout::build(r(w, h), &t, true, true);
            assert_eq!(l.area.width, w);
            assert_eq!(l.area.height, h);
            assert!(l.header.height > 0, "{w}x{h}: no header");
            assert!(l.body.height > 0, "{w}x{h}: no body");
            // The body must never overlap the footer.
            assert!(
                l.body.y + l.body.height <= l.status.y || l.status.height == 0,
                "{w}x{h}: body overruns the status bar"
            );
            // The header must not overlap the body.
            assert!(
                l.header.y + l.header.height <= l.body.y,
                "{w}x{h}: header overlaps the body"
            );
        }
    }

    #[test]
    fn regions_never_overlap_or_overflow() {
        let t = tokens();
        for w in [20u16, 40, 80, 100, 120, 160, 180, 300] {
            for h in [3u16, 5, 24, 30, 50, 80] {
                let l = ShellLayout::build(r(w, h), &t, true, true);
                for (name, region) in [
                    ("header", l.header),
                    ("rail", l.rail),
                    ("body", l.body),
                    ("context", l.context),
                    ("status", l.status),
                ] {
                    if region.width == 0 || region.height == 0 {
                        continue;
                    }
                    assert!(
                        region.x + region.width <= w,
                        "{w}x{h}: {name} overflows horizontally"
                    );
                    assert!(
                        region.y + region.height <= h,
                        "{w}x{h}: {name} overflows vertically"
                    );
                }
                if l.rail.width > 0 {
                    assert!(
                        l.rail.x + l.rail.width <= l.body.x,
                        "{w}x{h}: rail overlaps the body"
                    );
                }
                if l.context.width > 0 && l.body.width > 0 {
                    assert!(
                        l.body.x + l.body.width <= l.context.x + 1,
                        "{w}x{h}: body overlaps the context panel"
                    );
                }
            }
        }
    }

    /// Navigation must be *discoverable* at every size. The rail collapses its
    /// contents under pressure; it is never removed just because the terminal
    /// is narrow, or the user loses any way to know the other destinations
    /// exist.
    #[test]
    fn the_rail_survives_compact_and_is_dropped_only_when_unusable() {
        let t = tokens();
        for (w, h) in [(80u16, 24u16), (100, 30), (120, 40), (160, 50), (180, 50)] {
            let l = ShellLayout::build(r(w, h), &t, false, true);
            assert!(l.chrome.rail, "{w}x{h}: rail must remain discoverable");
            assert!(l.rail.width > 0);
            // And the chat column beside it must still be readable.
            assert!(
                l.body.width >= t.layout.min_chat_width,
                "{w}x{h}: body collapsed to {}",
                l.body.width
            );
        }
        // Too narrow for a rail *and* a readable chat column: navigation is
        // dropped rather than squeezing prose into a ribbon.
        for w in [1u16, 20, 40, 55] {
            assert!(
                !ShellLayout::build(r(w, 24), &t, false, true).chrome.rail,
                "{w}: rail should yield to content"
            );
        }
    }

    #[test]
    fn the_context_panel_never_squeezes_the_chat_past_its_minimum() {
        let t = tokens();
        // Ask for context at every width; the panel is dropped, not the chat.
        for w in 20u16..=300 {
            let l = ShellLayout::build(r(w, 40), &t, true, true);
            if l.chrome.context {
                assert!(
                    l.body.width >= t.layout.min_chat_width,
                    "{w}: context shown but chat is {0} < min {1}",
                    l.body.width,
                    t.layout.min_chat_width
                );
            }
        }
    }

    #[test]
    fn chat_is_capped_at_comfortable_width_on_a_very_wide_terminal() {
        let t = tokens();
        let l = ShellLayout::build(r(300, 50), &t, true, true);
        assert!(l.chrome.context);
        assert!(
            l.chat_width(&t) <= t.layout.comfortable_chat_width,
            "chat stretched to {} on a 300-column terminal",
            l.chat_width(&t)
        );
    }

    #[test]
    fn a_one_row_terminal_yields_the_status_bar_to_the_header() {
        let t = tokens();
        let l = ShellLayout::build(r(80, 1), &t, false, true);
        assert!(l.chrome.header);
        assert!(!l.chrome.status);
        assert_eq!(l.body.height, 0);
    }

    #[test]
    fn a_very_short_terminal_keeps_the_body_and_sacrifices_chrome() {
        let t = tokens();
        // Precedence at 4 rows: body first, then the header, then the status.
        let l = ShellLayout::build(r(80, 4), &t, false, true);
        assert!(l.chrome.status);
        assert!(l.body.height > 0, "body collapsed at 4 rows");
        assert!(l.header.height < t.layout.header_height);
        assert_eq!(
            l.header.height + l.body.height + l.status.height,
            4,
            "rows went missing"
        );
        // Below 4 rows the status bar is the first thing to go.
        let l = ShellLayout::build(r(80, 3), &t, false, true);
        assert!(!l.chrome.status);
        assert!(l.body.height > 0);
    }

    #[test]
    fn degenerate_viewports_never_panic_or_underflow() {
        let t = tokens();
        for w in 0u16..40 {
            for h in 0u16..10 {
                let l = ShellLayout::build(r(w, h), &t, true, true);
                // Must not panic, and no visible region may claim space the
                // viewport does not have.
                assert!(l.body.width <= w && l.body.height <= h);
                let m = l.modal_rect(&t, 100, 40);
                assert!(
                    m.width <= w.max(m.width.min(w)),
                    "modal wider than viewport"
                );
                assert!(m.width <= w);
                assert!(m.height <= h);
                assert!(m.x + m.width <= w);
                assert!(m.y + m.height <= h);
            }
        }
    }

    #[test]
    fn modals_are_centered_then_clamped_within_the_viewport() {
        let t = tokens();
        let l = ShellLayout::build(r(120, 40), &t, true, true);
        let m = l.modal_rect(&t, 80, 20);
        assert_eq!(m.width, 80);
        assert_eq!(m.height, 20);
        // Centered.
        assert_eq!(m.x, (120 - 80) / 2);
        assert_eq!(m.y, (40 - 20) / 2);

        // Too big for the viewport: clamped, not centered off-screen.
        let tiny = ShellLayout::build(r(40, 12), &t, true, true);
        let m = tiny.modal_rect(&t, 80, 20);
        assert_eq!(m.width, 40);
        assert_eq!(m.height, 12);
        assert_eq!(m.x, 0);
        assert_eq!(m.y, 0);
    }

    #[test]
    fn modal_fits_refuses_oversized_dialogs() {
        let t = tokens();
        let l = ShellLayout::build(r(80, 24), &t, true, true);
        assert!(l.modal_fits(&t, 60, 16));
        assert!(!l.modal_fits(&t, 200, 60));
    }

    // -- hit testing ----------------------------------------------------

    #[test]
    fn hit_testing_resolves_the_topmost_region() {
        let mut h = HitTargets::new();
        h.register(r(10, 2), HitTarget::Button { id: "under" });
        h.register(r(2, 2), HitTarget::Button { id: "over" });
        assert_eq!(h.hit(1, 0), HitTarget::Button { id: "over" });
        assert_eq!(h.hit(5, 0), HitTarget::Button { id: "under" });
        assert_eq!(h.hit(50, 50), HitTarget::None);
    }

    #[test]
    fn a_zero_area_region_is_never_clickable() {
        let mut h = HitTargets::new();
        h.register(r(0, 4), HitTarget::Button { id: "invisible" });
        h.register(r(4, 0), HitTarget::Button { id: "invisible2" });
        assert!(h.is_empty(), "clipped controls must not stay clickable");
        assert_eq!(h.hit(0, 0), HitTarget::None);
    }

    #[test]
    fn rows_outside_the_owner_are_not_registered() {
        let mut h = HitTargets::new();
        let owner = r(10, 3);
        h.register_row(owner, 2, HitTarget::ListRow { index: 2 });
        h.register_row(owner, 3, HitTarget::ListRow { index: 3 });
        assert_eq!(h.len(), 1, "row past the bottom must not register");
        assert_eq!(h.hit(0, 2), HitTarget::ListRow { index: 2 });
        assert_eq!(h.hit(0, 3), HitTarget::None);
    }
}

/// Phase 1 gate: the shell's geometry may be computed in exactly one place.
///
/// The Agent and Workbench renderers used to hard-code their own header height
/// (3), status height (1), rail width (16), and a `width >= 110` threshold for
/// the side panel. Those duplicated `ShellLayout`'s arithmetic, which is how the
/// two renderers drifted — and how a dialog sized with a percent-based helper
/// rendered 6 rows tall regardless of its content.
///
/// This is a source-level guard because the property is architectural: a render
/// test can observe that the layout is *correct*, never that there is only one
/// place computing it.
#[test]
fn shell_geometry_is_computed_only_by_shell_layout() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    // `Constraint::Min/Len` is fine inside a panel — that is content layout,
    // not shell geometry. What must not exist is a *shell* dimension literal.
    let shell_numbers = ["110", "89", "119", "149", "179"];
    let mut offenders: Vec<String> = Vec::new();

    for rel in ["agent/render.rs", "ui.rs"] {
        let path = root.join(rel);
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        for (n, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            if code.contains("ShellLayout::build") {
                continue;
            }
            for num in shell_numbers {
                // A width *comparison* against a magic breakpoint is the exact
                // pattern this gate exists to remove.
                if code.contains(&format!("width >= {num}"))
                    || code.contains(&format!("width > {num}"))
                    || code.contains(&format!("width <= {num}"))
                    || code.contains(&format!("width < {num}"))
                {
                    offenders.push(format!("{rel}:{}: width compared to {num}", n + 1));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "shell geometry must come from ShellLayout/Tokens, not renderer literals:\n{}",
        offenders.join("\n")
    );
}

#[cfg(test)]
mod panel_grid_tests {
    use super::*;

    fn r(w: u16, h: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        }
    }

    /// Every viewport the acceptance matrix names.
    const SIZES: &[(u16, u16)] = &[
        (80, 24),
        (100, 30),
        (120, 40),
        (160, 50),
        (180, 50),
        // Degenerate cases.
        (1, 1),
        (0, 24),
        (80, 0),
        (21, 24),
        (23, 24),
    ];

    #[test]
    fn panel_grid_always_returns_one_rect_per_panel() {
        for (w, h) in SIZES {
            let out = panel_grid(r(*w, *h), 4, 1);
            assert_eq!(out.len(), 4, "{w}x{h}: one rect per panel");
        }
    }

    #[test]
    fn panel_grid_never_returns_a_rect_outside_its_area() {
        for (w, h) in SIZES {
            let area = r(*w, *h);
            for (i, p) in panel_grid(area, 4, 1).iter().enumerate() {
                assert!(
                    p.x >= area.x && p.y >= area.y,
                    "{w}x{h} panel {i} starts before the area: {p:?}"
                );
                assert!(
                    p.x.saturating_add(p.width) <= area.x.saturating_add(area.width),
                    "{w}x{h} panel {i} overflows horizontally: {p:?}"
                );
                assert!(
                    p.y.saturating_add(p.height) <= area.y.saturating_add(area.height),
                    "{w}x{h} panel {i} overflows vertically: {p:?}"
                );
            }
        }
    }

    /// A four-panel dashboard must not render 3 + 1 just because three columns
    /// of the minimum width happen to fit. The orphan row leaves a wide empty
    /// band beside the last panel, which reads as a layout bug.
    #[test]
    fn a_four_panel_grid_balances_its_rows_whenever_a_balanced_choice_fits() {
        for (w, h) in [(80u16, 24u16), (100, 30), (120, 40), (160, 50), (180, 50)] {
            let g = panel_grid(r(w, h), 4, 1);
            let columns = {
                // Recover the column count from the geometry: panels sharing a
                // top row share an x.
                let mut xs: Vec<u16> = g.iter().map(|p| p.x).collect();
                xs.dedup();
                xs.len()
            };
            assert!(
                4 % columns == 0,
                "{w}x{h}: {columns} columns splits 4 panels into an orphan row"
            );
        }
    }

    /// Balancing must never add a column the width could not afford.
    #[test]
    fn balancing_never_exceeds_the_width_budget() {
        for w in 1u16..=200 {
            let g = panel_grid(r(w, 20), 4, 1);
            for p in &g {
                assert!(
                    p.width >= PANEL_MIN_WIDTH || w < PANEL_MIN_WIDTH,
                    "{w}: panel of width {} is below the floor",
                    p.width
                );
            }
        }
    }

    #[test]
    fn panel_grid_reflows_to_fewer_columns_as_width_shrinks() {
        // The point of the grid: at 80 columns the dashboard must stop trying
        // to fit four panels abreast.
        let wide = panel_grid(r(180, 50), 4, 1);
        let narrow = panel_grid(r(80, 24), 4, 1);
        // Four abreast needs 4 * PANEL_MIN_WIDTH + 3 gaps = 91 columns.
        assert_eq!(
            wide[1].y, wide[0].y,
            "at 180x50 four panels fit abreast, so all four share a row"
        );
        assert!(
            narrow[3].y > narrow[0].y,
            "at 80x24 four panels cannot fit abreast and must wrap, got {narrow:?}"
        );
        // Wrapping must not just mean "still four abreast but taller".
        assert!(
            narrow.iter().any(|p| p.x > narrow[0].x),
            "at 80x24 the panels should still share the first row, got {narrow:?}"
        );
    }

    #[test]
    fn panel_grid_keeps_every_panel_at_least_usable_when_it_can() {
        // Whenever the width genuinely affords n panels, each must clear the
        // floor — otherwise a panel silently renders as an unusable sliver.
        for (w, h) in SIZES {
            let area = r(*w, *h);
            let out = panel_grid(area, 4, 1);
            let abreast = out[1].y == out[0].y;
            if abreast && area.width >= PANEL_MIN_WIDTH * 4 + 3 {
                for p in &out {
                    assert!(
                        p.width >= PANEL_MIN_WIDTH,
                        "{w}x{h}: panel {p:?} is narrower than {PANEL_MIN_WIDTH} while \\
                         four panels are being shown abreast"
                    );
                }
            }
        }
    }

    #[test]
    fn panel_grid_covers_the_width_it_is_given() {
        // The last column absorbs the remainder, so no dead strip is left over.
        // Only meaningful when the panels form a single row.
        for (w, h) in SIZES {
            let area = r(*w, *h);
            let out = panel_grid(area, 4, 1);
            if out[1].y != out[0].y {
                continue; // wrapped; row coverage is not width coverage
            }
            let rightmost = out
                .iter()
                .map(|p| p.x.saturating_add(p.width))
                .max()
                .expect("at least one panel");
            assert_eq!(
                rightmost, area.width,
                "{w}x{h}: the row should reach the right edge, not stop short"
            );
        }
    }

    #[test]
    fn panel_grid_handles_zero_and_one_panels() {
        assert!(panel_grid(r(80, 24), 0, 1).is_empty());
        let one = panel_grid(r(80, 24), 1, 1);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].width, 80);
    }

    #[test]
    fn panel_grid_never_panics_on_a_zero_area() {
        for (w, h) in SIZES {
            let _ = panel_grid(r(*w, *h), 4, 1);
            let _ = panel_grid(r(*w, *h), 4, 0);
            let _ = panel_grid(r(*w, *h), 1, 5);
        }
    }
}
