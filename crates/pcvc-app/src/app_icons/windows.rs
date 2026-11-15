//! The shell icon of a session's executable on Windows.
//!
//! The icon comes from the executable's own resources at the requested
//! size (`SHDefExtractIconW`), else from the icon file the shell names for
//! it at that size (the generic program icon for an executable without
//! one), else from the shell at the system large icon size
//! (`SHGetFileInfoW`). Packaged (UWP/MSIX) apps keep their logos in the
//! package rather than the executable, and their install folder is not
//! readable, so they get the generic icon or none.

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetDIBits, GetObjectW, HBITMAP, HDC,
};
use windows_sys::Win32::System::Com::{
    COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize,
};
use windows_sys::Win32::UI::Shell::{
    SHDefExtractIconW, SHFILEINFOW, SHGFI_ICON, SHGFI_ICONLOCATION, SHGFI_LARGEICON, SHGetFileInfoW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

use super::{Found, IconKey, Resolver, raster, win_pixels};
use crate::icons::Rgba;

/// Larger bitmaps are not icons.
const MAX_EDGE: i32 = 1024;

pub struct WindowsResolver;

impl Resolver for WindowsResolver {
    /// The shell picks the image in the file that best fits the requested
    /// size and scales it to that size, without saying what it started
    /// from, so only the [`Source::Shell`] fallback (fixed at the system
    /// icon size) reports a source edge.
    fn resolve_found(&self, key: &IconKey) -> Option<Found> {
        let Some(path) = key.process_binary.as_deref() else {
            tracing::debug!("no executable path for an application icon");
            return None;
        };
        COM.with(|_| ());
        let wide_path = wide(path);
        let found = [Source::Executable, Source::ShellLocation, Source::Shell]
            .into_iter()
            .find_map(|source| {
                let icon = source.extract(&wide_path, key.size)?;
                let rgba = icon_rgba(&icon)?;
                let source_edge = match source {
                    Source::Shell => Some(rgba.width.max(rgba.height)),
                    Source::Executable | Source::ShellLocation => None,
                };
                let found = Found {
                    source_edge,
                    rgba: raster::fit_rgba(&rgba, key.size)?,
                };
                Some((source, found))
            });
        tracing::debug!(
            path,
            source = ?found.as_ref().map(|(source, _)| source),
            size = key.size,
            "application icon lookup"
        );
        found.map(|(_, found)| found)
    }
}

/// Where an icon came from, tried in this order.
#[derive(Debug, Clone, Copy)]
enum Source {
    /// The executable's own icon resource, at the requested size.
    Executable,
    /// The file and index the shell names for the executable's icon (the
    /// generic program icon for one without its own), at the requested
    /// size.
    ShellLocation,
    /// The shell's icon at the system large icon size.
    Shell,
}

impl Source {
    fn extract(self, path: &[u16], size: u32) -> Option<OwnedIcon> {
        match self {
            Source::Executable => extract_at(path, 0, size),
            Source::ShellLocation => {
                let mut info = SHFILEINFOW::default();
                // SAFETY: `path` is nul-terminated, `info` is writable and
                // its size is passed.
                let ok = unsafe {
                    SHGetFileInfoW(
                        path.as_ptr(),
                        0,
                        &mut info,
                        size_of::<SHFILEINFOW>() as u32,
                        SHGFI_ICONLOCATION,
                    )
                };
                let len = info.szDisplayName.iter().position(|&c| c == 0)?;
                if ok == 0 || len == 0 {
                    return None;
                }
                extract_at(&info.szDisplayName[..=len], info.iIcon, size)
            }
            Source::Shell => {
                let mut info = SHFILEINFOW::default();
                // SAFETY: as above.
                let ok = unsafe {
                    SHGetFileInfoW(
                        path.as_ptr(),
                        0,
                        &mut info,
                        size_of::<SHFILEINFOW>() as u32,
                        SHGFI_ICON | SHGFI_LARGEICON,
                    )
                };
                (ok != 0 && !info.hIcon.is_null()).then_some(OwnedIcon(info.hIcon))
            }
        }
    }
}

/// Icon `index` (a negative index is a resource id) of `file`, a
/// nul-terminated path, at `size` pixels.
fn extract_at(file: &[u16], index: i32, size: u32) -> Option<OwnedIcon> {
    let mut large: HICON = null_mut();
    // SAFETY: `file` is nul-terminated and outlives the call; the small
    // icon is not requested. The low word of the last argument is the large
    // icon size.
    let hr = unsafe {
        SHDefExtractIconW(
            file.as_ptr(),
            index,
            0,
            &mut large,
            null_mut(),
            size.min(256),
        )
    };
    (hr == 0 && !large.is_null()).then_some(OwnedIcon(large))
}

/// COM initialised on the icon thread, as the shell functions require.
struct ComGuard(bool);

impl ComGuard {
    fn init() -> Self {
        // SAFETY: no pointer arguments; balanced by CoUninitialize in Drop
        // when it succeeds (S_OK or S_FALSE).
        // The icon thread does not pump messages, so it joins the
        // multithreaded apartment rather than creating a single-threaded one.
        let hr = unsafe {
            CoInitializeEx(
                null(),
                (COINIT_MULTITHREADED | COINIT_DISABLE_OLE1DDE) as u32,
            )
        };
        Self(hr >= 0)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: pairs the successful CoInitializeEx on this thread.
            unsafe { CoUninitialize() };
        }
    }
}

