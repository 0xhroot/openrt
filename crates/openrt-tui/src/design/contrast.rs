//! Measured contrast: WCAG 2.x relative luminance and contrast ratio.
//!
//! # What this is and is not
//!
//! This module *computes* contrast ratios from the palette's literal sRGB
//! values, using the WCAG 2.x definition (`(L1 + 0.05) / (L2 + 0.05)` over
//! relative luminance from linearized sRGB). It is a real measurement, so the
//! numbers in the theme report are reproducible and checkable rather than
//! asserted.
//!
//! What it is **not**:
//!
//! * It does not verify WCAG conformance. Conformance is about the whole page,
//!   including non-text indicators and assistive technology; this crate
//!   renders a terminal grid, not a document.
//! * It cannot see how a given terminal renders a given color. Terminal themes
//!   remap, dim, and clamp colors, so a palette ratio is an upper bound on what
//!   an operator actually sees, and on a 16-color or low-color-depth terminal
//!   the number is fiction. That is why `HighContrast` and `Monochrome` are
//!   specified by *separation between tokens* and by non-color channels, not by
//!   ratio alone.
//!
//! Measured pairs are still the best evidence available headlessly, and they
//! catch the failure that actually happens: a "subtle" text token quietly
//! drifting down until body copy stops being readable.

use ratatui::style::Color;

/// Resolve a palette slot to sRGB, or `None` for a computed/default color.
///
/// A `Color::Rgb` is the only case that can be measured. Indexed and reset
/// colors are whatever the operator's terminal decides, so a ratio computed
/// from them would be a guess wearing a decimal point.
pub fn rgb(c: Color) -> Option<(f64, f64, f64)> {
    match c {
        Color::Rgb(r, g, b) => Some((
            f64::from(r) / 255.0,
            f64::from(g) / 255.0,
            f64::from(b) / 255.0,
        )),
        _ => None,
    }
}

/// Relative luminance of one linearized sRGB channel.
///
/// The sRGB transfer function is not linear; applying it is the entire reason
/// this measurement is worth computing. Skipping it inflates mid-tones and
/// makes a poor choice look acceptable.
fn channel(c: f64) -> f64 {
    if c <= 0.03928 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// WCAG relative luminance of an sRGB triple in 0.0..=1.0.
pub fn luminance(r: f64, g: f64, b: f64) -> f64 {
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

/// Contrast ratio between two colors, 1.0..=21.0.
///
/// `None` when either color is not measurable sRGB.
pub fn ratio(fg: Color, bg: Color) -> Option<f64> {
    let (fr, fg_, fb) = rgb(fg)?;
    let (br, bg_, bb) = rgb(bg)?;
    let l1 = luminance(fr, fg_, fb);
    let l2 = luminance(br, bg_, bb);
    let (hi, lo) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
    Some((hi + 0.05) / (lo + 0.05))
}

/// The ratio between two colors, or 0.0 when it cannot be measured.
///
/// For assertions where an unmeasurable color should not silently pass, use
/// [`ratio`] directly: a `0.0` here means "unknown", and an invariant that
/// accepts unknown is not an invariant.
pub fn ratio_or_zero(fg: Color, bg: Color) -> f64 {
    ratio(fg, bg).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Anchors from the WCAG 2.x definition, so a refactor cannot quietly
    /// change what "ratio" means.
    #[test]
    fn the_reference_values_are_exact() {
        let lum = |hex: (u8, u8, u8)| {
            let (r, g, b) = rgb(Color::Rgb(hex.0, hex.1, hex.2)).unwrap();
            luminance(r, g, b)
        };
        // Black is 0, white is 1.
        assert!((lum((0, 0, 0)) - 0.0).abs() < 1e-9);
        assert!((lum((255, 255, 255)) - 1.0).abs() < 1e-9);
        // White on black is the maximum: 21:1.
        let r = ratio(Color::Rgb(255, 255, 255), Color::Rgb(0, 0, 0)).unwrap();
        assert!((r - 21.0).abs() < 1e-6, "white/black = {r}");
        // A color is identical to itself at exactly 1:1.
        assert!(
            (ratio(Color::Rgb(18, 52, 86), Color::Rgb(18, 52, 86)).unwrap() - 1.0).abs() < 1e-9
        );
    }

    #[test]
    fn ratio_is_symmetric_and_never_below_one() {
        let a = Color::Rgb(18, 52, 86);
        let b = Color::Rgb(224, 224, 232);
        let ab = ratio(a, b).unwrap();
        let ba = ratio(b, a).unwrap();
        assert!(
            (ab - ba).abs() < 1e-9,
            "ratio is order-dependent: {ab} vs {ba}"
        );
        assert!(ab >= 1.0);
    }

    #[test]
    fn an_unmeasurable_color_reports_none_rather_than_a_guess() {
        // Indexed colors are the operator's terminal's business.
        assert!(ratio(Color::White, Color::Reset).is_none());
        assert_eq!(ratio_or_zero(Color::White, Color::Reset), 0.0);
    }

    #[test]
    fn the_measurement_rewards_darkening_rather_than_guessing() {
        // The transfer function matters: naive gamma-free averaging calls these
        // two nearly identical, the real measurement separates them.
        let faint = ratio(Color::Rgb(120, 122, 128), Color::Rgb(0, 0, 0)).unwrap();
        let clear = ratio(Color::Rgb(200, 202, 208), Color::Rgb(0, 0, 0)).unwrap();
        assert!(clear > faint * 1.5, "{clear} should clearly exceed {faint}");
    }
}
