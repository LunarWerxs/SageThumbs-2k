//! Putting an image on the Windows clipboard as CF_DIB.

use super::*;

/// Decode `path` and place it on the clipboard as CF_DIB (32bpp, bottom-up
/// BGRA — the conventional packed-DIB layout other apps expect).
pub fn copy_to_clipboard(path: &str) -> Result<()> {
    let bytes = read_capped(path)?;
    let img = decode::decode_full(&bytes)?.to_rgba8();
    let (w, h) = (img.width() as i32, img.height() as i32);
    copy_rgba_to_clipboard(w, h, &img.into_raw())
}

/// Place already-decoded top-down RGBA8 pixels on the clipboard as CF_DIB (32bpp, bottom-up
/// BGRA). The pixel half of [`copy_to_clipboard`]; also used by the Quick preview viewer's
/// Ctrl+C so a navigated-to PDF page / animation frame copies what is actually displayed.
pub fn copy_rgba_to_clipboard(w: i32, h: i32, rgba: &[u8]) -> Result<()> {
    if w <= 0 || h <= 0 {
        return Err(Error::new(E_FAIL, "image has zero or negative dimensions"));
    }
    if rgba.len() != (w as usize) * (h as usize) * 4 {
        return Err(Error::new(E_FAIL, "pixel buffer size mismatch"));
    }
    let row = (w * 4) as usize;
    let header = size_of::<BITMAPINFOHEADER>();
    let total = header + row * h as usize;

    // Assemble the whole packed DIB (BITMAPINFOHEADER + bottom-up BGRA pixels)
    // in a plain Vec first, so the only `unsafe` left is alloc / lock / copy /
    // SetClipboardData. The header is serialized field-by-field to match the
    // exact byte layout of a `#[repr(C)]` BITMAPINFOHEADER (40 bytes, no
    // padding); the pixels are emitted bottom row first with R/B swapped.
    let mut dib = Vec::with_capacity(total);
    // BITMAPINFOHEADER: positive biHeight = bottom-up DIB (CF_DIB convention).
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
    debug_assert_eq!(dib.len(), header);
    // Pixels: bottom-up, RGBA -> BGRA. Walk source rows in reverse (last to
    // first) and swap R/B per pixel.
    for src in rgba.chunks_exact(row).rev() {
        for px in src.chunks_exact(4) {
            dib.push(px[2]); // B
            dib.push(px[1]); // G
            dib.push(px[0]); // R
            dib.push(px[3]); // A
        }
    }
    debug_assert_eq!(dib.len(), total);

    // The unsafe HGLOBAL ownership dance lives once in `crate::clipboard`.
    if unsafe { crate::clipboard::set_clipboard(crate::clipboard::CF_DIB, &dib) } {
        Ok(())
    } else {
        Err(Error::new(E_FAIL, "copy to clipboard failed"))
    }
}
