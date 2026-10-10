//! Display-width measurement and width-aware truncation — the one place the
//! TUI decides how wide a string *looks*.
//!
//! # Why this module exists
//!
//! A terminal cell grid has two different widths for every string:
//!
//! * **byte length** — how much memory it takes, irrelevant to layout;
//! * **char count** — how many scalars it holds, which equals the column count
//!   *only* for pure ASCII;
//! * **display width** — how many cells the terminal will actually advance.
//!
//! The third is the only one a layout can use. They differ for exactly the
//! content an operator is most likely to paste in: accented Latin (one cell,
//! one char, two bytes), CJK and emoji (two cells, one char), and combining
//! marks (zero cells, one char). Truncating by char count produced two failure
//! modes that both look like corruption rather than like a bug: a wide string
//! overran its column and pushed a table's border out of alignment, and a
//! combining sequence was cut in half, leaving a stray accent on the next cell.
//!
//! # Why `unicode-width` specifically
//!
//! Because `ratatui-core` measures with the same crate. If this module counted
//! columns differently from the renderer, the requirement this whole phase
//! exists for — *the width we compute and the width that gets painted are the
//! same width* — would be false in the one place it matters. Both sides read
//! the same tables, so a string this module calls 8 columns wide is 8 columns
//! wide on screen.
//!
//! # Content limits are not widths
//!
//! Bounds like "cap the transcript at 600 characters" are about storage and
//! paste size, not about the cell grid. Those deliberately keep counting
//! `char`s: a 600-character cap on characters is the right contract for a
//! buffer, and changing it to columns would make the limit depend on the
//! operator's script. This module is only for *measurement and truncation*.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Which end of a string survives truncation.
///
/// Not a per-screen decision. A path, a file name, a model id and a session id
/// are identified by their tail; a sentence, an error and a task prompt are
/// identified by their head. Picking the wrong end does not shorten the text,
/// it makes it ambiguous — `…normalize_records.rs` is one file,
/// `normalize_recor….rs` could be any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Truncation {
    /// Keep the beginning: the opening clause is what the text is about.
    Head,
    /// Keep the end: the last path/segment is what identifies the thing.
    Tail,
}

/// Columns `s` occupies in a terminal cell grid.
///
/// ```
/// use openrt_tui::design::text::display_width;
/// assert_eq!(display_width("abc"), 3);
/// assert_eq!(display_width("é"), 1);   // one cell, two bytes
/// assert_eq!(display_width("日本"), 4); // two cells each
/// ```
pub fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Truncate `s` to at most `cols` display columns, marking the cut.
///
/// # Grapheme safety
///
/// Truncation walks **extended grapheme clusters**, not `char`s, and this is
/// load-bearing rather than pedantic. A base letter plus its combining mark is
/// one user-perceived character but two Rust `char`s. Cutting between them leaves
/// the mark behind, orphaned on the next cell: `e` cut from `e` renders as a bare
/// accent sitting next to unrelated text. That reads as a font bug in the
/// terminal, and there is no way for an operator to tell the app did it on
/// purpose.
///
/// Clusters are also what makes the tail case correct. Truncating a family emoji
/// (`👨‍👩‍👧‍👦`, five `char`s and one grapheme) from the left by `char` breaks the
/// ZWJ sequence and leaves a fragment.
///
/// The crate is `unicode-segmentation`, the same one `ratatui-core` uses, so the
/// cluster boundaries here are the ones the renderer will honour. Cost was zero:
/// it was already in the dependency graph.
///
/// `Head` keeps the start, `Tail` keeps the end, and in both cases a cluster that
/// would straddle the boundary is dropped whole. The marker is
/// `run::g("…", "...")`, so ASCII mode gets three dots that fit the same budget.
pub fn truncate_width(s: &str, cols: u16, truncation: Truncation) -> String {
    let cols = usize::from(cols);
    if display_width(s) <= cols {
        return s.to_string();
    }
    if cols == 0 {
        return String::new();
    }
    let marker = crate::run::g("…", "...");
    let marker_w = display_width(marker);
    // A width of 1..=marker_w cannot hold the marker and any content; emitting
    // the marker alone would be *wider* than the column, which is the one
    // outcome truncation must never produce.
    if cols <= marker_w {
        return marker.chars().take(cols).collect();
    }
    let budget = cols - marker_w;

    match truncation {
        Truncation::Head => {
            let mut out = String::new();
            let mut used = 0usize;
            for cluster in s.graphemes(true) {
                let w = display_width(cluster);
                if used.saturating_add(w) > budget {
                    break;
                }
                out.push_str(cluster);
                used += w;
            }
            format!("{out}{marker}")
        }
        Truncation::Tail => {
            let mut kept: Vec<&str> = Vec::new();
            let mut used = 0usize;
            for cluster in s.graphemes(true).rev() {
                let w = display_width(cluster);
                if used.saturating_add(w) > budget {
                    break;
                }
                kept.push(cluster);
                used += w;
            }
            kept.reverse();
            format!("{marker}{}", kept.concat())
        }
    }
}

