//! The shell icon of a session's executable on Windows.
//!
//! The icon comes from the executable's own resources at the requested
//! size (`SHDefExtractIconW`), else from the shell (`SHGetFileInfoW`, the
//! system large icon size, also the generic program icon for executables
//! without one). Packaged (UWP/MSIX) apps keep their logos in the package
//! rather than the executable, and their install folder is not readable,
//! so they get the generic icon or none.

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetDIBits, GetObjectW, HBITMAP, HDC,
};
use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows_sys::Win32::UI::Shell::{
    SHDefExtractIconW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGetFileInfoW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

use super::{IconKey, Resolver, raster, win_pixels};
use crate::icons::Rgba;

/// Larger bitmaps are not icons.
const MAX_EDGE: i32 = 1024;

pub struct WindowsResolver;

impl Resolver for WindowsResolver {
    fn resolve(&self, key: &IconKey) -> Option<Rgba> {
        let path = key.process_binary.as_deref()?;
        COM.with(|_| ());
        let icon = extract(path, key.size)?;
        let rgba = icon_rgba(&icon)?;
        let fitted = raster::fit_rgba(&rgba, key.size);
        tracing::debug!(
            app = %key.name,
            path,
            found = fitted.is_some(),
            "application icon lookup"
        );
        fitted
    }
}

/// COM initialised on the icon thread, as the shell functions require.
struct ComGuard(bool);

impl ComGuard {
    fn init() -> Self {
        // SAFETY: no pointer arguments; balanced by CoUninitialize in Drop
        // when it succeeds (S_OK or S_FALSE).
        let hr = unsafe { CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32) };
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

/// The executable's icon at `size` pixels, else the shell's large icon.
fn extract(path: &str, size: u32) -> Option<OwnedIcon> {
    let path = wide(path);
    let mut large: HICON = null_mut();
    // SAFETY: `path` is nul-terminated and outlives the call; the small
    // icon is not requested. The low word of the last argument is the large
    // icon size.
    let hr =
        unsafe { SHDefExtractIconW(path.as_ptr(), 0, 0, &mut large, null_mut(), size.min(256)) };
    if hr == 0 && !large.is_null() {
        return Some(OwnedIcon(large));
    }
    // SAFETY: SHFILEINFOW is plain data, valid when zeroed.
    let mut info: SHFILEINFOW = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is nul-terminated, `info` is writable and its size is
    // passed.
    let found = unsafe {
        SHGetFileInfoW(
            path.as_ptr(),
            0,
            &mut info,
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        )
    };
    (found != 0 && !info.hIcon.is_null()).then_some(OwnedIcon(info.hIcon))
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
    // SAFETY: BITMAP is plain data, valid when zeroed.
    let mut bitmap: BITMAP = unsafe { std::mem::zeroed() };
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
    let mask_rows = if win_pixels::has_alpha(&colors) || mask.0.is_null() {
        None
    } else {
        dib_rows(&dc, &mask, width, height)
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
