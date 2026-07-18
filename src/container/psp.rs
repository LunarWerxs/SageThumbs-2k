//! Corel / JASC Paint Shop Pro `.pspimage` / `.psp`.
//!
//! PSP files keep a pre-flattened preview in a **Composite Image Bank** block
//! (block id 16), and Paint Shop Pro writes that preview as an embedded **JPEG**
//! even when the image's own pixel data uses NONE/RLE/LZ77 compression (verified
//! across real PSP 6 and PSP X/8 saves — uncompressed, RLE, and layered). So we
//! don't need a full PSP channel codec: scope to block 16 and carve the composite
//! JPEG, then let the normal image tier decode it — the same embedded-preview
//! approach as our PSD / Affinity / camera-RAW paths. No official Windows
//! thumbnailer exists for PSP; this is the value.
//!
//! Layout: a 32-byte `"Paint Shop Pro Image File\n\x1A…"` signature, then
//! `u16 major` / `u16 minor`, then a flat list of blocks — each `"~BK\0"` + a
//! little-endian `u16 block-id` + `u32 content-length` + content. We walk the
//! top-level blocks to the Composite Image Bank, then pick the largest JPEG
//! inside it (the full composite; a small thumbnail JPEG may also be present).
//! Everything is bounds-checked and runs under `panic = "abort"`: malformed input
//! yields `None` and the shell falls back to the default icon.

const SIG: &[u8] = b"Paint Shop Pro Image File\n\x1a";
const BK: [u8; 4] = [0x7E, 0x42, 0x4B, 0x00]; // "~BK\0" block-header magic
const COMPOSITE_IMAGE_BANK: u16 = 16;

/// Header bytes before the first block: 32-byte signature + `u16` major + `u16`
/// minor version.
const HEADER_LEN: usize = 32 + 2 + 2;
/// Per-block prefix: `"~BK\0"` (4) + block id (2) + content length (4).
const BLOCK_PREFIX: usize = 10;

/// Bound the block walk / JPEG scan so a hostile or huge PSP can't run away. The
/// composite preview lives early in the file, so this comfortably covers real
/// inputs while capping pathological ones.
const MAX_SCAN: usize = 64 * 1024 * 1024;

/// At most this many JPEG candidates are examined — a crafted run of pseudo-SOI
/// markers can't make the scan loop.
const MAX_JPEGS: usize = 64;

/// True if `head` is a Paint Shop Pro image container.
pub fn looks_like_psp(head: &[u8]) -> bool {
    head.starts_with(SIG)
}

/// Carve the composite-preview JPEG out of the Composite Image Bank, or `None`.
/// The bytes flow back through the normal tiered decoder (`decode_image`).
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    if !looks_like_psp(bytes) {
        return None;
    }
    // Prefer the JPEG inside the Composite Image Bank (the flattened preview);
    // fall back to a whole-file (bounded) scan if the block walk can't find it.
    let (lo, hi) = composite_bank_range(bytes).unwrap_or((HEADER_LEN, bytes.len().min(MAX_SCAN)));
    let jpeg = largest_jpeg(bytes.get(lo..hi)?)?;
    (jpeg.len() >= 64 && jpeg.len() as u64 <= crate::container::MAX_COVER).then(|| jpeg.to_vec())
}

/// Walk the top-level `"~BK\0"` blocks and return the `[start, end)` byte range
/// of the Composite Image Bank block's content. Returns `None` if the framing
/// breaks or the block is absent (caller then scans the whole file).
fn composite_bank_range(b: &[u8]) -> Option<(usize, usize)> {
    let end = b.len().min(MAX_SCAN);
    let mut p = HEADER_LEN;
    while p + BLOCK_PREFIX <= end {
        if b.get(p..p + 4)? != BK {
            return None; // not a block header where one must be → give up, scan whole file
        }
        let id = u16::from_le_bytes([b[p + 4], b[p + 5]]);
        let len = u32::from_le_bytes([b[p + 6], b[p + 7], b[p + 8], b[p + 9]]) as usize;
        let content = p + BLOCK_PREFIX;
        let next = content.checked_add(len)?;
        if next > b.len() {
            return None;
        }
        if id == COMPOSITE_IMAGE_BANK {
            return Some((content, next));
        }
        p = next;
    }
    None
}

