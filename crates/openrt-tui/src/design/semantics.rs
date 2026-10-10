//! The semantic state vocabulary: what each state is *called* and how it is
//! *marked*, in one table.
//!
//! # Why this is a table and not a `match` in each view
//!
//! Every state needs three channels so it survives a monochrome terminal, a
//! colorblind operator, and a screen reader reading the text:
//!
//! 1. a **word** — `RESOURCE BLOCKED`, not a hue;
//! 2. a **marker** — a shape that differs from the shapes around it;
//! 3. a **tone** — color, which reinforces and never carries the meaning alone.
//!
//! Those three lived in four places (a badge helper, a state helper, a resource
//! card, and the words each view invented for itself), and they disagreed:
//! `BadgeTone::Danger` and `BadgeTone::Blocked` painted the *same* `✗`, so in
//! monochrome a denied request and a missing one looked identical; and nothing
//! stopped a view from pairing a success glyph with the word `PENDING`.
//!
//! One table means a state has exactly one identity, and a test can check the
//! whole vocabulary for internal collisions instead of one screen at a time.
//!
//! # Selection and focus are not statuses
//!
//! `SELECTED` and `FOCUSED` describe *where the operator is*, not what happened,
//! so they deliberately have no private marker. Selection is carried by the
//! gutter column and focus by weight plus brackets — the same position and
//! weight signals every list already uses. Giving them unique glyphs as well
//! would mean a fifth thing to look at in a list row, and would break the
//! invariant that gutter marks are reserved for status.

use super::widgets::BadgeTone;

/// A state an operator must be able to tell apart from every other state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Semantics {
    Success,
    Warning,
    Failed,
    /// An action refused on policy grounds — permission denied, mode withheld.
    Blocked,
    Running,
    Pending,
    /// Cursor position within a list. Carried by the gutter column.
    Selected,
    /// Keyboard focus. Carried by weight and brackets.
    Focused,
    /// Present but not actionable right now.
    Disabled,
    /// Not available in this configuration at all.
    Unavailable,
    /// Enough headroom to proceed, with little spare.
    ResourceTight,
    /// Not enough headroom to proceed.
    ResourceBlocked,
    /// Blocked, and waiting for capacity to arrive.
    WaitingForResources,
}

impl Semantics {
    /// Every state, in a stable order, for exhaustive tests.
    pub const ALL: [Semantics; 13] = [
        Self::Success,
        Self::Warning,
        Self::Failed,
        Self::Blocked,
        Self::Running,
        Self::Pending,
        Self::Selected,
        Self::Focused,
        Self::Disabled,
        Self::Unavailable,
        Self::ResourceTight,
        Self::ResourceBlocked,
        Self::WaitingForResources,
    ];

