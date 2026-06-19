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
/// true length is measured by walking its marker structure ([`jpeg_span_len`]), so
/// a stray `FF D9` inside an APPn/EXIF segment can't truncate the pick. Bounded.
fn largest_jpeg(data: &[u8]) -> Option<&[u8]> {
    let mut best: Option<(usize, usize)> = None;
    let lim = data.len().min(MAX_SCAN);
    let mut i = 0usize;
    let mut seen = 0usize;
    while i + 3 <= lim {
        if data[i] == 0xFF && data[i + 1] == 0xD8 && data[i + 2] == 0xFF {
            if let Some(len) = jpeg_span_len(data, i) {
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

/// Total byte length (`SOI..EOI` inclusive) of the JPEG at `off`, or `None` if it
/// isn't well-formed. Skips marker segments by their declared length and scans the
/// entropy stream with FF-stuffing / restart-marker awareness, so the real EOI is
/// found even past stray `FF D9` bytes in metadata. Fully bounds-checked — never
/// panics under `panic = "abort"`. (Mirrors `decode::jpeg_span_len`.)
fn jpeg_span_len(data: &[u8], off: usize) -> Option<usize> {
    if data.get(off..off.checked_add(2)?)? != [0xFF, 0xD8] {
        return None;
    }
    let mut p = off + 2;
    for _ in 0..4096 {
        if *data.get(p)? != 0xFF {
            return None;
        }
        while *data.get(p)? == 0xFF {
            p = p.checked_add(1)?; // skip 0xFF fill bytes
        }
        let marker = *data.get(p)?;
        p = p.checked_add(1)?;
        match marker {
            0xD9 => return Some(p - off), // EOI
            0xDA => {
                let len = u16::from_be_bytes([*data.get(p)?, *data.get(p + 1)?]) as usize;
                if len < 2 {
                    return None;
                }
                p = p.checked_add(len)?;
                loop {
                    if *data.get(p)? == 0xFF {
                        let n = *data.get(p + 1)?;
                        if n == 0x00 || (0xD0..=0xD7).contains(&n) {
                            p = p.checked_add(2)?; // stuffed FF / restart marker
                            continue;
                        }
                        break;
                    }
                    p = p.checked_add(1)?;
                }
            }
            0x01 | 0xD0..=0xD7 => {} // standalone markers, no payload
            _ => {
                let len = u16::from_be_bytes([*data.get(p)?, *data.get(p + 1)?]) as usize;
                if len < 2 {
                    return None;
                }
                p = p.checked_add(len)?;
            }
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
