//! List, table, and tree primitives.
//!
//! Every non-chat view in the app is one of these three shapes wearing a
//! different hat. Keeping them here means the decisions that are easy to get
//! subtly wrong happen once:
//!
//! * **Scrolling.** A view with 10 000 rows must not index a viewport. The
//!   cursor is an index; the window is derived, clamped, and always includes
//!   the cursor.
//! * **Truncation.** Long cells are ellipsized from the *left* for paths and
//!   model ids — the filename is the informative end — and from the right for
//!   prose.
//! * **Displacement.** When a list is longer than its window, the scrollbar is
//!   drawn in the row it actually maps to, so a thumb position is a truthful
//!   position and not decoration.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::tokens::{saturating_sub, Tokens};
use super::widgets::{ellipsize, sanitize_field};

/// A scrollable window over `total` rows showing `height` of them with
/// `cursor` selected.
///
/// Returns `(start, visible_count)`. Never panics on a zero-height area, an
/// empty list, or a cursor past the end — a stale cursor from a shrinking
/// collection clamps to the last row rather than disappearing.
pub fn window(total: usize, cursor: usize, height: u16) -> (usize, usize) {
    let h = usize::from(height);
    if h == 0 || total == 0 {
        return (0, 0);
    }
    if total <= h {
        return (0, total);
    }
    let cursor = cursor.min(total - 1);
    // A full page is the largest start that still fills the window.
    let max_start = total - h;
    // Keep the cursor in the middle third once it would otherwise fall off the
    // bottom, which is what makes wheel-scrolling feel like it has inertia.
    let start = cursor
        .saturating_sub(h / 3)
        .min(cursor.saturating_sub(0))
        .min(max_start);
    (start, h)
}

/// The visible row window for a list scrolled by *offset* rather than by cursor.
///
/// Some surfaces scroll by an explicit offset — a terminal's bottom-anchored
/// scrollback, an agent list with no selection — so there is no cursor to keep
/// visible and [`window`] does not apply. The clamping rule still has to live
/// here, because the alternative is one copy per call site, and two copies of a
/// scroll rule is how a "scrolled to the end" view ends up blank.
///
/// `offset` counts from the *start* of the list. The terminal used the opposite
/// convention (rows from the end), and that inversion was the source of the
/// blank-tail bug this replaces.
pub fn window_at(total: usize, offset: usize, height: u16) -> (usize, usize) {
    let h = usize::from(height);
    if h == 0 || total == 0 {
        return (0, 0);
    }
    if total <= h {
        return (0, total);
    }
    (offset.min(total - h), h)
}

/// The visible row window for a list, *including* the rows the displacement
/// markers occupy.
///
/// [`draw_list_rows_targeted`] and any caller that wants to materialise only the
/// visible rows must agree on this exactly. If they disagreed by one row, the
/// caller would build rows the painter then discarded, or the painter would show
/// blanks where the caller thought it had painted content. Keeping it in one
/// function is what makes that class of bug impossible.
pub fn list_window(
    total: usize,
    cursor: Option<usize>,
    inner_height: u16,
    show_displacement: bool,
) -> (usize, usize) {
    let show = show_displacement && cursor.is_some();
    // The displacement markers are painted as *rows*, so they have to be
    // subtracted before the window is measured. Measuring first and appending
    // afterwards produced `height + 2` lines for a `height` row area, which
    // silently clipped the last visible item and the `↓N` marker — and the one
    // existing test only asserted `↑`, so it passed. Reserving a fixed two rows
    // whenever anything is hidden also keeps the layout from jittering as the
    // window reaches either end.
    let reserved = if show && total > usize::from(inner_height) {
        2
    } else {
        0
    };
    window(
        total,
        cursor.unwrap_or(0),
        inner_height.saturating_sub(reserved),
    )
}

/// A one-column selectable list.
#[derive(Clone, Debug, Default)]
pub struct ListView<'a> {
    pub title: &'a str,
    pub items: Vec<ListItem<'a>>,
    /// The selected row, or `None` for a read-only list that has no selection.
    ///
    /// Modelled as an `Option` rather than a sentinel index because a sentinel
    /// silently becomes a real position: a `usize::MAX` cursor is clamped to the
    /// last row by [`window`], which scrolls a read-only view to the bottom and
    /// claims rows are hidden above.
    pub cursor: Option<usize>,
    /// Show `↑N`/`↓N` instead of a scrollbar when rows are hidden.
    pub show_displacement: bool,
    /// Rows in the *unwindowed* collection, when `items` is only the visible
    /// slice.
    ///
    /// `None` means `items` is the whole collection. `Some(n)` lets a caller
    /// materialise just the rows that fit — the Phase 8 requirement — while
    /// `↑N`/`↓N` still report the true number of hidden rows. Without this,
    /// bounding the rows would silently turn "1,240 rows hidden" into "none",
    /// which is worse than slow: it lies about what the operator cannot see.
    pub total: Option<usize>,
    /// Index of `items[0]` within the unwindowed collection.
    ///
    /// Zero unless `total` is set. The cursor and the window are both in
    /// unwindowed coordinates, so this maps them back onto the slice.
    pub offset: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListItem<'a> {
    pub label: String,
    /// Optional right-aligned secondary text (a count, a timestamp, a state).
    pub detail: Option<String>,
    /// Overrides the row's tone — e.g. a failing workspace.
    pub tone: Option<RowTone>,
    /// Free-form payload for hit targets.
    pub id: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowTone {
    Normal,
    Success,
    Warning,
    Danger,
    Info,
    Muted,
}

impl RowTone {
    fn fg(self, t: &Tokens) -> ratatui::style::Color {
        match self {
            Self::Normal => t.text.secondary,
            Self::Success => t.status.success,
            Self::Warning => t.status.warning,
            Self::Danger => t.status.danger,
            Self::Info => t.status.info,
            Self::Muted => t.text.muted,
        }
    }
}

/// Where a table's cells come from.
///
/// A table that owns its cells has to build every one of them before the painter
/// can take a window, so the table costs O(rows) per frame however many rows fit
/// on screen. [`TableRows::Lazy`] moves the cell construction behind a factory
/// and declares the total instead, so the window is decided first and only the
/// rows inside it are ever built.
///
/// This is not a second windowing rule: the total feeds the same [`window`] that
/// the eager path used, and [`TableRows::window`] is the only place a slice is
/// taken.
pub enum TableRows<'a> {
    /// Every row is already built. Appropriate for tables whose size is bounded
    /// by something other than a collection — a fixed set of columns, a handful
    /// of policy rows.
    Cells(Vec<Vec<String>>),
    /// A row total plus a factory for one row.
    Lazy {
        total: usize,
        at: Box<dyn Fn(usize) -> Vec<String> + 'a>,
    },
}