thread_local! {
    static COM: ComGuard = ComGuard::init();
}

struct OwnedIcon(HICON);

impl Drop for OwnedIcon {
    fn drop(&mut self) {
        // SAFETY: the icon was created for us by the shell and is destroyed
        // once.
        unsafe { DestroyIcon(self.0) };
    }
}

struct OwnedBitmap(HBITMAP);

impl Drop for OwnedBitmap {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: GetIconInfo created the bitmap for us; deleted once.
            unsafe { DeleteObject(self.0) };
        }
    }
}

struct OwnedDc(HDC);

impl Drop for OwnedDc {
    fn drop(&mut self) {
        // SAFETY: created by CreateCompatibleDC and deleted once.
        unsafe { DeleteDC(self.0) };
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The icon's pixels as straight RGBA.
fn icon_rgba(icon: &OwnedIcon) -> Option<Rgba> {
    let mut info = ICONINFO::default();
    // SAFETY: the icon is live and `info` is writable.
    if unsafe { GetIconInfo(icon.0, &mut info) } == 0 {
        return None;
    }
    let color = OwnedBitmap(info.hbmColor);
    let mask = OwnedBitmap(info.hbmMask);
    // Monochrome icons have no colour bitmap; they are not worth showing.
    if color.0.is_null() {
        return None;
    }
    let mut bitmap = BITMAP::default();
    // SAFETY: `bitmap` is writable for the size passed.
    let read = unsafe {
        GetObjectW(
            color.0,
            size_of::<BITMAP>() as i32,
            (&raw mut bitmap).cast::<c_void>(),
        )
    };
    let (width, height) = (bitmap.bmWidth, bitmap.bmHeight);
    if read == 0 || !(1..=MAX_EDGE).contains(&width) || !(1..=MAX_EDGE).contains(&height) {
        return None;
    }
    // SAFETY: a null DC makes a memory DC compatible with the screen.
    let dc = OwnedDc(unsafe { CreateCompatibleDC(null_mut()) });
    if dc.0.is_null() {
        return None;
    }
    let colors = dib_rows(&dc, &color, width, height)?;
    // Without alpha the mask holds the transparency, so an icon whose mask
    // cannot be read is not used rather than drawn on an opaque square.
    let mask_rows = if win_pixels::has_alpha(&colors) || mask.0.is_null() {
        None
    } else {
        Some(dib_rows(&dc, &mask, width, height)?)
    };
    Some(Rgba {
        pixels: win_pixels::icon_pixels(&colors, mask_rows.as_deref()),
        width: width as u32,
        height: height as u32,
    })
}

/// A bitmap's pixels as top-down 32-bit BGRA rows.
fn dib_rows(dc: &OwnedDc, bitmap: &OwnedBitmap, width: i32, height: i32) -> Option<Vec<u8>> {
    let mut header = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            // Negative: rows top to bottom.
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..BITMAPINFOHEADER::default()
        },
        ..BITMAPINFO::default()
    };
    let mut rows = vec![0u8; width as usize * height as usize * 4];
    // SAFETY: the DC and bitmap are live and the bitmap is not selected
    // into a DC; `rows` holds exactly `height` rows of `width` 32-bit
    // pixels as described by `header`.
    let copied = unsafe {
        GetDIBits(
            dc.0,
            bitmap.0,
            0,
            height as u32,
            rows.as_mut_ptr().cast::<c_void>(),
            &mut header,
            DIB_RGB_COLORS,
        )
    };
    (copied == height).then_some(rows)
}
