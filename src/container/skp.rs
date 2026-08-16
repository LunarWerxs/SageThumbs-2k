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
    const UTF16: &[u8] = &[
        b'S', 0, b'k', 0, b'e', 0, b't', 0, b'c', 0, b'h', 0, b'U', 0, b'p', 0,
    ];
    contains_ci(h, b"SketchUp Model") || find(h, UTF16).is_some()
}

/// A PNG chunk header is 4-byte length + 4-byte type; the walk in [`find_iend_end`] never
/// looks at more chunks than this before giving up, so a crafted file with a huge run of
/// tiny/zero-length chunks can't make the carve spin indefinitely.
const MAX_PNG_CHUNKS: usize = 4096;

/// Walk real PNG chunks from `start` (the 8-byte signature) to find the TRUE `IEND` chunk,
/// returning the offset just past its CRC. Unlike a raw byte-pattern search for `b"IEND"`
/// (what this used to do), a chunk walk can't be fooled by a coincidental `IEND` 4-byte
/// sequence sitting inside compressed `IDAT` data before the real `IEND` chunk — each hop is
/// exactly `4 (length) + 4 (type) + length (data) + 4 (CRC)` bytes, driven by the chunk's own
/// declared length, never a substring match.
///
/// Bounded two ways so a crafted length field can't turn this into unbounded work: gives up
/// past [`super::MAX_COVER`] bytes from `start` (a real embedded preview is a few KB, and
/// anything bigger fails [`extract`]'s own size check right after) or past
/// [`MAX_PNG_CHUNKS`] hops, whichever comes first.
fn find_iend_end(bytes: &[u8], start: usize) -> Option<usize> {
    let scan_limit = start
        .saturating_add(super::MAX_COVER as usize)
        .min(bytes.len());
    let mut p = start.checked_add(PNG_SIG.len())?;
    for _ in 0..MAX_PNG_CHUNKS {
        if p >= scan_limit {
            return None; // no IEND within the sane preview-size budget
        }
        let hdr = bytes.get(p..p.checked_add(8)?)?; // 4-byte length + 4-byte type
        let len = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
        let ty = &hdr[4..8];
        let end = p.checked_add(8)?.checked_add(len)?.checked_add(4)?; // + data + CRC
        if ty == b"IEND" {
            return (end <= bytes.len()).then_some(end);
        }
        p = end;
    }
    None // pathological chunk count for a "thumbnail" — treat as no valid PNG
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
    let end = find_iend_end(bytes, start)?;
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

        assert!(!looks_like_skp(
            b"just some random binary header bytes here....."
        ));
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
        assert!(
            image::load_from_memory(&got).is_ok(),
            "carved bytes must be a valid PNG"
        );
    }

    #[test]
    fn no_png_returns_none() {
        let mut skp = vec![0xFF, 0xFE, 0xFF, 0x0E];
        skp.extend_from_slice(&[0x11; 200]); // no PNG anywhere
        assert!(extract(&skp).is_none());
    }

    /// A177: a raw `find(bytes, b"IEND")` byte-pattern search truncates at the FIRST
    /// occurrence of those four bytes anywhere in the file — including a coincidental match
    /// inside compressed `IDAT` data, well before the chunk that is actually named `IEND`.
    /// This builds a real PNG chunk chain with such a decoy and proves the carve walks past
    /// it to the true `IEND` chunk instead of stopping short.
    #[test]
    fn ignores_a_coincidental_iend_byte_sequence_inside_idat_data() {
        fn chunk(ty: &[u8; 4], data: &[u8]) -> Vec<u8> {
            let mut c = Vec::new();
            c.extend_from_slice(&(data.len() as u32).to_be_bytes());
            c.extend_from_slice(ty);
            c.extend_from_slice(data);
            c.extend_from_slice(&[0u8; 4]); // dummy CRC — decodable_image only checks the magic
            c
        }

        let mut png = PNG_SIG.to_vec();
        png.extend_from_slice(&chunk(b"IHDR", &[0u8; 13]));
        // A decoy "IEND" sitting inside the IDAT payload — exactly what a raw substring
        // search would (wrongly) truncate at.
        let mut idat_data = vec![0xAB; 20];
        idat_data.extend_from_slice(b"IEND");
        idat_data.extend_from_slice(&[0xCD; 20]);
        png.extend_from_slice(&chunk(b"IDAT", &idat_data));
        png.extend_from_slice(&chunk(b"IEND", &[])); // the REAL IEND chunk
        let full_png_len = png.len();
        // Trailing junk after the real IEND (the rest of the .skp model data) must not be
        // swept into the carve either.
        png.extend_from_slice(&[0xEF; 64]);

        let mut skp = vec![0xFF, 0xFE, 0xFF, 0x0E];
        for c in "SketchUp Model".chars() {
            skp.push(c as u8);
            skp.push(0);
        }
        skp.extend_from_slice(&png);

        let got = extract(&skp).expect("should carve past the decoy IEND bytes");
        assert_eq!(
            got.len(),
            full_png_len,
            "must carve through to the REAL IEND chunk, not the literal bytes inside IDAT"
        );
        assert!(got.starts_with(PNG_SIG));
    }
}
