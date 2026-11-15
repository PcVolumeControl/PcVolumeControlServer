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
    let tree = usvg::Tree::from_data(svg, &usvg::Options::default()).map_err(|e| e.to_string())?;
    let mut pixmap =
        tiny_skia::Pixmap::new(size, size).ok_or_else(|| format!("invalid icon size {size}"))?;
    let natural = tree.size();
    let transform = tiny_skia::Transform::from_scale(
        size as f32 / natural.width(),
        size as f32 / natural.height(),
    );
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    let pixels = pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect();
    Ok(Rgba {
        pixels,
        width: size,
        height: size,
    })
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
}
