//! Turns an icon file into a square RGBA image of a given size.

use std::path::Path;

use resvg::tiny_skia::{FilterQuality, IntSize, Pixmap, PixmapPaint, Transform};

use crate::icons::{Rgba, fit, to_rgba};

/// Larger files are not icons worth reading.
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Larger PNGs are refused before decoding.
const MAX_PNG_EDGE: u32 = 2048;

/// Reads `path` (PNG, SVG or SVGZ, by extension) and renders it centred into
/// a `size` x `size` image, keeping its aspect ratio. Other formats and
/// unreadable files give `None`.
pub fn load(path: &Path, size: u32) -> Option<Rgba> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if !matches!(ext.as_str(), "png" | "svg" | "svgz") {
        tracing::debug!(path = %path.display(), "skipping unsupported icon format");
        return None;
    }
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let data = std::fs::read(path).ok()?;
    let pixmap = match ext.as_str() {
        "png" => png(&data, size),
        _ => crate::icons::render(&data, size).ok(),
    };
    if pixmap.is_none() {
        tracing::debug!(path = %path.display(), "icon file could not be decoded");
    }
    pixmap.map(to_rgba)
}

pub fn png(data: &[u8], size: u32) -> Option<Pixmap> {
    let (w, h) = png_dimensions(data)?;
    if w == 0 || h == 0 || w > MAX_PNG_EDGE || h > MAX_PNG_EDGE {
        return None;
    }
    let source = Pixmap::decode_png(data).ok()?;
    fit_pixmap(source, size)
}

/// Draws `source` centred into a `size` square, keeping its aspect ratio.
pub fn fit_pixmap(mut source: Pixmap, size: u32) -> Option<Pixmap> {
    // Bicubic sampling skips source pixels when shrinking by more than half,
    // so large images are halved first. The longer side sets the scale.
    while source.width().max(source.height()) / 2 >= size {
        source = scaled(&source, 0.5, FilterQuality::Bilinear)?;
    }
    let mut out = Pixmap::new(size, size)?;
    let (scale, dx, dy) = fit(source.width() as f32, source.height() as f32, size);
    let paint = PixmapPaint {
        quality: FilterQuality::Bicubic,
        ..PixmapPaint::default()
    };
    out.draw_pixmap(
        0,
        0,
        source.as_ref(),
        &paint,
        Transform::from_translate(dx, dy).pre_scale(scale, scale),
        None,
    );
    Some(out)
}

/// Width and height from a PNG's IHDR chunk.
fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    if !data.starts_with(SIGNATURE) || data.get(12..16)? != b"IHDR" {
        return None;
    }
    let be = |at: usize| Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?));
    Some((be(16)?, be(20)?))
}

fn scaled(source: &Pixmap, factor: f32, quality: FilterQuality) -> Option<Pixmap> {
    let w = ((source.width() as f32 * factor).round() as u32).max(1);
    let h = ((source.height() as f32 * factor).round() as u32).max(1);
    let mut out = Pixmap::new(w, h)?;
    let paint = PixmapPaint {
        quality,
        ..PixmapPaint::default()
    };
    out.draw_pixmap(
        0,
        0,
        source.as_ref(),
        &paint,
        Transform::from_scale(
            w as f32 / source.width() as f32,
            h as f32 / source.height() as f32,
        ),
        None,
    );
    Some(out)
}

/// PNG encoding of an image, the form in which icons can leave the process.
#[cfg_attr(not(test), expect(dead_code, reason = "no consumer outside tests"))]
pub fn encode_png(rgba: &Rgba) -> Option<Vec<u8>> {
    Pixmap::from_vec(
        premultiplied(&rgba.pixels),
        IntSize::from_wh(rgba.width, rgba.height)?,
    )?
    .encode_png()
    .ok()
}

fn premultiplied(pixels: &[u8]) -> Vec<u8> {
    pixels
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| {
            let a = u16::from(p[3]);
            let m = |c: u8| ((u16::from(c) * a + 127) / 255) as u8;
            [m(p[0]), m(p[1]), m(p[2]), p[3]]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use resvg::tiny_skia;

    use super::*;

    fn solid_png(w: u32, h: u32) -> Vec<u8> {
        let mut pixmap = Pixmap::new(w, h).unwrap();
        pixmap.fill(tiny_skia::Color::from_rgba8(255, 0, 255, 255));
        pixmap.encode_png().unwrap()
    }

    fn alpha_at(rgba: &Rgba, x: u32, y: u32) -> u8 {
        rgba.pixels[((y * rgba.width + x) * 4 + 3) as usize]
    }

    #[test]
    fn loads_and_fits_png() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wide.png");
        std::fs::write(&path, solid_png(512, 256)).unwrap();
        let icon = load(&path, 48).unwrap();
        assert_eq!((icon.width, icon.height), (48, 48));
        // Letterboxed: transparent above and below, opaque in the middle.
        assert_eq!(alpha_at(&icon, 24, 2), 0);
        assert_eq!(alpha_at(&icon, 24, 24), 255);
        assert_eq!(&icon.pixels[(24 * 48 + 24) * 4..][..3], &[255, 0, 255]);
    }

    #[test]
    fn loads_svg() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16" fill="#00f"/></svg>"##;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.SVG");
        std::fs::write(&path, svg).unwrap();
        let icon = load(&path, 32).unwrap();
        assert_eq!(icon.pixels.len(), 32 * 32 * 4);
        assert_eq!(alpha_at(&icon, 31, 31), 255);
    }

    #[test]
    fn rejects_unsupported_and_broken_files() {
        let dir = tempfile::tempdir().unwrap();
        for (name, data) in [
            ("a.xpm", b"/* XPM */".as_slice()),
            ("b.png", b"not a png".as_slice()),
            ("c.svg", b"<svg".as_slice()),
            ("noext", b"".as_slice()),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, data).unwrap();
            assert!(load(&path, 32).is_none(), "{name}");
        }
        assert!(load(&dir.path().join("missing.png"), 32).is_none());
        assert!(load(dir.path(), 32).is_none());
    }

    #[test]
    fn halves_by_the_longer_side() {
        let mut wide = Pixmap::new(1024, 128).unwrap();
        wide.fill(tiny_skia::Color::from_rgba8(0, 0, 0, 255));
        let icon = to_rgba(fit_pixmap(wide, 48).unwrap());
        assert_eq!((icon.width, icon.height), (48, 48));
        // 1024x128 fits as 48x6, centred vertically.
        assert_eq!(alpha_at(&icon, 24, 24), 255);
        assert_eq!(alpha_at(&icon, 24, 18), 0);
    }

    #[test]
    fn refuses_huge_png() {
        let mut data = solid_png(1, 1);
        data[16..20].copy_from_slice(&100_000u32.to_be_bytes());
        assert!(png(&data, 32).is_none());
    }

    #[test]
    fn png_round_trip() {
        let rgba = Rgba {
            pixels: vec![10, 20, 30, 255, 200, 100, 50, 128],
            width: 2,
            height: 1,
        };
        let encoded = encode_png(&rgba).unwrap();
        let decoded = to_rgba(Pixmap::decode_png(&encoded).unwrap());
        assert_eq!(decoded.pixels[..4], rgba.pixels[..4]);
        for (a, b) in decoded.pixels[4..].iter().zip(&rgba.pixels[4..]) {
            assert!(a.abs_diff(*b) <= 2);
        }
    }
}
