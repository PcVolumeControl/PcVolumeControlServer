//! The tile shown when a session has no icon: the first letter of its name
//! on a colour picked from the name, so it stays the same across runs.

use iced::Theme;
use iced::theme::palette::Pair;

/// The first letter or digit of `name`, upper-cased; `?` if there is none.
pub fn initial(name: &str) -> String {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map_or_else(|| "?".to_owned(), |c| c.to_uppercase().collect())
}

/// FNV-1a over the lower-cased name: stable across runs and platforms,
/// unlike the standard library's hasher.
fn hash(name: &str) -> u32 {
    name.to_lowercase().bytes().fold(0x811c_9dc5, |h: u32, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    })
}

/// How many colours [`colors`] picks from.
pub const COLOR_COUNT: usize = 5;

/// Which of the [`COLOR_COUNT`] colours `name` gets.
pub fn color_index(name: &str) -> usize {
    hash(name) as usize % COLOR_COUNT
}

/// Background and letter colours number `index` (see [`color_index`]) from
/// the theme's palette; the letter colour is the palette's readable text
/// colour for that background, in light and dark themes alike.
pub fn colors(theme: &Theme, index: usize) -> Pair {
    let p = theme.extended_palette();
    [
        p.primary.weak,
        p.success.weak,
        p.warning.weak,
        p.danger.weak,
        p.secondary.base,
    ][index % COLOR_COUNT]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials() {
        assert_eq!(initial("Firefox"), "F");
        assert_eq!(initial("  speech-dispatcher"), "S");
        assert_eq!(initial("\u{e9}t\u{e9}"), "\u{c9}");
        assert_eq!(initial("7 Days to Die"), "7");
        assert_eq!(initial("stra\u{df}e"), "S");
        assert_eq!(initial("\u{df}"), "SS");
        assert_eq!(initial("--"), "?");
        assert_eq!(initial(""), "?");
    }

    #[test]
    fn colours_are_deterministic() {
        // Fixed values: a change here changes every user's tiles.
        assert_eq!(hash(""), 0x811c_9dc5);
        assert_eq!(hash("a"), 0xe40c_292c);
        assert_eq!(color_index("Firefox"), color_index("firefox"));
        let names = [
            "Firefox", "Spotify", "Discord", "VLC", "Steam", "mpv", "Zoom",
        ];
        let indices: Vec<usize> = names.iter().map(|n| color_index(n)).collect();
        assert_eq!(
            indices,
            names.iter().map(|n| color_index(n)).collect::<Vec<_>>()
        );
        assert!(indices.iter().all(|&i| i < COLOR_COUNT));
        // The names spread over more than one colour.
        assert!(indices.iter().any(|&i| i != indices[0]));
        for theme in [Theme::Light, Theme::Dark] {
            for index in 0..COLOR_COUNT {
                let pair = colors(&theme, index);
                assert_ne!(pair.color, pair.text);
            }
        }
    }
}
