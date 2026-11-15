//! Shortening single-line text to a width with an ellipsis. iced 0.14 text
//! widgets only clip or wrap, so labels are measured with the renderer's
//! text engine and shortened before they are built.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;

use iced::advanced::graphics::text::Paragraph;
use iced::advanced::text::{self, Paragraph as _};
use iced::{Font, Size, alignment};

const ELLIPSIS: char = '\u{2026}';

/// Measurements remembered per thread; the same labels are laid out on
/// every frame.
const CACHE_CAPACITY: usize = 512;

thread_local! {
    static WIDTHS: RefCell<HashMap<(String, u32), f32>> = RefCell::new(HashMap::new());
}

/// Width of `content` on one line at `size` in the default font.
pub fn measure(content: &str, size: f32) -> f32 {
    let key = (content.to_owned(), size.to_bits());
    if let Some(width) = WIDTHS.with(|w| w.borrow().get(&key).copied()) {
        return width;
    }
    let width = Paragraph::with_text(text::Text {
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
    .min_width();
    WIDTHS.with(|w| {
        let mut w = w.borrow_mut();
        if w.len() >= CACHE_CAPACITY {
            w.clear();
        }
        w.insert(key, width);
    });
    width
}

/// `content` shortened with an ellipsis to fit `max_width` at `size`.
pub fn fit(content: &str, size: f32, max_width: f32) -> Cow<'_, str> {
    ellipsize(content, max_width, |s| measure(s, size))
}

/// The longest prefix of `content` that fits `max_width` with an ellipsis
/// appended, or `content` itself if it fits. Trailing spaces before the
/// ellipsis are dropped.
pub fn ellipsize<'s>(
    content: &'s str,
    max_width: f32,
    measure: impl Fn(&str) -> f32,
) -> Cow<'s, str> {
    if measure(content) <= max_width {
        return Cow::Borrowed(content);
    }
    let cuts: Vec<usize> = content.char_indices().map(|(i, _)| i).skip(1).collect();
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
    fn measures_real_text() {
        let short = measure("Firefox", 16.0);
        assert!(short > 0.0);
        assert!(measure("Firefox Firefox", 16.0) > short);
        assert!(measure("Firefox", 24.0) > short);
        let long = "A very long application name that does not fit";
        let fitted = fit(long, 16.0, 120.0);
        assert!(fitted.ends_with(ELLIPSIS));
        assert!(measure(&fitted, 16.0) <= 120.0);
        assert_eq!(fit("Firefox", 16.0, 500.0), "Firefox");
    }
}