impl Default for TableRows<'_> {
    fn default() -> Self {
        Self::Cells(Vec::new())
    }
}

impl std::fmt::Debug for TableRows<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cells(rows) => f.debug_tuple("Cells").field(rows).finish(),
            Self::Lazy { total, .. } => f.debug_struct("Lazy").field("total", total).finish(),
        }
    }
}

impl<'a> TableRows<'a> {
    /// A row source over a collection too large to materialise.
    pub fn lazy(total: usize, at: impl Fn(usize) -> Vec<String> + 'a) -> Self {
        Self::Lazy {
            total,
            at: Box::new(at),
        }
    }

    /// The number of rows the table has, whether or not they are built.
    pub fn len(&self) -> usize {
        match self {
            Self::Cells(rows) => rows.len(),
            Self::Lazy { total, .. } => *total,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The rows of a window, in order.
    ///
    /// The eager case borrows without copying; the lazy case builds each row on
    /// the way out. Either way the caller never sees a row outside the window,
    /// which is the whole point of the split.
    pub fn window(&self, start: usize, count: usize) -> impl Iterator<Item = Vec<String>> + '_ {
        let end = start.saturating_add(count);
        match self {
            Self::Cells(rows) => Box::new(rows.iter().skip(start).take(count).map(|r| r.clone()))
                as Box<dyn Iterator<Item = Vec<String>> + '_>,
            Self::Lazy { total, at } => Box::new((start..end.min(*total)).map(move |i| (at)(i))),
        }
    }
}

/// A window into a *filtered* collection.
///
/// A filter needs two things the unfiltered [`window`] cannot supply: how many
/// rows survive, and the source index of the *n*-th survivor. Both used to be
/// answered by collecting every matching index into a `Vec<usize>` on every
/// frame — an O(n) allocation per repaint for a list that shows about forty rows.
///
/// This counts matches in one scan without allocating, takes the window with the
/// same [`window`] rule as every other surface, and then yields only the source
/// indices inside it. Time is still O(n) per frame; **heap is O(window)**. That
/// distinction is the point, and it is stated rather than blurred: this is not O(1)
/// filtering, it is filtering that no longer allocates a vector the size of the
/// collection on every repaint.
pub struct FilteredWindow<'a> {
    len: usize,
    total: usize,
    cursor: usize,
    start: usize,
    count: usize,
    matches: &'a dyn Fn(usize) -> bool,
}

impl<'a> FilteredWindow<'a> {
    /// Count the survivors, place the cursor among them, and take the window.
    ///
    /// `cursor` is an index into the *filtered* sequence — the coordinate space the
    /// selection is kept in — so a filter that hides the selected row moves the
    /// selection with it instead of leaving it pointing at the wrong item.
    ///
    /// `show_displacement` must match what the painter will pass to
    /// [`list_window`], because that decides whether two rows are reserved for the
    /// `↑N`/`↓N` markers. A window computed one way and painted the other way is
    /// how a selected row ends up off screen while every line of the code looks
    /// right on its own.
    pub fn new(
        len: usize,
        matches: &'a dyn Fn(usize) -> bool,
        cursor: usize,
        height: u16,
        show_displacement: bool,
    ) -> Self {
        let mut total = 0usize;
        let mut cursor_pos = 0usize;
        for i in 0..len {
            if matches(i) {
                if total == cursor {
                    cursor_pos = total;
                }
                total += 1;
            }
        }
        // A cursor past the last survivor is clamped rather than left pointing at
        // nothing: an out-of-range window start reads as an empty list, which is
        // how a filter change could blank a populated view.
        if cursor >= total {
            cursor_pos = total.saturating_sub(1);
        }
        let (start, count) = list_window(total, Some(cursor_pos), height, show_displacement);
        Self {
            len,
            total,
            cursor: cursor_pos,
            start,
            count,
            matches,
        }
    }

    /// How many rows survive the filter.
    pub fn total(&self) -> usize {
        self.total
    }

    /// The cursor's position among the survivors.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Where the window starts, in filtered coordinates — the `offset` a painter
    /// needs alongside a filtered `total` and `cursor`.
    pub fn start(&self) -> usize {
        self.start
    }

    /// How many survivors are inside the window.
    pub fn count(&self) -> usize {
        self.count
    }

    /// Source indices of the survivors inside the window, in order.
    ///
    /// The scan runs to the end of the collection and skips to the window, because
    /// the `start`-th survivor may be anywhere in it. That is the same O(n) scan as
    /// the count; what it does *not* do is allocate a `usize` per survivor.
    pub fn indices(&self) -> impl Iterator<Item = usize> + '_ {
        let matches = self.matches;
        (0..self.len)
            .filter(move |i| matches(*i))
            .skip(self.start)
            .take(self.count)
    }
}

/// A multi-column table.
#[derive(Debug, Default)]
pub struct TableView<'a> {
    pub title: &'a str,
    pub columns: Vec<Column<'a>>,
    pub rows: TableRows<'a>,
    pub cursor: usize,
    /// Where `rows` begins in the table's own coordinate space.
    ///
    /// `rows` may already be a window — a filtered table passes only the surviving
    /// rows — while `cursor` and every `ListRow` hit index are positions in the
    /// *whole* table. Without this, a click on the tenth visible row of a filtered
    /// table would report index 10 while the selection space says 4,000.
    /// Unwindowed tables leave it at zero.
    pub offset: usize,
}

impl<'a> TableView<'a> {
    /// A table whose rows are already built.
    pub fn with_rows(rows: Vec<Vec<String>>) -> Self {
        Self {
            rows: TableRows::Cells(rows),
            ..Self::default()
        }
    }

