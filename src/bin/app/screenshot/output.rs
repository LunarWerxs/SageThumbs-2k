//! Where a finished capture goes: the system clipboard (CF_DIB) and a timestamped
//! PNG. Both take the already-composited top-down BGRA pixels from `overlay.rs`, so
//! this file knows nothing about windows or annotations.

use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;

/// Put a packed CF_DIB (bottom-up BGRA) on the clipboard from top-down BGRA pixels.
pub(super) unsafe fn copy_dib_to_clipboard(top_down_bgra: &[u8], w: i32, h: i32) {
    let header = core::mem::size_of::<BITMAPINFOHEADER>();
    let row = (w * 4) as usize;
    let total = header + row * h as usize;
    let mut dib = Vec::with_capacity(total);
    dib.extend_from_slice(&(header as u32).to_le_bytes()); // biSize
    dib.extend_from_slice(&w.to_le_bytes()); // biWidth
    dib.extend_from_slice(&h.to_le_bytes()); // biHeight (positive = bottom-up)
    dib.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    dib.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    dib.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    dib.extend_from_slice(&0u32.to_le_bytes()); // biSizeImage
    dib.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    dib.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
    // Emit rows bottom-up from the top-down source.
    for y in (0..h as usize).rev() {
        dib.extend_from_slice(&top_down_bgra[y * row..y * row + row]);
    }

    // The unsafe HGLOBAL ownership dance lives once in the lib's `clipboard` module.
    let _ = sagethumbs2k_core::clipboard::set_clipboard(sagethumbs2k_core::clipboard::CF_DIB, &dib);
}

/// BGRA (top-down) -> an opaque RGBA image (GDI bitmaps carry no alpha).
fn to_rgba(top_down_bgra: &[u8], w: i32, h: i32) -> Option<image::RgbaImage> {
    let mut rgba = vec![0u8; top_down_bgra.len()];
    for (dst, src) in rgba.chunks_exact_mut(4).zip(top_down_bgra.chunks_exact(4)) {
        dst[0] = src[2];
        dst[1] = src[1];
        dst[2] = src[0];
        dst[3] = 255;
    }
    image::RgbaImage::from_raw(w as u32, h as u32, rgba)
}

/// A capture's default filename, e.g. `Screenshot 2026-06-18 09.41.07.png`.
pub(super) unsafe fn timestamped_name() -> String {
    let st = windows::Win32::System::SystemInformation::GetLocalTime();
    format!(
        "Screenshot {:04}-{:02}-{:02} {:02}.{:02}.{:02}.png",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}

/// Auto-save a timestamped PNG into `dir` (created if missing). Used by Ctrl+S / the
/// Save button when "use a fixed save folder" is on, and by the editor-less instant
/// capture. Returns whether the file was written.
pub(super) fn save_png_to_dir(dir: &std::path::Path, top_down_bgra: &[u8], w: i32, h: i32) -> bool {
    let Some(img) = to_rgba(top_down_bgra, w, h) else { return false };
    let _ = std::fs::create_dir_all(dir);
    let name = unsafe { timestamped_name() };
    img.save(dir.join(name)).is_ok()
}

/// Save the PNG to an exact path — the location the user chose in the Save-As dialog
/// (Ctrl+S / Save with the fixed-folder option off). Returns whether it was written.
pub(super) fn save_png_to_path(path: &std::path::Path, top_down_bgra: &[u8], w: i32, h: i32) -> bool {
    let Some(img) = to_rgba(top_down_bgra, w, h) else { return false };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    img.save(path).is_ok()
}

/// Save the capture to a unique temp PNG and return its path — used by the Pin
/// button, which spawns a separate `--pin` process to load + float it.
pub(super) fn save_temp_png(top_down_bgra: &[u8], w: i32, h: i32) -> Option<String> {
    let img = to_rgba(top_down_bgra, w, h)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut p = std::env::temp_dir();
    p.push(format!("st2k_pin_{}_{}.png", std::process::id(), nanos));
    img.save(&p).ok()?;
    p.to_str().map(|s| s.to_string())
}