/// Largest valid embedded JPEG (`SOI..EOI` inclusive) in `data`. Each candidate's
/// true length is measured by walking its marker structure
/// ([`crate::container::jpeg_span_len`]), so a stray `FF D9` inside an APPn/EXIF
/// segment can't truncate the pick. Bounded.
fn largest_jpeg(data: &[u8]) -> Option<&[u8]> {
    let mut best: Option<(usize, usize)> = None;
    let lim = data.len().min(MAX_SCAN);
    let mut i = 0usize;
    let mut seen = 0usize;
    while i + 3 <= lim {
        if data[i] == 0xFF && data[i + 1] == 0xD8 && data[i + 2] == 0xFF {
            if let Some(len) = crate::container::jpeg_span_len(data, i) {
                if best.is_none_or(|(_, bl)| len > bl) {
                    best = Some((i, len));
                }
                seen += 1;
                if seen >= MAX_JPEGS {
                    break;
                }
                i += len;
                continue;
            }
        }
        i += 1;
    }
    let (start, len) = best?;
    data.get(start..start.checked_add(len)?)
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

    /// Build a minimal PSP: signature + version + one Composite Image Bank block
    /// whose content is `[bank info chunk][a JPEG]`, plus a decoy block before it.
    fn fake_psp(jpeg_bytes: &[u8]) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(SIG);
        f.extend_from_slice(&[0u8; 32 - SIG.len()]); // pad signature to 32
        f.extend_from_slice(&8u16.to_le_bytes()); // major
        f.extend_from_slice(&0u16.to_le_bytes()); // minor
        // Decoy block (General Image Attributes, id 0) of 4 bytes.
        f.extend_from_slice(&BK);
        f.extend_from_slice(&0u16.to_le_bytes());
        f.extend_from_slice(&4u32.to_le_bytes());
        f.extend_from_slice(&[1, 2, 3, 4]);
        // Composite Image Bank (id 16): 8-byte info chunk then the JPEG.
        let mut content = Vec::new();
        content.extend_from_slice(&8u32.to_le_bytes()); // chunk size
        content.extend_from_slice(&1u32.to_le_bytes()); // composite image count
        content.extend_from_slice(jpeg_bytes);
        f.extend_from_slice(&BK);
        f.extend_from_slice(&COMPOSITE_IMAGE_BANK.to_le_bytes());
        f.extend_from_slice(&(content.len() as u32).to_le_bytes());
        f.extend_from_slice(&content);
        f
    }

    #[test]
    fn carves_the_composite_bank_jpeg() {
        let j = jpeg(48, 32);
        let psp = fake_psp(&j);
        let got = extract(&psp).expect("composite JPEG");
        let d = image::load_from_memory(&got).expect("valid JPEG");
        assert_eq!((d.width(), d.height()), (48, 32));
    }

    #[test]
    fn rejects_non_psp() {
        assert!(!looks_like_psp(b"not a psp file at all"));
        assert!(extract(b"PK\x03\x04 zip not psp").is_none());
    }

    /// The whole PSP family (.pspbrush/.psptube/.pspframe/.pspshape/.pspselection/.pspmask)
    /// shares this container, and dispatch is by CONTENT (`container::extract_cover` sniffs
    /// the signature) — so registering those extensions is sufficient and no per-extension
    /// code exists or is needed. This locks BOTH halves of that claim: the bytes decode
    /// through the top-level dispatcher, and every extension is actually registered.
    #[test]
    fn psp_family_extensions_are_registered_and_content_dispatched() {
        let psp = fake_psp(&jpeg(40, 24));
        // Reached via the top-level dispatcher, not psp::extract directly — that is the
        // path a real thumbnail request takes, and the reason the extension is irrelevant.
        let cover = crate::container::extract_cover(&psp).expect("dispatched by magic");
        let bytes = match cover {
            crate::container::CoverOut::Bytes(b) => b,
            _ => panic!("expected raw carved bytes"),
        };
        let d = image::load_from_memory(&bytes).expect("valid JPEG");
        assert_eq!((d.width(), d.height()), (40, 24));

        for ext in ["pspimage", "psp", "pspbrush", "pspframe", "psptube", "pspshape",
                    "pspselection", "pspmask"] {
            assert!(crate::formats::is_known(ext), "{ext} must be registered");
            // Mixed case must match too — Explorer hands us whatever case is on disk.
            assert!(crate::formats::is_known(&ext.to_ascii_uppercase()), "{ext} uppercase");
        }
    }

    /// A `.pspmask` is allowed to be a plain Windows BMP rather than a PSP container. That
    /// needs no special case: the signature check fails and the file falls through to the
    /// normal image tier. This asserts we FAIL CLEANLY (None, no panic) rather than
    /// mis-carving, which is what makes the fall-through safe.
    #[test]
    fn bmp_flavoured_mask_is_not_mistaken_for_a_psp_container() {
        let mut bmp = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(8, 8))
            .write_to(&mut std::io::Cursor::new(&mut bmp), image::ImageFormat::Bmp)
            .unwrap();
        assert!(bmp.starts_with(b"BM"), "test fixture should be a BMP");
        assert!(!looks_like_psp(&bmp));
        assert!(extract(&bmp).is_none());
        // And it still decodes as an image by the normal tier.
        assert!(image::load_from_memory(&bmp).is_ok());
    }

    #[test]
    fn picks_largest_jpeg_when_bank_has_thumbnail_and_composite() {
        // A small thumbnail JPEG followed by the larger composite — we want the
        // composite (better source for downscaling).
        let mut both = jpeg(16, 16);
        both.extend_from_slice(&jpeg(200, 150));
        let psp = fake_psp(&both);
        let got = extract(&psp).expect("a JPEG");
        let d = image::load_from_memory(&got).unwrap();
        assert_eq!((d.width(), d.height()), (200, 150), "should pick the larger composite");
    }
}
