//! Embedded SVG icons and their rasterisation.

use resvg::{tiny_skia, usvg};

pub const APP: &[u8] = include_bytes!("../assets/app.svg");
pub const TRAY_OFFLINE: &[u8] = include_bytes!("../assets/tray-offline.svg");
pub const TRAY_ONLINE: &[u8] = include_bytes!("../assets/tray-online.svg");
pub const TRAY_CLIENTS: &[u8] = include_bytes!("../assets/tray-clients.svg");

/// Straight (not premultiplied) RGBA pixels.
pub struct Rgba {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Renders an SVG into a `size` x `size` RGBA image.
pub fn rasterize(svg: &[u8], size: u32) -> Result<Rgba, String> {
    render(svg, size).map(to_rgba)
}

/// Renders an SVG (or SVGZ) centred into a `size` x `size` pixmap, keeping
/// its aspect ratio.
pub fn render(svg: &[u8], size: u32) -> Result<tiny_skia::Pixmap, String> {
    let tree = usvg::Tree::from_data(svg, &usvg::Options::default()).map_err(|e| e.to_string())?;
    let mut pixmap =
        tiny_skia::Pixmap::new(size, size).ok_or_else(|| format!("invalid icon size {size}"))?;
    let natural = tree.size();
    let (scale, dx, dy) = fit(natural.width(), natural.height(), size);
    let transform = tiny_skia::Transform::from_translate(dx, dy).pre_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Ok(pixmap)
}

/// Scale and offset that fit a `w` x `h` image centred in a `size` square.
pub fn fit(w: f32, h: f32, size: u32) -> (f32, f32, f32) {
    let size = size as f32;
    let scale = size / w.max(h).max(f32::EPSILON);
    (scale, (size - w * scale) / 2.0, (size - h * scale) / 2.0)
}

/// Straight-alpha pixels of a premultiplied pixmap.
pub fn to_rgba(pixmap: tiny_skia::Pixmap) -> Rgba {
    let (width, height) = (pixmap.width(), pixmap.height());
    let pixels = pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect();
    Rgba {
        pixels,
        width,
        height,
    }
}

/// The window icon, or `None` if it cannot be rendered.
pub fn window_icon() -> Option<iced::window::Icon> {
    let rgba = rasterize(APP, 128).ok()?;
    iced::window::icon::from_rgba(rgba.pixels, rgba.width, rgba.height).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_icons_render() {
        for svg in [APP, TRAY_OFFLINE, TRAY_ONLINE, TRAY_CLIENTS] {
            let icon = rasterize(svg, 32).unwrap();
            assert_eq!(icon.pixels.len(), 32 * 32 * 4);
            assert!(icon.pixels.chunks(4).any(|p| p[3] == 255));
        }
    }

    #[test]
    fn packaged_icon_matches_app_icon() {
        let packaged = include_bytes!("../../../packaging/linux/pcvolumecontrol.svg");
        assert_eq!(packaged.as_slice(), APP);
    }

    /// Sizes in assets/app.ico, the Windows executable and installer icon.
    const ICO_SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];

    /// The (size, PNG data) frames of an ICO whose images are all PNG.
    fn ico_frames(ico: &[u8]) -> Vec<(u32, &[u8])> {
        let u16_at = |i: usize| u16::from_le_bytes([ico[i], ico[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes(ico[i..i + 4].try_into().unwrap()) as usize;
        assert_eq!((u16_at(0), u16_at(2)), (0, 1), "not an ICO file");
        (0..usize::from(u16_at(4)))
            .map(|i| {
                let entry = 6 + 16 * i;
                // A width byte of 0 means 256.
                let size = match ico[entry] {
                    0 => 256,
                    w => u32::from(w),
                };
                let (len, offset) = (u32_at(entry + 8), u32_at(entry + 12));
                (size, &ico[offset..offset + len])
            })
            .collect()
    }

    #[test]
    fn windows_icon_matches_app_icon() {
        let frames = ico_frames(include_bytes!("../assets/app.ico"));
        let sizes: Vec<u32> = frames.iter().map(|(size, _)| *size).collect();
        assert_eq!(sizes, ICO_SIZES);
        for (size, png) in frames {
            let decoded = tiny_skia::Pixmap::decode_png(png).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (size, size));
            let expected = render(APP, size).unwrap();
            let diff: u64 = decoded
                .data()
                .iter()
                .zip(expected.data())
                .map(|(a, b)| u64::from(a.abs_diff(*b)))
                .sum();
            // Small differences come from PNG's straight alpha round trip and
            // renderer updates; changed artwork differs far more.
            let mean = diff as f64 / expected.data().len() as f64;
            assert!(
                mean < 2.0,
                "{size}px frame of assets/app.ico differs from assets/app.svg \
                 (mean {mean:.2}); regenerate it with \
                 `cargo test -p pcvolumecontrol write_windows_icon -- --ignored`"
            );
        }
    }

    /// Regenerates assets/app.ico from assets/app.svg.
    #[test]
    #[ignore = "writes assets/app.ico"]
    fn write_windows_icon() {
        let pngs: Vec<Vec<u8>> = ICO_SIZES
            .iter()
            .map(|&size| render(APP, size).unwrap().encode_png().unwrap())
            .collect();
        let count = u16::try_from(pngs.len()).unwrap();
        let mut ico = Vec::new();
        ico.extend_from_slice(&[0, 0, 1, 0]);
        ico.extend_from_slice(&count.to_le_bytes());
        let mut offset = 6 + 16 * pngs.len();
        for (&size, png) in ICO_SIZES.iter().zip(&pngs) {
            let dim = if size == 256 { 0 } else { size as u8 };
            // Width, height, palette size, reserved, colour planes, bits per pixel.
            ico.extend_from_slice(&[dim, dim, 0, 0, 1, 0, 32, 0]);
            ico.extend_from_slice(&u32::try_from(png.len()).unwrap().to_le_bytes());
            ico.extend_from_slice(&u32::try_from(offset).unwrap().to_le_bytes());
            offset += png.len();
        }
        for png in &pngs {
            ico.extend_from_slice(png);
        }
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/app.ico");
        std::fs::write(path, ico).unwrap();
    }
}