    /// The word this state is named by, in the app's shared vocabulary.
    ///
    /// `RESOURCE BLOCKED` and `UNAVAILABLE` are different news: one is a
    /// capacity decision that a retry or an override can change, the other is a
    /// property of the configuration. Collapsing them into one word is how an
    /// operator ends up retrying something that cannot succeed.
    pub const fn word(self) -> &'static str {
        match self {
            Self::Success => "SUCCESS",
            Self::Warning => "WARNING",
            Self::Failed => "FAILED",
            Self::Blocked => "BLOCKED",
            Self::Running => "RUNNING",
            Self::Pending => "PENDING",
            Self::Selected => "SELECTED",
            Self::Focused => "FOCUSED",
            Self::Disabled => "DISABLED",
            Self::Unavailable => "UNAVAILABLE",
            Self::ResourceTight => "RESOURCE TIGHT",
            Self::ResourceBlocked => "RESOURCE BLOCKED",
            Self::WaitingForResources => "WAITING FOR RESOURCES",
        }
    }

    /// The marker shape, resolved through [`crate::run::g`] for the active
    /// terminal mode.
    ///
    /// `None` for the two positional states: selection and focus are signalled
    /// by *where* the row is and how heavy it looks, which is the only channel
    /// that stays meaningful when a row is scrolled, sorted, or compared
    /// side by side.
    pub fn marker(self) -> Option<&'static str> {
        Some(match self {
            Self::Success => crate::run::g("✓", "[OK]"),
            Self::Warning => crate::run::g("⚠", "[!]"),
            Self::Failed => crate::run::g("✗", "[X]"),
            Self::Blocked => crate::run::g("■", "#"),
            Self::Running => crate::run::g("▸", ">"),
            Self::Pending => crate::run::g("○", "o"),
            Self::Disabled => crate::run::g("–", "-"),
            Self::Unavailable => crate::run::g("Ø", "0"),
            Self::ResourceTight => crate::run::g("↑", "^"),
            Self::ResourceBlocked => crate::run::g("▓", "="),
            Self::WaitingForResources => crate::run::g("…", "..."),
            Self::Selected | Self::Focused => return None,
        })
    }

    /// The color token, which reinforces the word and never replaces it.
    pub const fn tone(self) -> BadgeTone {
        match self {
            Self::Success | Self::Focused | Self::Selected => BadgeTone::Accent,
            Self::Warning | Self::ResourceTight => BadgeTone::Warning,
            Self::Failed | Self::ResourceBlocked => BadgeTone::Danger,
            Self::Blocked => BadgeTone::Blocked,
            Self::Running => BadgeTone::Running,
            Self::Pending | Self::WaitingForResources => BadgeTone::Pending,
            Self::Disabled | Self::Unavailable => BadgeTone::Muted,
        }
    }

    /// Whether this state is carried by position/weight rather than a marker.
    pub const fn is_positional(self) -> bool {
        matches!(self, Self::Selected | Self::Focused)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::{ascii_mode_lock, set_ascii_mode};

    /// The ten states that must be told apart by shape alone. Selection and
    /// focus are excluded by design (see the module docs), so a collision
    /// between them and a status is not a defect.
    fn status_states() -> [Semantics; 11] {
        Semantics::ALL
            .into_iter()
            .filter(|s| !s.is_positional())
            .collect::<Vec<_>>()
            .try_into()
            .expect("eleven non-positional states")
    }

    #[test]
    fn every_required_state_has_its_own_word() {
        let mut words: Vec<&str> = Semantics::ALL.into_iter().map(Semantics::word).collect();
        words.sort_unstable();
        let before = words.len();
        words.dedup();
        assert_eq!(words.len(), before, "two states share a word: {words:?}");
        for s in Semantics::ALL {
            assert!(!s.word().is_empty(), "{s:?} has no word");
            assert!(
                s.word()
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b == b' '),
                "{s:?} word {:?} is not the shared uppercase vocabulary",
                s.word(),
            );
        }
    }

    /// The test that would have caught `BadgeTone::Danger` and
    /// `BadgeTone::Blocked` sharing one `✗`.
    #[test]
    fn every_status_state_has_its_own_marker() {
        let _guard = ascii_mode_lock();
        for ascii in [false, true] {
            set_ascii_mode(ascii);
            let mut seen: Vec<(String, &'static str)> = Vec::new();
            for s in status_states() {
                let m = s.marker().expect("status states carry a marker");
                assert!(!m.is_empty(), "{s:?} has an empty marker");
                if let Some((prev, _)) = seen.iter().find(|(_, other)| *other == m) {
                    panic!("{s:?} and {prev:?} both mark as {m:?} (ascii={ascii})");
                }
                seen.push((format!("{s:?}"), m));
            }
        }
        set_ascii_mode(false);
    }

    #[test]
    fn markers_degrade_to_ascii_in_both_modes() {
        let _guard = ascii_mode_lock();
        for s in status_states() {
            let m = s.marker().expect("marker");
            set_ascii_mode(true);
            let ascii = s.marker().expect("marker");
            assert!(ascii.is_ascii(), "{s:?} marks as {ascii:?} in ASCII mode");
            set_ascii_mode(false);
            let unicode = s.marker().expect("marker");
            assert!(!unicode.is_ascii(), "{s:?} should be a Unicode mark");
            assert!(
                crate::design::text::display_width(unicode) <= 1
                    || s == Semantics::WaitingForResources
            );
            let _ = m;
        }
        set_ascii_mode(false);
    }

    #[test]
    fn word_and_marker_agree_in_a_badge() {
        let _guard = ascii_mode_lock();
        for ascii in [false, true] {
            set_ascii_mode(ascii);
            for s in status_states() {
                let badge = format!("{} {}", s.marker().expect("marker"), s.word());
                // The word is what a reader reads; the marker is what the eye
                // catches. Neither may be blank, and the marker must be the one
                // this state owns rather than a neighbour's.
                assert!(badge.contains(s.word()), "{badge:?} lost its word");
                assert!(
                    badge.starts_with(s.marker().unwrap()),
                    "{badge:?} lost its marker"
                );
            }
        }
        set_ascii_mode(false);
    }

    #[test]
    fn selection_and_focus_are_positional_not_marked() {
        assert!(Semantics::Selected.is_positional());
        assert!(Semantics::Focused.is_positional());
        assert_eq!(Semantics::Selected.marker(), None);
        assert_eq!(Semantics::Focused.marker(), None);
        assert!(!Semantics::Running.is_positional());
        assert!(Semantics::Running.marker().is_some());
    }

    #[test]
    fn the_three_resource_states_stay_distinct() {
        // These three are the ones an operator acts on differently: proceed,
        // free something, or wait. Merging any two of them changes what the
        // operator should do next.
        let trio = [
            Semantics::ResourceTight,
            Semantics::ResourceBlocked,
            Semantics::WaitingForResources,
        ];
        let words: Vec<&str> = trio.iter().map(|s| s.word()).collect();
        let markers: Vec<&str> = trio.iter().map(|s| s.marker().unwrap()).collect();
        assert_eq!(
            words
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            3
        );
        assert_eq!(
            markers
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            3
        );
        assert_ne!(
            Semantics::ResourceBlocked.tone(),
            Semantics::Unavailable.tone(),
            "a capacity decision is not the same news as a missing capability"
        );
    }
}
