//! Maxon **Cinema 4D** `.c4d` — carve the embedded document/viewport preview.
//!
//! A `.c4d` file stores a JPEG preview of the scene (the image Cinema 4D shows in
//! its asset browser, and macOS shows in Finder) right after the file header — at
//! a small fixed offset (~45 bytes), 500 px wide. The file ALSO embeds 90×90
//! material-preview swatches and 256×256 texture thumbnails, but those live much
//! later in the file. So the rule is simple and robust: the document preview is
//! the **first** embedded JPEG, and only if it starts near the header AND is
//! preview-sized — that way we never mistake a little material sphere for the
//! scene. Files saved without a preview image (none in the header slot) fall back
//! to the default icon. No 3-D rendering, no codec — just slice out the JPEG and
//! let the image tier decode it (same approach as PSD/PSP/RAW).

/// The preview JPEG sits right after the header; material/texture JPEGs are far
/// deeper. A generous window that still excludes them (observed previews at ~45,
/// first swatch no earlier than ~1900).
const PREVIEW_MAX_OFFSET: usize = 2048;
/// Minimum long edge to accept as a *scene* preview — rejects the 90×90 material
/// swatches (and the odd 256×256 texture) while admitting the 500 px doc preview.
const MIN_PREVIEW_EDGE: u16 = 320;

/// Cinema 4D files carry a `C4DC4D` signature a byte or two into the header
/// (preceded by a type/version byte that varies).
pub fn looks_like_c4d(b: &[u8]) -> bool {
    b.len() >= 8 && b[..8].windows(6).any(|w| w == b"C4DC4D")
}

/// Carve the document preview JPEG, or `None` (no saved preview / not preview-sized).
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    if !looks_like_c4d(bytes) {
        return None;
    }
    // Find the first JPEG SOI. The doc preview, when present, is the first one and
    // sits within the header window.
    let soi = find_soi(bytes, PREVIEW_MAX_OFFSET)?;
    let (w, h) = jpeg_dims(&bytes[soi..])?;
    if w.max(h) < MIN_PREVIEW_EDGE {
        return None; // a material swatch, not the scene preview
    }
    let len = crate::container::jpeg_span_len(bytes, soi)?;
    let jpeg = bytes.get(soi..soi + len)?;
    (jpeg.len() as u64 <= crate::container::MAX_COVER).then(|| jpeg.to_vec())
}

/// Offset of the first `FF D8 FF` within the first `window` bytes, or `None`.
fn find_soi(b: &[u8], window: usize) -> Option<usize> {
    let lim = b.len().min(window);
    (0..lim.saturating_sub(2)).find(|&i| b[i] == 0xFF && b[i + 1] == 0xD8 && b[i + 2] == 0xFF)
}

/// Width/height from the JPEG starting at the slice head, read off the first SOF
/// marker (`FFC0`/`FFC1`/`FFC2`). Bounds-checked.
fn jpeg_dims(j: &[u8]) -> Option<(u16, u16)> {
    let mut p = 2usize; // past SOI
    while p + 9 < j.len() {
        if j[p] != 0xFF {
            p += 1;
            continue;
        }
        let marker = j[p + 1];
        if matches!(marker, 0xC0..=0xC2) {
            let h = u16::from_be_bytes([j[p + 5], j[p + 6]]);
            let w = u16::from_be_bytes([j[p + 7], j[p + 8]]);
            return Some((w, h));
        }
        // Skip length-prefixed segments; bail on entropy/markers we don't expect.
        if matches!(marker, 0xD8 | 0xD9 | 0x01) || (0xD0..=0xD7).contains(&marker) {
            p += 2;
        } else {
            let len = u16::from_be_bytes([j[p + 2], j[p + 3]]) as usize;
            if len < 2 {
                return None;
            }
            p += 2 + len;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let mut b = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(w, h))
            .write_to(&mut std::io::Cursor::new(&mut b), image::ImageFormat::Jpeg)
            .unwrap();
        b
    }

    /// header byte + "C4DC4D6" + version, then `gap` filler, then the preview JPEG,
    /// then a late material-swatch JPEG.
    fn fake_c4d(gap: usize, preview: &[u8], swatch: &[u8]) -> Vec<u8> {
        let mut f = vec![0x36];
        f.extend_from_slice(b"C4DC4D6");
        f.extend_from_slice(&[0u8; 4]);
        f.resize(8 + gap, 0);
        f.extend_from_slice(preview);
        f.resize(f.len() + 4000, 0); // push the swatch far out
        f.extend_from_slice(swatch);
        f
    }

    #[test]
    fn carves_scene_preview_not_swatch() {
        let preview = jpeg(500, 278);
        let swatch = jpeg(90, 90);
        let c4d = fake_c4d(37, &preview, &swatch);
        let got = extract(&c4d).expect("preview");
        let d = image::load_from_memory(&got).unwrap();
        assert_eq!((d.width(), d.height()), (500, 278), "should pick the 500px doc preview");
    }

    #[test]
    fn no_preview_when_only_swatches() {
        // First (and only early) JPEG is a 90×90 swatch → no scene preview.
        let swatch = jpeg(90, 90);
        let mut f = vec![0x51];
        f.extend_from_slice(b"C4DC4D6");
        f.resize(1924, 0);
        f.extend_from_slice(&swatch);
        assert!(extract(&f).is_none());
    }

    #[test]
    fn rejects_non_c4d() {
        assert!(!looks_like_c4d(b"PK\x03\x04 zip!!"));
        assert!(extract(b"not a c4d file").is_none());
    }
}
