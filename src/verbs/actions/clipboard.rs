//! Putting an image on the Windows clipboard as CF_DIB.

use super::*;

/// Decode `path` and place it on the clipboard as CF_DIB (32bpp, bottom-up
/// BGRA — the conventional packed-DIB layout other apps expect).
pub fn copy_to_clipboard(path: &str) -> Result<()> {
    let bytes = read_full_fidelity_capped(path)?;
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
    let dib = build_dib(w, h, rgba);

    // The unsafe HGLOBAL ownership dance lives once in `crate::clipboard`.
    if unsafe { crate::clipboard::set_clipboard(crate::clipboard::CF_DIB, &dib) } {
        Ok(())
    } else {
        Err(Error::new(E_FAIL, "copy to clipboard failed"))
    }
}

/// Assemble a packed CF_DIB (BITMAPINFOHEADER + bottom-up BGRA pixels) from
/// top-down RGBA8 pixels. Pure — no clipboard/HGLOBAL access — so it's
/// unit-testable without a real Windows clipboard. Callers must ensure `w`/`h`
/// are positive and `rgba.len() == w * h * 4` ([`copy_rgba_to_clipboard`] checks
/// both before calling this).
///
/// The header is serialized field-by-field to match the exact byte layout of a
/// `#[repr(C)]` BITMAPINFOHEADER (40 bytes, no padding); the pixels are emitted
/// bottom row first with R/B swapped (CF_DIB's bottom-up BGRA convention).
fn build_dib(w: i32, h: i32, rgba: &[u8]) -> Vec<u8> {
    let row = (w * 4) as usize;
    let header = size_of::<BITMAPINFOHEADER>();
    let total = header + row * h as usize;

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
    dib
}

#[cfg(test)]
mod tests {
    use super::build_dib;

    /// A 2x2 RGBA input with a distinct color per pixel, laid out top-down:
    /// ```text
    /// row0: (255,0,0,10)   (0,255,0,20)
    /// row1: (0,0,255,30)   (10,20,30,40)
    /// ```
    /// Pins both halves of the assembly a regression could silently break: the
    /// exact `BITMAPINFOHEADER` field bytes, and the bottom-up-row / R<->B-swap
    /// pixel order (a wrong field order, an off-by-one in `header + row*h`, or a
    /// botched channel swap would all slip past every other test — nothing else
    /// in the suite decodes a DIB back into pixels to check).
    #[test]
    fn build_dib_header_and_pixel_order_are_exact() {
        #[rustfmt::skip]
        let rgba: [u8; 16] = [
            255, 0,   0,   10,  0,  255, 0,   20,
            0,   0,   255, 30,  10, 20,  30,  40,
        ];
        let dib = build_dib(2, 2, &rgba);

        // BITMAPINFOHEADER, 40 bytes, little-endian, no padding.
        assert_eq!(dib.len(), 40 + 2 * 2 * 4, "header + 2x2 BGRA pixels");
        assert_eq!(&dib[0..4], &40u32.to_le_bytes(), "biSize");
        assert_eq!(&dib[4..8], &2i32.to_le_bytes(), "biWidth");
        assert_eq!(
            &dib[8..12],
            &2i32.to_le_bytes(),
            "biHeight (positive = bottom-up)"
        );
        assert_eq!(&dib[12..14], &1u16.to_le_bytes(), "biPlanes");
        assert_eq!(&dib[14..16], &32u16.to_le_bytes(), "biBitCount");
        assert_eq!(&dib[16..20], &0u32.to_le_bytes(), "biCompression = BI_RGB");
        assert_eq!(
            &dib[20..40],
            &[0u8; 20],
            "biSizeImage..biClrImportant all zero"
        );

        // Pixels: bottom-up (row1 first), each pixel BGRA (R/B swapped from source).
        let pixels = &dib[40..];
        assert_eq!(
            &pixels[0..4],
            &[255, 0, 0, 30],
            "row1 px0: RGBA(0,0,255,30) -> BGRA"
        );
        assert_eq!(
            &pixels[4..8],
            &[30, 20, 10, 40],
            "row1 px1: RGBA(10,20,30,40) -> BGRA"
        );
        assert_eq!(
            &pixels[8..12],
            &[0, 0, 255, 10],
            "row0 px0: RGBA(255,0,0,10) -> BGRA"
        );
        assert_eq!(
            &pixels[12..16],
            &[0, 255, 0, 20],
            "row0 px1: RGBA(0,255,0,20) -> BGRA"
        );
    }
}
