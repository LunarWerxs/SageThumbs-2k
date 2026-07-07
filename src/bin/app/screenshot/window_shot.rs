//! Headless window capture: render an HWND (even a hidden / off-screen one) to a PNG — or a
//! set of frames to an animated GIF — via `PrintWindow`. This is the primitive behind the
//! app's `--shot*` modes: it lets a UI change be verified, or a README/site asset be
//! regenerated, by rendering the real window(s) to a file — without ever showing a window or
//! driving the desktop. Reuses `output`'s BGRA → RGBA conversion / PNG writer.

use core::ffi::c_void;
use std::path::Path;

use image::RgbaImage;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

/// `PW_RENDERFULLCONTENT` — render the full (incl. DirectComposition / child) content, so a
/// modern-themed window with common controls captures faithfully. Not surfaced as a named
/// constant by windows-rs here, so we spell the documented value.
const PW_RENDERFULLCONTENT: PRINT_WINDOW_FLAGS = PRINT_WINDOW_FLAGS(0x0000_0002);

/// Capture the whole window `hwnd` (including its non-client title bar) to a top-down BGRA
/// buffer via `PrintWindow`. Works on an off-screen window — `PrintWindow` drives a fresh
/// `WM_PRINT` into our memory DC, so the window need never have been composited on-screen (a
/// plain `BitBlt` from the screen would be blank). Returns `(bgra, width, height)`.
pub(crate) unsafe fn capture_hwnd_bgra(hwnd: HWND) -> Option<(Vec<u8>, i32, i32)> {
    let mut r = RECT::default();
    if GetWindowRect(hwnd, &mut r).is_err() {
        return None;
    }
    let w = r.right - r.left;
    let h = r.bottom - r.top;
    // 64-bit size math + sane bail (mirrors overlay.rs): the i32 product `w*h*4` could only
    // overflow on an absurd window; cheap to close.
    let n = w as i64 * h as i64 * 4;
    if w <= 0 || h <= 0 || n <= 0 || n > i32::MAX as i64 {
        return None;
    }

    let screen = GetDC(None);
    let mem = CreateCompatibleDC(Some(screen));
    let bmp = CreateCompatibleBitmap(screen, w, h);
    let old = SelectObject(mem, HGDIOBJ(bmp.0));
    let printed = PrintWindow(hwnd, mem, PW_RENDERFULLCONTENT).as_bool();

    // Pull top-down BGRA (negative biHeight) — exactly what `output::to_rgba` wants.
    let mut bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buf = vec![0u8; n as usize];
    let got = GetDIBits(mem, bmp, 0, h as u32, Some(buf.as_mut_ptr() as *mut c_void), &mut bi, DIB_RGB_COLORS);

    SelectObject(mem, old);
    let _ = DeleteObject(HGDIOBJ(bmp.0));
    let _ = DeleteDC(mem);
    ReleaseDC(None, screen);

    if !printed || got == 0 {
        return None;
    }
    Some((buf, w, h))
}

/// Capture `hwnd` to a PNG at `path`. Returns whether the file was written.
pub(crate) unsafe fn capture_hwnd_to_png(hwnd: HWND, path: &Path) -> bool {
    match capture_hwnd_bgra(hwnd) {
        Some((buf, w, h)) => super::output::save_png_to_path(path, &buf, w, h),
        None => false,
    }
}

/// Downscale `img` to `target_w` wide (preserving aspect, Lanczos3) — keeps the GIF crisp
/// and small vs. the DPI-scaled native capture. A no-op if already ≤ `target_w`.
pub(crate) fn downscale_to_width(img: RgbaImage, target_w: u32) -> RgbaImage {
    if img.width() <= target_w || img.width() == 0 {
        return img;
    }
    let target_h = (img.height() as u64 * target_w as u64 / img.width() as u64).max(1) as u32;
    image::imageops::resize(&img, target_w, target_h, image::imageops::FilterType::Lanczos3)
}

/// Encode same-size RGBA `frames` to an animated, infinite-loop GIF at `path`, `delay_ms`
/// per frame. Returns whether it was written. (GIF is 256-colour paletted — the encoder
/// quantises each frame; fine for a UI walkthrough.)
pub(crate) fn encode_gif(frames: &[RgbaImage], path: &Path, delay_ms: u16) -> bool {
    use image::codecs::gif::{GifEncoder, Repeat};
    use image::{Delay, Frame};
    if frames.is_empty() {
        return false;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(file) = std::fs::File::create(path) else {
        return false;
    };
    // Speed 10 (of 1..=30) balances palette quality against encode time.
    let mut enc = GifEncoder::new_with_speed(std::io::BufWriter::new(file), 10);
    if enc.set_repeat(Repeat::Infinite).is_err() {
        return false;
    }
    for img in frames {
        let delay = Delay::from_numer_denom_ms(delay_ms as u32, 1);
        let frame = Frame::from_parts(img.clone(), 0, 0, delay);
        if enc.encode_frame(frame).is_err() {
            return false;
        }
    }
    true
}
