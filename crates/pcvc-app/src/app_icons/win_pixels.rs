//! Pixel conversion for Windows icons, kept free of Win32 calls so it is
//! tested on every platform.

/// Straight RGBA from the top-down BGRA rows `GetDIBits` returns for an
/// icon's colour bitmap. An icon with an alpha channel uses it; an older
/// one has zero alpha throughout and takes its transparency from the AND
/// mask, read the same way, where a set (white) pixel is transparent.
/// Without a mask such an icon is treated as opaque.
pub fn icon_pixels(bgra: &[u8], mask: Option<&[u8]>) -> Vec<u8> {
    let has_alpha = has_alpha(bgra);
    bgra.as_chunks::<4>()
        .0
        .iter()
        .enumerate()
        .flat_map(|(i, p)| {
            let alpha = if has_alpha {
                p[3]
            } else {
                match mask.and_then(|m| m.get(i * 4..i * 4 + 3)) {
                    Some(m) if m.iter().any(|&c| c != 0) => 0,
                    _ => 255,
                }
            };
            [p[2], p[1], p[0], alpha]
        })
        .collect()
}

/// Whether the alpha channel of `bgra` is used at all, which decides
/// whether the mask needs reading.
pub fn has_alpha(bgra: &[u8]) -> bool {
    bgra.as_chunks::<4>().0.iter().any(|p| p[3] != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpha_icons_keep_their_alpha() {
        let bgra = [10, 20, 30, 255, 1, 2, 3, 128, 0, 0, 0, 0];
        assert!(has_alpha(&bgra));
        // The mask is ignored when there is alpha.
        let mask = [255; 12];
        assert_eq!(
            icon_pixels(&bgra, Some(&mask)),
            vec![30, 20, 10, 255, 3, 2, 1, 128, 0, 0, 0, 0]
        );
    }

    #[test]
    fn mask_gives_transparency_without_alpha() {
        let bgra = [10, 20, 30, 0, 40, 50, 60, 0];
        assert!(!has_alpha(&bgra));
        let mask = [0, 0, 0, 0, 255, 255, 255, 0];
        assert_eq!(
            icon_pixels(&bgra, Some(&mask)),
            vec![30, 20, 10, 255, 60, 50, 40, 0]
        );
        assert_eq!(
            icon_pixels(&bgra, None),
            vec![30, 20, 10, 255, 60, 50, 40, 255]
        );
        // A short mask leaves the rest opaque.
        assert_eq!(icon_pixels(&bgra, Some(&mask[..4]))[7], 255);
    }
}
