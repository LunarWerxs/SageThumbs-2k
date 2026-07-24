//! Adobe InDesign `.indd` embedded preview thumbnail.
//!
//! InDesign writes the page preview as a base64-encoded JPEG inside its XMP packet,
//! in `<xmpGImg:image>…</xmpGImg:image>` elements. We scan for those literal tags,
//! base64-decode the content, and pick the largest JPEG (the full-page preview).
//!
//! Two gotchas, both handled: (1) the base64 lines are joined with the XML entity
//! `&#xA;` (NOT raw newlines) which must be stripped before decoding, or the stray
//! `x`/`A` chars corrupt the stream; (2) early elements can be truncated and trailing
//! ones can be per-page slivers, so we choose the LARGEST decoded JPEG, not the first
//! or last. A raw binary JPEG is present in some files but absent in others, so the
//! XMP path is the reliable one. Verified against three real `.indd` saves.
//!
//! Preview presence depends on InDesign's "Save Preview Images with Documents"
//! setting; without it there's no element → None. Bounds-checked (`panic = "abort"`).

use base64::{engine::general_purpose::STANDARD, Engine};

use super::util::find;

/// The 16-byte master GUID every `.indd` starts with (then ASCII "DOCUMENT").
const INDD_GUID: [u8; 16] = [
    0x06, 0x06, 0xED, 0xF5, 0xD8, 0x1D, 0x46, 0xE5, 0xBD, 0x31, 0xEF, 0xE7, 0xFE, 0x74, 0xB7, 0x1D,
];

const OPEN: &[u8] = b"<xmpGImg:image>";
const CLOSE: &[u8] = b"</xmpGImg:image>";
/// Cap on elements scanned and on a single decoded preview (bomb guard).
const MAX_ELEMENTS: usize = 24;

pub fn looks_like_indd(head: &[u8]) -> bool {
    head.starts_with(&INDD_GUID)
}

/// Extract the largest embedded JPEG preview, or None.
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut best: Option<Vec<u8>> = None;
    let mut pos = 0usize;
    for _ in 0..MAX_ELEMENTS {
        let open = match find(bytes.get(pos..)?, OPEN) {
            Some(o) => pos + o + OPEN.len(),
            None => break,
        };
        let close = match find(bytes.get(open..)?, CLOSE) {
            Some(c) => open + c,
            None => break,
        };
        pos = close + CLOSE.len();

        let raw = bytes.get(open..close)?;
        if raw.len() as u64 > super::MAX_COVER {
            continue;
        }
        if let Some(jpeg) = decode_b64_jpeg(raw) {
            if best.as_ref().is_none_or(|b| jpeg.len() > b.len()) {
                best = Some(jpeg);
            }
        }
    }
    best
}

/// Clean InDesign's base64 (strip the `&#xA;`/`&#xD;` entity separators and any
/// non-alphabet bytes), re-pad, decode, and accept only if it's a JPEG.
fn decode_b64_jpeg(raw: &[u8]) -> Option<Vec<u8>> {
    let mut b64: Vec<u8> = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        // Skip the literal XML entities "&#xA;" / "&#xD;" wholesale.
        if raw[i] == b'&'
            && raw
                .get(i..i + 5)
                .is_some_and(|w| w == b"&#xA;" || w == b"&#xD;")
        {
            i += 5;
            continue;
        }
        let c = raw[i];
        if c.is_ascii_alphanumeric() || c == b'+' || c == b'/' {
            b64.push(c);
        }
        i += 1;
    }
    while !b64.len().is_multiple_of(4) {
        b64.push(b'=');
    }
    let jpeg = STANDARD.decode(&b64).ok()?;
    jpeg.starts_with(&[0xFF, 0xD8, 0xFF]).then_some(jpeg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn tiny_jpeg() -> Vec<u8> {
        let mut b = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            8,
            8,
            image::Rgb([10, 90, 200]),
        ))
        .write_to(&mut Cursor::new(&mut b), image::ImageFormat::Jpeg)
        .unwrap();
        b
    }

    #[test]
    fn detects_guid() {
        let mut f = INDD_GUID.to_vec();
        f.extend_from_slice(b"DOCUMENT....");
        assert!(looks_like_indd(&f));
        assert!(!looks_like_indd(b"not indesign"));
    }

    #[test]
    fn decodes_xmp_jpeg_with_entity_separators() {
        let jpeg = tiny_jpeg();
        // Base64 with the &#xA; entity injected mid-stream (InDesign's line wrap).
        let mut b64 = STANDARD.encode(&jpeg);
        let mid = b64.len() / 2;
        b64.insert_str(mid, "&#xA;");
        let doc = format!("<xmpGImg:image>{b64}</xmpGImg:image>");

        let mut f = INDD_GUID.to_vec();
        f.extend_from_slice(doc.as_bytes());
        let got = extract(&f).expect("should decode the embedded JPEG");
        assert!(got.starts_with(&[0xFF, 0xD8, 0xFF]));
        assert!(image::load_from_memory(&got).is_ok());
    }

    #[test]
    fn picks_largest_of_several() {
        let small = tiny_jpeg();
        let big = {
            let mut b = Vec::new();
            image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
                64,
                64,
                image::Rgb([1, 2, 3]),
            ))
            .write_to(&mut Cursor::new(&mut b), image::ImageFormat::Jpeg)
            .unwrap();
            b
        };
        let doc = format!(
            "<xmpGImg:image>{}</xmpGImg:image>junk<xmpGImg:image>{}</xmpGImg:image>",
            STANDARD.encode(&small),
            STANDARD.encode(&big),
        );
        let mut f = INDD_GUID.to_vec();
        f.extend_from_slice(doc.as_bytes());
        let got = extract(&f).expect("some preview");
        assert!(got.len() >= big.len() - 4, "should pick the larger preview");
    }
}
