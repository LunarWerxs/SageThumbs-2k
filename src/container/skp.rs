//! SketchUp `.skp` embedded thumbnail.
//!
//! A SketchUp file saved from the GUI bakes a 256×256 PNG preview into the file,
//! near the header — sometimes behind a `CDib` MFC tag (DWORD `0x4` = PNG type,
//! DWORD size, then the PNG). We just CARVE that first embedded PNG (signature →
//! `IEND`), no parsing of the proprietary model and no SketchUp SDK. Confirmed
//! against real SketchUp 2017/2020 files; the format detail is documented in
//! <https://github.com/SketchUp/api-issue-tracker/issues/65>.
//!
//! Files saved WITHOUT a thumbnail (minimal / programmatically-created `.skp`)
//! carry no PNG here — `extract` returns `None` and the shell shows the default
//! icon. Like every container extractor this runs under `panic = "abort"`, so the
//! carve is bounds-checked and size-capped.

use super::util::{contains_ci, decodable_image, find};

const PNG_SIG: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// `.skp` files lead with "SketchUp Model" — as ASCII on older versions, UTF-16LE
/// on newer ones. Match either form within the first 64 bytes: specific enough to
/// dispatch without false-positives on other formats.
pub fn looks_like_skp(head: &[u8]) -> bool {
    let h = &head[..head.len().min(64)];
    // ASCII header (older): the literal "SketchUp Model". UTF-16LE header (newer):
    // S\0k\0e\0t\0c\0h\0U\0p\0. Both are specific enough to avoid false-positives.
    const UTF16: &[u8] = &[b'S', 0, b'k', 0, b'e', 0, b't', 0, b'c', 0, b'h', 0, b'U', 0, b'p', 0];
    contains_ci(h, b"SketchUp Model") || find(h, UTF16).is_some()
}

/// Carve the embedded thumbnail PNG, or `None` if this `.skp` has no preview.
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    // The thumbnail is the FIRST PNG and lives in the header region (observed at
    // byte 148–2231 across real files). Bound the START search so a thumbnail-less
    // file with deep texture PNGs can't yield the wrong image; the IEND search then
    // runs from there to the real end of that PNG.
    const SEARCH_WINDOW: usize = 2 * 1024 * 1024;
    let window = &bytes[..bytes.len().min(SEARCH_WINDOW)];
    let start = find(window, PNG_SIG)?;
    // A PNG ends at its `IEND` chunk: the 4-byte "IEND" type + a 4-byte CRC.
    let iend = find(&bytes[start..], b"IEND")?;
    let end = start.checked_add(iend)?.checked_add(8)?;
    let png = bytes.get(start..end)?;
    if png.len() as u64 > super::MAX_COVER {
        return None;
    }
    decodable_image(png.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn tiny_png() -> Vec<u8> {
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(3, 3))
            .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    #[test]
    fn detects_both_header_encodings() {
        let mut ascii = b"\xEF\xBB\xBFSketchUp Model".to_vec();
        ascii.extend_from_slice(&[0u8; 16]);
        assert!(looks_like_skp(&ascii));

        let mut utf16 = vec![0xFF, 0xFE, 0xFF, 0x0E];
        for c in "SketchUp Model".chars() {
            utf16.push(c as u8);
            utf16.push(0);
        }
        assert!(looks_like_skp(&utf16));

        assert!(!looks_like_skp(b"just some random binary header bytes here....."));
    }

    #[test]
    fn carves_first_embedded_png() {
        let png = tiny_png();
        // A fake .skp: UTF-16 header, a `CDib` tag + 8 bytes, then the PNG, then trailing junk.
        let mut skp = vec![0xFF, 0xFE, 0xFF, 0x0E];
        for c in "SketchUp Model".chars() {
            skp.push(c as u8);
            skp.push(0);
        }
        skp.extend_from_slice(b"CDib");
        skp.extend_from_slice(&4u32.to_le_bytes());
        skp.extend_from_slice(&(png.len() as u32).to_le_bytes());
        skp.extend_from_slice(&png);
        skp.extend_from_slice(&[0xAB; 64]); // trailing model data

        let got = extract(&skp).expect("should carve the PNG");
        assert!(got.starts_with(PNG_SIG));
        assert!(image::load_from_memory(&got).is_ok(), "carved bytes must be a valid PNG");
    }

    #[test]
    fn no_png_returns_none() {
        let mut skp = vec![0xFF, 0xFE, 0xFF, 0x0E];
        skp.extend_from_slice(&[0x11; 200]); // no PNG anywhere
        assert!(extract(&skp).is_none());
    }
}
