//! Serif Affinity (Photo / Designer / Publisher) `.afphoto`/`.afdesign`/`.afpub`
//! and the unified V2/V3 `.af`. These proprietary containers embed a standard
//! PNG preview; we scan for it and slice it out (no rendering, no codecs). There
//! is no official Windows thumbnailer for these — long-requested.
//!
//! Container magic is `00 FF 4B 41` (`\0\xFFKA`) with a varying revision/flags
//! byte after, so we gate only on the 4-byte prefix. The preview is selected as
//! the LAST embedded PNG whose longest edge is ≤ 512 (the thumbnail) — picking
//! "largest" grabs a full-res layer, "smallest" grabs a tiny layer icon.

const PNG_SIG: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
const IEND: [u8; 8] = [0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82];

/// True if `head` looks like an Affinity container.
pub fn looks_like_affinity(head: &[u8]) -> bool {
    head.starts_with(&[0x00, 0xFF, 0x4B, 0x41])
}

/// Extract the embedded thumbnail PNG, or None.
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut best_le512: Option<&[u8]> = None; // last PNG with max edge ≤ 512
    let mut last_any: Option<&[u8]> = None; // fallback: last valid PNG

    let mut i = 0usize;
    while i + PNG_SIG.len() <= bytes.len() {
        if bytes[i..i + 8] != PNG_SIG {
            i += 1;
            continue;
        }
        // Found a PNG header; find its IEND terminator (incl. CRC).
        match find(&bytes[i + 8..], &IEND) {
            Some(rel) => {
                let end = i + 8 + rel + IEND.len();
                let png = &bytes[i..end];
                if png.len() >= 57 {
                    last_any = Some(png);
                    if let Some((w, h)) = ihdr_dims(png) {
                        if w.max(h) <= 512 {
                            best_le512 = Some(png);
                        }
                    }
                }
                i = end; // resume after this PNG (don't rescan inside it)
            }
            None => break, // no terminator → no more complete PNGs
        }
    }
    best_le512.or(last_any).map(|p| p.to_vec())
}

/// PNG IHDR dimensions (bytes 12..16 must be "IHDR"; width/height big-endian).
fn ihdr_dims(png: &[u8]) -> Option<(u32, u32)> {
    if png.get(12..16)? != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(png.get(16..20)?.try_into().ok()?);
    let h = u32::from_be_bytes(png.get(20..24)?.try_into().ok()?);
    Some((w, h))
}

/// First index of `needle` in `hay`.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut b = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(w, h))
            .write_to(&mut std::io::Cursor::new(&mut b), image::ImageFormat::Png)
            .unwrap();
        b
    }

    #[test]
    fn picks_the_thumbnail_png_not_a_full_res_layer() {
        let big = png(1200, 800); // a full-res layer — must be skipped
        let thumb = png(256, 256); // the ≤512 thumbnail — must win
        let mut bytes = vec![0x00, 0xFF, 0x4B, 0x41, 0x09, 0x00]; // Affinity magic
        bytes.extend_from_slice(&big);
        bytes.extend_from_slice(b"some junk between");
        bytes.extend_from_slice(&thumb);

        let got = extract(&bytes).expect("embedded png");
        let d = image::load_from_memory(&got).unwrap();
        assert_eq!((d.width(), d.height()), (256, 256), "should pick the ≤512 preview");
        assert!(extract(b"no png in here").is_none());
    }
}
