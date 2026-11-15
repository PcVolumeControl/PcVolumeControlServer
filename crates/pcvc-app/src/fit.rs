//! Shortening single-line text to a width with an ellipsis. iced 0.14 text
//! widgets only clip or wrap, so labels are measured with the renderer's
//! text engine and shortened before they are built.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;

use iced::advanced::graphics::text::Paragraph;
use iced::advanced::text::{self, Paragraph as _};
use iced::{Font, Size, alignment};
use unicode_segmentation::UnicodeSegmentation;

const ELLIPSIS: char = '\u{2026}';

/// Labels whose fit is remembered per thread; the same labels are laid out
/// on every frame.
const CACHE_CAPACITY: usize = 256;

/// Fitted labels by content, then by (size, width) bits. `None` means the
/// content fits as it is.
type FitCache = HashMap<String, Vec<((u32, u32), Option<String>)>>;

thread_local! {
    static FITTED: RefCell<FitCache> = RefCell::new(HashMap::new());
}

/// Width of `content` on one line at `size`. Measured in the default font,
/// which is what the widgets draw with: the app sets no other.
pub fn measure(content: &str, size: f32) -> f32 {
    Paragraph::with_text(text::Text {
        content,
        bounds: Size::INFINITE,
        size: size.into(),
        line_height: text::LineHeight::default(),
        font: Font::default(),
        align_x: text::Alignment::Default,
        align_y: alignment::Vertical::Top,
        shaping: text::Shaping::default(),
        wrapping: text::Wrapping::None,
    })
    .min_width()
}

/// `content` shortened with an ellipsis to fit `max_width` at `size`.
pub fn fit(content: &str, size: f32, max_width: f32) -> Cow<'_, str> {
    let key = (size.to_bits(), max_width.to_bits());
    let cached = FITTED.with(|cache| {
        let cache = cache.borrow();
        let (_, fitted) = cache.get(content)?.iter().find(|(k, _)| *k == key)?;
        Some(fitted.clone())
    });
    let fitted = cached.unwrap_or_else(|| {
        let fitted = match ellipsize(content, max_width, |s| measure(s, size)) {
            Cow::Borrowed(_) => None,
            Cow::Owned(short) => Some(short),
        };
        FITTED.with(|cache| {
            let mut cache = cache.borrow_mut();
            if cache.len() >= CACHE_CAPACITY {
                cache.clear();
            }
            cache
                .entry(content.to_owned())
                .or_default()
                .push((key, fitted.clone()));
        });
        fitted
    });
    fitted.map_or(Cow::Borrowed(content), Cow::Owned)
}

/// The longest prefix of `content`, in whole grapheme clusters, that fits
/// `max_width` with an ellipsis appended, or `content` itself if it fits.
/// Trailing spaces before the ellipsis are dropped.
pub fn ellipsize<'s>(
    content: &'s str,
    max_width: f32,
    measure: impl Fn(&str) -> f32,
) -> Cow<'s, str> {
    if measure(content) <= max_width {
        return Cow::Borrowed(content);
    }
    let cuts: Vec<usize> = content
        .grapheme_indices(true)
        .map(|(i, _)| i)
        .skip(1)
        .collect();
    let shortened = |cut: usize| format!("{}{ELLIPSIS}", content[..cut].trim_end());
    // The number of cuts that fit, found by binary search: shorter prefixes
    // are never wider.
    let (mut lo, mut hi) = (0, cuts.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if measure(&shortened(cuts[mid - 1])) <= max_width {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    Cow::Owned(match lo {
        0 => ELLIPSIS.to_string(),
        n => shortened(cuts[n - 1]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ten units per character.
    fn width(s: &str) -> f32 {
        s.chars().count() as f32 * 10.0
    }

    #[test]
    fn ellipsizes_to_fit() {
        assert_eq!(ellipsize("Firefox", 70.0, width), "Firefox");
        assert!(matches!(
            ellipsize("Firefox", 70.0, width),
            Cow::Borrowed(_)
        ));
        assert_eq!(ellipsize("Firefox", 69.0, width), "Firef\u{2026}");
        assert_eq!(ellipsize("Firefox", 20.0, width), "F\u{2026}");
        assert_eq!(ellipsize("Firefox", 5.0, width), "\u{2026}");
        assert_eq!(
            ellipsize("speech dispatcher", 80.0, width),
            "speech\u{2026}"
        );
        assert_eq!(
            ellipsize("\u{e9}t\u{e9} long", 30.0, width),
            "\u{e9}t\u{2026}"
        );
        assert_eq!(ellipsize("", 0.0, width), "");
    }

    #[test]
    fn never_splits_grapheme_clusters() {
        // "e" plus a combining acute accent is one cluster of two chars.
        let accented = "Cafe\u{301} Radio";
        assert_eq!(ellipsize(accented, 50.0, width), "Caf\u{2026}");
        assert_eq!(ellipsize(accented, 60.0, width), "Cafe\u{301}\u{2026}");
        // A flag is two regional indicators.
        let flag = "\u{1f1e9}\u{1f1ea} Radio";
        assert_eq!(ellipsize(flag, 30.0, width), "\u{1f1e9}\u{1f1ea}\u{2026}");
        assert_eq!(ellipsize(flag, 25.0, width), "\u{2026}");
    }

    #[test]
    fn fits_real_text() {
        let short = measure("Firefox", 16.0);
        assert!(short > 0.0);
        assert!(measure("Firefox Firefox", 16.0) > short);
        assert!(measure("Firefox", 24.0) > short);
        let long = "A very long application name that does not fit";
        let fitted = fit(long, 16.0, 120.0);
        assert!(fitted.ends_with(ELLIPSIS));
        assert!(measure(&fitted, 16.0) <= 120.0);
        assert_eq!(fit(long, 16.0, 120.0), fitted, "cached alike");
        assert!(matches!(fit("Firefox", 16.0, 500.0), Cow::Borrowed(_)));
        assert!(matches!(fit("Firefox", 16.0, 500.0), Cow::Borrowed(_)));
    }
}