    /// A table over a collection too large to materialise: `total` is the row
    /// count and `at` builds one row.
    pub fn lazy(total: usize, at: impl Fn(usize) -> Vec<String> + 'a) -> Self {
        Self {
            rows: TableRows::lazy(total, at),
            ..Self::default()
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Column<'a> {
    pub header: &'a str,
    /// Fixed width, or `None` to share the leftover space evenly.
    pub width: Option<u16>,
    pub align: Align,
    /// Truncate from the left, keeping the tail — correct for paths and ids.
    pub tail: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// A collapsible tree.
#[derive(Clone, Debug, Default)]
pub struct TreeView<'a> {
    pub title: &'a str,
    pub roots: Vec<TreeNode<'a>>,
    pub expanded: Vec<String>,
    pub cursor: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeNode<'a> {
    pub label: String,
    pub id: &'a str,
    pub detail: Option<String>,
    pub children: Vec<TreeNode<'a>>,
}

/// One visible tree row, produced by flattening according to `expanded`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeRow<'a> {
    pub label: String,
    pub id: &'a str,
    pub depth: usize,
    pub expandable: bool,
    pub expanded: bool,
    pub is_last: bool,
}

/// Flatten a tree into visible rows.
///
/// A node is visible when every ancestor is expanded. The root list is always
/// visible, because a collapsed tree that renders nothing is indistinguishable
/// from a bug.
pub fn flatten_tree<'a>(roots: &'a [TreeNode<'a>], expanded: &[String]) -> Vec<TreeRow<'a>> {
    fn walk<'a>(
        nodes: &'a [TreeNode<'a>],
        expanded: &[String],
        depth: usize,
        out: &mut Vec<TreeRow<'a>>,
    ) {
        for (i, n) in nodes.iter().enumerate() {
            let is_expanded = expanded.iter().any(|e| e == n.id);
            let expandable = !n.children.is_empty();
            out.push(TreeRow {
                label: n.label.clone(),
                id: n.id,
                depth,
                expandable,
                expanded: is_expanded,
                is_last: i + 1 == nodes.len(),
            });
            if expandable && is_expanded {
                walk(&n.children, expanded, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(roots, expanded, 0, &mut out);
    out
}

/// Draw a one-column list, registering hit targets for each visible row.
pub fn draw_list(
    t: &Tokens,
    frame: &mut Frame,
    area: Rect,
    view: &ListView<'_>,
    hits: &mut super::layout::HitTargets,
) {
    let block = super::widgets::panel(t, view.title, super::widgets::PanelTone::Default);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    draw_list_rows(t, frame, inner, view, hits);
}

/// Paint list rows into an already-chrome'd `inner`.
///
/// Split out of [`draw_list`] so a view that has already drawn its own panel —
/// via [`super::shell::content_block`] — can share this row rendering instead of
/// re-implementing selection styling, windowing, displacement, truncation and hit
/// registration. Calling [`draw_list`] in such a view would draw a second border.
pub fn draw_list_rows(
    t: &Tokens,
    frame: &mut Frame,
    inner: Rect,
    view: &ListView<'_>,
    hits: &mut super::layout::HitTargets,
) {
    draw_list_rows_targeted(t, frame, inner, view, hits, |hits, rect, index| {
        hits.register(rect, super::layout::HitTarget::ListRow { index });
    });
}

/// [`draw_list_rows`], with the hit target supplied by the caller.
///
/// A surface whose rows mean something more specific than "the Nth list row" —
/// Workbench's four panels, where a row is identified by *which panel it is in*
/// — needs its own target variant. Routing those through this function keeps one
/// implementation of selection styling, windowing, displacement and truncation,
/// so a panel cannot develop a second copy of the rules that made it disagree
/// with the list beside it.
pub fn draw_list_rows_targeted(
    t: &Tokens,
    frame: &mut Frame,
    inner: Rect,
    view: &ListView<'_>,
    hits: &mut super::layout::HitTargets,
    mut register: impl FnMut(&mut super::layout::HitTargets, Rect, usize),
) {
    let offset = view.offset;
    draw_list_windowed_targeted(
        t,
        frame,
        inner,
        view.total.unwrap_or(view.items.len()),
        // Absolute rows the caller can produce: `items` is indexed from `offset`,
        // so a pre-sliced view holds rows `offset..offset + len`.
        view.offset + view.items.len(),
        view.cursor,
        offset,
        view.show_displacement,
        hits,
        &mut register,
        &mut |i| view.items[usize::try_from(i).unwrap_or(0)].clone(),
    );
}

/// [`draw_list_rows_targeted`] for a collection too large to build rows for.
///
/// The window, selection styling, truncation, displacement markers and hit
/// registration all come from the same implementation as the eager variant — this
/// only changes *where the rows come from*. `row` is called for the rows inside the
/// window and nowhere else, so a 10,000-row panel costs a screenful of `format!`
/// calls rather than ten thousand.
///
/// `offset` is where the caller's `row` numbers begin: 0 for a whole collection, or
/// the window's start for a caller that already sliced its own rows.
pub fn draw_list_rows_lazy_targeted<'a>(
    t: &Tokens,
    frame: &mut Frame,
    inner: Rect,
    total: usize,
    cursor: Option<usize>,
    offset: usize,
    show_displacement: bool,
    hits: &mut super::layout::HitTargets,
    mut register: impl FnMut(&mut super::layout::HitTargets, Rect, usize),
    mut row: impl FnMut(usize) -> ListItem<'a>,
) {
    draw_list_windowed_targeted(
        t,
        frame,
        inner,
        total,
        // The lazy caller holds the whole collection, so every windowed row is
        // buildable.
        total,
        cursor,
        offset,
        show_displacement,
        hits,
        &mut register,
        &mut row,
    );
}

// Test-only counter for list rows materialised by the shared list painter.
//
// `agent::render` keeps a companion counter for its own windowed lists; this one
// covers everything that reaches `draw_list`/`draw_list_rows_lazy_targeted`, which
// is most of the TUI. A hit-target count is only an upper bound on
// materialisation; a counter at the factory call is what proves the work did not
// happen.
#[cfg(test)]
thread_local! {
    static LIST_ROWS_MATERIALISED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_list_rows_materialised() {
    LIST_ROWS_MATERIALISED.with(|c| c.set(0));
}

#[cfg(test)]
pub(crate) fn list_rows_materialised() -> usize {
    LIST_ROWS_MATERIALISED.with(std::cell::Cell::get)
}

#[cfg(not(test))]
#[inline]
fn note_rows_materialised(_count: usize) {}

#[cfg(test)]
fn note_rows_materialised(count: usize) {
    LIST_ROWS_MATERIALISED.with(|c| c.set(c.get() + count));
}

/// The one implementation of list painting, shared by the eager and lazy callers.
///
/// `total` is the size of the whole collection and `cursor`/`offset` are in the
/// caller's coordinate space, so the window is always decided against the true
/// total — the eager path's `total: None` degenerates to `items.len()`, which is
/// what it always meant.
#[allow(clippy::too_many_arguments)]
fn draw_list_windowed_targeted<'a>(
    t: &Tokens,
    frame: &mut Frame,
    inner: Rect,
    total: usize,
    available: usize,
    cursor: Option<usize>,
    offset: usize,
    show_displacement_requested: bool,
    hits: &mut super::layout::HitTargets,
    register: &mut impl FnMut(&mut super::layout::HitTargets, Rect, usize),
    row: &mut impl FnMut(usize) -> ListItem<'a>,
) {
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let view_show_displacement = show_displacement_requested;

    // A read-only list has nothing to scroll to, so it shows no displacement.
    let show_displacement = view_show_displacement && cursor.is_some();
    // The cursor and the window are both in *unwindowed* coordinates, so the
    // window is computed against the true total and only then mapped onto the
    // slice the caller chose to materialise. A caller that passes every row
    // (`total: None`) gets `offset == 0` and the previous behaviour exactly.
    // The displacement markers are painted as *rows*, so they have to be
    // subtracted before the window is measured. Measuring first and appending
    // afterwards produced `height + 2` lines for a `height` row area, which
    // silently clipped the last visible item and the `↓N` marker — and the one
    // existing test only asserted `↑`, so it passed. Reserving a fixed two rows
    // whenever anything is hidden also keeps the layout from jittering as the
    // window reaches either end.
    let (start, count) = list_window(total, cursor, inner.height, show_displacement);
    // Clamp into what the caller actually holds. If a caller hands over a window
    // cut for a different height, painting stays inside it rather than silently
    // showing fewer rows than the window promised.
    let local_start = start.saturating_sub(offset);
    let count = count.min(available.saturating_sub(offset).saturating_sub(local_start));
    let hidden_above = start.min(usize::from(u16::MAX)) as u16;
    let hidden_below = total
        .saturating_sub(start + count)
        .min(usize::from(u16::MAX)) as u16;
    let reserve = u16::from(show_displacement && (hidden_above > 0 || hidden_below > 0));
    let list_w = saturating_sub(inner.width, reserve);

    let mut lines: Vec<Line> = Vec::new();
    if hidden_above > 0 && show_displacement {
        lines.push(Line::styled(
            format!("↑{hidden_above}"),
            Style::default().fg(t.text.muted),
        ));
    }
    note_rows_materialised(count);
    for row_idx in 0..count {
        let item = row(local_start + row_idx);
        let idx = start + row_idx;
        let selected = cursor == Some(idx);
        let tone = item.tone.unwrap_or(RowTone::Normal);
        let fg = if selected { t.text.inverse } else { tone.fg(t) };
        let bg = if selected {
            t.accent.primary
        } else {
            t.surfaces.surface
        };
        let style = Style::default().fg(fg).bg(bg);
        let mut text = truncate_tail(&sanitize_field(&item.label), list_w.saturating_sub(2));
        // A row squeezed to zero columns used to paint blank *and still register a
        // hit target*, leaving an invisible clickable row. A row either shows what
        // it is or is not interactive, so a label that truncates away falls back to
        // a placeholder. Only when there is no detail either: a right-aligned
        // detail is the label's other half, and blanking it would delete real
        // content (the settings key/value rows are exactly this shape).
        let placeholder = text.is_empty() && item.detail.is_none();
        if placeholder {
            text = crate::run::g("(unnamed)", "(unnamed)").to_string();
        }
        // The marker marks the *selected* row only. Drawing it on every row —
        // which this component did — makes every row read as selected.
        let marker = if selected {
            crate::run::g("›", ">").to_string()
        } else {
            " ".to_string()
        };
        let mut spans = vec![Span::styled(
            format!("{marker} {text}"),
            if selected {
                style.add_modifier(Modifier::BOLD)
            } else {
                style
            }
            // The placeholder is the absence of a label; it should not read as
            // one. It stays muted unless the row is selected, where the row's
            // own weight carries the state.
            .add_modifier(if placeholder && !selected {
                Modifier::DIM
            } else {
                Modifier::empty()
            }),
        )];
        if let Some(d) = &item.detail {
            // `text` was laid out in *columns*, so the space it consumes has to be
            // counted in columns too. Counting chars let a CJK label push the
            // right-aligned detail off the row.
            let used =
                u16::try_from(crate::design::text::display_width(&text) + 2).unwrap_or(u16::MAX);
            let room = list_w.saturating_sub(used + 1);
            if room > 0 {
                let room = usize::from(room);
                spans.push(Span::styled(
                    format!("{d:>room$}"),
                    Style::default()
                        .fg(if selected {
                            t.text.inverse
                        } else {
                            t.text.muted
                        })
                        .bg(bg),
                ));
            }
        }
        lines.push(Line::from(spans));
        // Every row registers, *including the selected one*. Skipping the selected
        // row made the most common click of all — "click the row I am already on to
        // activate it" — a no-op, because that row had no hit region.
        register(
            hits,
            Rect {
                x: inner.x,
                y: inner.y + u16::try_from(lines.len() - 1).unwrap_or(0),
                width: list_w,
                height: 1,
            },
            idx,
        );
    }
    if hidden_below > 0 && show_displacement {
        lines.push(Line::styled(
            format!("↓{hidden_below}"),
            Style::default().fg(t.text.muted),
        ));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Draw a table, shrinking the widest column first so the last column is the
/// one that gets squeezed.
pub fn draw_table(
    t: &Tokens,
    frame: &mut Frame,
    area: Rect,
    view: &TableView<'_>,
    hits: &mut super::layout::HitTargets,
) {
    let block = super::widgets::panel(t, view.title, super::widgets::PanelTone::Default);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    draw_table_rows(t, frame, inner, view, hits);
}

/// Paint a table into an already-chrome'd `inner`.
///
/// Split out of [`draw_table`] for the same reason as [`draw_list_rows`]: a view
/// that already drew its panel must be able to share this rendering rather than
/// hand-rolling column padding and re-deciding truncation.
pub fn draw_table_rows(
    t: &Tokens,
    frame: &mut Frame,
    inner: Rect,
    view: &TableView<'_>,
    hits: &mut super::layout::HitTargets,
) {
    if inner.height == 0 || inner.width == 0 || view.columns.is_empty() {
        return;
    }

    // Header row plus a rule; both come out of the vertical budget first.
    let has_header = inner.height >= 2 && view.columns.iter().any(|c| !c.header.is_empty());
    let widths = column_widths(view, inner.width, has_header);
    if has_header {
        let mut spans: Vec<Span> = Vec::new();
        let last = view.columns.len().saturating_sub(1);
        for (i, (c, w)) in view.columns.iter().zip(widths.iter()).enumerate() {
            spans.push(pad(
                &sanitize_field(c.header),
                *w,
                c.align,
                c.tail,
                Style::default().fg(t.text.muted),
            ));
            // `column_widths` budgets one extra column per column as a gutter;
            // emit it. Without it a cell that exactly fills its width runs into
            // the next header (`toolssrc`), which is unreadable.
            if i != last {
                spans.push(Span::raw(" "));
            }
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect { height: 1, ..inner },
        );
    }
    let body_y = inner.y + u16::from(has_header);
    let body_h = saturating_sub(inner.height, u16::from(has_header));
    let body = Rect {
        y: body_y,
        height: body_h,
        ..inner
    };
    if body_h == 0 {
        return;
    }

    // `view.cursor` is in table coordinates, so it is shifted by `offset` before
    // being used to window the (possibly already windowed) `rows`.
    let local = view.cursor.saturating_sub(view.offset);
    let (start, count) = window(view.rows.len(), local, body_h);
    let mut lines: Vec<Line> = Vec::new();
    for (r, row) in view.rows.window(start, count).enumerate() {
        // Back into table coordinates, so the hit index names the same row the
        // selection does.
        let idx = view.offset + start + r;
        let selected = idx == view.cursor;
        let bg = if selected {
            t.accent.primary
        } else {
            t.surfaces.surface
        };
        let fg = if selected {
            t.text.inverse
        } else {
            t.text.secondary
        };
        let mut spans: Vec<Span> = Vec::new();
        let last = view.columns.len().saturating_sub(1);
        for (i, c) in view.columns.iter().enumerate() {
            let cell = row.get(i).map(String::as_str).unwrap_or("");
            spans.push(pad(
                cell,
                widths[i],
                c.align,
                c.tail,
                Style::default().fg(fg).bg(bg),
            ));
            if i != last {
                spans.push(Span::styled(" ", Style::default().fg(fg).bg(bg)));
            }
        }
        lines.push(Line::from(spans));
        // The selected row registers too — see the note in the list renderer.
        hits.register(
            Rect {
                x: body.x,
                y: body.y + u16::try_from(r).unwrap_or(0),
                width: body.width,
                height: 1,
            },
            super::layout::HitTarget::ListRow { index: idx },
        );
    }
    frame.render_widget(Paragraph::new(lines), body);
}

/// Draw a tree with disclosure triangles and indent guides.
pub fn draw_tree(
    t: &Tokens,
    frame: &mut Frame,
    area: Rect,
    view: &TreeView<'_>,
    hits: &mut super::layout::HitTargets,
) {
    let block = super::widgets::panel(t, view.title, super::widgets::PanelTone::Default);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let rows = flatten_tree(&view.roots, &view.expanded);
    let (start, count) = window(rows.len(), view.cursor, inner.height);

    let mut lines: Vec<Line> = Vec::new();
    for (r, row) in rows.iter().skip(start).take(count).enumerate() {
        let idx = start + r;
        let selected = idx == view.cursor;
        let bg = if selected {
            t.accent.primary
        } else {
            t.surfaces.surface
        };
        let fg = if selected {
            t.text.inverse
        } else {
            t.text.secondary
        };
        let marker = if row.expandable {
            crate::run::g(
                if row.expanded { "▾" } else { "▸" },
                if row.expanded { "-" } else { "+" },
            )
        } else {
            crate::run::g("·", ".")
        };
        let indent = "  ".repeat(row.depth);
        let label = format!("{indent}{marker} {}", row.label);
        let clipped = truncate_tail(&label, inner.width.saturating_sub(1));
        lines.push(Line::styled(clipped, Style::default().fg(fg).bg(bg)));
        // The selected row registers too — see the note in the list renderer.
        hits.register(
            Rect {
                x: inner.x,
                y: inner.y + u16::try_from(r).unwrap_or(0),
                width: inner.width,
                height: 1,
            },
            super::layout::HitTarget::ListRow { index: idx },
        );
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Resolve column widths: fixed columns keep their width, the rest split what
/// is left. If that is still too narrow, every flexible column is trimmed
/// evenly rather than the last one collapsing to nothing.
fn column_widths(view: &TableView<'_>, total: u16, has_header: bool) -> Vec<u16> {
    let n = view.columns.len();
    let mut out = vec![0u16; n];
    let fixed: u16 = view
        .columns
        .iter()
        .filter_map(|c| c.width)
        .map(|w| w.saturating_add(1))
        .sum();
    let flexible = n - view.columns.iter().filter(|c| c.width.is_some()).count();
    let left = total.saturating_sub(fixed);
    if flexible > 0 {
        let each = left / flexible as u16;
        let mut i = 0;
        for (o, c) in out.iter_mut().zip(view.columns.iter()) {
            if c.width.is_none() {
                *o = each.max(1);
                i += 1;
            } else {
                *o = c.width.unwrap_or(0);
            }
        }
        // Hand the remainder to the last flexible column so the table fills.
        let used: u16 = out.iter().map(|w| w.saturating_add(1)).sum();
        if used < total {
            if let Some(last) = out.iter_mut().rev().find(|w| **w > 0) {
                *last = last.saturating_add(total - used);
            }
        }
        let _ = i;
    } else {
        for (o, c) in out.iter_mut().zip(view.columns.iter()) {
            *o = c.width.unwrap_or(0);
        }
    }
    let _ = has_header;
    out
}

fn pad(s: &str, w: u16, align: Align, tail: bool, style: Style) -> Span<'static> {
    let w = usize::from(w);
    let text = if tail {
        truncate_tail(s, u16::try_from(w).unwrap_or(u16::MAX))
    } else {
        ellipsize(s, u16::try_from(w).unwrap_or(u16::MAX))
    };
    let fill = w.saturating_sub(crate::design::text::display_width(&text));
    let body = match align {
        Align::Left => format!("{text}{}", " ".repeat(fill)),
        Align::Right => format!("{}{text}", " ".repeat(fill)),
    };
    Span::styled(body, style)
}

/// Truncate keeping the tail, so `a/b/c/deep/file.rs` becomes `…/file.rs`.
pub fn truncate_tail(s: &str, w: u16) -> String {
    crate::design::text::truncate_width(
        &sanitize_field(s),
        w,
        crate::design::text::Truncation::Tail,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::design::layout::HitTargets;
    use crate::theme::Theme;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use ratatui::Terminal;

    fn t() -> Tokens {
        Theme::default_theme().tokens()
    }

    /// Every painted cell that carries text, as `(fg, bg, symbol)`.
    ///
    /// Measuring the palette is not the same as measuring the screen: the
    /// renderer chooses which token goes where, and High Contrast proves the
    /// difference — `text_primary` on `surfaces.selected` is 1.07:1, and the
    /// list survives only because it inverts. Reading the buffer measures what
    /// the operator actually sees.
    ///
    /// Blank cells and box-drawing borders are excluded because they carry no
    /// text to read; borders are checked against the UI-component figure
    /// instead, below.
    fn painted_cells(
        w: u16,
        h: u16,
        theme: crate::theme::ThemeKind,
    ) -> Vec<(Color, Color, String)> {
        let tokens = crate::theme::Theme::new(theme).tokens();
        let view = ListView {
            items: vec![
                ListItem {
                    label: "alpha".into(),
                    detail: None,
                    tone: None,
                    id: "a",
                },
                ListItem {
                    label: "beta".into(),
                    detail: None,
                    tone: None,
                    id: "b",
                },
                ListItem {
                    label: "gamma".into(),
                    detail: Some("3".into()),
                    tone: None,
                    id: "c",
                },
            ],
            cursor: Some(1),
            title: " panel ",
            show_displacement: false,
            total: None,
            offset: 0,
        };
        let mut term = Terminal::new(TestBackend::new(w, h)).expect("term");
        term.draw(|frame| {
            let area = Rect {
                x: 0,
                y: 0,
                width: w,
                height: h,
            };
            let mut hits = HitTargets::new();
            draw_list(&tokens, frame, area, &view, &mut hits);
        })
        .expect("draw");
        let buf = term.backend().buffer().clone();
        (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .map(|(x, y)| {
                let cell = &buf[(x, y)];
                (cell.fg, cell.bg, cell.symbol().to_string())
            })
            .collect()
    }

    fn is_text(sym: &str) -> bool {
        !sym.trim().is_empty()
            && !sym
                .chars()
                .all(|c| matches!(c, '\u{2500}'..='\u{257f}' | '|'))
    }

    #[test]
    fn every_painted_text_cell_is_readable_in_every_theme() {
        for kind in crate::theme::ThemeKind::ALL {
            let floor = if kind == crate::theme::ThemeKind::HighContrast {
                7.0
            } else {
                4.5
            };
            for (fg, bg, sym) in painted_cells(30, 6, kind) {
                if !is_text(&sym) {
                    continue;
                }
                let Some(r) = crate::design::contrast::ratio(fg, bg) else {
                    continue;
                };
                assert!(
                    r + 1e-9 >= floor,
                    "{kind:?}: {sym:?} at {fg:?} on {bg:?} is {r:.4}:1 (< {floor})",
                );
            }
        }
    }

    /// Borders that carry *state* are UI components, not body text, so they are
    /// held to the WCAG 1.4.11 component figure of 3:1, and `border_focus` — the
    /// one element that has to be findable by eye — to 4.5:1, in every theme.
    ///
    /// The two purely structural borders are treated differently, and the
    /// difference is a deliberate decision rather than a passing threshold:
    ///
    /// * `border_subtle` and `border` are *chrome*. They separate regions; they
    ///   do not carry state, and no control is identified by them. WCAG 1.4.11
    ///   exempts purely decorative elements, so they are measured and printed
    ///   rather than asserted.
    /// * `HighContrast` is required to clear 3:1 even for chrome, because an
    ///   operator who chose that theme is asking for every edge to be findable.
    ///
    /// The consequence is stated plainly in the phase report: in the four normal
    /// themes a panel edge sits near 1.3–1.7:1, so region separation there is
    /// carried by layout and text, not by the border. An operator who needs
    /// visible edges switches to `HighContrast`; the value is measured, not
    /// assumed.
    #[test]
    fn state_borders_meet_the_ui_component_figure_in_every_theme() {
        for kind in crate::theme::ThemeKind::ALL {
            let p = kind.palette();
            let hc = kind == crate::theme::ThemeKind::HighContrast;
            for (name, c, want) in [
                ("border_active", p.border_active, 3.0),
                ("border_destructive", p.border_destructive, 3.0),
                ("border_focus", p.border_focus, 4.5),
                ("border", p.border, if hc { 3.0 } else { 0.0 }),
                ("border_subtle", p.border_subtle, 0.0),
            ] {
                // A panel can rest on any plane, so the figure has to hold
                // against the *worst* of them. Checking `bg` alone passes a
                // border that vanishes on a raised card.
                let worst = [p.bg, p.surface, p.elevated]
                    .iter()
                    .map(|b| crate::design::contrast::ratio(c, *b).unwrap_or(0.0))
                    .fold(f64::INFINITY, f64::min);
                println!("{kind:?} {name:18} {worst:8.4} (>= {want:.1}, worst plane)");
                if want > 0.0 {
                    assert!(
                        worst + 1e-9 >= want,
                        "{kind:?} {name} is {worst:.4}:1 (< {want})"
                    );
                }
            }
        }
    }

    #[test]
    fn a_selected_row_is_distinguishable_without_measuring_color() {
        let _guard = crate::run::ascii_mode_lock();
        // The P7 selection-vs-focus rule: selection is a *position* signal, so it
        // must still read in Monochrome, where no hue is available to say
        // "this one". The gutter marker carries it.
        let marker = crate::run::g("\u{203a}", ">");
        let tokens = crate::theme::Theme::new(crate::theme::ThemeKind::Monochrome).tokens();
        let mut term = Terminal::new(TestBackend::new(30, 8)).expect("term");
        term.draw(|frame| {
            let area = Rect {
                x: 0,
                y: 0,
                width: 30,
                height: 8,
            };
            let mut hits = HitTargets::new();
            draw_list(
                &tokens,
                frame,
                area,
                &ListView {
                    items: vec![
                        ListItem {
                            label: "alpha".into(),
                            detail: None,
                            tone: None,
                            id: "a",
                        },
                        ListItem {
                            label: "beta".into(),
                            detail: None,
                            tone: None,
                            id: "b",
                        },
                        ListItem {
                            label: "gamma".into(),
                            detail: None,
                            tone: None,
                            id: "c",
                        },
                    ],
                    cursor: Some(1),
                    title: " panel ",
                    show_displacement: false,
                    total: None,
                    offset: 0,
                },
                &mut hits,
            );
        })
        .expect("draw");
        let buf = term.backend().buffer().clone();
        let row = |y: u16| -> String { (0..30).map(|x| buf[(x, y)].symbol()).collect() };
        // Row 0 is the title, row 1 the first item; the cursor is on row 2.
        assert!(
            row(2).contains(marker),
            "no selection marker on the cursor row: {:?}",
            row(2)
        );
        assert!(
            !row(1).contains(marker),
            "an unselected row is marked: {:?}",
            row(1)
        );
        assert!(
            !row(3).contains(marker),
            "an unselected row is marked: {:?}",
            row(3)
        );
        assert!(
            row(2).contains("beta"),
            "the cursor row is not the selected item: {:?}",
            row(2)
        );
    }

    fn draw(w: u16, h: u16, f: impl FnOnce(&mut Frame, Rect, &mut HitTargets)) -> String {
        let mut term = Terminal::new(TestBackend::new(w.max(1), h.max(1))).expect("term");
        term.draw(|frame| {
            let a = frame.area();
            let mut hits = HitTargets::new();
            f(frame, a, &mut hits);
        })
        .expect("draw");
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn window_always_contains_the_cursor() {
        for total in [0usize, 1, 5, 50, 5000] {
            for cursor in [0, 1, 7, 49, 4999] {
                for h in [0u16, 1, 3, 20] {
                    let (s, c) = window(total, cursor, h);
                    let cur = cursor.min(total.saturating_sub(1));
                    if total > usize::from(h) && h > 0 {
                        assert!(
                            cur >= s,
                            "cursor {cur} above window {s} (total={total} h={h})"
                        );
                        assert!(
                            cur < s + c,
                            "cursor {cur} below window {s}..{} (total={total} h={h})",
                            s + c
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn window_is_a_noop_when_everything_fits() {
        assert_eq!(window(3, 0, 10), (0, 3));
        assert_eq!(window(0, 0, 10), (0, 0));
        assert_eq!(window(5, 2, 0), (0, 0));
    }

    #[test]
    fn window_clamps_a_cursor_past_the_end() {
        let (s, c) = window(10, 999, 4);
        assert_eq!((s, c), (6, 4));
    }

    #[test]
    fn window_never_reads_out_of_bounds() {
        for total in 1..40usize {
            for cursor in 0..40usize {
                let (s, c) = window(total, cursor, 5);
                assert!(s + c <= total, "s={s} c={c} total={total}");
            }
        }
    }

    #[test]
    fn tail_truncation_keeps_the_informative_end() {
        let _guard = crate::run::ascii_mode_lock();
        let marker = crate::run::g("\u{2026}", "...");
        // A 12-column budget: one column for the marker, eleven for the tail.
        assert_eq!(
            truncate_tail("a/b/c/deep/file.rs", 12),
            format!("{marker}eep/file.rs")
        );
        assert_eq!(truncate_tail("short", 10), "short");
        assert_eq!(truncate_tail("abc", 0), "");
        // Never panics on a width of 1, which is the classic off-by-one.
        assert_eq!(
            crate::design::text::display_width(&truncate_tail("abcdef", 1)),
            1,
        );
    }

    #[test]
    fn tail_truncation_measures_columns_not_characters() {
        let _guard = crate::run::ascii_mode_lock();
        let marker = crate::run::g("\u{2026}", "...");
        let mw = crate::design::text::display_width(marker);
        // Five CJK characters are 15 bytes, 5 characters, and **10 columns**.
        // The old implementation counted characters, so a 5-column budget was
        // believed to fit all five and a 4-column budget kept four characters
        // worth of 8 columns — painting over its own frame.
        assert_eq!(crate::design::text::display_width("配置文件名"), 10);
        assert_eq!(truncate_tail("配置文件名", 10), "配置文件名");
        // 5 columns = the marker plus 4 columns = the marker plus 2 characters.
        assert_eq!(truncate_tail("配置文件名", 5), format!("{marker}件名"));
        // Whatever it returns, it must fit the budget in cells.
        for cols in 0..14u16 {
            let out = truncate_tail("配置文件名", cols);
            assert!(
                crate::design::text::display_width(&out) <= usize::from(cols),
                "{out:?} is {} columns in {cols}",
                crate::design::text::display_width(&out),
            );
            assert!(
                !out.is_empty() || cols == 0,
                "no scroll marker at {cols}: {out:?}"
            );
            let _ = mw;
        }
    }

    #[test]
    fn flatten_tree_respects_expansion() {
        let roots = vec![TreeNode {
            label: "root".into(),
            id: "r",
            detail: None,
            children: vec![TreeNode {
                label: "child".into(),
                id: "c",
                detail: None,
                children: vec![],
            }],
        }];
        assert_eq!(
            flatten_tree(&roots, &[]).len(),
            1,
            "collapsed hides the child"
        );
        assert_eq!(flatten_tree(&roots, &["r".into()]).len(), 2);
        let rows = flatten_tree(&roots, &["r".into()]);
        assert_eq!(rows[1].depth, 1);
        assert!(rows[0].expandable && rows[0].expanded);
        assert!(!rows[1].expandable);
    }

    #[test]
    fn flatten_tree_keeps_roots_visible_even_when_all_collapsed() {
        let roots = vec![
            TreeNode {
                label: "a".into(),
                id: "a",
                detail: None,
                children: vec![],
            },
            TreeNode {
                label: "b".into(),
                id: "b",
                detail: None,
                children: vec![],
            },
        ];
        assert_eq!(flatten_tree(&roots, &[]).len(), 2);
    }

    #[test]
    fn list_renders_items_and_registers_hits() {
        let mut hits = HitTargets::new();
        let items: Vec<ListItem> = (0..3)
            .map(|i| ListItem {
                label: format!("row{i}"),
                detail: None,
                tone: None,
                id: "x",
            })
            .collect();
        let out = draw(20, 6, |f, a, h| {
            let v = ListView {
                title: " items ",
                items,
                cursor: Some(1),
                show_displacement: false,
                total: None,
                offset: 0,
            };
            draw_list(&t(), f, a, &v, h);
            hits = std::mem::take(h);
        });
        assert!(out.contains("row0") && out.contains("row2"), "{out}");
        assert!(out.contains("items"));
    }

    #[test]
    fn list_displacement_is_reported_when_rows_are_hidden() {
        let items: Vec<ListItem> = (0..50)
            .map(|i| ListItem {
                label: format!("r{i}"),
                detail: None,
                tone: None,
                id: "x",
            })
            .collect();
        let out = draw(20, 6, |f, a, h| {
            let v = ListView {
                title: " many ",
                items,
                cursor: Some(30),
                show_displacement: true,
                total: None,
                offset: 0,
            };
            draw_list(&t(), f, a, &v, h);
        });
        assert!(out.contains('↑'), "hidden rows above not reported: {out}");
        // The marker is a row, so it competes with items for vertical space.
        // Measuring the window first and appending the marker afterwards
        // produced one line too many for the rect, which silently clipped both
        // the last visible item and this marker off the bottom. The existing
        // assertion only covered `↑`, so the truncation went unnoticed.
        assert!(out.contains('↓'), "hidden rows below not reported: {out}");
        // The rows between the two markers are the real content, and they must
        // survive: a fix that hides `↓` by dropping items would pass the
        // assertion above while showing nothing.
        assert!(
            out.contains("r30") && out.contains("r31"),
            "the window was squeezed until the items vanished: {out}",
        );
    }

    #[test]
    fn a_scrolled_list_paints_every_row_it_reserved_for() {
        // With rows reserved for displacement, the item window must still show
        // `height - 2` real items, not fewer.
        let items: Vec<ListItem> = (0..50)
            .map(|i| ListItem {
                label: format!("r{i}"),
                detail: None,
                tone: None,
                id: "x",
            })
            .collect();
        let out = draw(20, 10, |f, a, h| {
            let v = ListView {
                title: " many ",
                items,
                cursor: Some(0),
                show_displacement: true,
                total: None,
                offset: 0,
            };
            draw_list(&t(), f, a, &v, h);
        });
        // A 10-row frame has 8 usable rows once the borders are subtracted, and
        // two of those are the displacement markers, leaving six items.
        for want in ["r0", "r1", "r2", "r3", "r4", "r5"] {
            assert!(out.contains(want), "row {want} missing from window: {out}");
        }
        assert!(
            !out.contains("r6"),
            "painted past the reserved window: {out}",
        );
        assert!(out.contains('↓'), "hidden rows below not reported: {out}");
    }

    #[test]
    fn a_pre_sliced_window_still_reports_the_true_hidden_row_count() {
        // Phase 8: a caller materialises only the rows that fit. The markers
        // must count the *whole* collection, otherwise bounding the work would
        // silently start lying about what is off screen.
        let items: Vec<ListItem> = (0..6)
            .map(|i| ListItem {
                label: format!("r{i}"),
                detail: None,
                tone: None,
                id: "x",
            })
            .collect();
        let out = draw(20, 10, |f, a, h| {
            let v = ListView {
                title: " many ",
                items,
                // Mid-collection, and `items` was cut from filtered index 596 —
                // the shape a caller produces when it materialises only the
                // window `design::collections::window` returned.
                cursor: Some(600),
                show_displacement: true,
                total: Some(1_240),
                offset: 596,
            };
            draw_list(&t(), f, a, &v, h);
        });
        assert!(
            out.contains("↑"),
            "a pre-sliced window must still report hidden rows above: {out}",
        );
        assert!(
            out.contains("↓"),
            "a pre-sliced window must still report hidden rows below: {out}",
        );
    }

    #[test]
    fn a_pre_sliced_window_reports_a_known_total() {
        // The exact count is the operator's only cue that the list is truncated,
        // so pin it rather than just the arrow glyphs.
        let items: Vec<ListItem> = (0..4)
            .map(|i| ListItem {
                label: format!("r{i}"),
                detail: None,
                tone: None,
                id: "x",
            })
            .collect();
        let out = draw(20, 10, |f, a, h| {
            let v = ListView {
                title: " many ",
                items,
                cursor: Some(0),
                show_displacement: true,
                total: Some(1_000),
                offset: 0,
            };
            draw_list(&t(), f, a, &v, h);
        });
        // 1,000 rows, only 4 of them materialised: the marker reports the rows
        // below the last one actually painted. If this ever printed a count
        // derived from the window height rather than from what was painted, it
        // would under-report; if it ignored `total` entirely it would claim
        // "0 hidden" and the operator would believe they had seen everything.
        assert!(
            out.contains("↓996"),
            "hidden-row count did not come from the true total: {out}",
        );
    }

    #[test]
    fn list_never_panics_on_degenerate_geometry() {
        for (w, h) in [(1u16, 1u16), (1, 40), (3, 1), (40, 2), (200, 60)] {
            let _ = draw(w, h, |f, a, hits| {
                let items = vec![ListItem {
                    label: "a-very-long-label-that-will-not-fit".into(),
                    detail: Some("detail".into()),
                    tone: Some(RowTone::Danger),
                    id: "x",
                }];
                let v = ListView {
                    title: " t ",
                    items,
                    cursor: Some(99),
                    show_displacement: true,
                    total: None,
                    offset: 0,
                };
                draw_list(&t(), f, a, &v, hits);
            });
        }
    }

    #[test]
    fn table_renders_a_header_and_all_columns() {
        let out = draw(60, 8, |f, a, h| {
            let v = TableView {
                title: " models ",
                columns: vec![
                    Column {
                        header: "ID",
                        width: Some(12),
                        align: Align::Left,
                        tail: true,
                    },
                    Column {
                        header: "SIZE",
                        width: None,
                        align: Align::Right,
                        tail: false,
                    },
                ],
                rows: TableRows::Cells(vec![vec!["gpt-4o-mini".into(), "12MB".into()]]),
                cursor: 0,
                offset: 0,
            };
            draw_table(&t(), f, a, &v, h);
        });
        assert!(out.contains("ID"), "{out}");
        assert!(out.contains("SIZE"), "{out}");
        assert!(out.contains("12MB"), "{out}");
    }

    #[test]
    fn tree_renders_indent_and_disclosure() {
        let out = draw(30, 8, |f, a, h| {
            let roots = vec![TreeNode {
                label: "ws".into(),
                id: "ws",
                detail: None,
                children: vec![TreeNode {
                    label: "room".into(),
                    id: "room",
                    detail: None,
                    children: vec![],
                }],
            }];
            let v = TreeView {
                title: " tree ",
                roots,
                expanded: vec!["ws".into()],
                cursor: 1,
            };
            draw_tree(&t(), f, a, &v, h);
        });
        assert!(out.contains("ws") && out.contains("room"), "{out}");
    }

    #[test]
    fn tree_never_panics_on_degenerate_geometry() {
        for (w, h) in [(1u16, 1u16), (2, 2), (200, 3)] {
            let _ = draw(w, h, |f, a, hits| {
                let roots: Vec<TreeNode> = (0..5)
                    .map(|i| TreeNode {
                        label: format!("deeply-nested-node-{i}"),
                        id: "n",
                        detail: None,
                        children: vec![TreeNode {
                            label: "c".into(),
                            id: "c",
                            detail: None,
                            children: vec![],
                        }],
                    })
                    .collect();
                let v = TreeView {
                    title: " t ",
                    roots,
                    expanded: vec![],
                    cursor: 999,
                };
                draw_tree(&t(), f, a, &v, hits);
            });
        }
    }

    #[test]
    fn row_tones_resolve_to_distinct_status_colors() {
        let t = t();
        let normal = RowTone::Normal.fg(&t);
        // Every status tone must be visually distinct from body text, or a
        // failing row is indistinguishable from a normal one.
        for tone in [
            RowTone::Success,
            RowTone::Warning,
            RowTone::Danger,
            RowTone::Info,
        ] {
            assert_ne!(tone.fg(&t), normal, "{tone:?} collides with normal text");
        }
        assert_ne!(RowTone::Danger.fg(&t), RowTone::Success.fg(&t));
        assert_ne!(RowTone::Warning.fg(&t), RowTone::Danger.fg(&t));
        assert_ne!(RowTone::Info.fg(&t), RowTone::Success.fg(&t));
    }
}
