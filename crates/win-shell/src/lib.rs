//! Windows shell icon extraction. Split out of `win-audio` (which stays
//! honestly about audio) and out of `app` (which stays zero-`unsafe`) — this
//! is the one place in the workspace that touches `HICON`/GDI rather than
//! WASAPI (app-icons.md decision 6).

use std::ffi::c_void;
use std::mem::size_of;
use std::path::Path;

use windows::core::{Owned, HSTRING};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, GetDIBits, GetObjectW, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, HDC,
};
use windows::Win32::UI::Shell::ExtractIconExW;
use windows::Win32::UI::WindowsAndMessaging::{GetIconInfo, HICON, ICONINFO};

/// Owned, decoded icon pixels crossing the worker -> UI thread boundary.
/// RGBA8, straight (non-premultiplied) alpha, row-major, top-down — matching
/// `ColorImage::from_rgba_unmultiplied`'s expectation exactly.
pub struct IconImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// `CreateCompatibleDC`'s handle has no `windows_core::Free` impl (unlike
/// `HICON`/`HBITMAP`, both freed automatically via `Owned` below), so it gets
/// the one manual guard — same shape as `win-audio::com::ComGuard`.
struct DcGuard(HDC);

impl Drop for DcGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = DeleteDC(self.0);
            }
        }
    }
}

/// Extracts the large (32 px) shell icon for an executable. `None` when the
/// path is empty or missing, the shell returns no icon, or any Win32 step
/// fails. Callers cache the `None` — this is never retried.
pub fn extract_icon_rgba(path: &Path) -> Option<IconImage> {
    if path.as_os_str().is_empty() {
        return None;
    }
    let wide = HSTRING::from(path.to_str()?);

    let mut large = HICON::default();
    // SAFETY: `large` is a valid out-pointer for the duration of this call;
    // ownership of the returned handle transfers to us on success (count > 0).
    let count = unsafe { ExtractIconExW(&wide, 0, Some(&mut large), None, 1) };
    if count == 0 || large.is_invalid() {
        return None;
    }
    // RAII from here on: `DestroyIcon` (via `Free`) runs on every exit path,
    // including every `?` below (app-icons.md's stated trap).
    let icon = unsafe { Owned::new(large) };

    let mut info = ICONINFO::default();
    // SAFETY: `icon` is a valid HICON owned by us; `info` is a valid out-param.
    unsafe { GetIconInfo(*icon, &mut info) }.ok()?;
    // `GetIconInfo` hands us two new owned bitmaps regardless of `fIcon`; both
    // must be freed on every path, used or not (app-icons.md contract).
    let color = unsafe { Owned::new(info.hbmColor) };
    let _mask = unsafe { Owned::new(info.hbmMask) };
    if color.is_invalid() {
        return None;
    }

    let mut bitmap = BITMAP::default();
    // SAFETY: `color` is a valid HBITMAP; `bitmap` is sized to receive it.
    let wrote = unsafe {
        GetObjectW(
            (*color).into(),
            size_of::<BITMAP>() as i32,
            Some(&mut bitmap as *mut BITMAP as *mut c_void),
        )
    };
    if wrote == 0 || bitmap.bmWidth <= 0 || bitmap.bmHeight <= 0 {
        return None;
    }
    let (width, height) = (bitmap.bmWidth, bitmap.bmHeight);

    // SAFETY: `CreateCompatibleDC(None)` needs no window; freed by `DcGuard`.
    let dc = DcGuard(unsafe { CreateCompatibleDC(None) });
    if dc.0.is_invalid() {
        return None;
    }

    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height, // negative: top-down rows (app-icons.md contract)
            biPlanes: 1,
            biBitCount: 32,
            biCompression: windows::Win32::Graphics::Gdi::BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buffer = vec![0u8; width as usize * height as usize * 4];
    // SAFETY: `buffer` is sized exactly for `width * height` 32bpp pixels,
    // matching `bmi`'s header; `dc`/`color` are both valid for this call.
    let lines = unsafe {
        GetDIBits(
            dc.0,
            *color,
            0,
            height as u32,
            Some(buffer.as_mut_ptr() as *mut c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        )
    };
    if lines == 0 {
        return None;
    }

    // BGRA -> RGBA (Win32 DIB order is not egui's), plus the all-zero-alpha
    // legacy-icon case: a 24 bpp icon decodes with every alpha byte 0, which
    // would otherwise ship as an invisible texture (app-icons.md decision 12).
    let mut any_alpha = false;
    for pixel in buffer.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        any_alpha |= pixel[3] != 0;
    }
    if !any_alpha {
        for pixel in buffer.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
    }

    Some(IconImage { width: width as u32, height: height as u32, rgba: buffer })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_path_extracts_to_none() {
        assert!(extract_icon_rgba(Path::new(r"C:\definitely\not\a\real\path.exe")).is_none());
    }

    #[test]
    fn an_empty_path_extracts_to_none() {
        assert!(extract_icon_rgba(Path::new("")).is_none());
    }

    #[test]
    #[ignore] // real-environment test, per this codebase's existing precedent
    fn a_real_system_exe_yields_thirty_two_pixel_rgba_with_visible_alpha() {
        let path = Path::new(r"C:\Windows\System32\notepad.exe");
        let image = extract_icon_rgba(path).expect("notepad.exe should yield an icon");
        assert_eq!((image.width, image.height), (32, 32));
        assert!(image.rgba.chunks_exact(4).any(|p| p[3] != 0), "icon must not be fully transparent");
    }
}