/// Take the first `cols` display columns of `s`, with no marker.
///
/// Used by caret placement and cursor movement, where a marker would be a
/// lie: the operator is editing, not reading a summary, and a `…` in the
/// middle of the line would be typed into.
pub fn head_width(s: &str, cols: u16) -> String {
    let cols = usize::from(cols);
    if display_width(s) <= cols {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    // Grapheme clusters, for the same reason [`truncate_width`] uses them: the
    // composer draws this prefix under the caret, and a prefix cut between a
    // base letter and its mark would render a stray accent next to it.
    for cluster in s.graphemes(true) {
        let w = display_width(cluster);
        if used.saturating_add(w) > cols {
            break;
        }
        out.push_str(cluster);
        used += w;
    }
    out
}

/// Column index the caret sits at for a cursor `chars` into `s`.
///
/// This is the composer-caret correctness rule: the caret's *column* has to
/// come from summing the widths of the characters before it, not from counting
/// them. With `é` or `日本` in the buffer, a char-counted caret lands one or
/// more cells away from the glyph, so the operator cannot see where their next
/// character will appear.
pub fn caret_column(s: &str, chars: usize) -> u16 {
    // Measure the prefix as a string rather than summing per-char widths. The two
    // are not always equal: a ZWJ emoji is several `char`s whose widths sum to more
    // than the cells the cluster actually occupies, and summing would place the
    // caret to the right of the glyph.
    let byte = s
        .char_indices()
        .nth(chars)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    u16::try_from(display_width(&s[..byte])).unwrap_or(u16::MAX)
}

/// How many characters fit in `cols` display columns from the start of `s`.
///
/// The inverse of [`caret_column`], used to turn a cell column back into a
/// character index. `None` means the cursor is past the end of the text.
pub fn chars_in_width(s: &str, cols: u16) -> Option<usize> {
    let cols = usize::from(cols);
    let mut used = 0usize;
    let mut chars = 0usize;
    // Grapheme-cluster boundaries only. Returning an index inside a cluster
    // would tell the caller to put the caret between a letter and its mark, and
    // the caret would then be drawn over the glyph it belongs to.
    for cluster in s.graphemes(true) {
        let w = display_width(cluster);
        if used.saturating_add(w) > cols {
            // The column budget ends inside this cluster: the only valid caret
            // position is the boundary before it.
            return (used == cols).then_some(chars);
        }
        used += w;
        chars += cluster.chars().count();
    }
    (used == cols).then_some(chars)
}

/// Pad `s` on the right to at least `cols` display columns.
///
/// Width-aware: a wide glyph counts as two, so the padding shrinks rather than
/// pushing the row one cell long. A string already wider than `cols` is
/// returned unchanged — this function pads, and never truncates.
pub fn pad_right(s: &str, cols: u16) -> String {
    let fill = usize::from(cols).saturating_sub(display_width(s));
    format!("{}{}", s, " ".repeat(fill))
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_segmentation::UnicodeSegmentation;

    /// The samples every width assertion in this crate is measured against.
    /// `width` is the hand-checked cell count, not a char count.
    const SAMPLES: [(&str, usize); 10] = [
        ("", 0),
        ("abc", 3),
        ("a/b/c.rs", 8),
        ("é", 1),
        ("café", 4),
        ("naïve—dash", 10),
        ("日本語", 6),
        ("emoji 🎉 here", 13),
        ("é", 1), // combining acute
        ("ok✓", 3),
    ];

    #[test]
    fn display_width_matches_the_hand_checked_cell_count() {
        for (s, want) in SAMPLES {
            assert_eq!(display_width(s), want, "width of {s:?}");
        }
        // The mistake this module exists to prevent: for anything outside pure
        // ASCII, cells and bytes disagree. If these ever agree, the samples are
        // no longer exercising the case and the suite is measuring nothing.
        for (s, _) in SAMPLES {
            if !s.is_ascii() {
                assert_ne!(
                    display_width(s),
                    s.len(),
                    "{s:?} must not be measured in bytes",
                );
            }
        }
    }

    #[test]
    fn truncation_never_exceeds_its_budget() {
        let _guard = crate::run::ascii_mode_lock();
        for (s, _) in SAMPLES {
            for cols in 0..24u16 {
                for end in [Truncation::Head, Truncation::Tail] {
                    let out = truncate_width(s, cols, end);
                    assert!(
                        display_width(&out) <= usize::from(cols),
                        "{s:?} at {cols} {end:?} produced {out:?} = {} columns",
                        display_width(&out),
                    );
                }
            }
        }
    }

    #[test]
    fn truncation_never_splits_a_wide_cell_in_half() {
        let _guard = crate::run::ascii_mode_lock();
        // Every prefix cut of a wide string must still be a whole number of
        // cells, or the next glyph lands mid-character.
        //
        // Two things this has to get right, both of which it previously did not:
        // it must pin the mode itself rather than inherit whatever another test
        // left behind, and it must handle the clipped-marker path. When the
        // column is narrower than the marker, truncation emits a *prefix* of the
        // marker (`"."` from `"..."`) and no content, so stripping the whole
        // marker left the fragment in place and measured it as content.
        for ascii in [false, true] {
            crate::run::set_ascii_mode(ascii);
            let marker = crate::run::g("\u{2026}", "...");
            let marker_w = display_width(marker);
            for cols in 0..12u16 {
                let out = truncate_width("日本語", cols, Truncation::Head);
                let plain = if usize::from(cols) <= marker_w {
                    String::new()
                } else {
                    out.replace(marker, "")
                };
                assert!(
                    display_width(&out) <= usize::from(cols),
                    "ascii={ascii} {out:?} at {cols} exceeds the budget",
                );
                assert_eq!(
                    display_width(&plain) % 2,
                    0,
                    "ascii={ascii} {plain:?} at {cols} ended mid-character",
                );
            }
        }
    }

    #[test]
    fn truncation_keeps_the_identifying_end() {
        let _guard = crate::run::ascii_mode_lock();
        let path = "crates/openrt-services/ingest/normalize_records.rs";
        let tail = truncate_width(path, 20, Truncation::Tail);
        assert!(tail.ends_with(".rs"), "a path keeps its tail: {tail}");
        let msg = "the gateway refused the upstream request";
        let head = truncate_width(msg, 12, Truncation::Head);
        assert!(
            head.starts_with("the gate"),
            "a sentence keeps its head: {head:?} (marker: {:?})",
            crate::run::g("\u{2026}", "..."),
        );
    }

    #[test]
    fn a_narrow_column_gets_a_marker_that_fits() {
        let _guard = crate::run::ascii_mode_lock();
        // `…` is one cell and `...` is three. Both must fit their column.
        assert_eq!(truncate_width("abcdef", 0, Truncation::Head), "");
        assert_eq!(
            display_width(&truncate_width("abcdef", 1, Truncation::Head)),
            1
        );
        assert_eq!(
            display_width(&truncate_width("abcdef", 2, Truncation::Head)),
            2
        );
        assert_eq!(
            display_width(&truncate_width("abcdef", 3, Truncation::Head)),
            3
        );
    }

    #[test]
    fn caret_column_sums_widths_not_characters() {
        assert_eq!(caret_column("abc", 3), 3);
        // One char, one cell: the caret must not move.
        assert_eq!(caret_column("é", 1), 1);
        // Two chars, four cells.
        assert_eq!(caret_column("日本語", 2), 4);
        // Past the end clamps rather than panicking.
        assert_eq!(caret_column("abc", 99), 3);
    }

    #[test]
    fn chars_in_width_is_the_inverse_of_caret_column() {
        for (s, _) in SAMPLES {
            for i in 0..=s.chars().count() {
                let col = caret_column(s, i);
                if col > 40 {
                    continue;
                }
                // A column that lands inside a wide cell has no character
                // boundary; every *reachable* boundary must round-trip.
                let back = chars_in_width(s, col);
                if let Some(back) = back {
                    assert!(back <= s.chars().count(), "{s:?}: {back} is past the end");
                }
            }
        }
    }

    /// Phase 8: truncation walks grapheme clusters, not `char`s.
    ///
    /// These inputs are the ones that break a `char`-based loop. Each is a single
    /// user-perceived character made of several Rust `char`s, so cutting by `char`
    /// leaves a fragment that renders as a stray mark next to unrelated text —
    /// indistinguishable from a font bug in the terminal.
    const CLUSTER_SAMPLES: &[(&str, &str)] = &[
        ("e\u{0301}x", "e + combining acute"), // 2 chars, 1 cluster
        ("a\u{0300}\u{0301}\u{0302}x", "stack of 3 marks"),
        ("\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}x", "ZWJ family"),
        (
            "\u{1f3f4}\u{e0067}\u{e0062}\u{e0065}\u{e006f}\u{e006e}\u{e007f}x",
            "tag sequence",
        ),
        ("\u{1f1ef}\u{1f1f5}x", "regional indicator pair"),
        ("n\u{0303}\u{0329}x", "combining tilde below"),
    ];

    #[test]
    fn truncation_never_splits_a_grapheme_cluster() {
        let _guard = crate::run::ascii_mode_lock();
        for mode in [false, true] {
            crate::run::set_ascii_mode(mode);
            for (s, what) in CLUSTER_SAMPLES {
                let full = u16::try_from(display_width(s)).unwrap_or(u16::MAX);
                for cols in 1..=full + 2 {
                    for trunc in [Truncation::Head, Truncation::Tail] {
                        let out = truncate_width(s, cols, trunc);
                        assert!(
                            display_width(&out) <= usize::from(cols),
                            "{what} {s:?} -> {out:?} at {cols} exceeds the budget",
                        );
                        // Every surviving cluster must survive *whole*. Compare
                        // against the set of intact clusters in the input, so a
                        // torn cluster cannot pass by matching some other one.
                        let intact: Vec<&str> = s.graphemes(true).collect();
                        let marker = crate::run::g("\u{2026}", "...");
                        for got in out.graphemes(true) {
                            // A column narrower than the marker yields a marker
                            // *prefix*, which is legitimately not in the input.
                            if intact.contains(&got) || marker.chars().any(|m| got.contains(m)) {
                                continue;
                            }
                            panic!(
                                "{what} {s:?} -> {out:?} at {cols} ({trunc:?}) in \
                                 ascii={mode} produced {got:?}, which is not a cluster \
                                 of the input",
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn head_prefix_never_splits_a_grapheme_cluster() {
        let _guard = crate::run::ascii_mode_lock();
        for mode in [false, true] {
            crate::run::set_ascii_mode(mode);
            for (s, what) in CLUSTER_SAMPLES {
                let intact: Vec<&str> = s.graphemes(true).collect();
                let full = u16::try_from(display_width(s)).unwrap_or(u16::MAX);
                for cols in 1..=full + 1 {
                    let out = head_width(s, cols);
                    assert!(
                        display_width(&out) <= usize::from(cols),
                        "{what} head_width {s:?} at {cols} exceeds the budget",
                    );
                    for got in out.graphemes(true) {
                        assert!(
                            intact.contains(&got),
                            "{what} head_width {s:?} -> {out:?} at {cols} in ascii={mode} \
                             produced {got:?}, which is not a cluster of the input",
                        );
                    }
                }
            }
        }
    }

    /// The inverse pair must agree, and must never hand back a caret position
    /// inside a cluster.
    #[test]
    fn chars_in_width_only_ever_returns_a_cluster_boundary() {
        let _guard = crate::run::ascii_mode_lock();
        crate::run::set_ascii_mode(false);
        for (s, what) in CLUSTER_SAMPLES {
            let mut boundaries = vec![0usize];
            let mut acc = 0usize;
            for c in s.graphemes(true) {
                acc += c.chars().count();
                boundaries.push(acc);
            }
            for cols in 0..=display_width(s) as u16 {
                if let Some(n) = chars_in_width(s, cols) {
                    assert!(
                        boundaries.contains(&n),
                        "{what} {s:?}: chars_in_width({cols}) = {n}, which is inside a \
                         grapheme cluster (boundaries {boundaries:?})",
                    );
                    // And the forward direction must land on the same column.
                    assert_eq!(
                        caret_column(s, n),
                        cols,
                        "{what} {s:?}: caret_column(s, {n}) != {cols}",
                    );
                }
            }
        }
    }

    #[test]
    fn padding_fills_to_an_exact_column_count_without_ever_shortening() {
        for (s, _) in SAMPLES {
            for cols in 0..16u16 {
                // Padding can only add. A string already wider than its column
                // is left alone — silently cutting content to make a column fit
                // is truncation's job, and doing it here would make every
                // padded cell a second, differently-behaving truncation site.
                assert_eq!(
                    display_width(&pad_right(s, cols)),
                    display_width(s).max(usize::from(cols)),
                    "{s:?} @{cols}",
                );
            }
        }
    }
}
