//! Putting an image on the Windows clipboard as CF_DIB, or a file as a `data:` URI.

use super::*;

/// Extension -> MIME type for `data:` URIs, covering the formats named in scope; anything
/// else falls back to the generic binary type rather than refusing the file (`is_image`
/// accepts many formats — HEIC, RAW, PSD, … — outside this short list).
fn mime_for_ext(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

/// Build a `data:<mime>;base64,<payload>` URI from raw file bytes and the source
/// extension (used only to pick the MIME type — the bytes themselves are copied
/// verbatim, never re-encoded). Pure, so it's unit-testable without touching a file.
pub(crate) fn build_data_uri(ext: &str, bytes: &[u8]) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    format!(
        "data:{};base64,{}",
        mime_for_ext(ext),
        STANDARD.encode(bytes)
    )
}

/// `VerbAction::CopyDataUri` - read the file through the same bounded reader
/// [`copy_to_clipboard`] uses (refuses + logs over the same size cap), base64 its raw
/// bytes into a `data:` URI, and place it on the clipboard as CF_UNICODETEXT.
pub fn copy_data_uri_to_clipboard(path: &str) -> Result<()> {
    let bytes = read_full_fidelity_capped(path)?;
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let uri = build_data_uri(ext, &bytes);
    let ok = unsafe {
        crate::clipboard::set_clipboard(
            crate::clipboard::CF_UNICODETEXT,
            &crate::clipboard::utf16_nul_bytes(&uri),
        )
    };
    if ok {
        Ok(())
    } else {
        Err(Error::new(E_FAIL, "copy to clipboard failed"))
    }
}

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
    use super::{build_data_uri, build_dib, mime_for_ext};

    /// Every extension in the MIME table maps to the exact type named in scope, and
    /// anything else — including extensions `is_image` accepts that aren't in this
    /// short list (HEIC, RAW, PSD, …) — falls back to the generic binary type rather
    /// than refusing the file.
    #[test]
    fn mime_for_ext_covers_the_table_and_falls_back() {
        assert_eq!(mime_for_ext("png"), "image/png");
        assert_eq!(mime_for_ext("PNG"), "image/png", "case-insensitive");
        assert_eq!(mime_for_ext("jpg"), "image/jpeg");
        assert_eq!(mime_for_ext("jpeg"), "image/jpeg");
        assert_eq!(mime_for_ext("gif"), "image/gif");
        assert_eq!(mime_for_ext("webp"), "image/webp");
        assert_eq!(mime_for_ext("svg"), "image/svg+xml");
        assert_eq!(mime_for_ext("avif"), "image/avif");
        assert_eq!(mime_for_ext("bmp"), "image/bmp");
        assert_eq!(mime_for_ext("ico"), "image/x-icon");
        assert_eq!(mime_for_ext("heic"), "application/octet-stream");
        assert_eq!(mime_for_ext(""), "application/octet-stream");
    }

    /// A known byte string's base64 is a fixed, checkable value — pins the URI shape
    /// (`data:<mime>;base64,<payload>`) and that the payload is real base64, not just
    /// "some string that happens to look encoded."
    #[test]
    fn build_data_uri_known_bytes() {
        assert_eq!(
            build_data_uri("png", b"hello"),
            "data:image/png;base64,aGVsbG8="
        );
        assert_eq!(build_data_uri("jpg", b"hi"), "data:image/jpeg;base64,aGk=");
        assert_eq!(
            build_data_uri("xyz", b""),
            "data:application/octet-stream;base64,"
        );
    }

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
